//! Integration tests for call attempts on Postgres: the eligibility claim and
//! capacity, the saved statechart, settlement, and the campaign predicates.
//! Gated on DATABASE_URL.

mod common;

use std::collections::HashSet;

use ad_attempt::{replay, AttemptEvent, NotDialedReason};
use ad_campaign::repo::CampaignRepo;
use ad_campaign::{CampaignState, Transition};
use ad_core::{AttemptId, CampaignId};
use ad_db::{Applied, Db, LeadUpdate, SettleInput};
use ad_events::CampaignEvent;
use ad_lead::repo::LeadRepo;
use ad_lead::LeadStatus;
use chrono::{Duration, Utc};
use common::{apply_all, campaign, claim, complete, leads, reject, running_campaign, t0};

async fn kinds(db: &Db, c: CampaignId) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM campaign_events WHERE campaign_id = $1 ORDER BY id")
        .bind(c.0)
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// A settlement rule that sets one status and emits no event.
fn to_status(
    status: LeadStatus,
) -> impl FnOnce(&SettleInput<'_>) -> (LeadUpdate, Vec<CampaignEvent>) {
    move |_| {
        (
            LeadUpdate {
                status,
                counts_as_call: true,
                next_eligible_at: None,
                wait_reason: None,
            },
            Vec::new(),
        )
    }
}

#[tokio::test]
async fn the_claim_takes_only_the_free_capacity() {
    let Some(db) = common::db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let c = running_campaign(&db, 3).await;
    leads(&db, c, 5).await;

    let first = claim(&db, c, 10).await;
    assert_eq!(first.len(), 3, "the cap is 3");
    assert!(claim(&db, c, 10).await.is_empty(), "no free capacity");
    assert_eq!(db.attempts().live_count(c).await.unwrap(), 3);

    // One call ends: one unit of capacity is free again.
    complete(&db, first[0].attempt_id, Utc::now(), 0).await;
    assert_eq!(db.attempts().live_count(c).await.unwrap(), 2);
    assert_eq!(claim(&db, c, 10).await.len(), 1);
}

#[tokio::test]
async fn concurrent_claims_are_disjoint_and_stay_within_the_cap() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 8).await;
    leads(&db, c, 20).await;
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move { claim(&db, c, 5).await }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.extend(t.await.unwrap().into_iter().map(|a| a.lead.id));
    }
    let unique: HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "a lead was claimed twice");
    assert_eq!(ids.len(), 8, "the four claims together fill the cap");
}

#[tokio::test]
async fn the_claim_reads_only_due_dialable_leads() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    let ls = leads(&db, c, 4).await;
    let set = |id: i64, sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(sql).bind(id).execute(db.pool()).await.unwrap();
        }
    };
    set(
        ls[0].id.0,
        "UPDATE leads SET status = 'queued', next_eligible_at = now() + interval '1 hour' WHERE id = $1",
    )
    .await;
    set(
        ls[1].id.0,
        "UPDATE leads SET status = 'queued', next_eligible_at = now() - interval '1 second' WHERE id = $1",
    )
    .await;
    set(
        ls[2].id.0,
        "UPDATE leads SET status = 'dispositioned' WHERE id = $1",
    )
    .await;
    // ls[3] stays new.

    let claimed: HashSet<_> = claim(&db, c, 10).await.iter().map(|a| a.lead.id).collect();
    assert_eq!(claimed, HashSet::from([ls[1].id, ls[3].id]));
    for id in [ls[1].id, ls[3].id] {
        assert_eq!(db.leads().get(id).await.unwrap().status, LeadStatus::InCall);
    }
}

#[tokio::test]
async fn a_campaign_that_is_not_running_claims_nothing() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = campaign(&db, 10).await;
    leads(&db, c, 2).await;
    assert!(claim(&db, c, 10).await.is_empty());
}

