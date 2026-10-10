//! The event-driven orchestrator (PLAN.md §0.10.8a). It is a singleton via a
//! Postgres advisory lock (`ad_db::leader`). As leader it:
//!   - subscribes to the Redis `CAMPAIGN_EVENTS` channel and reacts via the pure
//!     `ad_orchestrator::react` (schedule next batch, schedule/drop retry, pause
//!     on breaker, stop on completion);
//!   - dispatches a batch by claiming the hopper (`SKIP LOCKED`), gating on the
//!     persistent circuit breaker (`BreakerStore`), and placing calls through
//!     `ad_gateway` with a per-call spacer (target calls/sec, VICIdial-style);
//!   - writes each outcome back to Postgres idempotently and publishes
//!     `call_completed` (+ `retry_needed` on a non-answer);
//!   - pops due retry timers and re-admits those leads;
//!   - on a Redis blip, reconnects with backoff and reconciles from Postgres so
//!     no event-gap stalls a campaign.
//!
//! This replaces the earlier fixed 1 s poll. The pure decision logic lives in
//! `ad_orchestrator`; this module is the I/O wiring.

use std::sync::Arc;
use std::time::Duration;

use ad_circuit_breaker::Config as BreakerConfig;
use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::CallWindowSet;
use ad_core::time::{Clock, SystemClock};
use ad_core::{CampaignId, LeadId};
use ad_dispatch::CallOutcome;
use ad_events::{
    BreakerStore, CampaignEvent, Consumer, DueTimers, Publisher, RedisBreakerStore, RedisConsumer,
    RedisPublisher, RedisTimers, RetryReason,
};
use ad_gateway::{Gateway, GatewayConfig, LiveKitGateway};
use ad_hopper::{select, Order, SelectionInput};
use ad_lead::repo::LeadRepo;
use ad_lead::LeadStatus;
use ad_orchestrator::{react, ReactorAction, ReactorInput, RetryPolicy};

const ORCHESTRATOR_LEADER_KEY: i64 = 0x4144_5f4f_5243; // "AD_ORC"

