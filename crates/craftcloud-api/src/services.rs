//! Database-backed services shared by the setup and authentication routes:
//! the pool, bootstrap token, session settings, clock, rate limiters and the
//! bounded pool for CPU- and memory-heavy password hashing.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use craftcloud_auth::BootstrapToken;
use craftcloud_db::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::clock::{Clock, SystemClock};
use crate::error_response;
use crate::ratelimit::{AccountGuard, GuardConfig, Reservation, WindowLimiter, ip_key};

/// Maximum number of concurrent password hash or verify operations (each uses
/// about 19 MiB).
const MAX_CONCURRENT_HASHES: usize = 4;

/// Expired rows are cleaned at most this often per process.
const CLEANUP_INTERVAL_SECS: i64 = 600;

/// Route classes with their own per-address rate limit table.
#[derive(Debug, Clone, Copy)]
pub(crate) enum IpClass {
    Login,
    Setup,
    Reset,
    Accept,
}

/// Session, cookie and rate limit settings.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// A session expires after this long without (recorded) activity.
    pub idle_timeout: Duration,
    /// A session expires this long after login regardless of activity.
    pub absolute_timeout: Duration,
    /// Minimum time between recorded activity updates of one session.
    pub touch_interval: Duration,
    /// How long soft-deleted projects and files stay restorable.
    pub trash_retention: Duration,
    /// Operator-configured public URL without trailing slash; the only
    /// source for absolute URLs (never derived from request headers).
    pub public_url: Option<String>,
    /// Origin (`scheme://host[:port]`) derived from the public URL. Browser
    /// requests with a different `Origin` are rejected; without a configured
    /// origin every request carrying an `Origin` header is rejected.
    pub public_origin: Option<String>,
    /// Whether the session cookie gets the `Secure` attribute and the
    /// `__Host-` prefix. Only disabled when the public URL is plain http on a
    /// loopback host (local development).
    pub cookie_secure: bool,
    /// Requests per `rate_window` and client address (IPv4 exact, IPv6 /64)
    /// for each of the login, setup and reset routes.
    pub ip_limit: u32,
    /// Password change attempts per `rate_window` and user.
    pub pwchange_limit: u32,
    /// Invitations a user may create per `rate_window`.
    pub invite_limit: u32,
    /// Window of the request rate limits.
    pub rate_window: Duration,
    /// Maximum number of tracked keys in each request rate limit table.
    pub rate_max_entries: usize,
    /// Failed logins per account and client address in `failed_login_window`.
    pub failed_login_pair_limit: u32,
    /// Failed logins per account over all addresses in `failed_login_window`.
    pub failed_login_account_limit: u32,
    /// Window of the failed login limits (default 15 minutes).
    pub failed_login_window: Duration,
    /// Maximum number of e-mail addresses tracked for failed logins (one
    /// table for existing and unknown addresses; see `ratelimit` for the
    /// capacity rationale). Full table: new keys are refused, nothing evicted.
    pub account_guard_max_entries: usize,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(8 * 3600),
            absolute_timeout: Duration::from_secs(7 * 24 * 3600),
            touch_interval: Duration::from_secs(60),
            trash_retention: Duration::from_secs(30 * 24 * 3600),
            public_url: None,
            public_origin: None,
            cookie_secure: true,
            ip_limit: 10,
            pwchange_limit: 5,
            invite_limit: 30,
            rate_window: Duration::from_secs(60),
            rate_max_entries: 10_000,
            failed_login_pair_limit: 5,
            failed_login_account_limit: 50,
            failed_login_window: Duration::from_secs(15 * 60),
            account_guard_max_entries: 250_000,
        }
    }
}

impl AuthSettings {
    /// Name of the session cookie.
    #[must_use]
    pub fn cookie_name(&self) -> &'static str {
        if self.cookie_secure {
            "__Host-craftcloud_session"
        } else {
            "craftcloud_session"
        }
    }
}

/// Database-backed services. Create one per server and pass it to
/// [`crate::router_with_services`].
pub struct Services {
    pub(crate) pool: PgPool,
    pub(crate) bootstrap: Option<BootstrapToken>,
    pub(crate) settings: AuthSettings,
    pub(crate) clock: Arc<dyn Clock>,
    hash_permits: Arc<Semaphore>,
    // Separate tables per class: flooding one cannot evict another.
    login_ip: WindowLimiter,
    setup_ip: WindowLimiter,
    reset_ip: WindowLimiter,
    accept_ip: WindowLimiter,
    pwchange: WindowLimiter,
    invite_create: WindowLimiter,
    guard: Arc<AccountGuard>,
    last_cleanup: AtomicI64,
}

