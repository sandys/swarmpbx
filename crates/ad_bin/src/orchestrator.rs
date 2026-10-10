//! The real orchestrator role. It is a singleton via a Postgres advisory lock
//! (`ad_db::leader`), not a Kubernetes Service: run N replicas, exactly one wins
//! the lock and runs the loop, a standby takes over when the leader's DB
//! connection drops. Each tick, the leader lists running campaigns, gates each,
//! claims a hopper batch (`SKIP LOCKED`), and dispatches it through the real
//! `ad_gateway` (LiveKit SIP origination).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ad_circuit_breaker::{CircuitBreaker, Config as BreakerConfig};
use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::CallWindowSet;
use ad_core::time::{Clock, SystemClock};
use ad_dispatch::{CallInitiator, CallOutcome, Capacity, ComplianceData, Dispatcher};
use ad_gateway::{Gateway, GatewayConfig, LiveKitGateway};
use ad_hopper::{select, Order, SelectionInput};
use ad_lead::repo::LeadRepo;
use ad_orchestrator::{
    gate, CampaignScheduler, GateDecision, OrchestratorLoop, Schedule, TickSummary,
};

// Leader key for the single orchestrator (arbitrary stable constant).
const ORCHESTRATOR_LEADER_KEY: i64 = 0x4144_5f4f_5243; // "AD_ORC"

/// Capacity backed by an atomic counter with a configurable ceiling.
struct AtomicCapacity {
    in_use: Arc<AtomicUsize>,
    max: usize,
}
struct Slot {
    in_use: Arc<AtomicUsize>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.in_use.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Capacity for AtomicCapacity {
    type Slot = Slot;
    fn try_acquire(&self) -> Option<Slot> {
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
                return Some(Slot {
                    in_use: self.in_use.clone(),
                });
            }
        }
    }
}

/// A Postgres-backed scheduler that dispatches through the real gateway.
struct PgScheduler<I: CallInitiator> {
    db: ad_db::Db,
    initiator: I,
    clock: SystemClock,
    worker: String,
    batch_limit: i64,
    max_concurrent: usize,
    // The call window used to re-gate at dispatch. The MVP uses a wide window;
    // per-campaign windows land with campaign config.
    windows: CallWindowSet,
}

impl<I: CallInitiator> CampaignScheduler for PgScheduler<I> {
    async fn tick(&self) -> TickSummary {
        let mut summary = TickSummary::default();
        let now = self.clock.now_utc();

        let running = match self.db.campaigns().list_running().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "list_running failed");
                return summary;
            }
        };

        let dnc = DncSet::default();
        let breaker = CircuitBreaker::new();

        for campaign in running {
            summary.campaigns_considered += 1;

            // Gate: the campaign is running (by query); re-check schedule +
            // breaker. Schedule fails open; compliance re-gates per lead below.
            if let GateDecision::Hold(block) = gate(
                now,
                campaign.state,
                &Schedule::always(),
                &breaker,
                &BreakerConfig::default(),
            ) {
                tracing::debug!(campaign = campaign.id.0, ?block, "campaign held");
                continue;
            }

            // Claim a batch from the hopper (SKIP LOCKED across replicas).
            let claimed = match self
                .db
                .hopper()
                .claim_batch(campaign.id, &self.worker, self.batch_limit)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(campaign = campaign.id.0, error = %e, "claim_batch failed");
                    continue;
                }
            };
            if claimed.is_empty() {
                continue;
            }

            // Load the claimed leads.
            let mut leads = Vec::with_capacity(claimed.len());
            for item in &claimed {
                match self.db.leads().get(item.lead_id).await {
                    Ok(l) => leads.push(l),
                    Err(e) => {
                        tracing::error!(lead = item.lead_id.0, error = %e, "load lead failed")
                    }
                }
            }

            // Hopper eligibility (compliance + status + ordering) before dial.
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

            // Dispatch the eligible leads through the real gateway.
            let capacity = AtomicCapacity {
                in_use: Arc::new(AtomicUsize::new(0)),
                max: self.max_concurrent,
            };
            let dispatcher = Dispatcher {
                clock: &self.clock,
                initiator: &self.initiator,
                capacity: &capacity,
                compliance: ComplianceData {
                    windows: &self.windows,
                    dnc: &dnc,
                },
            };
            let results = dispatcher.dispatch_batch(&selection.eligible).await;
            let placed = results.iter().filter(|(_, o)| o.is_attempted()).count();
            if placed > 0 {
                summary.campaigns_dispatched += 1;
                summary.calls_placed += placed;
            }
            for (lead_id, outcome) in &results {
                if let CallOutcome::Failed(e) = outcome {
                    tracing::warn!(lead = lead_id, error = %e, "dispatch failed");
                }
            }
        }

        tracing::info!(
            considered = summary.campaigns_considered,
            dispatched = summary.campaigns_dispatched,
            placed = summary.calls_placed,
            "orchestrator tick"
        );
        summary
    }
}

/// Leadership backed by a held advisory-lock guard. We only enter the loop
/// after we win the lock, so this always reports true for this process; a
/// standby process blocks in `acquire_leadership` until it wins.
struct HeldLeadership;
impl ad_orchestrator::Leadership for HeldLeadership {
    fn is_leader(&self) -> bool {
        true
    }
}

fn env(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("{key} must be set"))
}

pub fn run() -> anyhow::Result<()> {
    let url = env("DATABASE_URL")?;
    let gw_config = GatewayConfig {
        livekit_url: env("LIVEKIT_URL")?,
        api_key: env("LIVEKIT_API_KEY")?,
        api_secret: env("LIVEKIT_API_SECRET")?,
        sip_trunk_id: env("AD_SIP_TRUNK_ID")?,
        room_prefix: std::env::var("AD_ROOM_PREFIX").unwrap_or_else(|_| "campaign".into()),
    };
    let tick_ms: u64 = std::env::var("AD_TICK_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let batch_limit: i64 = std::env::var("AD_BATCH_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let max_concurrent: usize = std::env::var("AD_MAX_CONCURRENT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let db = ad_db::Db::connect(&url).await?;

        // Leader election: block until we win the advisory lock. The guard holds
        // the lock for the lifetime of the loop; if this process dies, its DB
        // connection drops and a standby wins.
        tracing::info!("orchestrator: waiting to become leader");
        let _guard = loop {
            match ad_db::leader::try_become_leader(db.pool(), ORCHESTRATOR_LEADER_KEY).await? {
                Some(g) => break g,
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        tracing::info!("orchestrator: became leader, starting loop");

        let worker = std::env::var("HOSTNAME").unwrap_or_else(|_| "orchestrator".into());
        let scheduler = PgScheduler {
            db,
            initiator: Gateway::new(
                LiveKitGateway::new(&gw_config),
                gw_config.room_prefix.clone(),
            ),
            clock: SystemClock,
            worker,
            batch_limit,
            max_concurrent,
            windows: CallWindowSet::uniform(0, 2400).expect("full-day window is valid"),
        };
        let orch = OrchestratorLoop::new(HeldLeadership, scheduler);

        orch.run(
            || false, // run until the process is signalled
            || tokio::time::sleep(Duration::from_millis(tick_ms)),
        )
        .await;
        Ok::<(), anyhow::Error>(())
    })
}
