//! Every node: run the calls that the leader assigns to this node.
//!
//! A call runs as one task: start the agent, wait until it is in the room,
//! dial, carry the call until it ends, and save each result. Every write
//! carries the node and the assignment
//! generation (`apply_owned`), so a node that lost the call changes nothing.
//! The reconcile checks the live calls of this node that no task runs: a call
//! that a stopped node left, a lost dial result, or a lost end event.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ad_agent::{AgentSpec, CallFacts};
use ad_attempt::{AgentLeg, Attempt, AttemptEvent, Leg};
use ad_cluster::{decode, keys, Assignment, Snapshot};
use ad_core::time::Clock;
use ad_core::AttemptId;
use ad_db::{Applied, AttemptRow, LiveAttempt, Owner};
use ad_dispatch::Placement;
use ad_gateway::{CallEnd, Dialed, SipDial, DIAL_TIMEOUT};
use chrono::{DateTime, Utc};
use tokio::sync::watch;

use super::{Node, Running};

/// How often a node looks for the agent in the room before the dial.
const AGENT_POLL: Duration = Duration::from_millis(250);
/// A pending call without a task is run again after this time.
const PENDING_GRACE: Duration = Duration::from_secs(30);
/// The reconcile waits this long past the longest dial before it decides a dial
/// that a stopped node left.
const DIAL_GRACE: Duration = Duration::from_secs(10);
/// Rows read per reconcile pass.
const PASS_LIMIT: i64 = 5000;

/// Start a task for each call that appears under this node's prefix, and stop
/// the task of a call that the leader takes away.
pub(super) async fn follow(node: Arc<Node>, mut mine: watch::Receiver<Snapshot>) {
    loop {
        let snapshot = mine.borrow_and_update().clone();
        let wanted: HashMap<AttemptId, i32> = snapshot
            .values()
            .filter_map(|e| {
                let (_, attempt) = keys::parse_call_key(&e.key)?;
                let a: Assignment = decode(&e.value)?;
                Some((AttemptId(attempt), a.gen))
            })
            .collect();
        {
            let mut running = node.running.lock().unwrap_or_else(|e| e.into_inner());
            running.retain(|id, r| {
                let keep = wanted.get(id) == Some(&r.gen);
                if !keep {
                    r.task.abort();
                }
                keep
            });
            for (id, gen) in wanted {
                running.entry(id).or_insert_with(|| Running {
                    gen,
                    task: tokio::spawn(run_call(node.clone(), id, gen)),
                });
            }
        }
        if mine.changed().await.is_err() {
            return;
        }
    }
}

async fn run_call(node: Arc<Node>, id: AttemptId, gen: i32) {
    let owner = Owner {
        node: node.name.clone(),
        gen,
    };
    if let Err(e) = drive(&node, id, &owner).await {
        tracing::warn!(attempt = id.0, error = %e, "call task failed; the reconcile retries");
    }
}

/// Take the call as far as this node can: agent, then dial.
async fn drive(node: &Node, id: AttemptId, owner: &Owner) -> anyhow::Result<()> {
    let row = node.attempts.get(id).await?;
    if row.node.as_deref() != Some(owner.node.as_str()) || row.assign_gen != owner.gen {
        return Ok(());
    }
    let mut attempt = row.attempt.clone();
    if matches!(attempt.agent, AgentLeg::Wanted | AgentLeg::Starting { .. }) {
        match prepare_agent(node, &row, owner, attempt).await? {
            Some(next) => attempt = next,
            None => return Ok(()),
        }
    }
    if attempt.can_dial() {
        dial(node, &row, owner).await?;
    }
    Ok(())
}

/// Apply an owned event. `None` when another node owns the call now.
async fn step(
    node: &Node,
    id: AttemptId,
    owner: &Owner,
    event: &AttemptEvent,
) -> anyhow::Result<Option<Attempt>> {
    match node.attempts.apply_owned(id, event, owner).await? {
        Applied::Changed(a) => Ok(Some(a)),
        Applied::Ignored(_) => Ok(Some(node.attempts.get(id).await?.attempt)),
        Applied::Fenced => {
            tracing::info!(attempt = id.0, "another node owns the call now");
            Ok(None)
        }
    }
}

