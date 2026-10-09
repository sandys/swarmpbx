//! Circuit breaker (PLAN.md §0.10.6 / §9). The AI-native analog of abandonment
//! control: pause a campaign when the call failure rate in a rolling window
//! exceeds a threshold. Pure and deterministic — it holds a window of outcomes
//! and answers "is the circuit open?".

use chrono::{DateTime, Duration, Utc};

/// Breaker configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Open when failure_rate > threshold (fraction 0.0..=1.0).
    pub threshold: f64,
    /// Rolling window.
    pub window: Duration,
    /// Minimum calls in the window before the breaker can trip (avoids tripping
    /// on a tiny sample).
    pub min_calls: u64,
}

impl Default for Config {
    fn default() -> Self {
        // Matches the dograh defaults studied in research: 50% over 120s, min 5.
        Config {
            threshold: 0.5,
            window: Duration::seconds(120),
            min_calls: 5,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Outcome {
    at: DateTime<Utc>,
    failed: bool,
}

/// A rolling-window breaker for one campaign.
#[derive(Debug, Clone, Default)]
pub struct CircuitBreaker {
    outcomes: Vec<Outcome>,
}

/// Stats at a point in time, useful for logging/reporting the trip reason.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub failures: u64,
    pub total: u64,
    pub failure_rate: f64,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a call outcome.
    pub fn record(&mut self, at: DateTime<Utc>, failed: bool) {
        self.outcomes.push(Outcome { at, failed });
    }

    fn stats_in_window(&self, now: DateTime<Utc>, window: Duration) -> Stats {
        let cutoff = now - window;
        let mut failures = 0u64;
        let mut total = 0u64;
        for o in &self.outcomes {
            if o.at >= cutoff && o.at <= now {
                total += 1;
                if o.failed {
                    failures += 1;
                }
            }
        }
        let failure_rate = if total == 0 {
            0.0
        } else {
            failures as f64 / total as f64
        };
        Stats {
            failures,
            total,
            failure_rate,
        }
    }

    /// Whether the circuit is open (campaign should pause). Returns the stats
    /// used for the decision so callers can log the reason.
    pub fn is_open(&self, now: DateTime<Utc>, config: &Config) -> (bool, Stats) {
        let stats = self.stats_in_window(now, config.window);
        let open = stats.total >= config.min_calls && stats.failure_rate > config.threshold;
        (open, stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    fn cfg() -> Config {
        Config {
            threshold: 0.5,
            window: Duration::seconds(120),
            min_calls: 5,
        }
    }

    #[test]
    fn trips_when_failure_rate_exceeds_threshold() {
        let mut cb = CircuitBreaker::new();
        // 6 calls, 4 failed = 66% > 50%, and >= min 5.
        for i in 0..2 {
            cb.record(t(i), false);
        }
        for i in 2..6 {
            cb.record(t(i), true);
        }
        let (open, stats) = cb.is_open(t(6), &cfg());
        assert!(open);
        assert_eq!(stats.total, 6);
        assert_eq!(stats.failures, 4);
    }

    #[test]
    fn does_not_trip_below_min_calls_even_if_all_fail() {
        let mut cb = CircuitBreaker::new();
        // 4 calls all failed, but min_calls is 5 -> not open.
        for i in 0..4 {
            cb.record(t(i), true);
        }
        let (open, _) = cb.is_open(t(4), &cfg());
        assert!(!open);
    }

    #[test]
    fn does_not_trip_at_or_below_threshold() {
        let mut cb = CircuitBreaker::new();
        // 10 calls, exactly 5 failed = 50%, not > 50% -> not open.
        for i in 0..5 {
            cb.record(t(i), false);
        }
        for i in 5..10 {
            cb.record(t(i), true);
        }
        let (open, stats) = cb.is_open(t(10), &cfg());
        assert!(!open);
        assert_eq!(stats.failure_rate, 0.5);
    }

    #[test]
    fn window_excludes_old_outcomes() {
        let mut cb = CircuitBreaker::new();
        // Old failures outside the 120s window must not count.
        for i in 0..10 {
            cb.record(t(i), true);
        }
        // Now is 1000s later; all those are outside the 120s window.
        let (open, stats) = cb.is_open(t(1000), &cfg());
        assert!(!open);
        assert_eq!(stats.total, 0);
    }

    #[test]
    fn empty_breaker_is_closed() {
        let cb = CircuitBreaker::new();
        let (open, stats) = cb.is_open(t(0), &cfg());
        assert!(!open);
        assert_eq!(stats.total, 0);
    }
}
