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
use zeroize::Zeroize;

use crate::clock::{Clock, SystemClock};
use crate::error_response;
use crate::observe::Metrics;
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

impl IpClass {
    /// Label of the rate limit metric.
    fn label(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Setup => "setup",
            Self::Reset => "reset",
            Self::Accept => "accept",
        }
    }
}

/// Display name changes a user may make per rate window.
const PROFILE_LIMIT: u32 = 20;

/// FigCraft live operations (open, renew, snapshot, stop) a user may start per
/// rate window.
const FIGCRAFT_LIMIT: u32 = 20;

/// Session, cookie and rate limit settings.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// Server-maintained app catalog, optionally extended by operator config.
    pub app_catalog: Vec<craftcloud_core::apps::AppDefinition>,
    /// A session expires after this long without (recorded) activity.
    pub idle_timeout: Duration,
    /// A session expires this long after login regardless of activity.
    pub absolute_timeout: Duration,
    /// Lifetime of a desktop access token.
    pub device_access_ttl: Duration,
    /// Idle lifetime of a desktop device session.
    pub device_idle_timeout: Duration,
    /// Absolute lifetime of a desktop device session.
    pub device_absolute_timeout: Duration,
    /// Minimum time between recorded activity updates of one session.
    pub touch_interval: Duration,
    /// How long soft-deleted projects and files stay restorable.
    pub trash_retention: Duration,
    /// Largest file an upload session may declare, in bytes.
    pub max_file_size_bytes: u64,
    /// How long an upload session stays open.
    pub upload_ttl: Duration,
    /// Open (created or receiving) upload sessions per user.
    pub uploads_per_user: i64,
    /// Open upload sessions per organization.
    pub uploads_per_org: i64,
    /// Longest wait for the next piece of an upload body.
    pub upload_idle_timeout: Duration,
    /// Longest wait for a download client to take the next chunk.
    pub download_idle_timeout: Duration,
    /// Whether file downloads are recorded as audit events
    /// (`CRAFTCLOUD_AUDIT_DOWNLOADS`, default on).
    pub audit_downloads: bool,
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
    /// Member searches (`GET /organizations/{id}/members?query=`) a user may
    /// run per `rate_window`; limits enumeration of the member directory.
    pub member_search_limit: u32,
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
            app_catalog: craftcloud_core::apps::default_app_catalog(),
            idle_timeout: Duration::from_secs(8 * 3600),
            absolute_timeout: Duration::from_secs(7 * 24 * 3600),
            device_access_ttl: Duration::from_secs(15 * 60),
            device_idle_timeout: Duration::from_secs(30 * 24 * 3600),
            device_absolute_timeout: Duration::from_secs(90 * 24 * 3600),
            touch_interval: Duration::from_secs(60),
            trash_retention: Duration::from_secs(30 * 24 * 3600),
            max_file_size_bytes: 5 * 1024 * 1024 * 1024,
            upload_ttl: Duration::from_secs(24 * 3600),
            uploads_per_user: 10,
            uploads_per_org: 200,
            upload_idle_timeout: Duration::from_secs(60),
            download_idle_timeout: Duration::from_secs(60),
            audit_downloads: true,
            public_url: None,
            public_origin: None,
            cookie_secure: true,
            ip_limit: 10,
            pwchange_limit: 5,
            invite_limit: 30,
            member_search_limit: 60,
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
#[derive(Default)]
pub struct EmailTransportStatus {
    pub transport: Option<String>,
    pub sender: Option<String>,
    pub certificate_expires_at: Option<String>,
    pub last_error_code: Option<String>,
}

pub struct Services {
    pub(crate) pool: PgPool,
    pub(crate) bootstrap: Option<BootstrapToken>,
    pub(crate) two_factor_keys: TwoFactorKeys,
    pub(crate) settings: AuthSettings,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) storage: Option<craftcloud_storage::LocalStore>,
    pub(crate) figcraft: Option<Arc<dyn craftcloud_figcraft::Backend>>,
    pub(crate) figcraft_public_url: Option<String>,
    pub(crate) email_delivery_enabled: bool,
    pub(crate) email_transport_status: Option<Arc<dyn Fn() -> EmailTransportStatus + Send + Sync>>,
    pub(crate) figcraft_permits: Arc<Semaphore>,
    pub(crate) figcraft_job_permits: Arc<Semaphore>,
    hash_permits: Arc<Semaphore>,
    // Separate tables per class: flooding one cannot evict another.
    login_ip: WindowLimiter,
    setup_ip: WindowLimiter,
    reset_ip: WindowLimiter,
    accept_ip: WindowLimiter,
    pwchange: WindowLimiter,
    profile: WindowLimiter,
    invite_create: WindowLimiter,
    member_search: WindowLimiter,
    figcraft_ops: WindowLimiter,
    guard: Arc<AccountGuard>,
    // Separate from login failures: management proofs share one account/address budget.
    two_factor_management_guard: Arc<AccountGuard>,
    last_cleanup: AtomicI64,
    pub(crate) audit_budget: crate::audit::AuditBudget,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) health: crate::health::HealthCache,
    pub(crate) started: std::time::Instant,
    /// A store is configured but was refused at startup (it does not belong to
    /// this database): readiness reports it instead of silently omitting it.
    pub(crate) storage_unusable: bool,
    /// Set when the shutdown signal arrived: readiness answers 503.
    pub(crate) draining: std::sync::atomic::AtomicBool,
}

