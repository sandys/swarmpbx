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
