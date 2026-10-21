//! The leader: one node schedules for the whole cluster while it holds
//! `/agentdialer/leader`.
//!
//! - Dispatch: claim the due leads of a campaign, within the campaign cap and
//!   the free capacity of the cluster, and re-gate them on compliance.
//! - Assign: one queue gives each new call to the node with the most free
//!   capacity, at `AD_CALLS_PER_SEC`. The generation goes to Postgres first,
//!   then the key to kine, so the old owner is fenced before a new one starts.
//! - Settle: `attempt_ended` updates the lead, closes the room, and removes the
//!   assignment key; then the freed capacity dials again.
//! - Repair: move the calls of nodes that are gone, remove keys of ended or
//!   moved calls, and assign calls that never reached a node.
//!
//! A node that loses the leader key stops all of this at once.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ad_attempt::{AttemptEvent, NotDialedReason, Outcome};
use ad_cluster::{decode, keys, placement, Assignment, Mirror, NodeLoad};
use ad_compliance::dnc::DncSet;
use ad_compliance::DenyReason;
use ad_core::time::Clock;
use ad_core::{AttemptId, CampaignId, LeadId};
use ad_db::{ClaimedAttempt, LeadUpdate, PgEventConsumer};
use ad_dispatch::Pacer;
use ad_events::{BreakerStore, CampaignEvent, Consumer, EventError};
use ad_hopper::{select, ExcludeReason, Order, SelectionInput};
use ad_orchestrator::{react, settle, ReactorAction};
use tokio::sync::{mpsc, watch};

use super::{every, warn_err, AbortOnDrop, Node};

/// A claimed call that never reached a node is assigned again after this time.
const UNASSIGNED_GRACE: Duration = Duration::from_secs(10);
/// A written assignment counts in the load until the mirror shows it.
const RECENT_TTL: Duration = Duration::from_secs(10);
/// Rows read per repair pass.
const PASS_LIMIT: i64 = 5000;

/// Lead while this node holds the leader key.
pub(super) async fn serve(node: Arc<Node>, mut leading: watch::Receiver<bool>) {
    loop {
        if leading.wait_for(|l| *l).await.is_err() {
            return;
        }
        tracing::info!(node = node.name, "leader: scheduling started");
        let duties = AbortOnDrop(tokio::spawn(lead(node.clone())));
        if leading.wait_for(|l| !*l).await.is_err() {
            return;
        }
        drop(duties);
        tracing::warn!(
            node = node.name,
            "leader: leadership lost; scheduling stopped"
        );
    }
}

/// A call to place on a node. `from` is the node that held it before.
struct Job {
    attempt_id: AttemptId,
    from: Option<String>,
}

struct Scheduler {
    node: Arc<Node>,
    nodes: Mirror,
    calls: Mirror,
    queue: mpsc::UnboundedSender<Job>,
    /// The attempts in the queue, so a repair pass does not queue one twice.
    queued: Mutex<HashSet<AttemptId>>,
    /// Assignments written but not yet in the calls mirror.
    recent: Mutex<HashMap<String, Instant>>,
    pacer: Pacer,
}

