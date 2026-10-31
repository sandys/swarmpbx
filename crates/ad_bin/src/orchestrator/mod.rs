//! An agentdialer cluster node (PLAN.md §0.10.8b). Every `orchestrator`
//! replica is a node, so a Kubernetes scale-up adds nodes:
//!
//! - every node holds `/agentdialer/nodes/{node}` in kine and runs the calls
//!   that the leader assigns to it under `/agentdialer/calls/{node}/` (`calls`);
//! - one node also holds `/agentdialer/leader` and schedules (`leader`): it
//!   dispatches campaigns, assigns each new call to the node with the most
//!   free capacity, settles ended calls, and moves the calls of a node that is
//!   gone.
//!
//! Postgres is the truth for every call. A node changes an attempt only while
//! the attempt row names it with its assignment generation, so a node that
//! lost a call can no longer dial or change it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ad_agent::{Agents, LiveKitAccess};
use ad_circuit_breaker::Config as BreakerConfig;
use ad_cluster::{encode, keys, Hold, Kine, LeaderInfo, Mirror, NodeInfo};
use ad_compliance::safe_hours::CallWindowSet;
use ad_core::AttemptId;
use ad_db::{PgAttemptRepo, PgBreakerStore, PgClock};
use ad_gateway::{ClickToCall, Gateway, LiveKitGateway, SipCaller, SipTrunk, Telephony};
use ad_orchestrator::RetryPolicy;
use tokio::task::JoinHandle;

use crate::config::{self, DialProvider, OrchestratorConfig, SipSettings};

mod calls;
mod leader;

/// The settings of a node.
struct Settings {
    room_prefix: String,
    batch_limit: i64,
    per_call_gap: Duration,
    retry_sweep: Duration,
    event_sweep: Duration,
    attempt_check: Duration,
    breaker: BreakerConfig,
    retry: RetryPolicy,
    windows: CallWindowSet,
    /// The session stream that the agents reach (`AD_SESSION_URL`).
    session_url: Option<String>,
}

/// A running call: its task, and the generation of the assignment it runs.
struct Running {
    gen: i32,
    task: JoinHandle<()>,
}

struct Node {
    name: String,
    db: ad_db::Db,
    attempts: PgAttemptRepo,
    breaker: PgBreakerStore,
    gateway: Gateway<LiveKitGateway>,
    /// Places and carries the calls of this node, through its provider.
    caller: Telephony,
    agents: Agents,
    kine: Kine,
    /// The Postgres time plus the monotonic time (`ad_db::clock`), not the OS
    /// clock.
    clock: Arc<PgClock>,
    settings: Settings,
    /// The calls this node runs now.
    running: Mutex<HashMap<AttemptId, Running>>,
}

/// Aborts a task when dropped, so a task tree stops with its parent.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run `pass` every `interval` until the returned guard drops.
fn every<F, Fut>(interval: Duration, what: &'static str, pass: F) -> AbortOnDrop
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
{
    AbortOnDrop(tokio::spawn(async move {
        loop {
            if let Err(e) = pass().await {
                tracing::warn!(error = %e, "{what} failed");
            }
            tokio::time::sleep(interval).await;
        }
    }))
}

/// Log a failed side effect and carry on. Postgres stays the source of truth;
/// the sweep, the repair pass, and the reconcile recover what a lost write
/// leaves.
fn warn_err<T, E: std::fmt::Display>(result: Result<T, E>, what: &str) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "{what} failed");
            None
        }
    }
}

/// SIGTERM (a Kubernetes stop) or Ctrl-C.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler installs");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

