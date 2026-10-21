use ad_attempt::{NotDialedReason, Outcome};
use ad_events::RetryReason;
use ad_lead::LeadStatus;
use chrono::{DateTime, Duration, TimeZone, Utc};

use super::*;
use crate::retry::RetryPolicy;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 21, 15, 0, 0).unwrap()
}

fn not_dialed(reason: NotDialedReason) -> Outcome {
    Outcome::NotDialed {
        reason,
        detail: String::new(),
    }
}

#[test]
fn an_answered_call_is_dispositioned_and_counted() {
    let s = settle(
        &Outcome::Completed {
            detail: "bye".into(),
        },
        0,
        &RetryPolicy::default(),
        now(),
    );
    assert_eq!(s.status, LeadStatus::Dispositioned);
    assert!(s.counts_as_call);
    assert_eq!(s.next_eligible_at, None);
}

#[test]
fn busy_and_no_answer_retry_after_their_own_interval() {
    let p = RetryPolicy::default();
    let busy = settle(&Outcome::Busy { sip_code: 486 }, 0, &p, now());
    assert_eq!(busy.status, LeadStatus::Queued);
    assert_eq!(busy.next_eligible_at, Some(now() + p.busy));
    assert_eq!(busy.retry, Some((now() + p.busy, RetryReason::Busy)));
    assert_eq!(busy.wait_reason, Some("busy"));

    let na = settle(&Outcome::NoAnswer { sip_code: 480 }, 0, &p, now());
    assert_eq!(na.next_eligible_at, Some(now() + p.no_answer));
}

#[test]
fn the_retry_cap_dispositions_the_lead() {
    let p = RetryPolicy::default();
    let last = settle(
        &Outcome::Busy { sip_code: 486 },
        p.max_attempts - 1,
        &p,
        now(),
    );
    assert_eq!(last.status, LeadStatus::Dispositioned);
    assert!(last.exhausted);
    assert_eq!(last.retry, None);
}

#[test]
fn an_uncertain_dial_retries_as_an_error() {
    let s = settle(
        &Outcome::Uncertain {
            detail: "timeout".into(),
        },
        0,
        &RetryPolicy::default(),
        now(),
    );
    assert_eq!(s.retry.map(|r| r.1), Some(RetryReason::Error));
    assert!(s.counts_as_call);
}

#[test]
fn a_bad_number_is_never_dialed_again() {
    let s = settle(
        &Outcome::InvalidNumber { sip_code: 404 },
        0,
        &RetryPolicy::default(),
        now(),
    );
    assert_eq!(s.status, LeadStatus::Done);
}

#[test]
fn a_gate_denial_does_not_count_as_a_call() {
    let p = RetryPolicy::default();
    let window = settle(&not_dialed(NotDialedReason::Window), 0, &p, now());
    assert_eq!(window.status, LeadStatus::New);
    assert!(!window.counts_as_call);
    assert_eq!(window.next_eligible_at, Some(now() + p.window_recheck));

    let window_again = settle(&not_dialed(NotDialedReason::Window), 2, &p, now());
    assert_eq!(window_again.status, LeadStatus::Queued);

    assert_eq!(
        settle(&not_dialed(NotDialedReason::Dnc), 0, &p, now()).status,
        LeadStatus::Dnc
    );
    assert_eq!(
        settle(&not_dialed(NotDialedReason::UnknownTimezone), 0, &p, now()).status,
        LeadStatus::Blocked
    );
    assert_eq!(
        settle(&not_dialed(NotDialedReason::Excluded), 0, &p, now()).status,
        LeadStatus::Done
    );
}

#[test]
fn a_released_lead_is_due_at_once() {
    let s = settle(
        &not_dialed(NotDialedReason::Released),
        1,
        &RetryPolicy::default(),
        now(),
    );
    assert_eq!(s.status, LeadStatus::Queued);
    assert_eq!(s.next_eligible_at, None);
    assert!(!s.counts_as_call);
}

#[test]
fn the_window_recheck_is_configurable() {
    let p = RetryPolicy {
        window_recheck: Duration::minutes(1),
        ..RetryPolicy::default()
    };
    let s = settle(&not_dialed(NotDialedReason::Window), 0, &p, now());
    assert_eq!(s.next_eligible_at, Some(now() + Duration::minutes(1)));
}

#[test]
fn a_missing_agent_does_not_burn_the_lead() {
    let p = RetryPolicy::default();
    let s = settle(&not_dialed(NotDialedReason::NoAgent), 1, &p, now());
    assert_eq!(s.status, LeadStatus::Queued);
    assert!(!s.counts_as_call);
    assert_eq!(s.next_eligible_at, Some(now() + p.agent_recheck));
    assert_eq!(s.wait_reason, Some("no_agent"));
}