async fn lead(node: Arc<Node>) {
    let nodes = Mirror::start(node.kine.clone(), keys::nodes_prefix());
    let calls = Mirror::start(node.kine.clone(), keys::calls_prefix());
    nodes.listed().await;
    calls.listed().await;
    let (queue, jobs) = mpsc::unbounded_channel();
    let s = Arc::new(Scheduler {
        pacer: Pacer::new(node.settings.per_call_gap),
        node: node.clone(),
        nodes,
        calls,
        queue,
        queued: Mutex::default(),
        recent: Mutex::default(),
    });
    let _assigner = AbortOnDrop(tokio::spawn(assigner(s.clone(), jobs)));
    let sweeper = s.clone();
    let _sweep = every(node.settings.retry_sweep, "sweep", move || {
        let s = sweeper.clone();
        async move { s.sweep().await }
    });

    // LISTEN first, then sweep, so the events that the sweep writes reach our
    // own consumer. A reconnect sweeps again.
    loop {
        match PgEventConsumer::connect(node.db.pool(), node.settings.event_sweep).await {
            Ok(mut consumer) => {
                warn_err(s.sweep().await, "sweep");
                loop {
                    match consumer.next_event().await {
                        Ok(Some(event)) => warn_err(s.handle(&event).await, "event handling"),
                        Ok(None) => break,
                        Err(EventError::Decode(e)) => {
                            tracing::warn!(error = %e, "skipped an event that does not decode");
                            None
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "event listener error; reconnecting");
                            break;
                        }
                    };
                }
            }
            Err(e) => tracing::error!(error = %e, "event listener connect failed; retrying"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Take jobs one by one, so each placement sees the load of the one before.
async fn assigner(s: Arc<Scheduler>, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let mut waiting: VecDeque<Job> = VecDeque::new();
    loop {
        let job = match waiting.pop_front() {
            Some(job) => job,
            None => match jobs.recv().await {
                Some(job) => job,
                None => return,
            },
        };
        let loads = s.loads();
        let target = match job.from {
            Some(_) => placement::pick_any(&loads),
            None => placement::pick(&loads),
        };
        let Some(target) = target.map(str::to_string) else {
            // Every node is full (or none is alive): wait for a call to end or a
            // node to join.
            waiting.push_front(job);
            let mut calls = s.calls.subscribe();
            let mut nodes = s.nodes.subscribe();
            tokio::select! {
                _ = calls.changed() => {}
                _ = nodes.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            continue;
        };
        let start = s.pacer.reserve(Instant::now());
        tokio::time::sleep_until(start.into()).await;
        warn_err(s.place(&job, &target).await, "assign a call");
        s.queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&job.attempt_id);
    }
}

impl Scheduler {
    /// Give the call to `target`: the generation to Postgres, then the key to
    /// kine, then remove the key of the node that held it.
    async fn place(&self, job: &Job, target: &str) -> anyhow::Result<()> {
        let id = job.attempt_id;
        let Some(owner) = self.node.attempts.assign(id, target).await? else {
            // The call ended while it waited.
            if let Some(from) = &job.from {
                ad_cluster::unassign(&self.node.kine, from, id.0).await?;
            }
            return Ok(());
        };
        ad_cluster::assign(&self.node.kine, target, id.0, owner.gen).await?;
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(keys::call_key(target, id.0), Instant::now());
        if let Some(from) = job.from.as_deref().filter(|f| *f != target) {
            ad_cluster::unassign(&self.node.kine, from, id.0).await?;
        }
        tracing::info!(
            attempt = id.0,
            node = target,
            gen = owner.gen,
            moved_from = job.from.as_deref().unwrap_or(""),
            "call assigned"
        );
        Ok(())
    }

    fn enqueue(&self, attempt_id: AttemptId, from: Option<String>) {
        let fresh = self
            .queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(attempt_id);
        if fresh {
            let _ = self.queue.send(Job { attempt_id, from });
        }
    }

    /// The live nodes and their calls, with the assignments that the mirror
    /// does not show yet.
    fn loads(&self) -> BTreeMap<String, NodeLoad> {
        let calls = self.calls.current();
        let mut loads = ad_cluster::loads(&self.nodes.current(), &calls);
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        recent.retain(|key, at| !calls.contains_key(key) && at.elapsed() < RECENT_TTL);
        for key in recent.keys() {
            if let Some((node, _)) = keys::parse_call_key(key) {
                if let Some(l) = loads.get_mut(node) {
                    l.assigned += 1;
                }
            }
        }
        loads
    }

    /// The calls the cluster can still take.
    fn free(&self) -> i64 {
        let queued = self.queued.lock().unwrap_or_else(|e| e.into_inner()).len() as i64;
        i64::from(placement::free(&self.loads())) - queued
    }

    async fn handle(self: &Arc<Self>, event: &CampaignEvent) -> anyhow::Result<()> {
        match react(event) {
            ReactorAction::Dispatch { campaign_id } => self.dispatch(CampaignId(campaign_id)).await,
            ReactorAction::SettleAndDispatch {
                campaign_id,
                attempt_id,
            } => {
                self.settle(AttemptId(attempt_id)).await?;
                self.dispatch(CampaignId(campaign_id)).await
            }
            ReactorAction::Noop => Ok(()),
        }
    }

    /// Claim the due leads of a campaign within the free capacity, gate them,
    /// and queue them for a node. Complete the campaign when it is idle.
    async fn dispatch(&self, campaign_id: CampaignId) -> anyhow::Result<()> {
        let node = &self.node;
        if self.pause_if_breaker_open(campaign_id).await? {
            return Ok(());
        }
        loop {
            let limit = node.settings.batch_limit.min(self.free());
            if limit <= 0 {
                return Ok(());
            }
            let claimed = node
                .attempts
                .claim(
                    campaign_id,
                    &node.name,
                    limit,
                    &node.settings.room_prefix,
                    node.clock.now_utc(),
                )
                .await?;
            if claimed.is_empty() {
                if node.db.campaigns().complete_if_idle(campaign_id).await? {
                    tracing::info!(campaign = campaign_id.0, "campaign completed");
                }
                return Ok(());
            }
            let n = claimed.len() as i64;
            self.gate(campaign_id, claimed).await;
            if n < limit {
                return Ok(());
            }
        }
    }

    async fn pause_if_breaker_open(&self, campaign_id: CampaignId) -> anyhow::Result<bool> {
        let node = &self.node;
        let (open, stats) = match node
            .breaker
            .evaluate(campaign_id.0, node.clock.now_utc(), &node.settings.breaker)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // GAPS.md DIAL-8: a read error counts as "closed".
                tracing::warn!(campaign = campaign_id.0, error = %e, "breaker read failed");
                return Ok(false);
            }
        };
        if open
            && node
                .db
                .campaigns()
                .pause_for_breaker(campaign_id, stats.failure_rate)
                .await?
        {
            tracing::warn!(
                campaign = campaign_id.0,
                rate = stats.failure_rate,
                failures = stats.failures,
                total = stats.total,
                "breaker open; campaign paused"
            );
        }
        Ok(open)
    }

    /// Re-gate the claimed leads: end the denied attempts, queue the others.
    async fn gate(&self, campaign_id: CampaignId, claimed: Vec<ClaimedAttempt>) {
        let node = &self.node;
        let dnc = DncSet::default();
        let selection = select(
            &node.clock,
            SelectionInput {
                leads: claimed.iter().map(|c| c.lead.clone()).collect(),
                windows: &node.settings.windows,
                dnc: &dnc,
                call_count_limit: None,
                order: Order::UpCount,
                ruleset: None,
            },
        );
        let mut by_lead: HashMap<LeadId, ClaimedAttempt> =
            claimed.into_iter().map(|c| (c.lead.id, c)).collect();
        for excluded in &selection.excluded {
            let Some(a) = by_lead.remove(&excluded.lead.id) else {
                continue;
            };
            let (reason, detail) = not_dialed(&excluded.reason);
            tracing::info!(
                campaign = campaign_id.0,
                lead = a.lead.id.0,
                attempt = a.attempt_id.0,
                reason = ?reason,
                detail,
                "lead not dialed"
            );
            let event = AttemptEvent::NotDialed {
                at: node.clock.now_utc(),
                reason,
                detail,
            };
            warn_err(
                node.attempts.apply(a.attempt_id, &event).await,
                "end a gated attempt",
            );
        }
        for lead in &selection.eligible {
            if let Some(a) = by_lead.remove(&lead.id) {
                self.enqueue(a.attempt_id, None);
            }
        }
    }

    /// Update the lead from an ended attempt once, then close the room and
    /// remove the assignment.
    async fn settle(&self, attempt_id: AttemptId) -> anyhow::Result<()> {
        let node = &self.node;
        let policy = node.settings.retry;
        let now = node.clock.now_utc();
        let mut report = None;
        let done = node
            .attempts
            .settle(attempt_id, |input| {
                let s = settle(input.outcome, input.called_count, &policy, now);
                let mut events = Vec::new();
                if s.counts_as_call {
                    events.push(CampaignEvent::CallCompleted {
                        campaign_id: input.campaign_id.0,
                        lead_id: input.lead_id.0,
                        answered: matches!(input.outcome, Outcome::Completed { .. }),
                    });
                }
                if let Some((due, _)) = s.retry {
                    events.push(CampaignEvent::RetryScheduled {
                        campaign_id: input.campaign_id.0,
                        lead_id: input.lead_id.0,
                        due_epoch_ms: due.timestamp_millis(),
                    });
                }
                report = Some((
                    input.campaign_id,
                    input.lead_id,
                    input.outcome.class(),
                    s.exhausted,
                ));
                (
                    LeadUpdate {
                        status: s.status,
                        counts_as_call: s.counts_as_call,
                        next_eligible_at: s.next_eligible_at,
                        wait_reason: s.wait_reason,
                    },
                    events,
                )
            })
            .await?;
        if let (Some(update), Some((campaign, lead, outcome, exhausted))) = (done, report) {
            tracing::info!(
                campaign = campaign.0,
                lead = lead.0,
                attempt = attempt_id.0,
                outcome,
                status = update.status.as_str(),
                retry_at = update
                    .next_eligible_at
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default(),
                exhausted,
                "attempt settled"
            );
        }
        self.cleanup(attempt_id).await
    }

    /// Close the room of an ended attempt and remove its assignment key.
    async fn cleanup(&self, attempt_id: AttemptId) -> anyhow::Result<()> {
        let node = &self.node;
        let Some(brief) = node.attempts.briefs(&[attempt_id.0]).await?.pop() else {
            return Ok(());
        };
        if brief.live {
            return Ok(());
        }
        // The agent leaves when its room closes; a SIP leg still in it hangs up.
        if let Err(e) = node.gateway.delete_room(&brief.room).await {
            tracing::warn!(attempt = attempt_id.0, error = %e, "room close failed");
        }
        if let Some(n) = &brief.node {
            ad_cluster::unassign(&node.kine, n, attempt_id.0).await?;
        }
        Ok(())
    }

    /// Settle ended attempts, repair the assignments, and dispatch every
    /// running campaign. This dials the leads whose retry or hold became due,
    /// and it recovers work that a lost event or a stopped node left.
    async fn sweep(&self) -> anyhow::Result<()> {
        let node = &self.node;
        for id in node.attempts.unsettled(PASS_LIMIT).await? {
            warn_err(self.settle(id).await, "settle");
        }
        self.repair().await?;
        for campaign in node.db.campaigns().list_running().await? {
            warn_err(self.dispatch(campaign.id).await, "dispatch");
        }
        Ok(())
    }

    /// Make kine agree with Postgres.
    async fn repair(&self) -> anyhow::Result<()> {
        let node = &self.node;
        let live_nodes: HashSet<String> =
            ad_cluster::loads(&self.nodes.current(), &Default::default())
                .into_keys()
                .collect();
        let calls = self.calls.current();
        let keyed: Vec<(String, i64, i32)> = calls
            .values()
            .filter_map(|e| {
                let (n, a) = keys::parse_call_key(&e.key)?;
                let gen = decode::<Assignment>(&e.value)?.gen;
                Some((n.to_string(), a, gen))
            })
            .collect();
        let ids: Vec<i64> = keyed.iter().map(|(_, a, _)| *a).collect();
        let briefs: HashMap<i64, _> = node
            .attempts
            .briefs(&ids)
            .await?
            .into_iter()
            .map(|b| (b.attempt_id.0, b))
            .collect();

        // Keys: ended calls, stale generations, and calls on nodes that are gone.
        for (key_node, attempt, gen) in &keyed {
            match briefs.get(attempt) {
                None => ad_cluster::unassign(&node.kine, key_node, *attempt).await?,
                Some(b) if !b.live => {
                    if b.settled {
                        self.cleanup(b.attempt_id).await?;
                    }
                }
                Some(b) if b.node.as_deref() != Some(key_node) || b.assign_gen != *gen => {
                    ad_cluster::unassign(&node.kine, key_node, *attempt).await?;
                }
                Some(b) if !live_nodes.contains(key_node) => {
                    tracing::warn!(
                        attempt = attempt,
                        node = key_node,
                        "node gone; moving its call"
                    );
                    self.enqueue(b.attempt_id, Some(key_node.clone()));
                }
                Some(_) => {}
            }
        }

        // Live calls that no key places: claimed but never assigned, or the
        // leader stopped between Postgres and kine.
        let keyed_ids: HashSet<i64> = ids.into_iter().collect();
        let now = node.clock.now_utc();
        for live in node.attempts.live(PASS_LIMIT).await? {
            let unplaced = !keyed_ids.contains(&live.attempt_id.0);
            let old_enough = (now - live.created_at)
                .to_std()
                .is_ok_and(|age| age >= UNASSIGNED_GRACE);
            if unplaced && old_enough {
                self.enqueue(live.attempt_id, live.node.clone());
            }
        }
        Ok(())
    }
}

/// Map a dispatch-gate exclusion to the attempt's end.
fn not_dialed(reason: &ExcludeReason) -> (NotDialedReason, String) {
    match reason {
        ExcludeReason::Compliance(r) => {
            let class = match r {
                DenyReason::Dnc(_) => NotDialedReason::Dnc,
                DenyReason::UnknownLocalTime => NotDialedReason::UnknownTimezone,
                DenyReason::OutsideCallingWindow
                | DenyReason::Holiday
                | DenyReason::InvalidWindowConfig => NotDialedReason::Window,
            };
            (class, r.to_string())
        }
        ExcludeReason::CallCountExceeded => (NotDialedReason::Excluded, "call-count limit".into()),
        ExcludeReason::RoutingExcluded(verb) => {
            (NotDialedReason::Excluded, format!("routing rule: {verb}"))
        }
        ExcludeReason::IneligibleStatus => {
            (NotDialedReason::Released, "the lead is not dialable".into())
        }
    }
}