/// In-memory TOTP encryption keys. Keys are never copied into diagnostics.
pub struct TwoFactorKeys {
    pub(crate) current: Option<[u8; 32]>,
    pub(crate) old: Option<[u8; 32]>,
}

impl Drop for TwoFactorKeys {
    fn drop(&mut self) {
        if let Some(key) = &mut self.current {
            key.zeroize();
        }
        if let Some(key) = &mut self.old {
            key.zeroize();
        }
    }
}

impl Services {
    /// Waits until every HTTP-owned FigCraft operation releases its permit.
    /// Call after HTTP admission has stopped and bound the wait at shutdown.
    pub async fn drain_figcraft_jobs(&self) {
        wait_for_figcraft_jobs(Arc::clone(&self.figcraft_job_permits)).await;
    }

    /// Creates the services. `bootstrap` is `None` when no bootstrap secret
    /// file is configured; setup then answers 403 `INVALID_BOOTSTRAP_TOKEN`
    /// exactly like a wrong token (the reason is only logged).
    #[must_use]
    pub fn new(pool: PgPool, bootstrap: Option<BootstrapToken>, settings: AuthSettings) -> Self {
        let cap = settings.rate_max_entries;
        let pool_connections = pool.options().get_max_connections();
        let guard = AccountGuard::new(GuardConfig {
            pair_limit: settings.failed_login_pair_limit,
            account_limit: settings.failed_login_account_limit,
            window_secs: i64::try_from(settings.failed_login_window.as_secs()).unwrap_or(900),
            max_entries: settings.account_guard_max_entries,
        });
        let two_factor_management_guard = AccountGuard::new(GuardConfig {
            pair_limit: settings.failed_login_pair_limit,
            account_limit: settings.failed_login_account_limit,
            window_secs: i64::try_from(settings.failed_login_window.as_secs()).unwrap_or(900),
            max_entries: settings.account_guard_max_entries,
        });
        Self {
            pool,
            bootstrap,
            two_factor_keys: TwoFactorKeys {
                current: None,
                old: None,
            },
            settings,
            clock: Arc::new(SystemClock),
            storage: None,
            figcraft: None,
            figcraft_public_url: None,
            email_delivery_enabled: false,
            email_transport_status: None,
            figcraft_permits: Arc::new(Semaphore::new(4)),
            figcraft_job_permits: Arc::new(Semaphore::new(crate::figcraft_jobs::MAX_FIGCRAFT_JOBS)),
            hash_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)),
            login_ip: WindowLimiter::new(cap),
            setup_ip: WindowLimiter::new(cap),
            reset_ip: WindowLimiter::new(cap),
            accept_ip: WindowLimiter::new(cap),
            pwchange: WindowLimiter::new(cap),
            profile: WindowLimiter::new(cap),
            invite_create: WindowLimiter::new(cap),
            member_search: WindowLimiter::new(cap),
            figcraft_ops: WindowLimiter::new(cap),
            guard,
            two_factor_management_guard,
            last_cleanup: AtomicI64::new(i64::MIN / 2),
            audit_budget: crate::audit::AuditBudget::for_pool(pool_connections),
            metrics: Arc::new(Metrics::new()),
            health: crate::health::HealthCache::default(),
            started: std::time::Instant::now(),
            storage_unusable: false,
            draining: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Installs server TOTP encryption keys loaded at the process boundary.
    #[must_use]
    pub fn with_two_factor_keys(
        mut self,
        current: Option<[u8; 32]>,
        old: Option<[u8; 32]>,
    ) -> Self {
        self.two_factor_keys = TwoFactorKeys { current, old };
        self
    }

    /// Marks email delivery active after the server validates SMTP settings.
    #[must_use]
    pub fn with_email_delivery(mut self, enabled: bool) -> Self {
        self.email_delivery_enabled = enabled;
        self
    }

    /// Supplies sanitized transport metadata for the operator-only status route.
    pub fn with_email_transport_status(
        mut self,
        read: Arc<dyn Fn() -> EmailTransportStatus + Send + Sync>,
    ) -> Self {
        self.email_transport_status = Some(read);
        self
    }

    /// Whether the server has an active mail transport.
    pub(crate) const fn email_delivery_enabled(&self) -> bool {
        self.email_delivery_enabled
    }

    /// Encrypts a one-use mail capability under the AC-02 key and email AAD.
    pub(crate) fn encrypt_email_token(
        &self,
        owner: Uuid,
        token: &[u8],
    ) -> Result<Vec<u8>, craftcloud_auth::SecretBoxError> {
        let key = self
            .two_factor_keys
            .current
            .as_ref()
            .ok_or(craftcloud_auth::SecretBoxError::InvalidKey)?;
        craftcloud_auth::encrypt_secret_for(key, owner, b"email", token)
    }

