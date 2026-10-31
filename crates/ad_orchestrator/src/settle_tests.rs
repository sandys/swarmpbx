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
fn an_abandoned_call_counts_and_retries() {
    let p = RetryPolicy::default();
    let s = settle(
        &Outcome::Abandoned {
            detail: "no agent".into(),
        },
        0,
        &p,
        now(),
    );
    assert_eq!(s.status, LeadStatus::Queued);
    assert!(s.counts_as_call);
    assert_eq!(s.retry, Some((now() + p.abandoned, RetryReason::Abandoned)));
    assert_eq!(s.wait_reason, Some("abandoned"));
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

fn agent(code: Option<&str>, f: impl FnOnce(&mut ad_disposition::Report)) -> AgentOutcome {
    let mut r = ad_disposition::Report {
        schema_version: ad_disposition::SCHEMA_VERSION,
        disposition: code.map(Into::into),
        ..Default::default()
    };
    f(&mut r);
    ad_disposition::check(
        &r,
        &ad_disposition::Catalog::default(),
        Some("America/New_York"),
        now(),
    )
    .unwrap()
}

fn answered() -> Outcome {
    Outcome::Completed {
        detail: "bye".into(),
    }
}

#[test]
fn a_dnc_request_stops_the_lead_after_any_end_of_the_call() {
    let p = RetryPolicy::default();
    let dnc = agent(Some("DNC"), |_| {});
    for end in [answered(), Outcome::Busy { sip_code: 486 }] {
        let s = settle_with(&end, Some(&dnc), 0, &p, now());
        assert_eq!(s.status, LeadStatus::Dnc, "{end:?}");
        assert!(s.counts_as_call);
        assert_eq!((s.retry, s.next_eligible_at), (None, None));
    }
    let bare = agent(None, |r| r.dnc = Some(Some("campaign".into())));
    let s = settle_with(&answered(), Some(&bare), 2, &p, now());
    assert_eq!(s.status, LeadStatus::Dnc);
}

#[test]
fn a_callback_queues_the_lead_at_its_time_with_a_higher_priority() {
    let p = RetryPolicy::default();
    let cb = agent(Some("CALLBK"), |r| {
        r.callback = Some(ad_disposition::CallbackRequest {
            at: "2026-10-22T09:30".into(),
            zone: None,
        })
    });
    // 09:30 EDT is 13:30 UTC.
    let due = Utc.with_ymd_and_hms(2026, 10, 22, 13, 30, 0).unwrap();
    // The cap of retries does not stop a callback.
    let s = settle_with(&answered(), Some(&cb), p.max_attempts, &p, now());
    assert_eq!(s.status, LeadStatus::Queued);
    assert!(s.counts_as_call && !s.exhausted);
    assert_eq!(s.next_eligible_at, Some(due));
    assert_eq!(s.callback, Some(due));
    assert_eq!(s.wait_reason, Some("callback"));
    assert_eq!(s.priority, CALLBACK_PRIORITY);
}

#[test]
fn a_machine_answer_is_a_voicemail_retry() {
    let p = RetryPolicy::default();
    let s = settle_with(&answered(), Some(&agent(Some("AM"), |_| {})), 0, &p, now());
    assert_eq!(s.status, LeadStatus::Queued);
    assert_eq!(s.retry.map(|r| r.1), Some(RetryReason::Voicemail));
    assert_eq!(s.priority, 0);
}

#[test]
fn a_disposition_of_an_answered_call_settles_it() {
    let p = RetryPolicy::default();
    for code in ["SALE", "NI", "WN", "DONE"] {
        let s = settle_with(&answered(), Some(&agent(Some(code), |_| {})), 0, &p, now());
        assert_eq!(s.status, LeadStatus::Dispositioned, "{code}");
        assert_eq!((s.priority, s.callback), (0, None));
    }
}

#[test]
fn only_a_dnc_request_counts_when_the_callee_did_not_answer() {
    let p = RetryPolicy::default();
    let cb = agent(Some("CALLBK"), |r| {
        r.callback = Some(ad_disposition::CallbackRequest {
            at: "2026-10-22T09:30".into(),
            zone: None,
        })
    });
    let end = Outcome::NoAnswer { sip_code: 480 };
    assert_eq!(
        settle_with(&end, Some(&cb), 0, &p, now()),
        settle(&end, 0, &p, now())
    );
    let am = agent(Some("AM"), |_| {});
    let abandoned = Outcome::Abandoned {
        detail: String::new(),
    };
    assert_eq!(
        settle_with(&abandoned, Some(&am), 0, &p, now()),
        settle(&abandoned, 0, &p, now())
    );
}
