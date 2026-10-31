//! The leader: one node schedules for the whole cluster while it holds
//! `/agentdialer/leader`.
//!
//! - Dispatch: claim the due leads of a campaign, within the campaign cap, the
//!   free capacity of the cluster, and the pacing window of the campaign's
//!   agent pool, and re-gate them on compliance. The claim dials the customer
//!   first only while the campaign's abandonment rate is at or under the cap;
//!   else it starts the agent first.
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

use ad_agent::{AgentSpec, DialOrder};
use ad_attempt::{AttemptEvent, NotDialedReason, Outcome};
use ad_campaign::repo::CampaignRepo;
use ad_cluster::{decode, keys, placement, Assignment, Mirror, NodeLoad};
use ad_compliance::abandon::JurisdictionRule;
use ad_compliance::DenyReason;
use ad_core::time::Clock;
use ad_core::{AttemptId, CampaignId, LeadId};
use ad_db::{ClaimRequest, ClaimedAttempt, LeadUpdate, PaceClaim, PgEventConsumer, SettleInput};
use ad_dispatch::Pacer;
use ad_events::{BreakerStore, CampaignEvent, Consumer, EventError};
use ad_hopper::{select, ExcludeReason, Order, SelectionInput};
use ad_orchestrator::{react, settle_with, ReactorAction};
use ad_pacing::{Mode, State as PoolState};
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

/// How the next claims of a campaign dial: the dial order, and the pacing
/// window of its agent pool (GAPS.md DIAL-9).
struct Plan {
    agent_on_answer: Option<u32>,
    pace: Option<PaceClaim>,
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

    /// Claim the due leads of a campaign within the free capacity and the
    /// pacing window of its agent pool, gate them, and queue them for a node.
    /// Complete the campaign when it is idle.
    async fn dispatch(&self, campaign_id: CampaignId) -> anyhow::Result<()> {
        let node = &self.node;
        if self.pause_if_breaker_open(campaign_id).await? {
            return Ok(());
        }
        let Some(plan) = self.plan(campaign_id).await else {
            return Ok(());
        };
        loop {
            let limit = node.settings.batch_limit.min(self.free());
            if limit <= 0 {
                return Ok(());
            }
            let claimed = node
                .attempts
                .claim_paced(
                    campaign_id,
                    &ClaimRequest {
                        worker: &node.name,
                        limit,
                        room_prefix: &node.settings.room_prefix,
                        now: node.clock.now_utc(),
                        agent_on_answer: plan.agent_on_answer,
                        pace: plan.pace.as_ref(),
                    },
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

    /// How the next claims of a campaign dial. `None` claims nothing: the
    /// campaign cannot be read.
    async fn plan(&self, campaign_id: CampaignId) -> Option<Plan> {
        let node = &self.node;
        let campaign = match node.db.campaigns().get(campaign_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(campaign = campaign_id.0, error = %e, "campaign read failed; no claim");
                return None;
            }
        };
        let Some(spec) = campaign
            .agent
            .as_ref()
            .and_then(|a| AgentSpec::parse(a).ok())
        else {
            return Some(Plan {
                agent_on_answer: None,
                pace: None,
            });
        };
        // 0 fails closed: the settings do not resolve, and the limit is 1.
        let connect_within_ms = JurisdictionRule::CAMPAIGN.connect_within_ms().unwrap_or(0);
        // A mode that cannot be read is the smaller window.
        let mode = match node.db.pacing().mode(campaign_id).await {
            Ok(m) => m?,
            Err(e) => {
                tracing::warn!(campaign = campaign_id.0, error = %e, "pacing mode read failed; strict");
                Mode::Strict
            }
        };
        let pool = spec.pool();
        // The claim still uses the saved limit when this fails.
        match node.db.pacing().ingest(&pool, connect_within_ms).await {
            Ok(state) => {
                if self
                    .trip_pool(&pool, &state, connect_within_ms)
                    .await
                    .contains(&campaign_id)
                {
                    return None;
                }
            }
            Err(e) => tracing::warn!(pool, error = %e, "pacing samples not read"),
        }
        Some(Plan {
            agent_on_answer: self.agent_on_answer(campaign_id, &spec).await,
            pace: Some(PaceClaim {
                pool,
                mode,
                connect_within_ms,
            }),
        })
    }

    /// The agent's limit in milliseconds when the campaign dials the customer
    /// first, or `None` for agent first. Fail closed: above the abandonment
    /// cap, or when the ledger cannot be read, the dialer starts the agent
    /// before the dial, which cannot abandon a call.
    async fn agent_on_answer(&self, campaign_id: CampaignId, spec: &AgentSpec) -> Option<u32> {
        let node = &self.node;
        if spec.dial_order() != DialOrder::CustomerFirst {
            return None;
        }
        let rule = JurisdictionRule::CAMPAIGN;
        let since = node.clock.now_utc() - rule.window();
        let counts = match node.attempts.abandon_counts(campaign_id, since).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(campaign = campaign_id.0, error = %e, "ledger read failed; agent first");
                return None;
            }
        };
        if !rule.allows_customer_first(&counts) {
            tracing::warn!(
                campaign = campaign_id.0,
                rule = rule.name(),
                answered = counts.answered,
                abandoned = counts.abandoned,
                "abandonment rate above the cap; agent first"
            );
            return None;
        }
        rule.connect_within_ms()
    }

