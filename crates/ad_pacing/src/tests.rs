use super::*;

fn t(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap() + chrono::Duration::milliseconds(ms)
}

fn limits() -> Limits {
    Settings::default().resolve(2000).unwrap()
}

/// A sample that started at `start_ms`, ended 100 ms later, and filled the
/// window of limit `k`.
fn sample(start_ms: i64, join: Join, k: u32) -> Sample {
    Sample {
        attempt_id: start_ms,
        started_at: t(start_ms),
        at: t(start_ms + 100),
        join,
        window: k,
        limit: k,
        concurrent: 1,
    }
}

fn fast() -> Join {
    Join::Joined { ms: 400 }
}

fn feed(state: State, samples: &[Sample]) -> State {
    samples.iter().fold(state, |s, x| observe(&s, &limits(), x))
}

#[test]
fn the_defaults_come_from_the_abandonment_limit() {
    let l = limits();
    assert_eq!(
        (l.target_ms, l.backoff_ms, l.first_k, l.max_k, l.trip_after),
        (1000, 1500, 1, None, 5)
    );
    assert_eq!(State::new(&l).k, 1);
    assert!(State::new(&l).slow_start);
}

#[test]
fn settings_that_cannot_work_are_refused() {
    let bad = [
        Settings {
            target_ms: Some(0),
            ..Settings::default()
        },
        Settings {
            target_ms: Some(1500),
            backoff_ms: Some(1500),
            ..Settings::default()
        },
        Settings {
            backoff_ms: Some(2001),
            ..Settings::default()
        },
        Settings {
            first_k: Some(0),
            ..Settings::default()
        },
        Settings {
            first_k: Some(4),
            max_k: Some(3),
            ..Settings::default()
        },
    ];
    for s in bad {
        assert!(s.resolve(2000).is_err(), "{s:?} must be refused");
    }
    let ok = Settings {
        target_ms: Some(300),
        backoff_ms: Some(600),
        first_k: Some(4),
        max_k: Some(8),
        trip_after: Some(3),
    };
    assert_eq!(State::new(&ok.resolve(2000).unwrap()).k, 4);
}

#[test]
fn unknown_settings_fields_are_refused() {
    let v = serde_json::json!({"target": 900});
    assert!(serde_json::from_value::<Settings>(v).is_err());
}

#[test]
fn slow_start_doubles_k_after_each_round_of_fast_samples() {
    let s = State::new(&limits());
    let s = feed(s, &[sample(0, fast(), 1)]);
    assert_eq!(s.k, 2);
    let s = feed(s, &[sample(10, fast(), 2)]);
    assert_eq!((s.k, s.acks), (2, 1), "half a round");
    let s = feed(s, &[sample(20, fast(), 2)]);
    assert_eq!(s.k, 4);
    assert!(s.slow_start);
}

#[test]
fn the_first_sample_above_the_target_ends_slow_start_and_keeps_k() {
    let s = feed(State::new(&limits()), &[sample(0, fast(), 1)]);
    let s = feed(s, &[sample(10, Join::Joined { ms: 1200 }, 2)]);
    assert_eq!(s.k, 2);
    assert!(!s.slow_start);
}

#[test]
fn after_slow_start_each_round_of_fast_samples_adds_one() {
    let mut s = State::new(&limits());
    s.k = 4;
    s.slow_start = false;
    let s = feed(
        s,
        &[
            sample(0, fast(), 4),
            sample(1, fast(), 4),
            sample(2, fast(), 4),
        ],
    );
    assert_eq!(s.k, 4, "three of four samples");
    let s = feed(s, &[sample(3, fast(), 4)]);
    assert_eq!(s.k, 5);
}

#[test]
fn a_slow_join_a_failed_start_and_an_abandoned_call_halve_k() {
    for join in [Join::Joined { ms: 1501 }, Join::Failed] {
        let mut s = State::new(&limits());
        s.k = 8;
        let s = feed(s, &[sample(0, join, 8)]);
        assert_eq!(s.k, 4, "{join:?}");
        assert!(!s.slow_start);
        assert_eq!(s.slow, 1);
    }
}

#[test]
fn one_round_halves_k_once() {
    let mut s = State::new(&limits());
    s.k = 8;
    // Four calls of one burst, all claimed before the first result.
    let burst: Vec<Sample> = (0..4)
        .map(|i| Sample {
            attempt_id: i,
            started_at: t(i),
            at: t(1600 + i),
            join: Join::Failed,
            window: 4,
            limit: 8,
            concurrent: 4,
        })
        .collect();
    let s = feed(s, &burst);
    assert_eq!(s.k, 4);
    // A start after the halving is a new round.
    let s = feed(s, &[sample(2000, Join::Failed, 4)]);
    assert_eq!(s.k, 2);
}

#[test]
fn k_never_goes_below_one() {
    let s = feed(State::new(&limits()), &[sample(0, Join::Failed, 1)]);
    assert_eq!(s.k, 1);
}