#[tokio::test]
async fn the_saved_events_replay_to_the_saved_snapshot() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    assert_eq!(
        a.participant_identity,
        format!("sip-attempt-{}", a.attempt_id.0)
    );

    let at = t0();
    apply_all(
        &db,
        a.attempt_id,
        &[
            AttemptEvent::DialStarted { at },
            AttemptEvent::NoResponse {
                at: at + Duration::seconds(30),
                detail: "timeout".into(),
            },
            AttemptEvent::Present {
                at: at + Duration::seconds(40),
            },
        ],
    )
    .await;
    // A repeated reconcile result does not apply and saves nothing.
    let again = db
        .attempts()
        .apply(
            a.attempt_id,
            &AttemptEvent::Present {
                at: at + Duration::seconds(41),
            },
        )
        .await
        .unwrap();
    assert!(matches!(again, Applied::Ignored(_)));

    let events = db.attempts().events(a.attempt_id).await.unwrap();
    assert_eq!(events.len(), 3);
    let snapshot = db.attempts().snapshot(a.attempt_id).await.unwrap();
    assert_eq!(
        replay(ad_attempt::Attempt::default(), &events).unwrap(),
        snapshot
    );
    assert_eq!(snapshot.leg.name(), "connected");
    assert!(!kinds(&db, c).await.contains(&"attempt_ended".to_string()));
}

#[tokio::test]
async fn the_end_of_an_attempt_commits_with_its_event() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    reject(&db, a.attempt_id, 486, Utc::now(), 0).await;

    let row: (String, Option<String>, Option<i32>, bool) =
        sqlx::query_as("SELECT state, outcome, sip_code, failure FROM call_attempts WHERE id = $1")
            .bind(a.attempt_id.0)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(row, ("ended".into(), Some("busy".into()), Some(486), false));
    let payloads: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM campaign_events WHERE campaign_id = $1 AND kind = 'attempt_ended'",
    )
    .bind(c.0)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["attempt_id"], a.attempt_id.0);
}

#[tokio::test]
async fn settlement_updates_the_lead_once() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    let dialed = t0();
    apply_all(
        &db,
        a.attempt_id,
        &[
            AttemptEvent::DialStarted { at: dialed },
            AttemptEvent::Rejected {
                at: dialed,
                sip_code: Some(480),
                detail: "no answer".into(),
            },
        ],
    )
    .await;

    let due = dialed + Duration::minutes(30);
    let rule = move |input: &SettleInput<'_>| {
        assert_eq!(input.outcome.class(), "no_answer");
        assert_eq!(input.called_count, 0);
        (
            LeadUpdate {
                status: LeadStatus::Queued,
                counts_as_call: true,
                next_eligible_at: Some(due),
                wait_reason: Some("no_answer"),
            },
            vec![CampaignEvent::RetryScheduled {
                campaign_id: input.campaign_id.0,
                lead_id: input.lead_id.0,
                due_epoch_ms: due.timestamp_millis(),
            }],
        )
    };
    assert!(db
        .attempts()
        .settle(a.attempt_id, rule)
        .await
        .unwrap()
        .is_some());
    // A second settlement (a repeated event, or the sweep) does nothing.
    let second = db
        .attempts()
        .settle(a.attempt_id, to_status(LeadStatus::Done))
        .await
        .unwrap();
    assert_eq!(second, None);

    let lead = db.leads().get(a.lead.id).await.unwrap();
    assert_eq!(lead.status, LeadStatus::Queued);
    assert_eq!(lead.called_count, 1);
    assert_eq!(lead.last_local_call_time, Some(dialed));
    let wait: (Option<chrono::DateTime<Utc>>, Option<String>) =
        sqlx::query_as("SELECT next_eligible_at, retry_reason FROM leads WHERE id = $1")
            .bind(a.lead.id.0)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(wait, (Some(due), Some("no_answer".into())));
    assert_eq!(
        kinds(&db, c)
            .await
            .iter()
            .filter(|k| *k == "retry_scheduled")
            .count(),
        1
    );
    assert!(db
        .attempts()
        .unsettled(1000)
        .await
        .unwrap()
        .iter()
        .all(|id| *id != a.attempt_id));
}

#[tokio::test]
async fn a_live_attempt_cannot_be_settled() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    let r = db
        .attempts()
        .settle(a.attempt_id, to_status(LeadStatus::Done))
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(
        db.leads().get(a.lead.id).await.unwrap().status,
        LeadStatus::InCall
    );
}