    /// Pause the running campaigns of a pool whose agents do not start
    /// (GAPS.md DIAL-9 item 7). Returns the campaigns that it paused.
    async fn trip_pool(
        &self,
        pool: &str,
        state: &PoolState,
        connect_within_ms: u32,
    ) -> Vec<CampaignId> {
        if state.failed_in_row == 0 {
            return Vec::new();
        }
        let node = &self.node;
        let Some(running) = warn_err(
            node.db.campaigns().list_running().await,
            "running campaigns",
        ) else {
            return Vec::new();
        };
        let of_pool: Vec<CampaignId> = running
            .iter()
            .filter(|c| {
                c.agent
                    .as_ref()
                    .and_then(|a| AgentSpec::parse(a).ok())
                    .is_some_and(|s| s.pool() == pool)
            })
            .map(|c| c.id)
            .collect();
        let trip = node
            .db
            .pacing()
            .trip(pool, connect_within_ms, &of_pool)
            .await;
        let Some(Some(trip)) = warn_err(trip, "agent pool breaker") else {
            return Vec::new();
        };
        tracing::warn!(
            pool,
            campaigns = ?trip.paused.iter().map(|c| c.0).collect::<Vec<_>>(),
            failed_in_row = trip.failed_in_row,
            attempts = ?trip.attempt_ids,
            "agent pool breaker open; campaigns paused"
        );
        trip.paused
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
        let numbers: Vec<_> = claimed
            .iter()
            .filter_map(|c| ad_contact::phone_key(&c.lead.phone_number))
            .collect();
        let dnc = match node.db.dnc().set_for(campaign_id, &numbers).await {
            Ok(set) => set,
            Err(e) => {
                // Fail closed: without the DNC list, no claimed lead dials.
                tracing::warn!(campaign = campaign_id.0, error = %e, "the DNC list does not read");
                for a in claimed {
                    let event = AttemptEvent::NotDialed {
                        at: node.clock.now_utc(),
                        reason: NotDialedReason::Released,
                        detail: format!("the DNC list does not read: {e}"),
                    };
                    warn_err(
                        node.attempts.apply(a.attempt_id, &event).await,
                        "release a claimed attempt",
                    );
                }
                return;
            }
        };
        let selection = select(
            node.clock.as_ref(),
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
    /// remove the assignment. An answered call that its agent joined waits
    /// for the agent's outcome, at most `outcome_wait_s` after its end.
    async fn settle(&self, attempt_id: AttemptId) -> anyhow::Result<()> {
        let node = &self.node;
        let policy = node.settings.retry;
        let now = node.clock.now_utc();
        let mut report = None;
        let mut waits = false;
        let done = node
            .attempts
            .settle_or_wait(attempt_id, |input| {
                if waits_for_outcome(input, now) {
                    waits = true;
                    return None;
                }
                let s = settle_with(
                    input.outcome,
                    input.agent_outcome,
                    input.called_count,
                    &policy,
                    now,
                );
                let mut events = Vec::new();
                if s.counts_as_call {
                    events.push(CampaignEvent::CallCompleted {
                        campaign_id: input.campaign_id.0,
                        lead_id: input.lead_id.0,
                        answered: matches!(input.outcome, Outcome::Completed { .. }),
                    });
                }
                if let Some(due) = s.retry.map(|(due, _)| due).or(s.callback) {
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
                    input
                        .agent_outcome
                        .and_then(|o| o.disposition.as_ref())
                        .map(|d| d.code.clone()),
                ));
                Some((
                    LeadUpdate {
                        status: s.status,
                        counts_as_call: s.counts_as_call,
                        next_eligible_at: s.next_eligible_at,
                        wait_reason: s.wait_reason,
                        priority: s.priority,
                    },
                    events,
                ))
            })
            .await?;
        let Some(update) = done else {
            // While it waits for the outcome, the call frees its place on the
            // node. The room stays for the agent.
            if waits {
                self.cleanup(attempt_id, false).await?;
            }
            return Ok(());
        };
        if let Some((campaign, lead, outcome, exhausted, disposition)) = report {
            tracing::info!(
                campaign = campaign.0,
                lead = lead.0,
                attempt = attempt_id.0,
                outcome,
                disposition,
                status = update.status.as_str(),
                retry_at = update
                    .next_eligible_at
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default(),
                exhausted,
                "attempt settled"
            );
        }
        self.cleanup(attempt_id, true).await
    }

    /// Remove the assignment key of an ended attempt, and close its room.
    async fn cleanup(&self, attempt_id: AttemptId, close_room: bool) -> anyhow::Result<()> {
        let node = &self.node;
        let Some(brief) = node.attempts.briefs(&[attempt_id.0]).await?.pop() else {
            return Ok(());
        };
        if brief.live {
            return Ok(());
        }
        // The agent leaves when its room closes; a SIP leg still in it hangs up.
        if close_room {
            if let Err(e) = node.gateway.delete_room(&brief.room).await {
                tracing::warn!(attempt = attempt_id.0, error = %e, "room close failed");
            }
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
        // Keep the state of every pool current, also of a campaign that
        // stopped (the API shows it).
        let connect_within_ms = JurisdictionRule::CAMPAIGN.connect_within_ms().unwrap_or(0);
        for pool in warn_err(node.db.pacing().pools_with_samples().await, "pacing pools")
            .unwrap_or_default()
        {
            let state = node.db.pacing().ingest(&pool, connect_within_ms).await;
            if let Some(state) = warn_err(state, "pacing ingest") {
                self.trip_pool(&pool, &state, connect_within_ms).await;
            }
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
                    self.cleanup(b.attempt_id, b.settled).await?;
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

/// An answered call that its agent joined, with a session and no outcome
/// yet, waits for the outcome until `outcome_wait_s` after its end.
fn waits_for_outcome(input: &SettleInput<'_>, now: chrono::DateTime<chrono::Utc>) -> bool {
    if input.agent_outcome.is_some()
        || !input.session
        || input.attempt.agent_ready_at.is_none()
        || !matches!(input.outcome, Outcome::Completed { .. })
    {
        return false;
    }
    let wait = input
        .agent
        .as_ref()
        .and_then(|a| AgentSpec::parse(a).ok())
        .map_or(ad_agent::DEFAULT_OUTCOME_WAIT_S, |s| s.outcome_wait_s());
    input
        .attempt
        .ended_at()
        .is_some_and(|end| now < end + chrono::Duration::seconds(wait.into()))
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
                DenyReason::InvalidNumber => NotDialedReason::Excluded,
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

#[cfg(test)]
mod tests {
    use ad_attempt::{apply, Attempt, AttemptEvent as E};
    use chrono::{DateTime, Duration, TimeZone, Utc};

    use super::*;

    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + s, 0).unwrap()
    }

    /// Agent first: the agent joins, and the call is answered at 2 s and ends at 10 s.
    fn ended_with_agent() -> Attempt {
        run(
            Attempt::with_agent(),
            &[
                E::AgentStarting {
                    at: at(0),
                    detail: String::new(),
                },
                E::AgentReady {
                    at: at(1),
                    identity: "agent".into(),
                },
                E::DialStarted { at: at(1) },
                E::Answered { at: at(2) },
                E::Hangup {
                    at: at(10),
                    detail: "callee".into(),
                },
            ],
        )
    }

    /// Customer first: the callee hangs up at 3 s, before the agent joins.
    fn ended_before_the_agent() -> Attempt {
        run(
            Attempt::with_agent_on_answer(2000),
            &[
                E::DialStarted { at: at(1) },
                E::Answered { at: at(2) },
                E::AgentStarting {
                    at: at(2),
                    detail: String::new(),
                },
                E::Hangup {
                    at: at(3),
                    detail: "callee".into(),
                },
            ],
        )
    }

    fn run(first: Attempt, events: &[E]) -> Attempt {
        events.iter().fold(first, |a, e| {
            apply(&a, e).expect("the statechart takes the event")
        })
    }

    fn input<'a>(attempt: &'a Attempt, agent: Option<serde_json::Value>) -> SettleInput<'a> {
        SettleInput {
            campaign_id: CampaignId(1),
            lead_id: LeadId(1),
            outcome: attempt.outcome().expect("the attempt ended"),
            called_count: 0,
            attempt,
            agent_outcome: None,
            session: true,
            agent,
        }
    }

    #[test]
    fn the_leader_waits_for_the_outcome_of_an_answered_call_with_an_agent() {
        let a = ended_with_agent();
        assert!(matches!(a.outcome(), Some(Outcome::Completed { .. })));
        let end = at(10);
        let default = ad_agent::DEFAULT_OUTCOME_WAIT_S as i64;
        assert!(waits_for_outcome(&input(&a, None), end));
        assert!(waits_for_outcome(
            &input(&a, None),
            end + Duration::seconds(default) - Duration::milliseconds(1)
        ));
        assert!(!waits_for_outcome(
            &input(&a, None),
            end + Duration::seconds(default)
        ));

        let spec = serde_json::json!({"runtime": "pipecat", "start_url": "http://bot/start", "outcome_wait_s": 3});
        assert!(waits_for_outcome(&input(&a, Some(spec.clone())), at(12)));
        assert!(!waits_for_outcome(&input(&a, Some(spec)), at(13)));
        let none = serde_json::json!({"runtime": "pipecat", "start_url": "http://bot/start", "outcome_wait_s": 0});
        assert!(!waits_for_outcome(&input(&a, Some(none)), end));
    }

    #[test]
    fn the_leader_does_not_wait_when_no_outcome_can_come() {
        let a = ended_with_agent();
        let reported = ad_disposition::AgentOutcome {
            schema_version: 1,
            disposition: None,
            answered_by: ad_disposition::AnsweredBy::Human,
            callback: None,
            dnc: None,
            transfer_to: None,
            summary: None,
            disclosures: Vec::new(),
        };
        let with_outcome = SettleInput {
            agent_outcome: Some(&reported),
            ..input(&a, None)
        };
        assert!(!waits_for_outcome(&with_outcome, at(10)));
        let no_session = SettleInput {
            session: false,
            ..input(&a, None)
        };
        assert!(!waits_for_outcome(&no_session, at(10)));
        // The agent never joined, so it has nothing to report.
        let not_joined = ended_before_the_agent();
        assert!(matches!(
            not_joined.outcome(),
            Some(Outcome::Completed { .. })
        ));
        assert!(!waits_for_outcome(&input(&not_joined, None), at(3)));
        // The agent joined, but the callee was busy: the dialer knows the result.
        let busy = run(
            Attempt::with_agent(),
            &[
                E::AgentStarting {
                    at: at(0),
                    detail: String::new(),
                },
                E::AgentReady {
                    at: at(1),
                    identity: "agent".into(),
                },
                E::DialStarted { at: at(1) },
                E::Rejected {
                    at: at(2),
                    sip_code: Some(486),
                    detail: "busy".into(),
                },
            ],
        );
        assert!(busy.agent_ready_at.is_some());
        assert!(!waits_for_outcome(&input(&busy, None), at(2)));
    }
}
