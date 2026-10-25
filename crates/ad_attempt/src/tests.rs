use chrono::{DateTime, Duration, TimeZone, Utc};
use proptest::prelude::*;

use super::*;

fn t(s: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap() + Duration::seconds(s)
}

fn run(events: &[AttemptEvent]) -> Attempt {
    replay(Attempt::default(), events).expect("every event applies")
}

#[test]
fn answered_call_ends_completed_on_hangup() {
    let a = run(&[
        AttemptEvent::DialStarted { at: t(0) },
        AttemptEvent::Answered { at: t(3) },
        AttemptEvent::Hangup {
            at: t(60),
            detail: "bye".into(),
        },
    ]);
    assert_eq!(a.dialed_at, Some(t(0)));
    assert_eq!(a.answered_at, Some(t(3)));
    assert_eq!(a.ended_at(), Some(t(60)));
    assert_eq!(
        a.outcome(),
        Some(&Outcome::Completed {
            detail: "bye".into()
        })
    );
    assert_eq!(a.seq, 3);
}

#[test]
fn a_live_call_stays_live_until_it_ends() {
    let mut a = Attempt::default();
    for e in [
        AttemptEvent::DialStarted { at: t(0) },
        AttemptEvent::Answered { at: t(1) },
    ] {
        a = apply(&a, &e).unwrap();
        assert!(a.leg.is_live(), "{} must hold capacity", a.leg.name());
    }
    a = apply(&a, &AttemptEvent::Absent { at: t(9) }).unwrap();
    assert!(!a.leg.is_live());
}

#[test]
fn rejection_uses_the_sip_code() {
    let rejected = |code| {
        run(&[
            AttemptEvent::DialStarted { at: t(0) },
            AttemptEvent::Rejected {
                at: t(1),
                sip_code: code,
                detail: "x".into(),
            },
        ])
        .outcome()
        .unwrap()
        .class()
    };
    assert_eq!(rejected(Some(486)), "busy");
    assert_eq!(rejected(Some(480)), "no_answer");
    assert_eq!(rejected(Some(603)), "declined");
    assert_eq!(rejected(Some(404)), "invalid_number");
    assert_eq!(rejected(Some(503)), "failed");
    assert_eq!(rejected(None), "failed");
}

#[test]
fn sip_classes_cover_the_common_codes() {
    let class = |c| classify_sip(c, String::new()).class();
    for c in [486, 600] {
        assert_eq!(class(c), "busy", "{c}");
    }
    for c in [408, 480, 487] {
        assert_eq!(class(c), "no_answer", "{c}");
    }
    for c in [603, 607] {
        assert_eq!(class(c), "declined", "{c}");
    }
    for c in [404, 410, 484, 485, 604] {
        assert_eq!(class(c), "invalid_number", "{c}");
    }
    for c in [403, 500, 502, 503, 488] {
        assert_eq!(class(c), "failed", "{c}");
    }
}

#[test]
fn only_failures_count_for_the_breaker() {
    let failures: Vec<&str> = [
        Outcome::Completed { detail: "".into() },
        Outcome::Busy { sip_code: 486 },
        Outcome::NoAnswer { sip_code: 480 },
        Outcome::Declined { sip_code: 603 },
        Outcome::InvalidNumber { sip_code: 404 },
        Outcome::Failed {
            sip_code: Some(503),
            detail: "".into(),
        },
        Outcome::SetupError { detail: "".into() },
        Outcome::Uncertain { detail: "".into() },
        Outcome::NotDialed {
            reason: NotDialedReason::Window,
            detail: "".into(),
        },
    ]
    .iter()
    .filter(|o| o.is_failure())
    .map(|o| o.class())
    .collect();
    assert_eq!(failures, ["failed", "setup_error", "uncertain"]);
}

#[test]
fn a_request_that_never_left_is_a_setup_error() {
    let a = run(&[
        AttemptEvent::DialStarted { at: t(0) },
        AttemptEvent::NotSent {
            at: t(0),
            detail: "connection refused".into(),
        },
    ]);
    assert_eq!(a.outcome().unwrap().class(), "setup_error");
}