/// Everything the leader needs to run the event loop.
struct Orchestrator {
    db: ad_db::Db,
    gateway: Gateway<LiveKitGateway>,
    publisher: RedisPublisher,
    timers: RedisTimers,
    breaker: RedisBreakerStore,
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
    fn now_ms(&self) -> i64 {
        self.clock.now_utc().timestamp_millis()
    }

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
            self.publisher
                .publish(&CampaignEvent::CircuitBreakerTripped {
                    campaign_id: campaign_id.0,
                    failure_rate: stats.failure_rate,
                })
                .await
                .ok();
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
            self.publisher
                .publish(&CampaignEvent::CampaignCompleted {
                    campaign_id: campaign_id.0,
                })
                .await
                .ok();
            return Ok(());
        }

        let mut leads = Vec::with_capacity(claimed.len());
        for item in &claimed {
            if let Ok(l) = self.db.leads().get(item.lead_id).await {
                leads.push(l);
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

        let mut processed = 0u32;
        let mut failed = 0u32;
        for lead in &selection.eligible {
            // Place the call via the gateway (the CallInitiator seam).
            let outcome = {
                use ad_dispatch::CallInitiator;
                self.gateway.initiate(lead).await
            };
            let answered = matches!(outcome, CallOutcome::Answered);
            let attempt_at = self.clock.now_utc();

            // Record the outcome in the breaker (persistent window).
            let is_failure = outcome.is_failure();
            self.breaker
                .record(campaign_id.0, attempt_at, is_failure)
                .await
                .ok();

            // Decide the retry + next lead status.
            let reason = Self::retry_reason(&outcome);
            let next_status = if answered {
                LeadStatus::Dispositioned
            } else if reason.is_some() {
                LeadStatus::Queued // will be re-admitted by a due-timer
            } else {
                LeadStatus::Dispositioned
            };

            // Idempotent write-back. Only publish/schedule when newly recorded.
            let outcome_str = outcome_label(&outcome);
            let is_new = self
                .db
                .leads()
                .record_outcome(
                    campaign_id,
                    lead.id,
                    outcome_str,
                    answered,
                    attempt_at,
                    next_status,
                )
                .await
                .unwrap_or(false);

            if is_new {
                processed += 1;
                if is_failure {
                    failed += 1;
                }
                self.publisher
                    .publish(&CampaignEvent::CallCompleted {
                        campaign_id: campaign_id.0,
                        lead_id: lead.id.0,
                        answered,
                    })
                    .await
                    .ok();
                if let Some(r) = reason {
                    self.publisher
                        .publish(&CampaignEvent::RetryNeeded {
                            campaign_id: campaign_id.0,
                            lead_id: lead.id.0,
                            reason: r,
                        })
                        .await
                        .ok();
                }
            }

            // Per-call spacer: bound the origination rate.
            tokio::time::sleep(self.per_call_gap).await;
        }

        // Announce the batch so the reactor schedules the next one.
        self.publisher
            .publish(&CampaignEvent::BatchCompleted {
                campaign_id: campaign_id.0,
                processed_count: processed,
                failed_count: failed,
                batch_size: claimed.len() as u32,
            })
            .await
            .ok();

        // If this batch's failures opened the breaker, announce the trip once
        // here (not in the event handler, which would self-feed).
        if let Ok((true, stats)) = self
            .breaker
            .evaluate(campaign_id.0, self.clock.now_utc(), &self.breaker_cfg)
            .await
        {
            self.publisher
                .publish(&CampaignEvent::CircuitBreakerTripped {
                    campaign_id: campaign_id.0,
                    failure_rate: stats.failure_rate,
                })
                .await
                .ok();
        }
        Ok(())
    }

    /// Handle one event via the pure reactor, then perform the I/O.
    async fn handle(&self, event: &CampaignEvent) -> anyhow::Result<()> {
        let open = match self
            .breaker
            .evaluate(event.campaign_id(), self.clock.now_utc(), &self.breaker_cfg)
            .await
        {
            Ok((o, _)) => o,
            Err(_) => false,
        };
        // Attempts cap is enforced per-lead inside record_outcome write-back; we
        // pass the lead's current called_count for RetryNeeded.
        let lead_attempts = match event {
            CampaignEvent::RetryNeeded { lead_id, .. } => self
                .db
                .leads()
                .get(LeadId(*lead_id))
                .await
                .map(|l| l.called_count)
                .unwrap_or(0),
            _ => 0,
        };
        let action = react(
            event,
            &ReactorInput {
                now_epoch_ms: self.now_ms(),
                breaker_open: open,
                lead_attempts,
                policy: self.retry_policy,
            },
        );

        match action {
            ReactorAction::ScheduleNextBatch { campaign_id } => {
                self.dispatch_batch(CampaignId(campaign_id)).await?;
            }
            ReactorAction::ScheduleRetry {
                campaign_id,
                lead_id,
                due_epoch_ms,
            } => {
                self.timers
                    .schedule(campaign_id, lead_id, due_epoch_ms)
                    .await
                    .ok();
                self.publisher
                    .publish(&CampaignEvent::RetryScheduled {
                        campaign_id,
                        lead_id,
                        due_epoch_ms,
                    })
                    .await
                    .ok();
            }
            ReactorAction::DropRetry {
                campaign_id,
                lead_id,
            } => {
                tracing::info!(
                    campaign = campaign_id,
                    lead = lead_id,
                    "retry cap reached; dropping"
                );
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

    /// Pop due retry timers for all running campaigns and re-admit leads.
    async fn drain_due_timers(&self) -> anyhow::Result<()> {
        let now_ms = self.now_ms();
        let now = self.clock.now_utc();
        for campaign in self.db.campaigns().list_running().await? {
            // Do not re-admit retries while the breaker is open; the campaign is
            // paused. The timers stay scheduled and drain once it closes. (This
            // also prevents a kick/trip event storm while paused.)
            if let Ok((true, _)) = self
                .breaker
                .evaluate(campaign.id.0, now, &self.breaker_cfg)
                .await
            {
                continue;
            }
            let due = self
                .timers
                .pop_due(campaign.id.0, now_ms)
                .await
                .unwrap_or_default();
            if due.is_empty() {
                continue;
            }
            for lead_id in &due {
                // Re-admit: clear the claim on the existing hopper row so the
                // next batch can select it again. (The row was claimed, not
                // removed, when first dialed.)
                sqlx::query(
                    "UPDATE hopper SET claimed_by = NULL, claimed_at = NULL \
                     WHERE lead_id = $1 AND campaign_id = $2",
                )
                .bind(lead_id)
                .bind(campaign.id.0)
                .execute(self.db.pool())
                .await
                .ok();
            }
            // One kick per drain (not per lead) so the next batch dials the
            // re-admitted leads.
            self.publisher
                .publish(&CampaignEvent::BatchCompleted {
                    campaign_id: campaign.id.0,
                    processed_count: 0,
                    failed_count: 0,
                    batch_size: 0,
                })
                .await
                .ok();
        }
        Ok(())
    }

    /// On startup / after a Redis blip: kick each running campaign so claimable
    /// work is dialed even without a triggering event.
    async fn reconcile(&self) -> anyhow::Result<()> {
        for campaign in self.db.campaigns().list_running().await? {
            self.dispatch_batch(campaign.id).await.ok();
        }
        Ok(())
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

fn env(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("{key} must be set"))
}

pub fn run() -> anyhow::Result<()> {
    let url = env("DATABASE_URL")?;
    let redis_url = env("REDIS_URL")?;
    let gw_config = GatewayConfig {
        livekit_url: env("LIVEKIT_URL")?,
        api_key: env("LIVEKIT_API_KEY")?,
        api_secret: env("LIVEKIT_API_SECRET")?,
        sip_trunk_id: env("AD_SIP_TRUNK_ID")?,
        room_prefix: std::env::var("AD_ROOM_PREFIX").unwrap_or_else(|_| "campaign".into()),
    };
    let batch_limit: i64 = std::env::var("AD_BATCH_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let calls_per_sec: f64 = std::env::var("AD_CALLS_PER_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10.0);
    let per_call_gap = Duration::from_millis(((1000.0 / calls_per_sec).max(1.0)) as u64);
    let timer_poll_ms: u64 = std::env::var("AD_TIMER_POLL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let db = ad_db::Db::connect(&url).await?;

        tracing::info!("orchestrator: waiting to become leader");
        let _guard = loop {
            match ad_db::leader::try_become_leader(db.pool(), ORCHESTRATOR_LEADER_KEY).await? {
                Some(g) => break g,
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        tracing::info!("orchestrator: became leader, starting event loop");

        let worker = std::env::var("HOSTNAME").unwrap_or_else(|_| "orchestrator".into());
        let orch = Arc::new(Orchestrator {
            db,
            gateway: Gateway::new(
                LiveKitGateway::new(&gw_config),
                gw_config.room_prefix.clone(),
            ),
            publisher: RedisPublisher::connect(&redis_url)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            timers: RedisTimers::connect(&redis_url)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            breaker: RedisBreakerStore::connect(&redis_url)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            clock: SystemClock,
            worker,
            batch_limit,
            breaker_cfg: BreakerConfig::default(),
            retry_policy: RetryPolicy::default(),
            per_call_gap,
            windows: CallWindowSet::uniform(0, 2400).expect("full-day window"),
        });

        // Background: pop due retry timers on an interval.
        let timer_orch = orch.clone();
        tokio::spawn(async move {
            loop {
                timer_orch.drain_due_timers().await.ok();
                tokio::time::sleep(Duration::from_millis(timer_poll_ms)).await;
            }
        });

        // Main: subscribe FIRST, then reconcile, so events the reconcile
        // publishes (call_completed / retry_needed) are received by our own
        // consumer. Reconnect with backoff + reconcile closes any gap.
        loop {
            match RedisConsumer::connect(&redis_url).await {
                Ok(mut consumer) => {
                    orch.reconcile().await.ok(); // after subscribe: no lost self-events
                    loop {
                        match consumer.next_event().await {
                            Ok(Some(event)) => {
                                if let Err(e) = orch.handle(&event).await {
                                    tracing::error!(error = %e, "event handling failed");
                                }
                            }
                            Ok(None) => break, // stream ended; reconnect
                            Err(e) => {
                                tracing::error!(error = %e, "consumer error; reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "redis connect failed; retrying");
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}
