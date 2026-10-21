//! Integration tests against a real kine (the etcd API on Postgres). Gated on
//! AD_TEST_ETCD_ENDPOINTS. Each test uses its own key prefix.

use std::time::{Duration, Instant};

use ad_cluster::{assign, keys, loads, orphans, unassign, Hold, Kine, Mirror, NodeInfo, Write};

const DEADLINE: Duration = Duration::from_secs(20);

async fn kine() -> Option<Kine> {
    let endpoints = std::env::var("AD_TEST_ETCD_ENDPOINTS").ok()?;
    let list: Vec<String> = endpoints.split(',').map(str::to_string).collect();
    Some(
        Kine::connect_ready(&list, Some(Duration::from_secs(60)))
            .await
            .expect("kine answers within 60 s"),
    )
}

fn unique(name: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("/test/{name}-{nanos}")
}

/// Poll `check` until it is true or the deadline passes.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = Instant::now() + DEADLINE;
    while !check().await {
        assert!(Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn conditional_writes_follow_the_revision() {
    let Some(k) = kine().await else {
        eprintln!("skipping: AD_TEST_ETCD_ENDPOINTS not set");
        return;
    };
    let key = unique("cas");
    let Write::Done { mod_revision: r1 } = k.create(&key, b"one", None).await.unwrap() else {
        panic!("the first create must apply");
    };
    assert_eq!(k.get(&key).await.unwrap().unwrap().mod_revision, r1);

    // A second create conflicts and shows the current value.
    match k.create(&key, b"two", None).await.unwrap() {
        Write::Conflict { current: Some(e) } => assert_eq!(e.value, b"one"),
        other => panic!("expected a conflict, got {other:?}"),
    }
    // An update at a stale revision conflicts; at the right one it applies.
    assert!(matches!(
        k.update(&key, b"two", None, r1 - 1).await.unwrap(),
        Write::Conflict { current: Some(_) }
    ));
    let Write::Done { mod_revision: r2 } = k.update(&key, b"two", None, r1).await.unwrap() else {
        panic!("the update must apply");
    };
    assert!(r2 > r1);
    let e = k.get(&key).await.unwrap().unwrap();
    assert_eq!(
        (e.value.as_slice(), e.mod_revision),
        (b"two".as_slice(), r2)
    );

    k.delete(&key).await.unwrap();
    assert_eq!(k.get(&key).await.unwrap(), None);
    // A second delete is harmless.
    k.delete(&key).await.unwrap();
    assert_eq!(k.get(&key).await.unwrap(), None);
}

#[tokio::test]
async fn a_key_with_a_ttl_expires_without_rewrites() {
    let Some(k) = kine().await else {
        return;
    };
    let key = unique("ttl");
    assert!(matches!(
        k.create(&key, b"x", Some(1)).await.unwrap(),
        Write::Done { .. }
    ));
    eventually("the key expires", || {
        let k = k.clone();
        let key = key.clone();
        async move { k.get(&key).await.unwrap().is_none() }
    })
    .await;
}

#[tokio::test]
async fn the_mirror_follows_puts_deletes_and_expiry() {
    let Some(k) = kine().await else {
        return;
    };
    let prefix = format!("{}/", unique("mirror"));
    k.create(&format!("{prefix}a"), b"1", None).await.unwrap();
    let mirror = Mirror::start(k.clone(), prefix.clone());
    mirror.listed().await;
    assert!(
        mirror.current().contains_key(&format!("{prefix}a")),
        "listed before it reports ready"
    );
    let has = |key: String, want: bool| {
        let rx = mirror.subscribe();
        async move { rx.borrow().contains_key(&key) == want }
    };
    eventually("the listed key is there", || {
        has(format!("{prefix}a"), true)
    })
    .await;

    k.create(&format!("{prefix}b"), b"2", Some(1))
        .await
        .unwrap();
    eventually("a new key appears", || has(format!("{prefix}b"), true)).await;
    eventually("an expired key goes", || has(format!("{prefix}b"), false)).await;

    k.delete(&format!("{prefix}a")).await.unwrap();
    eventually("a deleted key goes", || has(format!("{prefix}a"), false)).await;
}

#[tokio::test]
async fn one_candidate_leads_and_another_takes_over() {
    let Some(k) = kine().await else {
        return;
    };
    let key = unique("leader");
    let ttl = Duration::from_secs(2);
    let a = Hold::claim(k.clone(), key.clone(), b"node-a".to_vec(), ttl);
    let b = Hold::claim(k.clone(), key.clone(), b"node-b".to_vec(), ttl);
    eventually("one candidate leads", || {
        let (a, b) = (a.holds(), b.holds());
        async move { a ^ b }
    })
    .await;
    let (leader, other) = if a.holds() { (a, b) } else { (b, a) };
    // Both stay as they are over several renewals.
    tokio::time::sleep(ttl * 2).await;
    assert!(leader.holds() && !other.holds(), "exactly one leader");

    // The leader stops without a clean release: the key expires by TTL.
    drop(leader);
    eventually("the other candidate takes over", || {
        let holds = other.holds();
        async move { holds }
    })
    .await;
}

#[tokio::test]
async fn a_released_node_key_goes_at_once_and_a_lost_one_comes_back() {
    let Some(k) = kine().await else {
        return;
    };
    let key = unique("node");
    let hold = Hold::own(
        k.clone(),
        key.clone(),
        b"me".to_vec(),
        Duration::from_secs(3),
    );
    eventually("the node key is held", || {
        let holds = hold.holds();
        async move { holds }
    })
    .await;
    // Someone deletes the key: the holder writes it again.
    k.delete(&key).await.unwrap();
    eventually("the node key is back", || {
        let k = k.clone();
        let key = key.clone();
        async move { k.get(&key).await.unwrap().is_some() }
    })
    .await;
    hold.release().await;
    assert_eq!(k.get(&key).await.unwrap(), None);
}

#[tokio::test]
async fn assignments_show_load_and_orphans() {
    let Some(k) = kine().await else {
        return;
    };
    // Real cluster keys, with node names unique to this test.
    let live = format!("live-{}", unique("n").replace(['/', '-'], ""));
    let gone = format!("gone-{}", unique("n").replace(['/', '-'], ""));
    let node_key = keys::node_key(&live);
    let info = ad_cluster::encode(&NodeInfo {
        node: live.clone(),
        capacity: 4,
    });
    let hold = Hold::own(k.clone(), node_key, info, Duration::from_secs(5));
    assign(&k, &live, 1, 1).await.unwrap();
    assign(&k, &live, 2, 1).await.unwrap();
    assign(&k, &gone, 3, 1).await.unwrap();
    // Assigning the same call again (a new generation) overwrites it.
    assign(&k, &live, 2, 2).await.unwrap();

    let nodes = Mirror::start(k.clone(), keys::nodes_prefix());
    let calls = Mirror::start(k.clone(), keys::calls_prefix());
    eventually("the leader view sees both", || {
        let (n, c) = (nodes.current(), calls.current());
        let live = live.clone();
        async move { loads(&n, &c).get(&live).is_some_and(|l| l.assigned == 2) }
    })
    .await;
    let o = orphans(&nodes.current(), &calls.current());
    assert!(o.contains(&(gone.clone(), 3)), "{o:?}");
    assert!(!o.iter().any(|(n, _)| *n == live));

    for (node, attempt) in [(&live, 1), (&live, 2), (&gone, 3)] {
        unassign(&k, node, attempt).await.unwrap();
        assert_eq!(k.get(&keys::call_key(node, attempt)).await.unwrap(), None);
    }
    hold.release().await;
}