#[test]
fn no_response_holds_capacity_until_a_reconcile() {
    let uncertain = run(&[
        AttemptEvent::DialStarted { at: t(0) },
        AttemptEvent::NoResponse {
            at: t(40),
            detail: "timeout".into(),
        },
    ]);
    assert_eq!(uncertain.leg.name(), "uncertain");
    assert!(uncertain.leg.is_live());

    // The reconcile finds the call in the room: it connected after all.
    let found = apply(&uncertain, &AttemptEvent::Present { at: t(50) }).unwrap();
    assert_eq!(found.leg.name(), "connected");

    // The reconcile does not find it: the result stays unknown.
    let gone = apply(&uncertain, &AttemptEvent::Absent { at: t(50) }).unwrap();
    assert_eq!(
        gone.outcome(),
        Some(&Outcome::Uncertain {
            detail: "timeout".into()
        })
    );
}

#[test]
fn a_dial_left_by_a_stopped_dialer_is_reconciled() {
    let dialing = run(&[AttemptEvent::DialStarted { at: t(0) }]);
    let found = apply(&dialing, &AttemptEvent::Present { at: t(90) }).unwrap();
    assert_eq!(found.leg.name(), "connected");
    let gone = apply(&dialing, &AttemptEvent::Absent { at: t(90) }).unwrap();
    assert_eq!(gone.outcome().unwrap().class(), "uncertain");
}

#[test]
fn the_gate_ends_an_attempt_without_a_dial() {
    let a = run(&[AttemptEvent::NotDialed {
        at: t(0),
        reason: NotDialedReason::Dnc,
        detail: "system DNC".into(),
    }]);
    assert_eq!(
        a.outcome(),
        Some(&Outcome::NotDialed {
            reason: NotDialedReason::Dnc,
            detail: "system DNC".into()
        })
    );
    assert_eq!(a.dialed_at, None);
}

#[test]
fn events_that_do_not_apply_are_ignored() {
    let pending = Attempt::default();
    assert_eq!(
        apply(
            &pending,
            &AttemptEvent::Hangup {
                at: t(0),
                detail: "".into()
            }
        ),
        Err(Ignored {
            state: "pending",
            event: "hangup"
        })
    );
    // A hangup while dialing comes before the dial result. The dial result
    // carries the SIP code, so the hangup does not end the attempt.
    let dialing = run(&[AttemptEvent::DialStarted { at: t(0) }]);
    assert!(apply(
        &dialing,
        &AttemptEvent::Hangup {
            at: t(1),
            detail: "".into()
        }
    )
    .is_err());
    // A second dial request is refused: one attempt places one call.
    assert!(apply(&dialing, &AttemptEvent::DialStarted { at: t(1) }).is_err());
    let connected = run(&[
        AttemptEvent::DialStarted { at: t(0) },
        AttemptEvent::Answered { at: t(1) },
    ]);
    assert!(apply(&connected, &AttemptEvent::Present { at: t(2) }).is_err());
}

#[test]
fn snapshot_and_events_survive_json() {
    let events = vec![
        AttemptEvent::DialStarted { at: t(0) },
        AttemptEvent::NoResponse {
            at: t(1),
            detail: "timeout".into(),
        },
        AttemptEvent::Present { at: t(2) },
        AttemptEvent::Hangup {
            at: t(3),
            detail: "bye".into(),
        },
    ];
    let a = run(&events);
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(serde_json::from_value::<Attempt>(json).unwrap(), a);
    for e in &events {
        let back: AttemptEvent = serde_json::from_value(serde_json::to_value(e).unwrap()).unwrap();
        assert_eq!(&back, e);
        assert_eq!(serde_json::to_value(e).unwrap()["kind"], e.kind());
    }
}

fn with_agent_ready() -> Attempt {
    replay(
        Attempt::with_agent(),
        &[
            AttemptEvent::AgentStarting {
                at: t(0),
                detail: "dispatch AD_1".into(),
            },
            AttemptEvent::AgentReady {
                at: t(1),
                identity: "agent-AJ_1".into(),
            },
        ],
    )
    .expect("the agent becomes ready")
}

#[test]
fn the_dial_waits_for_the_agent() {
    let wanted = Attempt::with_agent();
    assert!(!wanted.can_dial());
    assert!(apply(&wanted, &AttemptEvent::DialStarted { at: t(0) }).is_err());

    let starting = apply(
        &wanted,
        &AttemptEvent::AgentStarting {
            at: t(0),
            detail: "".into(),
        },
    )
    .unwrap();
    assert!(!starting.can_dial());
    assert!(apply(&starting, &AttemptEvent::DialStarted { at: t(1) }).is_err());

    let ready = with_agent_ready();
    assert!(ready.can_dial());
    assert_eq!(ready.agent_started_at, Some(t(0)));
    assert_eq!(ready.agent_ready_at, Some(t(1)));
    let dialing = apply(&ready, &AttemptEvent::DialStarted { at: t(2) }).unwrap();
    assert_eq!(dialing.leg.name(), "dialing");
    assert_eq!(dialing.agent.name(), "ready");
}