    /// Whether a current key is available for setting up new TOTP factors.
    #[must_use]
    pub fn two_factor_available(&self) -> bool {
        self.two_factor_keys.current.is_some()
    }

    /// Attaches the object store; without one, upload routes answer 503
    /// `STORAGE_UNAVAILABLE`.
    #[must_use]
    pub fn with_storage(mut self, store: craftcloud_storage::LocalStore) -> Self {
        self.storage = Some(store);
        self
    }

    /// Configures the FigCraft backend and the operator supplied public URL.
    #[must_use]
    pub fn with_figcraft(
        mut self,
        backend: Arc<dyn craftcloud_figcraft::Backend>,
        public_url: String,
    ) -> Self {
        self.figcraft = Some(backend);
        self.figcraft_public_url = Some(public_url);
        self
    }

    /// FigCraft live is available only when every required local dependency exists.
    #[must_use]
    pub fn figcraft_available(&self) -> bool {
        self.figcraft.is_some()
            && self.figcraft_public_url.is_some()
            && self.two_factor_keys.current.is_some()
            && self.storage.is_some()
    }

    /// The global FigCraft job permits; a hidden seam so integration tests can
    /// exhaust capacity and prove which callers are admitted.
    #[doc(hidden)]
    #[must_use]
    pub fn figcraft_job_permits_for_tests(&self) -> Arc<Semaphore> {
        Arc::clone(&self.figcraft_job_permits)
    }

    /// Runs blocking FigCraft network work on the blocking pool with bounded concurrency.
    pub(crate) async fn run_figcraft<T, F>(&self, job: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        run_blocking(Arc::clone(&self.figcraft_permits), job).await
    }

    /// Records that an object store is configured but refused (binding
    /// mismatch): uploads and downloads answer 503, and readiness fails.
    #[must_use]
    pub fn with_storage_refused(mut self) -> Self {
        self.storage_unusable = true;
        self
    }

    /// Uses `metrics` for the counters of these services. The server passes
    /// the registry that also sits in [`crate::ApiConfig`].
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// The metrics registry these services count into.
    #[must_use]
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// The two budgets of best-effort audit writes (denials and failed logins,
    /// downloads and exports); tests hold their permits to simulate pressure.
    #[must_use]
    pub fn audit_semaphores(&self) -> (Arc<Semaphore>, Arc<Semaphore>) {
        (
            Arc::clone(&self.audit_budget.lossy),
            Arc::clone(&self.audit_budget.important),
        )
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

    /// The ready 429 response with `Retry-After`; counts the rejection.
    fn too_many(&self, class: &str, retry_after: u64, headers: &HeaderMap) -> Box<Response> {
        self.metrics.rate_limited(class);
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
            .map_err(|retry| self.too_many(class.label(), retry, headers))
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
            .map_err(|retry| self.too_many("pwchange", retry, headers))
    }

    /// Per-user rate limit for display name changes (`PROFILE_LIMIT` per
    /// window); counts the request.
    pub(crate) fn rate_limit_profile(
        &self,
        user_id: Uuid,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        self.profile
            .check(
                &user_id.to_string(),
                self.now_secs(),
                PROFILE_LIMIT,
                self.window_secs(),
            )
            .map_err(|retry| self.too_many("profile", retry, headers))
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
            .map_err(|retry| self.too_many("invite", retry, headers))
    }

    /// Per-user rate limit for member searches; counts the request.
    pub(crate) fn rate_limit_member_search(
        &self,
        user_id: Uuid,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        self.member_search
            .check(
                &user_id.to_string(),
                self.now_secs(),
                self.settings.member_search_limit,
                self.window_secs(),
            )
            .map_err(|retry| self.too_many("member_search", retry, headers))
    }

    /// Per-user rate limit for FigCraft live operations; counts the request.
    pub(crate) fn rate_limit_figcraft(
        &self,
        user_id: Uuid,
        headers: &HeaderMap,
    ) -> Result<(), Box<Response>> {
        self.figcraft_ops
            .check(
                &user_id.to_string(),
                self.now_secs(),
                FIGCRAFT_LIMIT,
                self.window_secs(),
            )
            .map_err(|retry| self.too_many("figcraft", retry, headers))
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
            .map_err(|retry| self.too_many("failed_login", retry, headers))
    }

    /// Reserves a failed-proof budget shared by all two-factor management routes.
    pub(crate) fn two_factor_management_reserve(
        &self,
        user_id: Uuid,
        ip: Option<IpAddr>,
        headers: &HeaderMap,
    ) -> Result<Reservation, Box<Response>> {
        self.two_factor_management_guard
            .reserve(&user_id.to_string(), &ip_key(ip), self.now_secs())
            .map_err(|retry| self.too_many("two_factor_management", retry, headers))
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

pub(crate) async fn wait_for_figcraft_jobs(permits: Arc<Semaphore>) {
    if let Ok(permit) = permits
        .acquire_many_owned(crate::figcraft_jobs::MAX_FIGCRAFT_JOBS as u32)
        .await
    {
        drop(permit);
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
