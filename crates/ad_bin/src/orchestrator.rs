//! The event-driven orchestrator (PLAN.md §0.10.8a). It is a singleton via a
//! Postgres advisory lock (`ad_db::leader`). As leader it:
//!   - listens on the Postgres event channel (`ad_db::PgEventConsumer`: an event
//!     table plus LISTEN/NOTIFY) and reacts via the pure `ad_orchestrator::react`
//!     (schedule next batch, pause on breaker, stop on completion);
//!   - dispatches a batch by claiming the hopper (`SKIP LOCKED`), gating on the
//!     circuit breaker (`PgBreakerStore`, the `call_outcomes` window), and
//!     placing calls through `ad_gateway` with a per-call spacer;
//!   - writes each outcome back to Postgres idempotently, with the retry
//!     decision (`next_eligible_at`) in the same transaction, and publishes
//!     `call_completed` (+ `retry_scheduled`);
//!   - re-admits leads whose retry is due, on an interval;
//!   - after a listener reconnect, reconciles from Postgres so no gap stalls a
//!     campaign.
//!
//! Postgres is the only store; there is no Redis. The pure decision logic lives
//! in `ad_orchestrator`; this module is the I/O wiring.

use std::sync::Arc;
use std::time::Duration;

use ad_circuit_breaker::Config as BreakerConfig;
use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::CallWindowSet;
use ad_core::time::{Clock, SystemClock};
use ad_core::CampaignId;
use ad_db::{OutcomeRecord, PgBreakerStore, PgEventConsumer, PgEventLog, ScheduledRetry};
use ad_dispatch::CallOutcome;
use ad_events::{BreakerStore, CampaignEvent, Consumer, EventError, Publisher, RetryReason};
use ad_gateway::{Gateway, LiveKitGateway};
use ad_hopper::{select, Order, SelectionInput};
use ad_lead::repo::LeadRepo;
use ad_lead::LeadStatus;
use ad_orchestrator::{
    react, retry_decision, ReactorAction, ReactorInput, RetryDecision, RetryPolicy,
};

use crate::config::{self, OrchestratorConfig};

const ORCHESTRATOR_LEADER_KEY: i64 = 0x4144_5f4f_5243; // "AD_ORC"

/// Everything the leader needs to run the event loop.
struct Orchestrator {
    db: ad_db::Db,
    gateway: Gateway<LiveKitGateway>,
    publisher: PgEventLog,
    breaker: PgBreakerStore,
    clock: SystemClock,
    worker: String,
    batch_limit: i64,
    breaker_cfg: BreakerConfig,
    retry_policy: RetryPolicy,
    /// Minimum gap between call originations (per-call spacer).
    per_call_gap: Duration,
    windows: CallWindowSet,
}

impl Orchestrator {
    /// Classify a dispatch outcome into a retry reason, or `None` if the call
    /// connected (answered) or should not retry.
    fn retry_reason(outcome: &CallOutcome) -> Option<RetryReason> {
        match outcome {
            CallOutcome::Answered => None,
            CallOutcome::NotAnswered(_) => Some(RetryReason::NoAnswer),
            CallOutcome::Failed(_) => Some(RetryReason::Failed),
            // Compliance-denied and no-capacity are not call failures; the lead
            // stays claimable and is retried by normal selection, not a timer.
            CallOutcome::ComplianceDenied(_) | CallOutcome::NoCapacity => None,
        }
    }

