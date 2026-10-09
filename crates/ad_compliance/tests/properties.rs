//! Property tests for compliance invariants.

use ad_compliance::safe_hours::{self, CallWindowSet, LeadLocation};
use ad_compliance::{Decision, DenyReason};
use ad_core::time::FixedClock;
use chrono::TimeZone;
use proptest::prelude::*;

proptest! {
    // INVARIANT: a missing timezone ALWAYS denies with UnknownLocalTime,
    // regardless of the instant or the window. This is the fail-closed core.
    #[test]
    fn missing_tz_always_fails_closed(secs in 0i64..4_000_000_000i64) {
        let clock = FixedClock(chrono::Utc.timestamp_opt(secs, 0).single().unwrap());
        let windows = CallWindowSet::uniform(0, 2400).unwrap(); // widest possible
        let loc = LeadLocation { timezone: None };
        let d = safe_hours::check(&clock, &loc, &windows);
        prop_assert_eq!(d, Decision::Denied(DenyReason::UnknownLocalTime));
    }

    // INVARIANT: a fully-closed window (all days CLOSED) NEVER allows a call,
    // for any valid tz and any instant.
    #[test]
    fn fully_closed_window_never_allows(secs in 0i64..4_000_000_000i64) {
        let clock = FixedClock(chrono::Utc.timestamp_opt(secs, 0).single().unwrap());
        let windows = CallWindowSet {
            by_weekday: [ad_compliance::safe_hours::DayWindow::CLOSED; 7],
            holidays: vec![],
        };
        let loc = LeadLocation { timezone: Some("America/New_York".into()) };
        let d = safe_hours::check(&clock, &loc, &windows);
        prop_assert!(!d.is_allowed());
    }

    // INVARIANT: a 24h-open window (00:00-24:00) with a valid tz ALWAYS allows
    // (no holidays) — the complement sanity check so the gate isn't vacuously
    // denying everything.
    #[test]
    fn fully_open_window_always_allows(secs in 0i64..4_000_000_000i64) {
        let clock = FixedClock(chrono::Utc.timestamp_opt(secs, 0).single().unwrap());
        let windows = CallWindowSet::uniform(0, 2400).unwrap();
        let loc = LeadLocation { timezone: Some("Asia/Kolkata".into()) };
        let d = safe_hours::check(&clock, &loc, &windows);
        prop_assert_eq!(d, Decision::Allowed);
    }
}