#[test]
fn an_agent_that_fails_ends_the_attempt_without_a_dial() {
    for start in [
        Attempt::with_agent(),
        apply(
            &Attempt::with_agent(),
            &AttemptEvent::AgentStarting {
                at: t(0),
                detail: "".into(),
            },
        )
        .unwrap(),
    ] {
        let failed = apply(
            &start,
            &AttemptEvent::AgentFailed {
                at: t(30),
                detail: "no agent joined in 30 s".into(),
            },
        )
        .unwrap();
        assert_eq!(failed.agent.name(), "failed");
        assert_eq!(
            failed.outcome(),
            Some(&Outcome::NotDialed {
                reason: NotDialedReason::NoAgent,
                detail: "no agent joined in 30 s".into()
            })
        );
        assert_eq!(failed.dialed_at, None);
    }
}

#[test]
fn the_agent_leaving_ends_a_connected_call() {
    let connected = replay(
        with_agent_ready(),
        &[
            AttemptEvent::DialStarted { at: t(2) },
            AttemptEvent::Answered { at: t(5) },
        ],
    )
    .unwrap();
    let ended = apply(&connected, &AttemptEvent::AgentLeft { at: t(60) }).unwrap();
    assert_eq!(ended.agent.name(), "left");
    assert_eq!(
        ended.outcome(),
        Some(&Outcome::Completed {
            detail: "the agent ended the call".into()
        })
    );
}

#[test]
fn the_agent_leaving_before_the_dial_releases_the_lead() {
    let ended = apply(&with_agent_ready(), &AttemptEvent::AgentLeft { at: t(3) }).unwrap();
    assert!(matches!(
        ended.outcome(),
        Some(Outcome::NotDialed {
            reason: NotDialedReason::NoAgent,
            ..
        })
    ));
}

#[test]
fn the_agent_leaving_during_the_dial_waits_for_the_dial_result() {
    let dialing = apply(&with_agent_ready(), &AttemptEvent::DialStarted { at: t(2) }).unwrap();
    let left = apply(&dialing, &AttemptEvent::AgentLeft { at: t(3) }).unwrap();
    assert_eq!(left.leg.name(), "dialing");
    assert_eq!(left.agent.name(), "left");
    let rejected = apply(
        &left,
        &AttemptEvent::Rejected {
            at: t(4),
            sip_code: Some(487),
            detail: "cancelled".into(),
        },
    )
    .unwrap();
    assert_eq!(rejected.outcome().unwrap().class(), "no_answer");
}

#[test]
fn agent_events_do_not_apply_without_an_agent() {
    let none = Attempt::default();
    for e in [
        AttemptEvent::AgentStarting {
            at: t(0),
            detail: "".into(),
        },
        AttemptEvent::AgentReady {
            at: t(0),
            identity: "x".into(),
        },
        AttemptEvent::AgentFailed {
            at: t(0),
            detail: "".into(),
        },
        AttemptEvent::AgentLeft { at: t(0) },
    ] {
        assert!(apply(&none, &e).is_err(), "{}", e.kind());
    }
}

/// A customer-first attempt that the callee answered at t(5), with a 2 s limit.
fn answered_customer_first() -> Attempt {
    replay(
        Attempt::with_agent_on_answer(2000),
        &[
            AttemptEvent::DialStarted { at: t(0) },
            AttemptEvent::Answered { at: t(5) },
            AttemptEvent::AgentStarting {
                at: t(5),
                detail: "dispatch".into(),
            },
        ],
    )
    .unwrap()
}

fn ms(s: i64, ms: i64) -> DateTime<Utc> {
    t(s) + Duration::milliseconds(ms)
}

