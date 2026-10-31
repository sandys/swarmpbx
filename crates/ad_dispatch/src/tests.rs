//! Dispatch loop tests with a stub initiator and a counting capacity source.

use super::*;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::{self, CallWindowSet};
use ad_core::time::FixedClock;
use ad_core::{CampaignId, LeadId};
use ad_lead::{Lead, LeadStatus};

// --- stub initiator: records calls and returns a programmed placement ---
struct StubInitiator {
    calls: Arc<AtomicUsize>,
    placement: Placement,
}
impl CallInitiator for StubInitiator {
    async fn place(&self, _request: &DialRequest<'_>) -> Placement {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.placement.clone()
    }
}

// --- counting capacity: at most `max` concurrent slots, RAII release ---
struct CountingCapacity {
    in_use: Arc<AtomicUsize>,
    max: usize,
}
struct SlotGuard {
    in_use: Arc<AtomicUsize>,
}
impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.in_use.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Capacity for CountingCapacity {
    type Slot = SlotGuard;
    fn try_acquire(&self) -> Option<SlotGuard> {
        // Compare-and-increment under the cap.
        loop {
            let cur = self.in_use.load(Ordering::SeqCst);
            if cur >= self.max {
                return None;
            }
            if self
                .in_use
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(SlotGuard {
                    in_use: self.in_use.clone(),
                });
            }
        }
    }
}

fn lead(id: i64, tz: Option<&str>, phone: &str) -> Lead {
    let t = safe_hours::utc(2026, 1, 1, 0, 0);
    Lead {
        id: LeadId(id),
        campaign_id: CampaignId(1),
        phone_number: phone.into(),
        timezone: tz.map(|s| s.into()),
        state: None,
        postal_code: None,
        status: LeadStatus::Queued,
        called_count: 0,
        last_local_call_time: None,
        context: BTreeMap::new(),
        created_at: t,
        updated_at: t,
    }
}

fn windows() -> CallWindowSet {
    CallWindowSet::uniform(900, 2100).unwrap()
}
fn clock_inside() -> FixedClock {
    FixedClock(safe_hours::utc(2026, 6, 1, 15, 0)) // 11:00 ET
}