#[tokio::test]
async fn webhooks_find_the_live_attempt_by_room() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    let found = db.attempts().live_in_room(&a.room).await.unwrap();
    assert_eq!(
        found,
        vec![ad_db::RoomAttempt {
            attempt_id: a.attempt_id,
            callee_identity: a.participant_identity.clone(),
            agent_identity: None,
        }]
    );
    complete(&db, a.attempt_id, Utc::now(), 0).await;
    assert!(db
        .attempts()
        .live_in_room(&a.room)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn completion_waits_for_live_calls_and_due_work() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    let ls = leads(&db, c, 2).await;
    let claimed = claim(&db, c, 1).await;
    let a = &claimed[0];

    // A new lead and a live attempt: not idle.
    assert!(!db.campaigns().complete_if_idle(c).await.unwrap());

    // The attempt ends but is not settled yet: not idle.
    complete(&db, a.attempt_id, Utc::now(), 0).await;
    db.attempts()
        .settle(a.attempt_id, to_status(LeadStatus::Dispositioned))
        .await
        .unwrap();
    // The other lead waits for a retry in an hour: not idle.
    let other = ls.iter().find(|l| l.id != a.lead.id).unwrap();
    sqlx::query(
        "UPDATE leads SET status = 'queued', next_eligible_at = now() + interval '1 hour' WHERE id = $1",
    )
    .bind(other.id.0)
    .execute(db.pool())
    .await
    .unwrap();
    assert!(!db.campaigns().complete_if_idle(c).await.unwrap());

    sqlx::query("UPDATE leads SET status = 'done' WHERE id = $1")
        .bind(other.id.0)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(db.campaigns().complete_if_idle(c).await.unwrap());
    assert!(
        !db.campaigns().complete_if_idle(c).await.unwrap(),
        "only once"
    );
    assert_eq!(
        db.campaigns().get(c).await.unwrap().state,
        CampaignState::Completed
    );
    let completed = kinds(&db, c)
        .await
        .into_iter()
        .filter(|k| k == "campaign_completed")
        .count();
    assert_eq!(completed, 1);
}

#[tokio::test]
async fn a_live_or_unsettled_attempt_blocks_completion() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    // An operator closes the lead while its call is live. The lead no longer
    // blocks completion; the live attempt must.
    let close_lead = || {
        sqlx::query("UPDATE leads SET status = 'done' WHERE id = $1")
            .bind(a.lead.id.0)
            .execute(db.pool())
    };
    close_lead().await.unwrap();
    assert!(!db.campaigns().complete_if_idle(c).await.unwrap());

    // The attempt ends, but it has not updated its lead yet.
    apply_all(
        &db,
        a.attempt_id,
        &[AttemptEvent::NotDialed {
            at: Utc::now(),
            reason: NotDialedReason::Dnc,
            detail: "system DNC".into(),
        }],
    )
    .await;
    assert!(db
        .attempts()
        .unsettled(1000)
        .await
        .unwrap()
        .contains(&a.attempt_id));
    assert!(!db.campaigns().complete_if_idle(c).await.unwrap());

    db.attempts()
        .settle(a.attempt_id, to_status(LeadStatus::Dnc))
        .await
        .unwrap();
    assert!(db.campaigns().complete_if_idle(c).await.unwrap());
}

#[tokio::test]
async fn a_breaker_pause_is_saved_once() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    assert!(db.campaigns().pause_for_breaker(c, 0.8).await.unwrap());
    assert!(!db.campaigns().pause_for_breaker(c, 0.8).await.unwrap());
    assert_eq!(
        db.campaigns().get(c).await.unwrap().state,
        CampaignState::Paused
    );
    let trips = kinds(&db, c)
        .await
        .into_iter()
        .filter(|k| k == "circuit_breaker_tripped")
        .count();
    assert_eq!(trips, 1);
}

#[tokio::test]
async fn entering_running_writes_an_event_and_an_illegal_transition_changes_nothing() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    assert_eq!(kinds(&db, c).await, ["campaign_running"]);

    let illegal = db
        .campaigns()
        .transition(c, Transition::Resume, Utc::now())
        .await
        .unwrap();
    assert!(illegal.is_err(), "a running campaign cannot resume");
    assert_eq!(kinds(&db, c).await, ["campaign_running"]);

    for t in [Transition::Pause, Transition::Resume] {
        db.campaigns()
            .transition(c, t, Utc::now())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        kinds(&db, c).await,
        ["campaign_running", "campaign_running"]
    );
    let reset: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT breaker_reset_at FROM campaigns WHERE id = $1")
            .bind(c.0)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(reset.is_some(), "a resume resets the breaker");
}

