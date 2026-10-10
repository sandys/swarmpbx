//! Orchestrator loop tests with fake leadership + scheduler.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct FakeLeader(bool);
impl Leadership for FakeLeader {
    fn is_leader(&self) -> bool {
        self.0
    }
}

struct CountingScheduler {
    ticks: Arc<AtomicUsize>,
    per_tick: TickSummary,
}
impl CampaignScheduler for CountingScheduler {
    async fn tick(&self) -> TickSummary {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        self.per_tick
    }
}

#[tokio::test(flavor = "current_thread")]
async fn leader_ticks_and_returns_summary() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let sched = CountingScheduler {
        ticks: ticks.clone(),
        per_tick: TickSummary {
            campaigns_considered: 2,
            campaigns_dispatched: 1,
            calls_placed: 3,
        },
    };
    let orch = OrchestratorLoop::new(FakeLeader(true), sched);
    let summary = orch.tick_if_leader().await;
    assert_eq!(summary.unwrap().calls_placed, 3);
    assert_eq!(ticks.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn non_leader_does_not_tick() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let sched = CountingScheduler {
        ticks: ticks.clone(),
        per_tick: TickSummary::default(),
    };
    let orch = OrchestratorLoop::new(FakeLeader(false), sched);
    assert!(orch.tick_if_leader().await.is_none());
    assert_eq!(
        ticks.load(Ordering::SeqCst),
        0,
        "standby must not run the scheduler"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_loops_until_should_stop() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let sched = CountingScheduler {
        ticks: ticks.clone(),
        per_tick: TickSummary::default(),
    };
    let orch = OrchestratorLoop::new(FakeLeader(true), sched);

    // Stop after 3 iterations (should_stop is checked at the top of each loop).
    let iters = Arc::new(AtomicUsize::new(0));
    let iters2 = iters.clone();
    orch.run(
        move || iters2.fetch_add(1, Ordering::SeqCst) >= 3,
        || async {}, // no-op sleep
    )
    .await;

    // should_stop returns false for iters 0,1,2 then true at 3 -> 3 ticks.
    assert_eq!(ticks.load(Ordering::SeqCst), 3);
}
