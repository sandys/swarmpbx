//! Every node: run the calls that the leader assigns to this node.
//!
//! A call runs as one task, in the campaign's dial order. Agent first: start
//! the agent, wait until it is in the room, and dial. Customer first: dial,
//! and on the answer start the agent, which must be in the room within the
//! abandonment limit. Then carry the call until it ends, and save each
//! result. Every write carries the node and the assignment generation
//! (`apply_owned`), so a node that lost the call changes nothing.
//! The reconcile checks the live calls of this node that no task runs: a call
//! that a stopped node left, a lost dial result, or a lost end event.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ad_agent::{AgentSpec, CallFacts};
use ad_attempt::{AgentLeg, Attempt, AttemptEvent, Leg, NotDialedReason};
use ad_cluster::{decode, keys, Assignment, Snapshot};
use ad_core::time::Clock;
use ad_core::AttemptId;
use ad_db::{Applied, AttemptRow, LiveAttempt, Owner};
use ad_dispatch::Placement;
use ad_gateway::{CallEnd, Dialed, LiveCall, SipDial, DIAL_TIMEOUT};
use chrono::{DateTime, Utc};
use tokio::sync::{watch, Notify};

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

/// Take the call as far as this node can: agent first, then dial; or dial
/// first, and the agent on the answer.
async fn drive(node: &Node, id: AttemptId, owner: &Owner) -> anyhow::Result<()> {
    let row = node.attempts.get(id).await?;
    if row.node.as_deref() != Some(owner.node.as_str()) || row.assign_gen != owner.gen {
        return Ok(());
    }
    let times = CallTimes::new(node.clock.as_ref());
    let mut attempt = row.attempt.clone();
    if let Leg::Pending = attempt.leg {
        match attempt.agent {
            AgentLeg::Wanted | AgentLeg::Starting { .. } => {
                match prepare_agent(node, &row, owner, attempt, &times).await? {
                    Some(next) => attempt = next,
                    None => return Ok(()),
                }
            }
            AgentLeg::OnAnswer => {
                if let Err(e) = node.gateway.create_room(&row.room).await {
                    let event = AttemptEvent::NotDialed {
                        at: times.now(),
                        reason: NotDialedReason::NoAgent,
                        detail: format!("room: {e}"),
                    };
                    step(node, id, owner, &event).await?;
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    if attempt.can_dial() {
        dial(node, &row, owner, &times).await?;
    }
    Ok(())
}

/// What the agent learns about the call.
fn call_facts(row: &AttemptRow) -> CallFacts<'_> {
    CallFacts {
        attempt_id: row.attempt_id.0,
        campaign_id: row.campaign_id.0,
        lead_id: row.lead_id.0,
        room: &row.room,
        callee_identity: &row.participant_identity,
        phone_number: &row.phone_number,
        lead_context: &row.lead_context,
    }
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
    times: &CallTimes,
) -> anyhow::Result<Option<Attempt>> {
    let id = row.attempt_id;
    let now = || times.now();
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
        let sent = now();
        let event = match node.agents.start(&spec, &call_facts(row)).await {
            Ok(detail) => AttemptEvent::AgentStarting { at: sent, detail },
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
async fn dial(
    node: &Node,
    row: &AttemptRow,
    owner: &Owner,
    times: &CallTimes,
) -> anyhow::Result<()> {
    let id = row.attempt_id;
    // Save the dial start before the INVITE leaves, so a stopped node leaves a
    // `dialing` attempt for the reconcile, never a silent call.
    let started = AttemptEvent::DialStarted { at: times.now() };
    let customer_first = match step(node, id, owner, &started).await? {
        Some(a) if a.leg.name() == "dialing" => matches!(a.agent, AgentLeg::OnAnswer),
        _ => return Ok(()),
    };
    // Customer first: the agent starts at the answer, while the call joins the
    // room, because the abandonment limit counts from the answer.
    let answer = Notify::new();
    let start = async {
        answer.notified().await;
        start_agent(node, row, times).await
    };
    tokio::pin!(start);
    let mut agent_started = None;
    let t = Instant::now();
    let dial = node.caller.dial(SipDial {
        room: &row.room,
        identity: &row.participant_identity,
        phone_number: &row.phone_number,
        call_id: format!("attempt-{}-{}@{}", id.0, owner.gen, owner.node),
        on_answer: customer_first.then_some(&answer),
    });
    tokio::pin!(dial);
    let dialed = loop {
        tokio::select! {
            dialed = &mut dial => break dialed,
            event = &mut start, if customer_first && agent_started.is_none() => {
                agent_started = Some(event);
            }
        }
    };
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
    // The answer time is the 200 OK, not the end of the room join: the
    // abandonment limit counts from it.
    let event = match &live {
        Some(live) => AttemptEvent::Answered {
            at: times.at(live.answered()),
        },
        None => placement.into_event(times.now()),
    };
    // A fenced step drops `live`, which hangs up: the call is not ours.
    let Some(attempt) = step(node, id, owner, &event).await? else {
        return Ok(());
    };
    let Some(live) = live else {
        return Ok(());
    };
    let end = match attempt.agent {
        AgentLeg::OnAnswer => {
            let started = match agent_started {
                Some(event) => Started::Done(event),
                None => Started::Pending(start.as_mut()),
            };
            match agent_on_answer(node, row, owner, &attempt, live, started, times).await? {
                Some(end) => end,
                None => return Ok(()),
            }
        }
        _ => live.run(None).await,
    };
    let at = times.now();
    let event = match &end {
        CallEnd::NoAgent => AttemptEvent::AgentFailed {
            at,
            detail: format!(
                "no agent in the room {} ms after the answer",
                attempt.agent_within_ms.unwrap_or(0)
            ),
        },
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

/// The agent's start request, as `AgentStarting` or `AgentFailed`.
async fn start_agent(node: &Node, row: &AttemptRow, times: &CallTimes) -> AttemptEvent {
    let failed = |detail: String| AttemptEvent::AgentFailed {
        at: times.now(),
        detail,
    };
    let sent = times.now();
    match row.agent.as_ref().map(AgentSpec::parse) {
        Some(Ok(spec)) => match node.agents.start(&spec, &call_facts(row)).await {
            Ok(detail) => AttemptEvent::AgentStarting { at: sent, detail },
            Err(e) => failed(e.to_string()),
        },
        Some(Err(e)) => failed(e.to_string()),
        None => failed("the campaign has no agent".into()),
    }
}

/// The start request of a customer-first agent: done during the dial, or
/// still in flight.
enum Started<F> {
    Done(AttemptEvent),
    Pending(F),
}

/// Customer first: carry the call while the agent starts. The call hangs up
/// when no agent is in the room by the attempt's deadline, also while the
/// start request is in flight. `None` when the attempt ended or moved here;
/// dropping the call hangs up.
async fn agent_on_answer<F: Future<Output = AttemptEvent> + Unpin>(
    node: &Node,
    row: &AttemptRow,
    owner: &Owner,
    attempt: &Attempt,
    live: LiveCall,
    started: Started<F>,
    times: &CallTimes,
) -> anyhow::Result<Option<CallEnd>> {
    let id = row.attempt_id;
    let answered = live.answered();
    let by = answered + Duration::from_millis(attempt.agent_within_ms.unwrap_or(0).into());
    let joined = live.agent_joined();
    let run = live.run(Some(by));
    tokio::pin!(run);
    let started = match started {
        Started::Done(event) => event,
        Started::Pending(start) => tokio::select! {
            end = &mut run => return Ok(Some(end)),
            event = start => event,
        },
    };
    match step(node, id, owner, &started).await? {
        Some(a) if a.leg.is_live() => {}
        _ => return Ok(None),
    }
    let identity = tokio::select! {
        end = &mut run => return Ok(Some(end)),
        Some(identity) = agent_identity(joined) => identity,
    };
    tracing::info!(
        attempt = id.0,
        agent = identity,
        after_answer_ms = answered.elapsed().as_millis() as u64,
        "agent in the room after the answer"
    );
    let ready = AttemptEvent::AgentReady {
        at: times.now(),
        identity,
    };
    match step(node, id, owner, &ready).await? {
        Some(a) if a.leg.is_live() => Ok(Some(run.await)),
        _ => Ok(None),
    }
}

async fn agent_identity(mut joined: watch::Receiver<Option<String>>) -> Option<String> {
    joined
        .wait_for(|j| j.is_some())
        .await
        .ok()
        .and_then(|j| j.clone())
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

/// The times of one call: one read of the node clock, then only the
/// monotonic clock. A re-sync of the node clock during the call cannot change
/// a duration of the call: the ring, the join time (a pacing sample and the
/// abandonment test), or the talk time.
#[derive(Debug, Clone, Copy)]
struct CallTimes {
    start: DateTime<Utc>,
    mono: Instant,
}

impl CallTimes {
    fn new(clock: &dyn Clock) -> Self {
        Self {
            start: clock.now_utc(),
            mono: Instant::now(),
        }
    }

    fn at(&self, mono: Instant) -> DateTime<Utc> {
        self.start
            + chrono::Duration::from_std(mono.saturating_duration_since(self.mono))
                .unwrap_or_default()
    }

    fn now(&self) -> DateTime<Utc> {
        self.at(Instant::now())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A node clock that steps back one hour after its first read.
    struct Stepping {
        start: DateTime<Utc>,
        reads: AtomicU32,
    }

    impl Clock for Stepping {
        fn now_utc(&self) -> DateTime<Utc> {
            match self.reads.fetch_add(1, Ordering::SeqCst) {
                0 => self.start,
                _ => self.start - chrono::Duration::hours(1),
            }
        }
    }

    #[test]
    fn a_step_of_the_node_clock_does_not_change_the_times_of_a_call() {
        let start: DateTime<Utc> = "2030-01-01T00:00:00Z".parse().unwrap();
        let clock = Stepping {
            start,
            reads: AtomicU32::new(0),
        };
        let times = CallTimes::new(&clock);
        assert_eq!(clock.now_utc(), start - chrono::Duration::hours(1));
        let answered = times.mono + Duration::from_millis(1250);
        assert_eq!(
            times.at(answered),
            start + chrono::Duration::milliseconds(1250)
        );
        let earlier = times.mono.checked_sub(Duration::from_millis(5)).unwrap();
        assert_eq!(times.at(earlier), start, "never before the first read");
        assert!(times.now() >= start);
    }
}