pub fn run() -> anyhow::Result<()> {
    let cfg = OrchestratorConfig::from_lookup(config::process_env)?;
    tracing::info!(config = ?cfg, "orchestrator: configuration loaded");
    let per_call_gap = cfg.per_call_gap();
    let OrchestratorConfig {
        database_url,
        gateway: gw,
        batch_limit,
        retry_sweep_ms,
        event_sweep_ms,
        attempt_check_ms,
        etcd_endpoints,
        sip,
        provider,
        node_capacity,
        cluster_ttl_s,
        worker,
        session_url,
        ..
    } = cfg;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let db = ad_db::Db::connect_ready(&database_url, None).await?;
        let clock = PgClock::start(db.pool().clone()).await;
        let kine = Kine::connect_ready(&etcd_endpoints, None).await?;
        let sip_caller = start_sip(&sip, &worker, &gw, clock.clone()).await?;
        let caller = match provider {
            DialProvider::SipTrunk => Telephony::SipTrunk(sip_caller),
            DialProvider::TwilioClickToCall(twilio) => {
                Telephony::TwilioClickToCall(ClickToCall::new(sip_caller, twilio))
            }
        };
        tracing::info!(
            node = worker,
            provider = caller.name(),
            "node: telephony provider"
        );
        let node = Arc::new(Node {
            name: worker.clone(),
            attempts: db.attempts(),
            breaker: db.breaker(),
            db,
            gateway: Gateway::new(LiveKitGateway::new(&gw, clock.clone())),
            caller,
            agents: Agents::new(
                LiveKitAccess {
                    url: gw.livekit_url.clone(),
                    api_key: gw.api_key.clone(),
                    api_secret: gw.api_secret.clone(),
                },
                clock.clone(),
            ),
            kine: kine.clone(),
            clock,
            settings: Settings {
                room_prefix: gw.room_prefix.clone(),
                batch_limit,
                per_call_gap,
                retry_sweep: Duration::from_millis(retry_sweep_ms),
                event_sweep: Duration::from_millis(event_sweep_ms),
                attempt_check: Duration::from_millis(attempt_check_ms),
                breaker: BreakerConfig::default(),
                retry: RetryPolicy::default(),
                windows: CallWindowSet::uniform(0, 2400).expect("full-day window"),
                session_url,
            },
            running: Mutex::default(),
        });
        let ttl = Duration::from_secs(cluster_ttl_s);

        // This node: membership, and the calls assigned to it.
        let membership = Hold::own(
            kine.clone(),
            keys::node_key(&worker),
            encode(&NodeInfo {
                node: worker.clone(),
                capacity: node_capacity,
            }),
            ttl,
        );
        let mine = Mirror::start(kine.clone(), keys::node_calls_prefix(&worker));
        let _follow = AbortOnDrop(tokio::spawn(calls::follow(node.clone(), mine.subscribe())));
        let n = node.clone();
        let _reconcile = every(node.settings.attempt_check, "reconcile", move || {
            let n = n.clone();
            async move { calls::reconcile(&n).await }
        });
        tracing::info!(node = worker, capacity = node_capacity, "node: started");

        // The leader schedules for the whole cluster.
        let election = Hold::claim(
            kine.clone(),
            keys::leader_key(),
            encode(&LeaderInfo {
                node: worker.clone(),
            }),
            ttl,
        );
        let _lead = AbortOnDrop(tokio::spawn(leader::serve(node.clone(), election.status())));

        shutdown_signal().await;
        // Delete our keys now, so the next leader takes over and moves our
        // calls at once instead of after the TTL.
        tracing::info!(node = worker, "node: stopping");
        election.release().await;
        membership.release().await;
        anyhow::Ok(())
    })
}

/// Resolve `host:port`, waiting with backoff: the SIP edge and the carrier
/// are services that may start after the node.
async fn resolve_ready(host: &str) -> std::net::SocketAddr {
    let mut wait = Duration::from_millis(250);
    loop {
        match tokio::net::lookup_host(host).await.map(|mut a| a.next()) {
            Ok(Some(addr)) => return addr,
            Ok(None) => tracing::warn!(host, "no address yet"),
            Err(e) => tracing::warn!(host, error = %e, "cannot resolve yet"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(5));
    }
}

/// Start the SIP user agent of this node. The call path: through the SIP
/// edge when one is set, else straight to the trunk.
async fn start_sip(
    sip: &SipSettings,
    worker: &str,
    gw: &ad_gateway::GatewayConfig,
    clock: Arc<PgClock>,
) -> anyhow::Result<SipCaller> {
    let proxy = match &sip.proxy {
        Some(p) => Some(resolve_ready(p).await),
        None => None,
    };
    let advertise = match sip.advertise_ip {
        Some(ip) => ip,
        None => {
            // The local address of the route to the next hop.
            let next = match proxy {
                Some(p) => p,
                None => resolve_ready(&sip.trunk).await,
            };
            let probe = std::net::UdpSocket::bind(("0.0.0.0", 0))?;
            probe.connect(next)?;
            probe.local_addr()?.ip()
        }
    };
    let ua = ad_sip::SipUa::start(ad_sip::SipConfig {
        bind: sip.bind,
        advertise,
        proxy,
        user: "agentdialer".into(),
        caller_id: sip.caller_id.clone(),
    })
    .await?;
    tracing::info!(
        node = worker,
        sip = %ua.local_addr(),
        %advertise,
        proxy = ?proxy,
        trunk = sip.trunk,
        "node: SIP user agent started"
    );
    Ok(SipCaller::new(
        ua,
        advertise,
        sip.media_public_ip,
        SipTrunk {
            host: sip.trunk.clone(),
            auth: sip
                .auth
                .clone()
                .map(|(username, password)| ad_sip::TrunkAuth { username, password }),
        },
        gw.clone(),
        clock,
    ))
}
