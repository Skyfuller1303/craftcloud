//! Time source, injectable so tests can move time.

use time::OffsetDateTime;

/// Source of the current time.
pub trait Clock: Send + Sync {
    /// The current UTC time.
    fn now(&self) -> OffsetDateTime;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}
