//! Conservative estimates of the daemon's boot clock (Linux `CLOCK_BOOTTIME`).
//!
//! Input deadlines travel as absolute daemon boot-clock values. The daemon
//! samples its clock after our `Capabilities` request leaves and before the
//! reply arrives, so anchoring the sample at the reply's local arrival time
//! yields a lower bound of the daemon clock at every later local instant. A
//! deadline derived from a lower bound expires on the daemon no later than
//! intended: at most one round trip early, never late.
use std::time::Instant;

/// Allowance for the local monotonic clock running faster than the daemon's.
const DRIFT_PPM: u128 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaemonClock {
    sample_ns: u64,
    /// Local arrival time of the reply that carried `sample_ns`.
    received: Instant,
}

impl DaemonClock {
    pub(crate) fn new(sample_ns: u64, received: Instant) -> Self {
        Self {
            sample_ns,
            received,
        }
    }

    /// A lower bound of the daemon boot clock at local instant `at`.
    pub(crate) fn lower_bound(&self, at: Instant) -> u64 {
        match at.checked_duration_since(self.received) {
            Some(elapsed) => {
                let elapsed = elapsed.as_nanos();
                let guarded = elapsed - elapsed * DRIFT_PPM / 1_000_000;
                self.sample_ns
                    .saturating_add(u64::try_from(guarded).unwrap_or(u64::MAX))
            }
            None => {
                // The sample was taken no later than `received`.
                let before = self.received.duration_since(at).as_nanos();
                let guarded = before + before * DRIFT_PPM / 1_000_000;
                self.sample_ns
                    .saturating_sub(u64::try_from(guarded).unwrap_or(u64::MAX))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn estimates_never_exceed_the_daemon_clock() {
        // The daemon sampled 1_000 at some instant between send and receipt.
        let sent = Instant::now();
        let received = sent + Duration::from_millis(40);
        let clock = DaemonClock::new(1_000_000_000, received);
        // Worst case: the sample was taken just as the request arrived, so the
        // true daemon clock at `received` may already be 40 ms later. The
        // estimate assumes the earliest possible daemon time.
        let later = received + Duration::from_secs(5);
        let estimate = clock.lower_bound(later);
        assert!(estimate <= 1_000_000_000 + 5_000_000_000);
        // The drift guard costs at most 0.05 % of the elapsed interval.
        assert!(estimate >= 1_000_000_000 + 5_000_000_000 - 2_500_000);
        assert_eq!(clock.lower_bound(received), 1_000_000_000);
        assert!(clock.lower_bound(sent) < 1_000_000_000 - 40_000_000 + 1);
    }

    #[test]
    fn earlier_instants_saturate_at_zero() {
        let received = Instant::now() + Duration::from_secs(10);
        let clock = DaemonClock::new(5, received);
        assert_eq!(clock.lower_bound(received - Duration::from_secs(1)), 0);
    }
}
