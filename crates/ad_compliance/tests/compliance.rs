//! Tests for the compliance engine. These are the risk-pole tests: they must
//! fail without the logic and pass with it, and cover the gnarly cases
//! (DST, half-hour offsets, midnight wrap, holidays, fail-closed).

use ad_compliance::abandon::{AbandonCounts, AbandonLedger, JurisdictionRule};
use ad_compliance::dnc::{DncScope, DncSet};
use ad_compliance::safe_hours::{self, CallWindowSet, DayWindow, LeadLocation};
use ad_compliance::{Decision, DenyReason};
use ad_core::time::FixedClock;

fn clock(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> FixedClock {
    FixedClock(safe_hours::utc(y, mo, d, h, mi))
}

fn loc(tz: &str) -> LeadLocation {
    LeadLocation {
        timezone: Some(tz.to_string()),
    }
}

// ---- safe-hours: fail-closed ----

#[test]
fn unknown_timezone_denies_fail_closed() {
    let windows = CallWindowSet::uniform(900, 2100).unwrap();
    let no_tz = LeadLocation { timezone: None };
    let d = safe_hours::check(&clock(2026, 6, 1, 15, 0), &no_tz, &windows);
    assert_eq!(d, Decision::Denied(DenyReason::UnknownLocalTime));

    let bad_tz = loc("Not/AZone");
    let d2 = safe_hours::check(&clock(2026, 6, 1, 15, 0), &bad_tz, &windows);
    assert_eq!(d2, Decision::Denied(DenyReason::UnknownLocalTime));
}

#[test]
fn invalid_hhmm_window_is_rejected_at_construction() {
    // 2560 is not a valid HHMM -> None -> caller fails closed.
    assert!(DayWindow::from_hhmm(2560, 2100).is_none());
    assert!(CallWindowSet::uniform(900, 9999).is_none());
}

// ---- safe-hours: basic window, in and out ----

#[test]
fn within_window_allows_outside_denies_new_york() {
    let windows = CallWindowSet::uniform(900, 2100).unwrap(); // 9am-9pm local
                                                              // 2026-06-01 is a Monday. 15:00 UTC = 11:00 EDT (summer, UTC-4): inside.
    let inside = safe_hours::check(
        &clock(2026, 6, 1, 15, 0),
        &loc("America/New_York"),
        &windows,
    );
    assert_eq!(inside, Decision::Allowed);

    // 02:00 UTC Mon = 22:00 EDT previous Sun -> outside 9-21 window.
    let outside = safe_hours::check(&clock(2026, 6, 1, 2, 0), &loc("America/New_York"), &windows);
    assert_eq!(outside, Decision::Denied(DenyReason::OutsideCallingWindow));
}

// ---- safe-hours: half-hour offset zone (India +5:30) ----

#[test]
fn half_hour_offset_zone_kolkata() {
    let windows = CallWindowSet::uniform(900, 2100).unwrap();
    // 04:00 UTC = 09:30 IST (UTC+5:30): just inside the 09:00 open.
    let inside = safe_hours::check(&clock(2026, 6, 1, 4, 0), &loc("Asia/Kolkata"), &windows);
    assert_eq!(inside, Decision::Allowed);
    // 03:00 UTC = 08:30 IST: before 09:00 open -> outside.
    let before = safe_hours::check(&clock(2026, 6, 1, 3, 0), &loc("Asia/Kolkata"), &windows);
    assert_eq!(before, Decision::Denied(DenyReason::OutsideCallingWindow));
}

// ---- safe-hours: DST boundary (US spring-forward 2026-03-08) ----

#[test]
fn dst_spring_forward_respects_local_wallclock() {
    let windows = CallWindowSet::uniform(900, 2100).unwrap();
    // Before DST (early March) NY is UTC-5; after 2026-03-08 02:00 local it's UTC-4.
    // Pick 13:30 UTC on 2026-03-09 (Mon, after switch): = 09:30 EDT -> inside.
    let after = safe_hours::check(
        &clock(2026, 3, 9, 13, 30),
        &loc("America/New_York"),
        &windows,
    );
    assert_eq!(after, Decision::Allowed);
    // 13:30 UTC on 2026-03-02 (Mon, before switch, UTC-5) = 08:30 EST -> outside.
    let before = safe_hours::check(
        &clock(2026, 3, 2, 13, 30),
        &loc("America/New_York"),
        &windows,
    );
    assert_eq!(before, Decision::Denied(DenyReason::OutsideCallingWindow));
}

// ---- safe-hours: midnight-wrap window ----

#[test]
fn midnight_wrapping_window() {
    // 20:00 -> 02:00 window (overnight).
    let w = DayWindow::from_hhmm(2000, 200).unwrap();
    let windows = CallWindowSet {
        by_weekday: [w; 7],
        holidays: vec![],
    };
    // 23:00 UTC = 23:00 in UTC tz -> inside.
    let inside = safe_hours::check(&clock(2026, 6, 1, 23, 0), &loc("UTC"), &windows);
    assert_eq!(inside, Decision::Allowed);
    // 01:00 UTC -> inside (before 02:00).
    let inside2 = safe_hours::check(&clock(2026, 6, 1, 1, 0), &loc("UTC"), &windows);
    assert_eq!(inside2, Decision::Allowed);
    // 12:00 UTC -> outside.
    let outside = safe_hours::check(&clock(2026, 6, 1, 12, 0), &loc("UTC"), &windows);
    assert_eq!(outside, Decision::Denied(DenyReason::OutsideCallingWindow));
}

// ---- safe-hours: holiday ----

#[test]
fn holiday_denies_even_inside_window() {
    let mut windows = CallWindowSet::uniform(900, 2100).unwrap();
    windows
        .holidays
        .push(chrono::NaiveDate::from_ymd_opt(2026, 7, 4).unwrap());
    // 2026-07-04 16:00 UTC = 12:00 EDT (inside window) but it's a holiday.
    let d = safe_hours::check(
        &clock(2026, 7, 4, 16, 0),
        &loc("America/New_York"),
        &windows,
    );
    assert_eq!(d, Decision::Denied(DenyReason::Holiday));
}

// ---- safe-hours: closed day ----

#[test]
fn closed_day_denies() {
    let mut windows = CallWindowSet::uniform(900, 2100).unwrap();
    // Close Sunday (index 6).
    windows.by_weekday[6] = DayWindow::CLOSED;
    // 2026-06-07 is a Sunday. 16:00 UTC = 12:00 EDT.
    let d = safe_hours::check(
        &clock(2026, 6, 7, 16, 0),
        &loc("America/New_York"),
        &windows,
    );
    assert_eq!(d, Decision::Denied(DenyReason::OutsideCallingWindow));
}

// ---- DNC scopes ----

#[test]
fn dnc_scope_precedence_and_match() {
    let mut set = DncSet::default();
    set.system.insert("18005551234".into());
    set.campaign.insert("4155559999".into());
    set.area_codes.insert("212".into());

    assert_eq!(
        set.check("1-800-555-1234"),
        Decision::Denied(DenyReason::Dnc(DncScope::System))
    );
    assert_eq!(
        set.check("(415) 555-9999"),
        Decision::Denied(DenyReason::Dnc(DncScope::Campaign))
    );
    assert_eq!(
        set.check("212-867-5309"),
        Decision::Denied(DenyReason::Dnc(DncScope::AreaCode))
    );
    assert_eq!(set.check("312-867-5309"), Decision::Allowed);
}

#[test]
fn dnc_empty_number_fails_closed() {
    let set = DncSet::default();
    assert!(matches!(
        set.check("not-a-number"),
        Decision::Denied(DenyReason::Dnc(_))
    ));
}

// ---- abandonment ledger: regulator denominator ----

#[test]
fn abandon_rate_uses_human_answered_denominator() {
    let now = safe_hours::utc(2026, 6, 15, 12, 0);
    let mut ledger = AbandonLedger::new();
    // 100 answered-by-human, 2 abandoned => 2% <= 3% US cap. Compliant.
    for i in 0..100 {
        ledger.record(now - chrono::Duration::minutes(i), i < 2);
    }
    let rate = ledger.rate(now, &JurisdictionRule::UsTsr).unwrap();
    assert!((rate - 0.02).abs() < 1e-9, "rate was {rate}");
    assert!(ledger.is_compliant(now, &JurisdictionRule::UsTsr));

    // Add 2 more abandons => 4/102 ~ 3.9% > 3% => non-compliant.
    ledger.record(now, true);
    ledger.record(now, true);
    assert!(!ledger.is_compliant(now, &JurisdictionRule::UsTsr));
}

#[test]
fn abandon_rate_window_excludes_old_events() {
    let now = safe_hours::utc(2026, 6, 15, 12, 0);
    let mut ledger = AbandonLedger::new();
    // An old abandon 40 days ago is outside the 30-day US window.
    ledger.record(now - chrono::Duration::days(40), true);
    // One recent non-abandoned human answer.
    ledger.record(now - chrono::Duration::days(1), false);
    let rate = ledger.rate(now, &JurisdictionRule::UsTsr).unwrap();
    assert_eq!(rate, 0.0, "old event must be excluded from the window");
}

#[test]
fn abandon_rate_undefined_when_no_human_answers_is_compliant() {
    let now = safe_hours::utc(2026, 6, 15, 12, 0);
    let ledger = AbandonLedger::new();
    assert!(ledger.rate(now, &JurisdictionRule::UsTsr).is_none());
    assert!(ledger.is_compliant(now, &JurisdictionRule::UsTsr));
}

#[test]
fn jurisdiction_flags() {
    assert!(!JurisdictionRule::UsTsr.is_experimental());
    assert!(JurisdictionRule::UkOfcom.is_experimental());
    assert!(JurisdictionRule::IndiaTrai.is_experimental());
    assert_eq!(JurisdictionRule::IndiaTrai.cap(), 0.02);
}

#[test]
fn customer_first_stops_above_the_cap() {
    let rule = JurisdictionRule::UsTsr;
    let counts = |answered, abandoned| AbandonCounts {
        answered,
        abandoned,
    };
    assert!(rule.allows_customer_first(&counts(0, 0)), "no answers yet");
    assert!(rule.allows_customer_first(&counts(100, 3)), "3% is the cap");
    assert!(!rule.allows_customer_first(&counts(100, 4)));
    assert!(!rule.allows_customer_first(&counts(1, 1)));
    assert_eq!(counts(200, 3).rate(), Some(0.015));
}

#[test]
fn an_agent_must_join_within_two_seconds() {
    for rule in [
        JurisdictionRule::UsTsr,
        JurisdictionRule::UkOfcom,
        JurisdictionRule::IndiaTrai,
    ] {
        assert_eq!(
            rule.connect_within(),
            chrono::Duration::seconds(2),
            "{rule:?}"
        );
    }
    assert_eq!(JurisdictionRule::CAMPAIGN, JurisdictionRule::UsTsr);
}