#[test]
fn customer_first_dials_before_the_agent_starts() {
    let a = Attempt::with_agent_on_answer(2000);
    assert!(a.can_dial());
    let starting = AttemptEvent::AgentStarting {
        at: t(0),
        detail: "".into(),
    };
    assert!(apply(&a, &starting).is_err(), "no agent before the answer");
    let dialing = apply(&a, &AttemptEvent::DialStarted { at: t(0) }).unwrap();
    assert!(
        apply(&dialing, &starting).is_err(),
        "no agent while it rings"
    );
}

#[test]
fn customer_first_agent_in_time_carries_the_call() {
    let ready = apply(
        &answered_customer_first(),
        &AttemptEvent::AgentReady {
            at: ms(7, 0),
            identity: "agent-1".into(),
        },
    )
    .unwrap();
    assert_eq!(ready.leg.name(), "connected");
    assert_eq!(ready.agent_started_at, Some(t(5)));
    assert_eq!(ready.agent_ready_at, Some(t(7)));
    let ended = apply(&ready, &AttemptEvent::AgentLeft { at: t(60) }).unwrap();
    assert_eq!(ended.outcome().unwrap().class(), "completed");
}

#[test]
fn customer_first_late_agent_abandons_the_call() {
    let late = apply(
        &answered_customer_first(),
        &AttemptEvent::AgentReady {
            at: ms(7, 1),
            identity: "agent-1".into(),
        },
    )
    .unwrap();
    assert_eq!(
        late.outcome(),
        Some(&Outcome::Abandoned {
            detail: "the agent joined 2001 ms after the answer; the limit is 2000 ms".into()
        })
    );
}

#[test]
fn customer_first_no_agent_abandons_the_call() {
    for start in [
        answered_customer_first(),
        replay(
            Attempt::with_agent_on_answer(2000),
            &[
                AttemptEvent::DialStarted { at: t(0) },
                AttemptEvent::Answered { at: t(5) },
            ],
        )
        .unwrap(),
    ] {
        let failed = apply(
            &start,
            &AttemptEvent::AgentFailed {
                at: t(7),
                detail: "no agent in the room 2000 ms after the answer".into(),
            },
        )
        .unwrap();
        assert_eq!(failed.agent.name(), "failed");
        assert_eq!(failed.outcome().unwrap().class(), "abandoned");
        let absent = apply(&start, &AttemptEvent::Absent { at: t(6) }).unwrap();
        assert_eq!(absent.outcome().unwrap().class(), "abandoned");
    }
}

#[test]
fn customer_first_hangup_before_the_limit_is_not_abandoned() {
    let hangup = |at| {
        apply(
            &answered_customer_first(),
            &AttemptEvent::Hangup {
                at,
                detail: "the callee hung up".into(),
            },
        )
        .unwrap()
    };
    assert_eq!(
        hangup(ms(6, 0)).outcome(),
        Some(&Outcome::Completed {
            detail: "the callee hung up before the agent joined".into()
        })
    );
    assert_eq!(hangup(ms(7, 1)).outcome().unwrap().class(), "abandoned");
}

#[test]
fn customer_first_without_a_limit_fails_closed() {
    let mut a = answered_customer_first();
    a.agent_within_ms = None;
    let ready = apply(
        &a,
        &AttemptEvent::AgentReady {
            at: t(5),
            identity: "agent-1".into(),
        },
    )
    .unwrap();
    assert_eq!(ready.outcome().unwrap().class(), "abandoned");
}

#[test]
fn an_old_snapshot_without_the_agent_region_still_reads() {
    let old = serde_json::json!({
        "leg": {"state": "pending"},
        "seq": 0,
        "dialed_at": null,
        "answered_at": null
    });
    let a: Attempt = serde_json::from_value(old).unwrap();
    assert_eq!(a.agent, AgentLeg::None);
    assert!(a.can_dial());
}

fn any_event() -> impl Strategy<Value = AttemptEvent> {
    let reason = prop_oneof![
        Just(NotDialedReason::Window),
        Just(NotDialedReason::Dnc),
        Just(NotDialedReason::UnknownTimezone),
        Just(NotDialedReason::Excluded),
        Just(NotDialedReason::Released),
        Just(NotDialedReason::NoAgent),
    ];
    (0i64..1000, any::<Option<u16>>(), reason, 0u8..13).prop_map(|(s, code, reason, k)| {
        let at = t(s);
        let detail = String::from("d");
        match k {
            0 => AttemptEvent::NotDialed { at, reason, detail },
            1 => AttemptEvent::DialStarted { at },
            2 => AttemptEvent::Answered { at },
            3 => AttemptEvent::Rejected {
                at,
                sip_code: code,
                detail,
            },
            4 => AttemptEvent::NotSent { at, detail },
            5 => AttemptEvent::NoResponse { at, detail },
            6 => AttemptEvent::Hangup { at, detail },
            7 => AttemptEvent::Present { at },
            8 => AttemptEvent::Absent { at },
            9 => AttemptEvent::AgentStarting { at, detail },
            10 => AttemptEvent::AgentReady {
                at,
                identity: detail,
            },
            11 => AttemptEvent::AgentFailed { at, detail },
            _ => AttemptEvent::AgentLeft { at },
        }
    })
}