impl Services {
    /// Creates the services. `bootstrap` is `None` when no bootstrap secret
    /// file is configured; setup then answers 403 `INVALID_BOOTSTRAP_TOKEN`
    /// exactly like a wrong token (the reason is only logged).
    #[must_use]
    pub fn new(pool: PgPool, bootstrap: Option<BootstrapToken>, settings: AuthSettings) -> Self {
        let cap = settings.rate_max_entries;
        let guard = AccountGuard::new(GuardConfig {
            pair_limit: settings.failed_login_pair_limit,
            account_limit: settings.failed_login_account_limit,
            window_secs: i64::try_from(settings.failed_login_window.as_secs()).unwrap_or(900),
            max_entries: settings.account_guard_max_entries,
        });
        Self {
            pool,
            bootstrap,
            settings,
            clock: Arc::new(SystemClock),
            hash_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)),
            login_ip: WindowLimiter::new(cap),
            setup_ip: WindowLimiter::new(cap),
            reset_ip: WindowLimiter::new(cap),
            accept_ip: WindowLimiter::new(cap),
            pwchange: WindowLimiter::new(cap),
            invite_create: WindowLimiter::new(cap),
            guard,
            last_cleanup: AtomicI64::new(i64::MIN / 2),
        }
    }

    /// Replaces the clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) fn now_secs(&self) -> i64 {
        self.clock.now().unix_timestamp()
    }

    fn window_secs(&self) -> i64 {
        i64::try_from(self.settings.rate_window.as_secs()).unwrap_or(60)
    }

    /// The ready 429 response with `Retry-After`.
    fn too_many(retry_after: u64, headers: &HeaderMap) -> Box<Response> {
        let mut response = error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "Too many requests. Try again later.",
            headers,
        );
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        Box::new(response)
    }

    /// Per-address rate limit of a route class; counts the request.
    pub(crate) fn rate_limit_ip(
        &self,
        class: IpClass,
        ip: Option<IpAddr>,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        let limiter = match class {
            IpClass::Login => &self.login_ip,
            IpClass::Setup => &self.setup_ip,
            IpClass::Reset => &self.reset_ip,
            IpClass::Accept => &self.accept_ip,
        };
        limiter
            .check(
                &ip_key(ip),
                self.now_secs(),
                self.settings.ip_limit,
                self.window_secs(),
            )
            .map_err(|retry| Self::too_many(retry, headers))
    }

    /// Per-user rate limit for password change attempts; counts the request.
    pub(crate) fn rate_limit_pwchange(
        &self,
        user_id: Uuid,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        self.pwchange
            .check(
                &user_id.to_string(),
                self.now_secs(),
                self.settings.pwchange_limit,
                self.window_secs(),
            )
            .map_err(|retry| Self::too_many(retry, headers))
    }

    /// Per-user rate limit for creating invitations; counts the request.
    pub(crate) fn rate_limit_invite(
        &self,
        user_id: Uuid,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        self.invite_create
            .check(
                &user_id.to_string(),
                self.now_secs(),
                self.settings.invite_limit,
                self.window_secs(),
            )
            .map_err(|retry| Self::too_many(retry, headers))
    }

    /// Checks the failed-login limits for `email` (the normalized address of
    /// every login attempt, known or not) and reserves the attempt atomically.
    /// The returned reservation must be finished with `record_failure` or
    /// `record_success`; dropping it releases it.
    pub(crate) fn login_reserve(
        &self,
        email: &str,
        ip: Option<IpAddr>,
        headers: &HeaderMap,
    ) -> Result<Reservation, Box<Response>> {
        self.guard
            .reserve(email, &ip_key(ip), self.now_secs())
            .map_err(|retry| Self::too_many(retry, headers))
    }

    /// True at most once per [`CLEANUP_INTERVAL_SECS`] per process.
    pub(crate) fn cleanup_due(&self) -> bool {
        let now = self.now_secs();
        let last = self.last_cleanup.load(Ordering::Relaxed);
        now.saturating_sub(last) >= CLEANUP_INTERVAL_SECS
            && self
                .last_cleanup
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    /// Runs CPU- and memory-heavy work on the blocking pool under a
    /// concurrency permit. See [`run_blocking`].
    pub(crate) async fn run_blocking<T, F>(&self, job: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        run_blocking(self.hash_permits.clone(), job).await
    }
}

/// Runs a blocking job under a concurrency permit.
///
/// The owned permit moves into the blocking closure, so it is released only
/// when the CPU- and memory-heavy job has really finished. If the request
/// future is dropped (for example by the handler timeout) the job keeps
/// running and keeps counting against the limit.
pub(crate) async fn run_blocking<T, F>(permits: Arc<Semaphore>, job: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = permits.acquire_owned().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let result = job();
        drop(permit);
        result
    })
    .await
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permit_is_held_until_the_blocking_job_ends_even_if_the_caller_is_dropped() {
        let permits = Arc::new(Semaphore::new(1));
        let slow = || {
            std::thread::sleep(Duration::from_millis(400));
            "hash".to_owned()
        };
        // The caller gives up (like the handler timeout does) while the job runs.
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            run_blocking(permits.clone(), slow),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the request future must have been dropped"
        );
        assert_eq!(
            permits.available_permits(),
            0,
            "permit leaked with the dropped future"
        );

        // Released only after the blocking job completed.
        let mut released = false;
        for _ in 0..100 {
            if permits.available_permits() == 1 {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(released, "permit was never released");
    }

    #[tokio::test]
    async fn permit_is_released_after_the_job_returns() {
        let permits = Arc::new(Semaphore::new(1));
        assert_eq!(run_blocking(permits.clone(), || 7).await, Some(7));
        assert_eq!(permits.available_permits(), 1);
    }

    #[test]
    fn cookie_name_depends_on_the_secure_flag() {
        let mut settings = AuthSettings::default();
        assert_eq!(settings.cookie_name(), "__Host-craftcloud_session");
        settings.cookie_secure = false;
        assert_eq!(settings.cookie_name(), "craftcloud_session");
    }
}
