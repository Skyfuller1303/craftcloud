//! In-memory, bounded rate limiting.
//!
//! Two building blocks, both single-instance only: counters live in process
//! memory, so limits are per server process and are cleared by a restart
//! (which is also the operator's remedy if a legitimate account got blocked).
//!
//! * [`WindowLimiter`]: fixed-window request counter for IP addresses (IPv6
//!   grouped by /64) and similar evictable keys. Expired windows are swept
//!   before anything is evicted; only if the table is still full the oldest
//!   window is dropped. Each limiter class has its own table, so flooding one
//!   class cannot evict the counters of another.
//! * [`AccountGuard`]: counts only **failed** logins, keyed by the normalized
//!   e-mail address for every login attempt, whether or not an account with
//!   that address exists, so the 401/429 pattern cannot reveal existence. One
//!   table, one policy: counters whose window is active are never evicted;
//!   expired entries are swept first; if the table is still full, **new keys
//!   fail closed** (429) instead of evicting anything. An attempt *reserves*
//!   its slot atomically when it is checked, so concurrent attempts cannot all
//!   pass the check before any failure is recorded; the reservation turns into
//!   a failure, or is released on success or when the request is dropped.
//!
//! Capacity: 250 000 e-mail keys by default. Filling the table needs 250 000
//! distinct failing addresses within one 15 minute window, i.e. more than 270
//! verifications per second; password verification (Argon2, at most four at a
//! time) tops out at roughly 100 per second, so the table cannot be filled
//! within a window through the login route. Memory: about 200 bytes per
//! entry typically (key, counters, small per-address map), at most about 1.5
//! KiB if all 16 per-address counters are in use, so roughly 50 MB typical and
//! 400 MB worst case at the cap.
//!
//! Residual risk: roughly ten addresses (or one /56 of IPv6 space, which is
//! 256 distinct /64 prefixes) are enough to push a real account over the
//! global per-account ceiling (50 failures per 15 minutes) and lock its owner
//! out for the window. There is no hard lockout, and a server restart clears
//! all counters (the operator's remedy).

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

/// Maximum number of per-IP counters kept for one account.
const PAIR_CAP_PER_ACCOUNT: usize = 16;

/// Rate limit key for a client address: the full IPv4 address, the /64
/// prefix of an IPv6 address, or `unknown` without address information.
pub(crate) fn ip_key(ip: Option<IpAddr>) -> String {
    match ip.map(|ip| ip.to_canonical()) {
        None => "unknown".to_owned(),
        Some(IpAddr::V4(v4)) => v4.to_string(),
        Some(IpAddr::V6(v6)) => {
            let [a, b, c, d, ..] = v6.segments();
            format!("{a:x}:{b:x}:{c:x}:{d:x}::/64")
        }
    }
}

// ---- WindowLimiter ---------------------------------------------------------

pub(crate) struct WindowLimiter {
    inner: Mutex<WindowInner>,
    cap: usize,
}

#[derive(Default)]
struct WindowInner {
    /// key -> (window start in unix seconds, requests in window, sequence)
    entries: HashMap<String, (i64, u32, u64)>,
    /// Entries ordered by (window start, sequence) for sweeping and eviction.
    by_age: BTreeMap<(i64, u64), String>,
    next_seq: u64,
}

impl WindowLimiter {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(WindowInner::default()),
            cap: cap.max(1),
        }
    }

    /// Counts one request for `key`. Returns `Err(retry_after_secs)` (at
    /// least 1) when more than `limit` requests happened in the current
    /// window of `window_secs` seconds.
    pub(crate) fn check(
        &self,
        key: &str,
        now: i64,
        limit: u32,
        window_secs: i64,
    ) -> Result<(), u64> {
        let window = window_secs.max(1);
        if limit == 0 {
            return Err(window.unsigned_abs());
        }
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.sweep(now, window);
        if let Some(entry) = inner.entries.get_mut(key) {
            if entry.1 >= limit {
                let remaining = window.saturating_sub(now.saturating_sub(entry.0)).max(1);
                return Err(remaining.unsigned_abs());
            }
            entry.1 += 1;
            return Ok(());
        }
        while inner.entries.len() >= self.cap {
            let Some((_, oldest)) = inner.by_age.pop_first() else {
                break;
            };
            inner.entries.remove(&oldest);
        }
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.wrapping_add(1);
        inner.entries.insert(key.to_owned(), (now, 1, seq));
        inner.by_age.insert((now, seq), key.to_owned());
        Ok(())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }
}