fn any_start() -> impl Strategy<Value = Attempt> {
    prop_oneof![
        Just(Attempt::default()),
        Just(Attempt::with_agent()),
        Just(Attempt::with_agent_on_answer(2000)),
    ]
}

/// Apply every event that applies; return the end state and the applied ones.
fn run_all(start: &Attempt, events: &[AttemptEvent]) -> (Attempt, Vec<AttemptEvent>) {
    let mut a = start.clone();
    let mut applied = Vec::new();
    for e in events {
        if let Ok(next) = apply(&a, e) {
            assert_eq!(next.seq, a.seq + 1);
            a = next;
            applied.push(e.clone());
        }
    }
    (a, applied)
}

proptest! {
    // The database stores only the events that applied. Their replay must give
    // the snapshot, whatever arrived in between.
    #[test]
    fn replay_of_applied_events_gives_the_snapshot(
        start in any_start(),
        events in proptest::collection::vec(any_event(), 0..30)
    ) {
        let (a, applied) = run_all(&start, &events);
        prop_assert_eq!(replay(start, &applied).unwrap(), a);
    }

    // An ended attempt never changes again, so it never takes capacity back.
    #[test]
    fn ended_is_terminal(
        start in any_start(),
        events in proptest::collection::vec(any_event(), 0..30),
        after in any_event()
    ) {
        let (a, _) = run_all(&start, &events);
        if !a.leg.is_live() {
            prop_assert!(apply(&a, &after).is_err());
        }
    }

    // An attempt that never started a dial cannot connect.
    #[test]
    fn no_answer_without_a_dial(
        start in any_start(),
        events in proptest::collection::vec(any_event(), 0..30)
    ) {
        let mut a = start;
        for e in &events {
            if let Ok(next) = apply(&a, e) {
                a = next;
            }
            if a.answered_at.is_some() {
                prop_assert!(a.dialed_at.is_some());
            }
        }
    }

    // When the campaign has an agent, no dial starts before the agent is in
    // the room (GAPS.md BETTER-1).
    #[test]
    fn no_dial_before_the_agent_is_ready(
        events in proptest::collection::vec(any_event(), 0..30)
    ) {
        let mut a = Attempt::with_agent();
        for e in &events {
            if let Ok(next) = apply(&a, e) {
                a = next;
            }
            if a.dialed_at.is_some() {
                prop_assert!(a.agent_ready_at.is_some());
            }
        }
    }

    // Customer first: a call never stays connected with an agent that came
    // late, and an answered call that ends with no agent is abandoned or
    // ended by the callee in time.
    #[test]
    fn customer_first_never_carries_a_late_agent(
        events in proptest::collection::vec(any_event(), 0..30)
    ) {
        let (a, _) = run_all(&Attempt::with_agent_on_answer(2000), &events);
        if let (Leg::Connected { .. }, AgentLeg::Ready { since }) = (&a.leg, &a.agent) {
            prop_assert!(*since <= a.agent_due().unwrap());
        }
        if let (Some(outcome), Some(_), None) = (a.outcome(), a.answered_at, a.agent_ready_at) {
            prop_assert!(
                matches!(outcome, Outcome::Abandoned { .. })
                    || outcome.detail().ends_with("before the agent joined"),
                "{outcome:?}"
            );
        }
    }

    // Only an answered call can be abandoned.
    #[test]
    fn abandoned_only_after_an_answer(
        start in any_start(),
        events in proptest::collection::vec(any_event(), 0..30)
    ) {
        let (a, _) = run_all(&start, &events);
        if let Some(Outcome::Abandoned { .. }) = a.outcome() {
            prop_assert!(a.answered_at.is_some());
        }
    }
}