    /// Claim and dispatch one batch for a campaign. Gated by the breaker.
    async fn dispatch_batch(&self, campaign_id: CampaignId) -> anyhow::Result<()> {
        let now = self.clock.now_utc();

        // Breaker gate (persistent): if open, publish the trip and pause.
        let (open, stats) = self
            .breaker
            .evaluate(campaign_id.0, now, &self.breaker_cfg)
            .await
            .map_err(|e| anyhow::anyhow!("breaker evaluate: {e}"))?;
        if open {
            warn_err(
                self.publisher
                    .publish(&CampaignEvent::CircuitBreakerTripped {
                        campaign_id: campaign_id.0,
                        failure_rate: stats.failure_rate,
                    })
                    .await,
                "publish circuit_breaker_tripped",
                campaign_id.0,
            );
            tracing::warn!(
                campaign = campaign_id.0,
                rate = stats.failure_rate,
                "breaker open; pausing"
            );
            return Ok(());
        }

        let claimed = self
            .db
            .hopper()
            .claim_batch(campaign_id, &self.worker, self.batch_limit)
            .await?;
        if claimed.is_empty() {
            // No work: the campaign is complete for now.
            warn_err(
                self.publisher
                    .publish(&CampaignEvent::CampaignCompleted {
                        campaign_id: campaign_id.0,
                    })
                    .await,
                "publish campaign_completed",
                campaign_id.0,
            );
            return Ok(());
        }

        let mut leads = Vec::with_capacity(claimed.len());
        for item in &claimed {
            match self.db.leads().get(item.lead_id).await {
                Ok(l) => leads.push(l),
                Err(e) => tracing::warn!(
                    campaign = campaign_id.0,
                    lead = item.lead_id.0,
                    error = %e,
                    "claimed lead could not be loaded; skipped"
                ),
            }
        }
        let dnc = DncSet::default();
        let selection = select(
            &self.clock,
            SelectionInput {
                leads,
                windows: &self.windows,
                dnc: &dnc,
                call_count_limit: None,
                order: Order::UpCount,
                ruleset: None,
            },
        );

        for excluded in &selection.excluded {
            tracing::info!(
                campaign = campaign_id.0,
                lead = excluded.lead.id.0,
                reason = ?excluded.reason,
                "lead excluded at dispatch"
            );
        }

        let mut processed = 0u32;
        let mut failed = 0u32;
        for lead in &selection.eligible {
            // Place the call via the gateway (the CallInitiator seam).
            let started = std::time::Instant::now();
            let outcome = {
                use ad_dispatch::CallInitiator;
                self.gateway.initiate(lead).await
            };
            let setup_ms = started.elapsed().as_millis() as u64;
            let answered = matches!(outcome, CallOutcome::Answered);
            let attempt_at = self.clock.now_utc();
            let is_failure = outcome.is_failure();

            // Decide the retry (pure) and persist it with the outcome. The
            // outcome row is also the breaker's record.
            let reason = Self::retry_reason(&outcome);
            let decision = reason
                .map(|r| retry_decision(r, lead.called_count + 1, &self.retry_policy, attempt_at));
            let retry = match (reason, decision) {
                (Some(r), Some(RetryDecision::RetryAt(due_at))) => Some(ScheduledRetry {
                    due_at,
                    reason: r.as_str(),
                }),
                _ => None,
            };
            if decision == Some(RetryDecision::Exhausted) {
                tracing::info!(
                    campaign = campaign_id.0,
                    lead = lead.id.0,
                    attempts = lead.called_count + 1,
                    "retry cap reached; no retry"
                );
            }
            let next_status = if retry.is_some() {
                LeadStatus::Queued // re-admitted when next_eligible_at is due
            } else {
                LeadStatus::Dispositioned
            };

            // Idempotent write-back. Only publish when newly recorded.
            let outcome_str = outcome_label(&outcome);
            let is_new = warn_err(
                self.db
                    .leads()
                    .record_outcome(&OutcomeRecord {
                        campaign_id,
                        lead_id: lead.id,
                        outcome: outcome_str,
                        answered,
                        attempt_at,
                        next_status,
                        retry,
                    })
                    .await,
                "record call outcome",
                campaign_id.0,
            )
            .unwrap_or(false);

            tracing::info!(
                campaign = campaign_id.0,
                lead = lead.id.0,
                outcome = outcome_str,
                detail = outcome_detail(&outcome),
                setup_ms,
                recorded = is_new,
                retry_at = retry.map(|r| r.due_at.to_rfc3339()).unwrap_or_default(),
                "call attempt"
            );

            if is_new {
                processed += 1;
                if is_failure {
                    failed += 1;
                }
                warn_err(
                    self.publisher
                        .publish(&CampaignEvent::CallCompleted {
                            campaign_id: campaign_id.0,
                            lead_id: lead.id.0,
                            answered,
                        })
                        .await,
                    "publish call_completed",
                    campaign_id.0,
                );
                if let Some(r) = retry {
                    warn_err(
                        self.publisher
                            .publish(&CampaignEvent::RetryScheduled {
                                campaign_id: campaign_id.0,
                                lead_id: lead.id.0,
                                due_epoch_ms: r.due_at.timestamp_millis(),
                            })
                            .await,
                        "publish retry_scheduled",
                        campaign_id.0,
                    );
                }
            }

            // Per-call spacer: bound the origination rate.
            tokio::time::sleep(self.per_call_gap).await;
        }

        tracing::info!(
            campaign = campaign_id.0,
            claimed = claimed.len(),
            eligible = selection.eligible.len(),
            excluded = selection.excluded.len(),
            recorded = processed,
            failed,
            "batch finished"
        );

        // Announce the batch so the reactor schedules the next one.
        warn_err(
            self.publisher
                .publish(&CampaignEvent::BatchCompleted {
                    campaign_id: campaign_id.0,
                    processed_count: processed,
                    failed_count: failed,
                    batch_size: claimed.len() as u32,
                })
                .await,
            "publish batch_completed",
            campaign_id.0,
        );

        // If this batch's failures opened the breaker, announce the trip once
        // here (not in the event handler, which would self-feed).
        if let Some((true, stats)) = warn_err(
            self.breaker
                .evaluate(campaign_id.0, self.clock.now_utc(), &self.breaker_cfg)
                .await,
            "evaluate breaker",
            campaign_id.0,
        ) {
            warn_err(
                self.publisher
                    .publish(&CampaignEvent::CircuitBreakerTripped {
                        campaign_id: campaign_id.0,
                        failure_rate: stats.failure_rate,
                    })
                    .await,
                "publish circuit_breaker_tripped",
                campaign_id.0,
            );
        }
        Ok(())
    }

