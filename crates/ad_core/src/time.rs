//! A `Clock` abstraction so time-dependent logic (critically, the compliance
//! safe-hours engine) is deterministic and testable. Production uses
//! `SystemClock`; tests inject `FixedClock`.

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    fn now_utc(&self) -> DateTime<Utc>;
}

/// Real wall-clock time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock frozen at a fixed instant. For tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now_utc(&self) -> DateTime<Utc> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_returns_exact_instant() {
        let t = Utc::now();
        let clock = FixedClock(t);
        assert_eq!(clock.now_utc(), t);
        // Calling twice yields the same instant (determinism for tests).
        assert_eq!(clock.now_utc(), clock.now_utc());
    }

    #[test]
    fn system_clock_does_not_go_backwards() {
        let clock = SystemClock;
        let a = clock.now_utc();
        let b = clock.now_utc();
        assert!(b >= a, "system clock went backwards");
    }

    #[test]
    fn clock_is_object_safe() {
        // The dyn usage (Arc<dyn Clock>) the API relies on must compile.
        let clock: Box<dyn Clock> = Box::new(FixedClock(Utc::now()));
        let _ = clock.now_utc();
    }
}