/// Start the agent and wait until it is in the room. Returns the attempt
/// after the agent steps, or `None` when the attempt ended or moved.
async fn prepare_agent(
    node: &Node,
    row: &AttemptRow,
    owner: &Owner,
    mut attempt: Attempt,
) -> anyhow::Result<Option<Attempt>> {
    let id = row.attempt_id;
    let now = || node.clock.now_utc();
    let fail = |detail: String| AttemptEvent::AgentFailed { at: now(), detail };

    let spec = match row.agent.as_ref().map(AgentSpec::parse) {
        Some(Ok(spec)) => spec,
        Some(Err(e)) => return step(node, id, owner, &fail(e.to_string())).await,
        None => return step(node, id, owner, &fail("the campaign has no agent".into())).await,
    };
    if let AgentLeg::Wanted = attempt.agent {
        if let Err(e) = node.gateway.create_room(&row.room).await {
            return step(node, id, owner, &fail(format!("room: {e}"))).await;
        }
        let call = CallFacts {
            attempt_id: id.0,
            campaign_id: row.campaign_id.0,
            lead_id: row.lead_id.0,
            room: &row.room,
            callee_identity: &row.participant_identity,
            phone_number: &row.phone_number,
            lead_context: &row.lead_context,
        };
        let event = match node.agents.start(&spec, &call).await {
            Ok(detail) => AttemptEvent::AgentStarting { at: now(), detail },
            Err(e) => fail(e.to_string()),
        };
        match step(node, id, owner, &event).await? {
            Some(next) => attempt = next,
            None => return Ok(None),
        }
    }
    let AgentLeg::Starting { since } = attempt.agent else {
        return Ok(Some(attempt));
    };

    // A node that takes the call over waits only for the rest of the time.
    let timeout = Duration::from_secs(spec.join_timeout_s().into());
    let deadline = Instant::now() + timeout.saturating_sub(age(since, now()));
    loop {
        match node
            .gateway
            .other_in_room(&row.room, &row.participant_identity)
            .await
        {
            Ok(Some(identity)) => {
                tracing::info!(
                    attempt = id.0,
                    agent = identity,
                    runtime = spec.runtime(),
                    "agent in the room"
                );
                let ready = AttemptEvent::AgentReady {
                    at: now(),
                    identity,
                };
                return step(node, id, owner, &ready).await;
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(attempt = id.0, error = %e, "room check failed"),
        }
        if Instant::now() >= deadline {
            let detail = format!("no agent joined in {} s", spec.join_timeout_s());
            tracing::warn!(attempt = id.0, runtime = spec.runtime(), "{detail}");
            return step(node, id, owner, &fail(detail)).await;
        }
        tokio::time::sleep(AGENT_POLL).await;
    }
}

/// Place the call, carry it until it ends, and save each result. The
/// capacity stays taken until the attempt ends.
async fn dial(node: &Node, row: &AttemptRow, owner: &Owner) -> anyhow::Result<()> {
    let id = row.attempt_id;
    // Save the dial start before the INVITE leaves, so a stopped node leaves a
    // `dialing` attempt for the reconcile, never a silent call.
    let started = AttemptEvent::DialStarted {
        at: node.clock.now_utc(),
    };
    match step(node, id, owner, &started).await? {
        Some(a) if a.leg.name() == "dialing" => {}
        _ => return Ok(()),
    }
    let t = Instant::now();
    let dialed = node
        .caller
        .dial(SipDial {
            room: &row.room,
            identity: &row.participant_identity,
            phone_number: &row.phone_number,
            call_id: format!("attempt-{}-{}@{}", id.0, owner.gen, owner.node),
        })
        .await;
    let (placement, live) = match dialed {
        Dialed::Answered(live) => (Placement::Answered, Some(live)),
        Dialed::Failed(placement) => (placement, None),
    };
    let (result, sip_code, detail) = match &placement {
        Placement::Answered => ("answered", None, ""),
        Placement::Rejected { sip_code, detail } => ("rejected", *sip_code, detail.as_str()),
        Placement::NotSent(d) => ("not_sent", None, d.as_str()),
        Placement::NoResponse(d) => ("no_response", None, d.as_str()),
    };
    tracing::info!(
        campaign = row.campaign_id.0,
        lead = row.lead_id.0,
        attempt = id.0,
        node = owner.node,
        result,
        sip_code,
        detail,
        setup_ms = t.elapsed().as_millis() as u64,
        "call attempt"
    );
    // A fenced step drops `live`, which hangs up: the call is not ours.
    if step(node, id, owner, &placement.into_event(node.clock.now_utc()))
        .await?
        .is_none()
    {
        return Ok(());
    }
    let Some(live) = live else {
        return Ok(());
    };
    let end = live.run().await;
    let at = node.clock.now_utc();
    let event = match &end {
        CallEnd::CalleeHangup => AttemptEvent::Hangup {
            at,
            detail: "the callee hung up".into(),
        },
        CallEnd::AgentLeft => AttemptEvent::AgentLeft { at },
        CallEnd::MediaTimeout => AttemptEvent::Hangup {
            at,
            detail: "no RTP from the callee".into(),
        },
        CallEnd::Other(why) => AttemptEvent::Hangup {
            at,
            detail: why.clone(),
        },
    };
    tracing::info!(attempt = id.0, node = owner.node, end = ?end, "call ended");
    step(node, id, owner, &event).await?;
    Ok(())
}

/// Check this node's live calls that no task runs.
pub(super) async fn reconcile(node: &Arc<Node>) -> anyhow::Result<()> {
    let now = node.clock.now_utc();
    let busy: Vec<AttemptId> = {
        let running = node.running.lock().unwrap_or_else(|e| e.into_inner());
        running
            .iter()
            .filter(|(_, r)| !r.task.is_finished())
            .map(|(id, _)| *id)
            .collect()
    };
    for live in node.attempts.live(PASS_LIMIT).await? {
        if live.node.as_deref() != Some(node.name.as_str())
            || busy.contains(&live.attempt_id)
            || !due_for_check(node, &live, now)
        {
            continue;
        }
        let owner = Owner {
            node: node.name.clone(),
            gen: live.assign_gen,
        };
        if let Leg::Pending = live.attempt.leg {
            // The call task ended early (for example a storage error): run it
            // again from the saved state.
            let task = tokio::spawn(run_call(node.clone(), live.attempt_id, live.assign_gen));
            node.running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    live.attempt_id,
                    Running {
                        gen: live.assign_gen,
                        task,
                    },
                );
            continue;
        }
        let present = match node
            .gateway
            .present(&live.room, &live.participant_identity)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(attempt = live.attempt_id.0, error = %e, "presence check failed");
                continue;
            }
        };
        let event = if present {
            AttemptEvent::Present { at: now }
        } else {
            AttemptEvent::Absent { at: now }
        };
        match node
            .attempts
            .apply_owned(live.attempt_id, &event, &owner)
            .await?
        {
            Applied::Changed(a) => tracing::info!(
                campaign = live.campaign_id.0,
                attempt = live.attempt_id.0,
                event = event.kind(),
                state = a.leg.name(),
                "attempt reconciled"
            ),
            // A connected call that is still in its room.
            Applied::Ignored(_) => node.attempts.mark_checked(live.attempt_id, now).await?,
            Applied::Fenced => {}
        }
    }
    Ok(())
}

fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - since).to_std().unwrap_or_default()
}

/// Whether a live call without a task is ready for a check.
fn due_for_check(node: &Node, live: &LiveAttempt, now: DateTime<Utc>) -> bool {
    match &live.attempt.leg {
        Leg::Pending => age(live.created_at, now) >= PENDING_GRACE,
        Leg::Dialing { since } => age(*since, now) >= DIAL_TIMEOUT + DIAL_GRACE,
        Leg::Uncertain { since, .. } => age(*since, now) >= DIAL_GRACE,
        Leg::Connected { since } => {
            age(live.checked_at.unwrap_or(*since), now) >= node.settings.attempt_check
        }
        Leg::Ended { .. } => false,
    }
}