    /// Handle one event via the pure reactor, then perform the I/O.
    async fn handle(&self, event: &CampaignEvent) -> anyhow::Result<()> {
        // A breaker read error counts as "closed" (GAPS.md DIAL-8); it is logged.
        let open = warn_err(
            self.breaker
                .evaluate(event.campaign_id(), self.clock.now_utc(), &self.breaker_cfg)
                .await,
            "evaluate breaker",
            event.campaign_id(),
        )
        .map(|(o, _)| o)
        .unwrap_or(false);
        let action = react(event, &ReactorInput { breaker_open: open });

        match action {
            ReactorAction::ScheduleNextBatch { campaign_id } => {
                self.dispatch_batch(CampaignId(campaign_id)).await?;
            }
            ReactorAction::PauseCampaign { campaign_id } => {
                // Do NOT publish circuit_breaker_tripped here: this arm is also
                // reached by handling a CircuitBreakerTripped event, which would
                // feed back into itself. The trip is published once by
                // dispatch_batch when the breaker first opens.
                tracing::warn!(campaign = campaign_id, "campaign paused (breaker open)");
            }
            ReactorAction::StopCampaign { campaign_id } => {
                tracing::info!(campaign = campaign_id, "campaign complete");
            }
            ReactorAction::Noop => {}
        }
        Ok(())
    }

    /// Re-admit the leads whose retry is due, for every running campaign.
    async fn readmit_due_retries(&self) -> anyhow::Result<()> {
        let now = self.clock.now_utc();
        for campaign in self.db.campaigns().list_running().await? {
            // Do not re-admit retries while the breaker is open; the campaign is
            // paused. The leads stay due and are re-admitted once it closes.
            // (This also prevents a kick/trip event storm while paused.)
            if let Ok((true, _)) = self
                .breaker
                .evaluate(campaign.id.0, now, &self.breaker_cfg)
                .await
            {
                continue;
            }
            let due = warn_err(
                self.db.hopper().readmit_due(campaign.id, now).await,
                "re-admit due retries",
                campaign.id.0,
            )
            .unwrap_or_default();
            if due.is_empty() {
                continue;
            }
            tracing::info!(
                campaign = campaign.id.0,
                leads = due.len(),
                "retries due; leads re-admitted"
            );
            // One kick per sweep (not per lead) so the next batch dials the
            // re-admitted leads.
            warn_err(
                self.publisher
                    .publish(&CampaignEvent::BatchCompleted {
                        campaign_id: campaign.id.0,
                        processed_count: 0,
                        failed_count: 0,
                        batch_size: 0,
                    })
                    .await,
                "publish batch_completed (retry kick)",
                campaign.id.0,
            );
        }
        Ok(())
    }