#[tokio::test]
async fn reconcile_lists_live_attempts_and_records_checks() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    let mine = |db: Db, id: AttemptId| async move {
        db.attempts()
            .live(100_000)
            .await
            .unwrap()
            .into_iter()
            .find(|l| l.attempt_id == id)
    };
    let live = mine(db.clone(), a.attempt_id).await.expect("listed");
    assert_eq!(live.attempt.leg.name(), "pending");
    assert_eq!(live.checked_at, None);

    let at = t0();
    db.attempts().mark_checked(a.attempt_id, at).await.unwrap();
    let live = mine(db.clone(), a.attempt_id).await.unwrap();
    assert_eq!(live.checked_at, Some(at));
}

#[tokio::test]
async fn a_node_that_lost_the_call_is_fenced() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let a = claim(&db, c, 1).await.remove(0);
    let first = db
        .attempts()
        .assign(a.attempt_id, "node-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.gen, 1);
    let start = AttemptEvent::DialStarted { at: t0() };

    // The leader moves the call to another node.
    let second = db
        .attempts()
        .assign(a.attempt_id, "node-b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.gen, 2);
    // The old owner can no longer dial or change it.
    assert_eq!(
        db.attempts()
            .apply_owned(a.attempt_id, &start, &first)
            .await
            .unwrap(),
        Applied::Fenced
    );
    // The same node name with an old generation is fenced too.
    let stale = ad_db::Owner {
        node: "node-b".into(),
        gen: 1,
    };
    assert_eq!(
        db.attempts()
            .apply_owned(a.attempt_id, &start, &stale)
            .await
            .unwrap(),
        Applied::Fenced
    );
    assert!(db.attempts().events(a.attempt_id).await.unwrap().is_empty());
    // The new owner can.
    assert!(matches!(
        db.attempts()
            .apply_owned(a.attempt_id, &start, &second)
            .await
            .unwrap(),
        Applied::Changed(_)
    ));
    // An ended attempt is not assigned again.
    apply_all(
        &db,
        a.attempt_id,
        &[
            AttemptEvent::Answered { at: t0() },
            AttemptEvent::Hangup {
                at: t0(),
                detail: "bye".into(),
            },
        ],
    )
    .await;
    assert_eq!(
        db.attempts().assign(a.attempt_id, "node-c").await.unwrap(),
        None
    );
    let brief = db
        .attempts()
        .briefs(&[a.attempt_id.0])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        (brief.node.as_deref(), brief.assign_gen, brief.live),
        (Some("node-b"), 2, false)
    );
}

#[tokio::test]
async fn a_campaign_with_an_agent_gets_rooms_per_attempt_and_waits_for_the_agent() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    sqlx::query("UPDATE campaigns SET agent = $2 WHERE id = $1")
        .bind(c.0)
        .bind(serde_json::json!({"runtime": "livekit", "agent_name": "sales"}))
        .execute(db.pool())
        .await
        .unwrap();
    leads(&db, c, 2).await;
    let claimed = claim(&db, c, 2).await;
    for a in &claimed {
        assert_eq!(
            a.room,
            format!("room-{}", a.attempt_id.0),
            "one room per attempt"
        );
        assert_eq!(a.agent.as_ref().unwrap()["agent_name"], "sales");
        let s = db.attempts().snapshot(a.attempt_id).await.unwrap();
        assert_eq!(s.agent.name(), "wanted");
        assert!(!s.can_dial(), "no dial before the agent is ready");
    }
    let a = &claimed[0];
    apply_all(
        &db,
        a.attempt_id,
        &[
            AttemptEvent::AgentStarting {
                at: t0(),
                detail: "dispatch".into(),
            },
            AttemptEvent::AgentReady {
                at: t0(),
                identity: "agent-AJ_1".into(),
            },
        ],
    )
    .await;
    let row = db.attempts().get(a.attempt_id).await.unwrap();
    assert_eq!(row.agent_identity.as_deref(), Some("agent-AJ_1"));
    assert!(row.attempt.can_dial());
    let found = db.attempts().live_in_room(&a.room).await.unwrap();
    assert_eq!(found[0].agent_identity.as_deref(), Some("agent-AJ_1"));
}

