//! Property tests for the join-paced controller.

use ad_pacing::{limit, observe, Join, Limits, Mode, Sample, Settings, State};
use chrono::{DateTime, Duration, Utc};
use proptest::prelude::*;

fn t(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::milliseconds(ms)
}

fn join() -> impl Strategy<Value = Join> {
    prop_oneof![
        (0u32..5000).prop_map(|ms| Join::Joined { ms }),
        Just(Join::Failed),
        Just(Join::Unknown),
    ]
}

fn sample() -> impl Strategy<Value = Sample> {
    (
        0i64..100_000,
        0i64..5000,
        join(),
        0u32..64,
        1u32..64,
        0u32..64,
    )
        .prop_map(|(start, len, join, window, limit, concurrent)| Sample {
            started_at: t(start),
            at: t(start + len),
            join,
            window,
            limit,
            concurrent,
        })
}

fn limits() -> impl Strategy<Value = Limits> {
    (
        1u32..900,
        0u32..1000,
        1u32..16,
        proptest::option::of(0u32..64),
    )
        .prop_map(|(target, extra, first, max)| {
            let max = max.map(|m| m.max(first));
            Settings {
                target_ms: Some(target),
                backoff_ms: Some((target + 1 + extra).min(2000)),
                first_k: Some(first),
                max_k: max,
            }
            .resolve(2000)
            .unwrap()
        })
}

fn run(l: &Limits, samples: &[Sample]) -> Vec<State> {
    let mut states = vec![State::new(l)];
    for s in samples {
        let next = observe(states.last().unwrap(), l, s);
        states.push(next);
    }
    states
}

proptest! {
    // INVARIANT: the limit is always at least 1 and never above max_k.
    #[test]
    fn k_stays_between_one_and_max_k(l in limits(), samples in prop::collection::vec(sample(), 0..200)) {
        for s in run(&l, &samples) {
            for mode in [Mode::Adaptive, Mode::Strict] {
                let k = limit(&s, &l, mode);
                prop_assert!(k >= 1);
                if let Some(max) = l.max_k {
                    prop_assert!(k <= max);
                }
            }
        }
    }

    // INVARIANT: a slow join or a failed start never raises k or the proven
    // number.
    #[test]
    fn a_slow_sample_never_raises_k(l in limits(), samples in prop::collection::vec(sample(), 0..100), extra in 1u32..3000, s in sample()) {
        let before = run(&l, &samples).pop().unwrap();
        for j in [Join::Failed, Join::Joined { ms: l.backoff_ms + extra }] {
            let after = observe(&before, &l, &Sample { join: j, ..s });
            prop_assert!(after.k <= before.k);
            prop_assert!(after.proven <= before.proven);
        }
    }

    // INVARIANT: fast joins only never lower k.
    #[test]
    fn fast_samples_never_lower_k(l in limits(), samples in prop::collection::vec(sample(), 0..100)) {
        let fast: Vec<Sample> = samples
            .into_iter()
            .map(|s| Sample { join: Join::Joined { ms: l.target_ms }, ..s })
            .collect();
        let states = run(&l, &fast);
        for w in states.windows(2) {
            prop_assert!(w[1].k >= w[0].k);
        }
    }

    // INVARIANT: strict never allows more than adaptive.
    #[test]
    fn strict_never_allows_more_than_adaptive(l in limits(), samples in prop::collection::vec(sample(), 0..100)) {
        for s in run(&l, &samples) {
            prop_assert!(limit(&s, &l, Mode::Strict) <= limit(&s, &l, Mode::Adaptive));
        }
    }

    // INVARIANT: a sample that says nothing leaves the state as it was.
    #[test]
    fn an_unknown_sample_changes_nothing(l in limits(), samples in prop::collection::vec(sample(), 0..50), s in sample()) {
        let before = run(&l, &samples).pop().unwrap();
        prop_assert_eq!(observe(&before, &l, &Sample { join: Join::Unknown, ..s }), before);
    }
}
