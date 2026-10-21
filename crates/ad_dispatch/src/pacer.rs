//! Launch pacing (GAPS.md DIAL-3). Calls run at the same time, but they start
//! at most once per `interval`. Each launch reserves its own start time, so
//! tasks that wait together do not start in a burst. An idle pacer saves no
//! credit: after a pause, the next start is now, not a burst of missed starts.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Pacer {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Pacer {
            interval,
            next: Mutex::new(None),
        }
    }

    /// Reserve the next start time at or after `now`.
    pub fn reserve(&self, now: Instant) -> Instant {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let slot = match *next {
            Some(n) if n > now => n,
            _ => now,
        };
        *next = Some(slot + self.interval);
        slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_reserved_together_are_spaced_by_the_interval() {
        let p = Pacer::new(Duration::from_millis(100));
        let now = Instant::now();
        let slots: Vec<Duration> = (0..4).map(|_| p.reserve(now) - now).collect();
        assert_eq!(
            slots,
            [0, 100, 200, 300].map(Duration::from_millis).to_vec()
        );
    }

    #[test]
    fn an_idle_pacer_does_not_burst() {
        let p = Pacer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        p.reserve(t0);
        let later = t0 + Duration::from_secs(10);
        assert_eq!(p.reserve(later), later);
        assert_eq!(p.reserve(later), later + Duration::from_millis(100));
    }
}
