//! The orchestrator run-loop. Holding leadership (a Postgres advisory lock,
//! supplied by the binary), it repeatedly ticks: for each running campaign it
//! evaluates the gate, and when allowed, claims a hopper batch and dispatches
//! it. The loop here is pure control flow over a `CampaignScheduler` trait and
//! a `Leadership` trait, so it is unit-testable with fakes; the real scheduler
//! (Postgres claim + ad_gateway dispatch) is wired in the binary.

use std::future::Future;

/// Leadership check. The binary backs this with `ad_db::leader` — the loop only
/// runs its tick while this returns true.
pub trait Leadership: Send + Sync {
    fn is_leader(&self) -> bool;
}

/// Summary of one tick: how many campaigns were considered, how many dispatched
/// a batch, and how many calls were placed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickSummary {
    pub campaigns_considered: usize,
    pub campaigns_dispatched: usize,
    pub calls_placed: usize,
}

/// Does the per-tick work: look at running campaigns, gate each, claim + dispatch
/// where allowed. Implemented over Postgres + the gateway in the binary.
pub trait CampaignScheduler: Send + Sync {
    fn tick(&self) -> impl Future<Output = TickSummary> + Send;
}

/// The loop. Generic over leadership + scheduler so tests inject fakes.
pub struct OrchestratorLoop<L: Leadership, S: CampaignScheduler> {
    pub leadership: L,
    pub scheduler: S,
}

impl<L: Leadership, S: CampaignScheduler> OrchestratorLoop<L, S> {
    pub fn new(leadership: L, scheduler: S) -> Self {
        Self {
            leadership,
            scheduler,
        }
    }

    /// Run exactly one tick if we are the leader; otherwise do nothing. Returns
    /// the tick summary (None when not leader).
    pub async fn tick_if_leader(&self) -> Option<TickSummary> {
        if !self.leadership.is_leader() {
            return None;
        }
        Some(self.scheduler.tick().await)
    }

    /// Run the loop until `should_stop` returns true, sleeping `interval`
    /// between ticks. A standby (non-leader) still loops, so it can take over
    /// promptly once it wins the lock. `sleep` is injected for testability.
    pub async fn run<Stop, Sleep, SleepFut>(&self, mut should_stop: Stop, mut sleep: Sleep)
    where
        Stop: FnMut() -> bool + Send,
        Sleep: FnMut() -> SleepFut + Send,
        SleepFut: Future<Output = ()> + Send,
    {
        while !should_stop() {
            self.tick_if_leader().await;
            sleep().await;
        }
    }
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod loop_tests;