    /// On startup and after a listener reconnect: kick each running campaign so
    /// claimable work is dialed even without a triggering event.
    async fn reconcile(&self) -> anyhow::Result<()> {
        for campaign in self.db.campaigns().list_running().await? {
            warn_err(
                self.dispatch_batch(campaign.id).await,
                "reconcile dispatch",
                campaign.id.0,
            );
        }
        Ok(())
    }
}

/// Log a failed side effect and carry on. Postgres stays the source of truth;
/// `reconcile` and the retry sweep recover what a lost event or write leaves.
fn warn_err<T, E: std::fmt::Display>(result: Result<T, E>, what: &str, campaign: i64) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(campaign, error = %e, "{what} failed");
            None
        }
    }
}

fn outcome_detail(o: &CallOutcome) -> &str {
    match o {
        CallOutcome::NotAnswered(d) | CallOutcome::Failed(d) | CallOutcome::ComplianceDenied(d) => {
            d
        }
        CallOutcome::Answered | CallOutcome::NoCapacity => "",
    }
}

fn outcome_label(o: &CallOutcome) -> &'static str {
    match o {
        CallOutcome::Answered => "answered",
        CallOutcome::NotAnswered(_) => "no_answer",
        CallOutcome::Failed(_) => "failed",
        CallOutcome::ComplianceDenied(_) => "compliance_denied",
        CallOutcome::NoCapacity => "no_capacity",
    }
}

pub fn run() -> anyhow::Result<()> {
    let cfg = OrchestratorConfig::from_lookup(config::process_env)?;
    tracing::info!(config = ?cfg, "orchestrator: configuration loaded");
    let per_call_gap = cfg.per_call_gap();
    let OrchestratorConfig {
        database_url,
        gateway: gw_config,
        batch_limit,
        retry_sweep_ms,
        event_sweep_ms,
        worker,
        ..
    } = cfg;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let db = ad_db::Db::connect(&database_url).await?;

        tracing::info!("orchestrator: waiting to become leader");
        let _guard = loop {
            match ad_db::leader::try_become_leader(db.pool(), ORCHESTRATOR_LEADER_KEY).await? {
                Some(g) => break g,
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        tracing::info!("orchestrator: became leader, starting event loop");

        let orch = Arc::new(Orchestrator {
            publisher: db.events(),
            breaker: db.breaker(),
            db,
            gateway: Gateway::new(
                LiveKitGateway::new(&gw_config),
                gw_config.room_prefix.clone(),
            ),
            clock: SystemClock,
            worker,
            batch_limit,
            breaker_cfg: BreakerConfig::default(),
            retry_policy: RetryPolicy::default(),
            per_call_gap,
            windows: CallWindowSet::uniform(0, 2400).expect("full-day window"),
        });

        // Background: re-admit leads whose retry is due, on an interval.
        let sweep_orch = orch.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = sweep_orch.readmit_due_retries().await {
                    tracing::warn!(error = %e, "retry sweep failed");
                }
                tokio::time::sleep(Duration::from_millis(retry_sweep_ms)).await;
            }
        });

        // Main: LISTEN first, then reconcile, so the events reconcile publishes
        // are received by our own consumer. A reconnect reconciles again.
        let sweep = Duration::from_millis(event_sweep_ms);
        loop {
            match PgEventConsumer::connect(orch.db.pool(), sweep).await {
                Ok(mut consumer) => {
                    if let Err(e) = orch.reconcile().await {
                        tracing::warn!(error = %e, "reconcile failed");
                    }
                    loop {
                        match consumer.next_event().await {
                            Ok(Some(event)) => {
                                if let Err(e) = orch.handle(&event).await {
                                    tracing::error!(error = %e, "event handling failed");
                                }
                            }
                            Ok(None) => break,
                            // The consumer moved past the bad row; keep going.
                            Err(EventError::Decode(e)) => {
                                tracing::warn!(error = %e, "skipped an event that does not decode");
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "event listener error; reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "event listener connect failed; retrying");
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}