impl WindowInner {
    /// Removes every entry whose window has ended.
    fn sweep(&mut self, now: i64, window: i64) {
        while let Some((&(start, _), _)) = self.by_age.first_key_value() {
            if now.saturating_sub(start) < window {
                break;
            }
            if let Some((_, key)) = self.by_age.pop_first() {
                self.entries.remove(&key);
            }
        }
    }
}

// ---- AccountGuard ----------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub(crate) struct GuardConfig {
    /// Failures per (account, client address) in the window.
    pub(crate) pair_limit: u32,
    /// Failures per account over all addresses in the window.
    pub(crate) account_limit: u32,
    pub(crate) window_secs: i64,
    /// Maximum number of tracked e-mail keys.
    pub(crate) max_entries: usize,
}

#[derive(Clone, Copy)]
struct Counter {
    start: i64,
    count: u32,
}

impl Counter {
    fn active(&self, now: i64, window: i64) -> bool {
        self.count > 0 && now.saturating_sub(self.start) < window
    }

    /// Failures counted in the current window (0 if the window ended).
    fn current(&self, now: i64, window: i64) -> u32 {
        if self.active(now, window) {
            self.count
        } else {
            0
        }
    }

    fn remaining(&self, now: i64, window: i64) -> u64 {
        window
            .saturating_sub(now.saturating_sub(self.start))
            .max(1)
            .unsigned_abs()
    }

    fn bump(&mut self, now: i64, window: i64) {
        if self.active(now, window) {
            self.count = self.count.saturating_add(1);
        } else {
            *self = Counter {
                start: now,
                count: 1,
            };
        }
    }
}

#[derive(Clone, Copy)]
struct PairState {
    failures: Counter,
    /// Attempts that passed the check and have not finished yet.
    pending: u32,
}

struct EmailState {
    global: Counter,
    pending: u32,
    pairs: HashMap<String, PairState>,
}

impl EmailState {
    fn new() -> Self {
        Self {
            global: Counter { start: 0, count: 0 },
            pending: 0,
            pairs: HashMap::new(),
        }
    }

    /// Whether the entry may be swept: no active failures, nothing in flight.
    fn idle(&self, now: i64, window: i64) -> bool {
        self.pending == 0
            && !self.global.active(now, window)
            && self
                .pairs
                .values()
                .all(|p| p.pending == 0 && !p.failures.active(now, window))
    }

    /// Seconds to wait if another attempt from `ip` must be refused now.
    /// Counts reserved (in flight) attempts like failures.
    fn refusal(&self, ip: &str, now: i64, cfg: &GuardConfig) -> Option<u64> {
        let mut retry: Option<u64> = None;
        let global = self
            .global
            .current(now, cfg.window_secs)
            .saturating_add(self.pending);
        if global >= cfg.account_limit {
            retry = Some(self.global.remaining(now, cfg.window_secs));
        }
        if let Some(pair) = self.pairs.get(ip) {
            let used = pair
                .failures
                .current(now, cfg.window_secs)
                .saturating_add(pair.pending);
            if used >= cfg.pair_limit {
                let r = pair.failures.remaining(now, cfg.window_secs);
                retry = Some(retry.map_or(r, |existing| existing.max(r)));
            }
        }
        retry
    }

    /// Makes room for a new per-address entry without touching entries that
    /// are active or in flight. Returns whether there is room.
    fn pair_room(&mut self, now: i64, cfg: &GuardConfig) -> bool {
        if self.pairs.len() < PAIR_CAP_PER_ACCOUNT {
            return true;
        }
        self.pairs
            .retain(|_, p| p.pending > 0 || p.failures.active(now, cfg.window_secs));
        self.pairs.len() < PAIR_CAP_PER_ACCOUNT
    }
}

#[derive(Default)]
struct GuardInner {
    entries: HashMap<String, EmailState>,
}

pub(crate) struct AccountGuard {
    inner: Mutex<GuardInner>,
    cfg: GuardConfig,
}

/// A reserved login attempt. It counts against the limits from the moment it
/// was granted. Finish it with [`Reservation::record_failure`] or
/// [`Reservation::record_success`]; dropping it (for example because the
/// request timed out or failed before the password was checked) just releases
/// the reservation.
pub(crate) struct Reservation {
    guard: Arc<AccountGuard>,
    email: String,
    ip: String,
    /// Whether a per-address slot was reserved (it can be missing if the
    /// per-account address table was full of active entries).
    pair_reserved: bool,
    finished: bool,
}