fn dispatcher<'a>(
    clock: &'a FixedClock,
    init: &'a StubInitiator,
    cap: &'a CountingCapacity,
    windows: &'a CallWindowSet,
    dnc: &'a DncSet,
) -> Dispatcher<'a, StubInitiator, CountingCapacity> {
    Dispatcher {
        clock,
        initiator: init,
        capacity: cap,
        compliance: ComplianceData { windows, dnc },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn answered_lead_calls_initiator_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let init = StubInitiator {
        calls: calls.clone(),
        placement: Placement::Answered,
    };
    let cap = CountingCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: 10,
    };
    let (w, dnc) = (windows(), DncSet::default());
    let clock = clock_inside();
    let d = dispatcher(&clock, &init, &cap, &w, &dnc);

    let out = d
        .dispatch_lead(&lead(1, Some("America/New_York"), "+13125550100"))
        .await;
    assert_eq!(out, CallOutcome::Answered);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Slot released after the call (RAII): in_use back to 0.
    assert_eq!(cap.in_use.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn compliance_regate_blocks_unknown_tz_and_does_not_initiate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let init = StubInitiator {
        calls: calls.clone(),
        placement: Placement::Answered,
    };
    let cap = CountingCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: 10,
    };
    let (w, dnc) = (windows(), DncSet::default());
    let clock = clock_inside();
    let d = dispatcher(&clock, &init, &cap, &w, &dnc);

    let out = d.dispatch_lead(&lead(1, None, "+13125550100")).await;
    assert!(matches!(out, CallOutcome::ComplianceDenied(_)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "must not initiate when compliance denies"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dnc_regate_blocks() {
    let calls = Arc::new(AtomicUsize::new(0));
    let init = StubInitiator {
        calls: calls.clone(),
        placement: Placement::Answered,
    };
    let cap = CountingCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: 10,
    };
    let dnc = DncSet::from_lists(&["+18005551234".into()], &[], &[]).unwrap();
    let w = windows();
    let clock = clock_inside();
    let d = dispatcher(&clock, &init, &cap, &w, &dnc);

    let out = d
        .dispatch_lead(&lead(1, Some("America/New_York"), "+1-800-555-1234"))
        .await;
    assert!(
        matches!(&out, CallOutcome::ComplianceDenied(r) if r.starts_with("DNC")),
        "{out:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn capacity_cap_reports_no_capacity_when_slots_full() {
    let calls = Arc::new(AtomicUsize::new(0));
    let init = StubInitiator {
        calls: calls.clone(),
        placement: Placement::Answered,
    };
    // max=0 => never any capacity.
    let cap = CountingCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: 0,
    };
    let (w, dnc) = (windows(), DncSet::default());
    let clock = clock_inside();
    let d = dispatcher(&clock, &init, &cap, &w, &dnc);

    let out = d
        .dispatch_lead(&lead(1, Some("America/New_York"), "+13125550201"))
        .await;
    assert_eq!(out, CallOutcome::NoCapacity);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "must not initiate without a slot"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn batch_releases_slots_so_more_than_cap_can_dial_sequentially() {
    // max=1 slot, but 3 leads: since each call releases its slot before the
    // next (sequential batch), all 3 get Answered — progressive pacing bounded
    // by *concurrent* capacity, not total.
    let calls = Arc::new(AtomicUsize::new(0));
    let init = StubInitiator {
        calls: calls.clone(),
        placement: Placement::Answered,
    };
    let cap = CountingCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: 1,
    };
    let (w, dnc) = (windows(), DncSet::default());
    let clock = clock_inside();
    let d = dispatcher(&clock, &init, &cap, &w, &dnc);

    let leads = vec![
        lead(1, Some("America/New_York"), "+13125550201"),
        lead(2, Some("America/New_York"), "+13125550202"),
        lead(3, Some("America/New_York"), "+13125550203"),
    ];
    let out = d.dispatch_batch(&leads).await;
    assert!(out.iter().all(|(_, o)| *o == CallOutcome::Answered));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(cap.in_use.load(Ordering::SeqCst), 0, "all slots released");
}

#[test]
fn outcome_failure_classification() {
    assert!(CallOutcome::Failed("x".into()).is_failure());
    assert!(!CallOutcome::NoCapacity.is_failure());
    assert!(!CallOutcome::ComplianceDenied("x".into()).is_failure());
    assert!(CallOutcome::Answered.is_attempted());
    assert!(!CallOutcome::NoCapacity.is_attempted());
}

#[tokio::test(flavor = "current_thread")]
async fn placements_map_to_dispatch_outcomes() {
    let (w, dnc) = (windows(), DncSet::default());
    let clock = clock_inside();
    let cap = CountingCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: 10,
    };
    let cases = [
        (
            Placement::Rejected {
                sip_code: Some(486),
                detail: "busy".into(),
            },
            CallOutcome::NotAnswered("busy".into()),
        ),
        (
            Placement::Rejected {
                sip_code: Some(503),
                detail: "down".into(),
            },
            CallOutcome::Failed("down".into()),
        ),
        (
            Placement::NoResponse("timeout".into()),
            CallOutcome::Failed("timeout".into()),
        ),
    ];
    for (placement, want) in cases {
        let init = StubInitiator {
            calls: Arc::new(AtomicUsize::new(0)),
            placement,
        };
        let d = dispatcher(&clock, &init, &cap, &w, &dnc);
        let got = d
            .dispatch_lead(&lead(1, Some("America/New_York"), "+13125550201"))
            .await;
        assert_eq!(got, want);
    }
}

#[test]
fn placements_become_attempt_events() {
    let at = safe_hours::utc(2026, 6, 1, 15, 0);
    assert_eq!(
        Placement::Answered.into_event(at),
        ad_attempt::AttemptEvent::Answered { at }
    );
    assert_eq!(
        Placement::NotSent("refused".into()).into_event(at),
        ad_attempt::AttemptEvent::NotSent {
            at,
            detail: "refused".into()
        }
    );
    assert_eq!(
        Placement::NoResponse("timeout".into()).into_event(at),
        ad_attempt::AttemptEvent::NoResponse {
            at,
            detail: "timeout".into()
        }
    );
}