async fn with_agent(db: &Db, c: CampaignId) {
    sqlx::query("UPDATE campaigns SET agent = $2 WHERE id = $1")
        .bind(c.0)
        .bind(serde_json::json!({"runtime": "livekit", "agent_name": "sales"}))
        .execute(db.pool())
        .await
        .unwrap();
}

async fn claim_on_answer(db: &Db, c: CampaignId) -> ad_db::ClaimedAttempt {
    db.attempts()
        .claim(c, "test-worker", 1, "room", Utc::now(), Some(2000))
        .await
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn customer_first_dials_first_and_the_retry_of_an_abandoned_call_waits_for_the_agent() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    with_agent(&db, c).await;
    leads(&db, c, 1).await;
    let a = claim_on_answer(&db, c).await;
    let s = db.attempts().snapshot(a.attempt_id).await.unwrap();
    assert_eq!(s.agent.name(), "on_answer");
    assert_eq!(s.agent_within_ms, Some(2000));
    assert!(s.can_dial(), "customer first dials before the agent starts");
    let state: String = sqlx::query_scalar("SELECT agent_state FROM call_attempts WHERE id = $1")
        .bind(a.attempt_id.0)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(state, "on_answer");

    apply_all(
        &db,
        a.attempt_id,
        &[
            AttemptEvent::DialStarted { at: t0() },
            AttemptEvent::Answered { at: t0() },
            AttemptEvent::AgentStarting {
                at: t0(),
                detail: "dispatch".into(),
            },
            AttemptEvent::AgentFailed {
                at: t0() + Duration::seconds(2),
                detail: "no agent in the room 2000 ms after the answer".into(),
            },
        ],
    )
    .await;
    let rule = |input: &SettleInput<'_>| {
        assert_eq!(input.outcome.class(), "abandoned");
        (
            LeadUpdate {
                status: LeadStatus::Queued,
                counts_as_call: true,
                next_eligible_at: None,
                wait_reason: Some("abandoned"),
            },
            Vec::new(),
        )
    };
    db.attempts().settle(a.attempt_id, rule).await.unwrap();

    let retry = claim_on_answer(&db, c).await;
    assert_eq!(retry.lead.id, a.lead.id);
    let s = db.attempts().snapshot(retry.attempt_id).await.unwrap();
    assert_eq!(s.agent.name(), "wanted", "the retry starts the agent first");
}

#[tokio::test]
async fn the_ledger_counts_the_answered_calls_in_the_window() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    with_agent(&db, c).await;
    leads(&db, c, 3).await;
    let claimed = db
        .attempts()
        .claim(c, "test-worker", 3, "room", Utc::now(), Some(2000))
        .await
        .unwrap();
    let now = Utc::now();
    let answered = [
        AttemptEvent::DialStarted { at: now },
        AttemptEvent::Answered { at: now },
    ];
    let ends = [
        AttemptEvent::Hangup {
            at: now,
            detail: "the callee hung up".into(),
        },
        AttemptEvent::AgentFailed {
            at: now,
            detail: "no agent".into(),
        },
    ];
    for (a, end) in claimed.iter().zip(ends) {
        apply_all(&db, a.attempt_id, &answered).await;
        apply_all(&db, a.attempt_id, &[end]).await;
    }
    apply_all(
        &db,
        claimed[2].attempt_id,
        &[
            AttemptEvent::DialStarted { at: now },
            AttemptEvent::Rejected {
                at: now,
                sip_code: Some(486),
                detail: "busy".into(),
            },
        ],
    )
    .await;
    let counts = db
        .attempts()
        .abandon_counts(c, now - Duration::days(30))
        .await
        .unwrap();
    assert_eq!(
        (counts.answered, counts.abandoned),
        (2, 1),
        "a busy call is no answer"
    );
    let later = db
        .attempts()
        .abandon_counts(c, now + Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(
        (later.answered, later.abandoned),
        (0, 0),
        "outside the window"
    );
}