impl Reservation {
    /// The attempt failed: the reservation becomes a counted failure.
    pub(crate) fn record_failure(mut self, now: i64) {
        self.finished = true;
        self.guard
            .finish(&self.email, &self.ip, self.pair_reserved, Some(now));
    }

    /// The attempt succeeded: the reservation is released and the failure
    /// counter of this address is reset.
    pub(crate) fn record_success(mut self) {
        self.finished = true;
        self.guard
            .finish(&self.email, &self.ip, self.pair_reserved, None);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.finished {
            self.guard
                .release(&self.email, &self.ip, self.pair_reserved);
        }
    }
}

impl AccountGuard {
    pub(crate) fn new(cfg: GuardConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GuardInner::default()),
            cfg,
        })
    }

    /// Checks the limits for a login attempt for `email` from `ip` and, if
    /// allowed, reserves the attempt in the same critical section. Returns
    /// the seconds to wait when refused (also when the table is full of
    /// active entries and this would be a new key: fail closed).
    pub(crate) fn reserve(
        self: &Arc<Self>,
        email: &str,
        ip: &str,
        now: i64,
    ) -> Result<Reservation, u64> {
        let cfg = self.cfg;
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if !inner.entries.contains_key(email) {
            if inner.entries.len() >= cfg.max_entries {
                inner.entries.retain(|_, e| !e.idle(now, cfg.window_secs));
            }
            if inner.entries.len() >= cfg.max_entries {
                return Err(cfg.window_secs.max(1).unsigned_abs());
            }
            inner.entries.insert(email.to_owned(), EmailState::new());
        }
        let Some(state) = inner.entries.get_mut(email) else {
            return Err(cfg.window_secs.max(1).unsigned_abs());
        };
        if let Some(retry) = state.refusal(ip, now, &cfg) {
            return Err(retry);
        }
        state.pending = state.pending.saturating_add(1);
        let pair_reserved = if let Some(pair) = state.pairs.get_mut(ip) {
            pair.pending = pair.pending.saturating_add(1);
            true
        } else if state.pair_room(now, &cfg) {
            state.pairs.insert(
                ip.to_owned(),
                PairState {
                    failures: Counter { start: 0, count: 0 },
                    pending: 1,
                },
            );
            true
        } else {
            false
        };
        drop(inner);
        Ok(Reservation {
            guard: Arc::clone(self),
            email: email.to_owned(),
            ip: ip.to_owned(),
            pair_reserved,
            finished: false,
        })
    }

    fn release(&self, email: &str, ip: &str, pair_reserved: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(state) = inner.entries.get_mut(email) {
            state.pending = state.pending.saturating_sub(1);
            if pair_reserved && let Some(pair) = state.pairs.get_mut(ip) {
                pair.pending = pair.pending.saturating_sub(1);
            }
        }
    }

    /// Releases the reservation and either counts a failure (`failed_at`) or
    /// resets the address's failure counter (success).
    fn finish(&self, email: &str, ip: &str, pair_reserved: bool, failed_at: Option<i64>) {
        let window = self.cfg.window_secs;
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(state) = inner.entries.get_mut(email) else {
            return;
        };
        state.pending = state.pending.saturating_sub(1);
        if pair_reserved && let Some(pair) = state.pairs.get_mut(ip) {
            pair.pending = pair.pending.saturating_sub(1);
        }
        match failed_at {
            Some(now) => {
                state.global.bump(now, window);
                if let Some(pair) = state.pairs.get_mut(ip) {
                    pair.failures.bump(now, window);
                }
            }
            None => {
                if let Some(pair) = state.pairs.get_mut(ip) {
                    pair.failures.count = 0;
                }
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ip_key ----

    #[test]
    fn ipv4_is_exact_and_ipv6_is_grouped_by_64() {
        let v4 = |s: &str| ip_key(Some(s.parse().unwrap()));
        assert_eq!(v4("203.0.113.7"), "203.0.113.7");
        assert_ne!(v4("203.0.113.7"), v4("203.0.113.8"));
        // Same /64, different interface identifiers.
        assert_eq!(
            v4("2001:db8:1:2::1"),
            v4("2001:db8:1:2:ffff:ffff:ffff:ffff")
        );
        assert_eq!(v4("2001:db8:1:2::1"), v4("2001:0db8:0001:0002:0:0:0:9"));
        // Different /64.
        assert_ne!(v4("2001:db8:1:2::1"), v4("2001:db8:1:3::1"));
        // IPv4-mapped IPv6 behaves like the IPv4 address.
        assert_eq!(v4("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(ip_key(None), "unknown");
    }

    // ---- WindowLimiter ----

    #[test]
    fn allows_up_to_the_limit_then_reports_retry_after() {
        let limiter = WindowLimiter::new(100);
        for _ in 0..3 {
            assert_eq!(limiter.check("k", 1000, 3, 60), Ok(()));
        }
        assert_eq!(limiter.check("k", 1010, 3, 60), Err(50));
        assert_eq!(limiter.check("k", 1059, 3, 60), Err(1));
        assert_eq!(limiter.check("other", 1010, 3, 60), Ok(()));
    }

    #[test]
    fn window_rolls_over_and_the_rolled_key_moves_to_the_back() {
        let limiter = WindowLimiter::new(2);
        assert_eq!(limiter.check("a", 0, 1, 60), Ok(()));
        assert_eq!(limiter.check("b", 10, 1, 60), Ok(()));
        // "a" rolls into a new window at t=60 and becomes the newest entry.
        assert_eq!(limiter.check("a", 60, 1, 60), Ok(()));
        // At t=61 "b" (window start 10) is expired and swept, not "a".
        assert_eq!(limiter.check("c", 71, 1, 60), Ok(()));
        assert!(
            limiter.check("a", 72, 1, 60).is_err(),
            "a must still be tracked"
        );
    }

    #[test]
    fn zero_limit_always_blocks() {
        assert_eq!(WindowLimiter::new(10).check("k", 0, 0, 60), Err(60));
    }

    #[test]
    fn expired_windows_are_swept_before_active_ones_are_evicted() {
        let limiter = WindowLimiter::new(3);
        assert_eq!(limiter.check("old1", 0, 1, 60), Ok(()));
        assert_eq!(limiter.check("old2", 1, 1, 60), Ok(()));
        assert_eq!(limiter.check("active", 100, 1, 60), Ok(()));
        // Table is full of two expired and one active entry; a new key sweeps
        // the expired ones and must not evict "active".
        assert_eq!(limiter.check("new", 110, 1, 60), Ok(()));
        assert!(limiter.check("active", 111, 1, 60).is_err());
        assert_eq!(limiter.len(), 2);
    }

    #[test]
    fn tracked_keys_are_bounded() {
        let limiter = WindowLimiter::new(3);
        for i in 0..1000 {
            let _ = limiter.check(&format!("flood{i}"), 5, 1, 60);
            assert!(limiter.len() <= 3);
        }
    }

    // ---- AccountGuard ----

    fn cfg() -> GuardConfig {
        GuardConfig {
            pair_limit: 5,
            account_limit: 50,
            window_secs: 900,
            max_entries: 100,
        }
    }

    fn fail(guard: &Arc<AccountGuard>, email: &str, ip: &str, now: i64) -> bool {
        match guard.reserve(email, ip, now) {
            Ok(reservation) => {
                reservation.record_failure(now);
                true
            }
            Err(_) => false,
        }
    }

    #[test]
    fn pair_limit_blocks_one_address_but_not_another() {
        let guard = AccountGuard::new(cfg());
        for _ in 0..5 {
            assert!(fail(&guard, "a@x.test", "1.1.1.1", 100));
        }
        assert_eq!(guard.reserve("a@x.test", "1.1.1.1", 101).err(), Some(899));
        // The correct password from a different address is still allowed.
        assert!(guard.reserve("a@x.test", "2.2.2.2", 101).is_ok());
        // Released after the window.
        assert!(guard.reserve("a@x.test", "1.1.1.1", 1000).is_ok());
    }

    #[test]
    fn success_releases_and_resets_the_address_counter() {
        let guard = AccountGuard::new(cfg());
        for _ in 0..4 {
            assert!(fail(&guard, "a@x.test", "1.1.1.1", 100));
        }
        guard
            .reserve("a@x.test", "1.1.1.1", 101)
            .unwrap()
            .record_success();
        for _ in 0..4 {
            assert!(fail(&guard, "a@x.test", "1.1.1.1", 102));
        }
        assert!(guard.reserve("a@x.test", "1.1.1.1", 103).is_ok());
    }

    #[test]
    fn concurrent_attempts_beyond_the_limit_cannot_all_pass_the_check() {
        let guard = AccountGuard::new(cfg());
        let passed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let kept = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(std::sync::Barrier::new(64));
        let handles: Vec<_> = (0..64)
            .map(|_| {
                let (guard, passed, kept, barrier) =
                    (guard.clone(), passed.clone(), kept.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    // All attempts are in flight (their password checks have not
                    // finished) when the limit is evaluated.
                    if let Ok(reservation) = guard.reserve("race@x.test", "9.9.9.9", 100) {
                        passed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        kept.lock().unwrap().push(reservation);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            passed.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "at most pair_limit may pass"
        );
        // Once they fail, nothing more gets through either.
        let held: Vec<_> = std::mem::take(&mut *kept.lock().unwrap());
        for reservation in held {
            reservation.record_failure(100);
        }
        assert!(guard.reserve("race@x.test", "9.9.9.9", 101).is_err());
    }

    #[test]
    fn global_ceiling_also_counts_attempts_in_flight() {
        let mut config = cfg();
        config.pair_limit = 1000;
        config.account_limit = 3;
        let guard = AccountGuard::new(config);
        let held: Vec<_> = (0..3)
            .map(|i| {
                guard
                    .reserve("a@x.test", &format!("10.0.0.{i}"), 100)
                    .unwrap()
            })
            .collect();
        assert!(guard.reserve("a@x.test", "10.0.0.99", 100).is_err());
        drop(held); // released without counting
        assert!(guard.reserve("a@x.test", "10.0.0.99", 100).is_ok());
    }

    #[test]
    fn dropping_a_reservation_releases_it_without_counting_a_failure() {
        let guard = AccountGuard::new(cfg());
        for _ in 0..50 {
            let reservation = guard.reserve("a@x.test", "1.1.1.1", 100).unwrap();
            drop(reservation); // timeout / early error
        }
        assert!(guard.reserve("a@x.test", "1.1.1.1", 100).is_ok());
    }

    #[test]
    fn global_ceiling_blocks_every_address_for_that_account() {
        let guard = AccountGuard::new(cfg());
        for i in 0..50 {
            assert!(fail(&guard, "a@x.test", &format!("10.0.0.{i}"), 100));
        }
        assert!(guard.reserve("a@x.test", "99.99.99.99", 101).is_err());
        assert!(guard.reserve("b@x.test", "99.99.99.99", 101).is_ok());
    }

    #[test]
    fn full_table_fails_closed_for_new_keys_and_never_evicts_active_ones() {
        let mut config = cfg();
        config.max_entries = 10;
        let guard = AccountGuard::new(config);
        // Fill with active counters: ten different e-mail addresses failing.
        for i in 0..10 {
            assert!(fail(&guard, &format!("k{i}@x.test"), "1.1.1.1", 100));
        }
        // Whether or not an account exists for the new address, it is refused
        // in exactly the same way, and active counters stay intact.
        for i in 0..5_000 {
            let key = format!("flood{i}@x.test");
            assert!(guard.reserve(&key, "2.2.2.2", 200).is_err());
        }
        assert_eq!(guard.len(), 10);
        for _ in 0..4 {
            assert!(fail(&guard, "k0@x.test", "1.1.1.1", 200));
        }
        assert!(
            guard.reserve("k0@x.test", "1.1.1.1", 201).is_err(),
            "pair limit of the old counter still applies"
        );
        // After the windows end the table is swept and new keys work again.
        assert!(guard.reserve("fresh@x.test", "2.2.2.2", 100 + 901).is_ok());
    }

    #[test]
    fn idle_entries_are_swept_before_failing_closed() {
        let mut config = cfg();
        config.max_entries = 3;
        let guard = AccountGuard::new(config);
        for i in 0..3 {
            // Successful logins leave only idle entries behind.
            guard
                .reserve(&format!("s{i}@x.test"), "1.1.1.1", 100)
                .unwrap()
                .record_success();
        }
        assert!(guard.reserve("new@x.test", "1.1.1.1", 101).is_ok());
    }

    #[test]
    fn per_account_address_table_is_bounded() {
        let guard = AccountGuard::new(cfg());
        for i in 0..1000 {
            let _ = fail(&guard, "a@x.test", &format!("addr{i}"), 100);
        }
        let inner = guard.inner.lock().unwrap();
        let state = inner.entries.get("a@x.test").unwrap();
        assert!(state.pairs.len() <= PAIR_CAP_PER_ACCOUNT);
    }
}
