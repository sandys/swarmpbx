//! Orchestrator scheduling-gate tests.

use super::*;

use ad_campaign::CampaignState;
use ad_circuit_breaker::{CircuitBreaker, Config as BreakerConfig};
use chrono::Duration;

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    use chrono::TimeZone;
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
}

fn cb_cfg() -> BreakerConfig {
    BreakerConfig {
        threshold: 0.5,
        window: Duration::seconds(120),
        min_calls: 5,
    }
}

#[test]
fn running_in_window_closed_breaker_dispatches() {
    let cb = CircuitBreaker::new();
    let d = gate(
        utc(2026, 6, 1, 12, 0),
        CampaignState::Running,
        &Schedule::always(),
        &cb,
        &cb_cfg(),
    );
    assert_eq!(d, GateDecision::Dispatch);
}

#[test]
fn non_running_campaign_holds() {
    let cb = CircuitBreaker::new();
    for state in [
        CampaignState::Draft,
        CampaignState::Syncing,
        CampaignState::Paused,
        CampaignState::Completed,
        CampaignState::Failed,
    ] {
        let d = gate(
            utc(2026, 6, 1, 12, 0),
            state,
            &Schedule::always(),
            &cb,
            &cb_cfg(),
        );
        assert_eq!(
            d,
            GateDecision::Hold(GateBlock::NotRunning),
            "state {state:?}"
        );
    }
}

#[test]
fn outside_business_schedule_holds() {
    // Mon-Fri 09:00-17:00 in New York; Sunday closed.
    let win = (900u16 / 100 * 60, 1700u16 / 100 * 60); // 540..1020 minutes
    let windows = [win, win, win, win, win, (0, 0), (0, 0)]; // Sat/Sun closed
    let sched = Schedule {
        timezone: "America/New_York".into(),
        windows: Some(windows),
    };
    let cb = CircuitBreaker::new();

    // 2026-06-07 is a Sunday -> closed.
    let sun = gate(
        utc(2026, 6, 7, 16, 0),
        CampaignState::Running,
        &sched,
        &cb,
        &cb_cfg(),
    );
    assert_eq!(sun, GateDecision::Hold(GateBlock::OutsideSchedule));

    // 2026-06-01 Monday 02:00 UTC = 22:00 ET Sunday -> outside the Mon window.
    let before = gate(
        utc(2026, 6, 1, 2, 0),
        CampaignState::Running,
        &sched,
        &cb,
        &cb_cfg(),
    );
    assert_eq!(before, GateDecision::Hold(GateBlock::OutsideSchedule));

    // 2026-06-01 Monday 15:00 UTC = 11:00 ET -> inside 9-17 window.
    let inside = gate(
        utc(2026, 6, 1, 15, 0),
        CampaignState::Running,
        &sched,
        &cb,
        &cb_cfg(),
    );
    assert_eq!(inside, GateDecision::Dispatch);
}

#[test]
fn open_circuit_breaker_holds() {
    let now = utc(2026, 6, 1, 12, 0);
    let mut cb = CircuitBreaker::new();
    // 6 calls, 5 failed = 83% > 50% over the window -> open.
    for i in 0..1 {
        cb.record(now - Duration::seconds(i), false);
    }
    for i in 1..6 {
        cb.record(now - Duration::seconds(i), true);
    }
    let d = gate(
        now,
        CampaignState::Running,
        &Schedule::always(),
        &cb,
        &cb_cfg(),
    );
    assert_eq!(d, GateDecision::Hold(GateBlock::CircuitOpen));
}

#[test]
fn unknown_schedule_timezone_fails_open() {
    // Business schedule fails OPEN (compliance still gates per-lead at dispatch).
    let windows = [(540u16, 1020u16); 7];
    let sched = Schedule {
        timezone: "Not/AZone".into(),
        windows: Some(windows),
    };
    let cb = CircuitBreaker::new();
    let d = gate(
        utc(2026, 6, 1, 3, 0),
        CampaignState::Running,
        &sched,
        &cb,
        &cb_cfg(),
    );
    assert_eq!(d, GateDecision::Dispatch);
}
