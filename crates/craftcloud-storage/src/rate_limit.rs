//! Shared asynchronous bandwidth pacing for archive network transfers.

use std::{sync::Arc, time::Duration};

use tokio::{sync::Mutex, time::Instant};

/// One shared budget across provider uploads, downloads, and verification reads.
#[derive(Clone)]
pub(crate) struct BandwidthLimit(Option<Arc<RateSchedule>>);

struct RateSchedule {
    bytes_per_second: u64,
    next_slot: Mutex<Instant>,
}

impl BandwidthLimit {
    /// `mbps` uses decimal megabits per second. Zero disables limiting.
    pub(crate) fn from_mbps(mbps: Option<u64>) -> Self {
        let Some(mbps) = mbps.filter(|value| *value > 0) else {
            return Self(None);
        };
        let bytes_per_second = mbps.saturating_mul(125_000);
        Self(Some(Arc::new(RateSchedule {
            bytes_per_second,
            next_slot: Mutex::new(Instant::now()),
        })))
    }

    /// Waits through the shared slot assigned to `bytes`.
    pub(crate) async fn pace(&self, bytes: usize) {
        let Some(schedule) = self.0.as_ref() else {
            return;
        };
        if bytes == 0 {
            return;
        }
        let deadline = {
            let mut next = schedule.next_slot.lock().await;
            let now = Instant::now();
            let start = (*next).max(now);
            let nanos = (bytes as u128)
                .saturating_mul(1_000_000_000)
                .div_ceil(schedule.bytes_per_second as u128)
                .min(u64::MAX as u128) as u64;
            let deadline = start + Duration::from_nanos(nanos);
            *next = deadline;
            deadline
        };
        tokio::time::sleep_until(deadline).await;
    }
}

#[cfg(test)]
mod tests {
    use super::BandwidthLimit;
    use std::time::Instant;

    #[tokio::test]
    async fn single_stream_is_paced() {
        let limit = BandwidthLimit::from_mbps(Some(1));
        let started = Instant::now();
        for _ in 0..4 {
            limit.pace(32 * 1024).await;
        }
        assert!(started.elapsed() >= std::time::Duration::from_millis(950));
    }

    #[tokio::test]
    async fn concurrent_streams_share_budget() {
        let limit = BandwidthLimit::from_mbps(Some(1));
        let started = Instant::now();
        let first = limit.clone();
        let second = limit.clone();
        let ((), ()) = tokio::join!(
            async move {
                for _ in 0..4 {
                    first.pace(32 * 1024).await;
                }
            },
            async move {
                for _ in 0..4 {
                    second.pace(32 * 1024).await;
                }
            },
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(1900));
    }
}