#[test]
fn k_does_not_grow_when_the_window_was_not_full() {
    let mut s = State::new(&limits());
    s.k = 8;
    let quiet = Sample {
        window: 2,
        ..sample(0, fast(), 8)
    };
    let s = feed(s, &vec![quiet; 20]);
    assert_eq!((s.k, s.acks), (8, 0));
}

#[test]
fn a_sample_that_says_nothing_changes_nothing() {
    let s = State::new(&limits());
    assert_eq!(feed(s.clone(), &[sample(0, Join::Unknown, 1)]), s);
}

#[test]
fn max_k_caps_k_and_the_limit() {
    let l = Settings {
        max_k: Some(3),
        ..Settings::default()
    }
    .resolve(2000)
    .unwrap();
    let mut s = State::new(&l);
    s.k = 2;
    let s = [sample(0, fast(), 2), sample(1, fast(), 2)]
        .iter()
        .fold(s, |s, x| observe(&s, &l, x));
    assert_eq!(s.k, 3);
    let mut big = s.clone();
    big.k = 50;
    assert_eq!(limit(&big, &l, Mode::Adaptive), 3);
}

#[test]
fn strict_allows_one_start_more_than_the_pool_proved() {
    let l = limits();
    let mut s = State::new(&l);
    s.k = 8;
    assert_eq!(limit(&s, &l, Mode::Strict), 1);
    assert_eq!(limit(&s, &l, Mode::Adaptive), 8);
    let s = observe(
        &s,
        &l,
        &Sample {
            concurrent: 3,
            ..sample(0, fast(), 8)
        },
    );
    assert_eq!(s.proven, 3);
    assert_eq!(limit(&s, &l, Mode::Strict), 4);
    // Three at once were slow now: the pool proved two.
    let s = observe(
        &s,
        &l,
        &Sample {
            concurrent: 3,
            ..sample(10, Join::Joined { ms: 1600 }, 8)
        },
    );
    assert_eq!(s.proven, 2);
    assert_eq!(limit(&s, &l, Mode::Strict), 3);
}

#[test]
fn a_mid_sample_neither_proves_nor_halves() {
    let mut s = State::new(&limits());
    s.k = 4;
    s.proven = 2;
    let next = feed(
        s.clone(),
        &[Sample {
            concurrent: 4,
            ..sample(0, Join::Joined { ms: 1500 }, 4)
        }],
    );
    assert_eq!((next.k, next.proven), (4, 2));
}

/// `n` failed starts, 100 ms apart, from attempt `first` on.
fn failures(first: i64, n: i64) -> Vec<Sample> {
    (first..first + n)
        .map(|i| Sample {
            attempt_id: i,
            ..sample(i * 100, Join::Failed, 1)
        })
        .collect()
}

#[test]
fn five_failed_starts_in_a_row_at_k_1_open_the_breaker() {
    let l = limits();
    let s = feed(State::new(&l), &failures(1, 4));
    assert!(!tripped(&s, &l));
    let s = feed(s, &failures(5, 1));
    assert!(tripped(&s, &l));
    assert_eq!(
        (s.failed_in_row, s.failed.clone()),
        (5, vec![1, 2, 3, 4, 5])
    );
}

#[test]
fn a_join_starts_the_count_again_and_an_unknown_end_keeps_it() {
    let l = limits();
    let s = feed(State::new(&l), &failures(1, 4));
    let joined = feed(s.clone(), &[sample(500, Join::Joined { ms: 1900 }, 1)]);
    assert_eq!((joined.failed_in_row, joined.failed.len()), (0, 0));
    let s = feed(s, &[sample(500, Join::Unknown, 1)]);
    let s = feed(s, &failures(6, 1));
    assert!(
        tripped(&s, &l),
        "the callee's hang-up does not break the run"
    );
}

#[test]
fn the_breaker_opens_only_at_k_1() {
    let l = limits();
    let mut s = State::new(&l);
    s.k = 4;
    s.failed_in_row = 7;
    assert!(!tripped(&s, &l));
    s.k = 1;
    assert!(tripped(&s, &l));
}

#[test]
fn trip_after_is_a_setting() {
    let l = Settings {
        trip_after: Some(2),
        ..Settings::default()
    }
    .resolve(2000)
    .unwrap();
    let s = (1..=2).fold(State::new(&l), |s, i| {
        observe(&s, &l, &sample(i * 100, Join::Failed, 1))
    });
    assert!(tripped(&s, &l));
    let zero = Settings {
        trip_after: Some(0),
        ..Settings::default()
    };
    assert!(zero.resolve(2000).is_err());
}

#[test]
fn the_state_keeps_the_last_failed_attempts() {
    let mut s = feed(State::new(&limits()), &failures(1, 30));
    assert_eq!(s.failed_in_row, 30);
    assert_eq!(s.failed, (11..=30).collect::<Vec<i64>>());
    s.reset_failures();
    assert_eq!((s.failed_in_row, s.failed.len()), (0, 0));
}

#[test]
fn the_state_round_trips_through_json() {
    let mut s = State::new(&limits());
    s.halved_at = Some(t(5));
    let back: State = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}
