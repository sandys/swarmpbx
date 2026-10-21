//! A live copy of every key under a prefix, kept current by list + watch, like
//! a Kubernetes informer. Readers get whole snapshots on a `watch` channel and
//! compare them with the previous one, so a missed event cannot leave them
//! wrong: the next snapshot carries the state.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use etcd_client::EventType;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::kine::{ClusterError, Entry, Kine};

pub type Snapshot = Arc<BTreeMap<String, Entry>>;

/// A running mirror. Dropping it stops the task.
pub struct Mirror {
    listed: watch::Receiver<bool>,
    rx: watch::Receiver<Snapshot>,
    task: JoinHandle<()>,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mirror {
    /// Start mirroring `prefix`. The first snapshot is empty until the first
    /// list completes.
    pub fn start(kine: Kine, prefix: String) -> Self {
        let (tx, rx) = watch::channel(Snapshot::default());
        let (listed_tx, listed) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                if let Err(e) = follow(&kine, &prefix, &tx, &listed_tx).await {
                    tracing::warn!(prefix, error = %e, "kine watch ended; listing again");
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        Self { listed, rx, task }
    }

    /// Wait until the first list is done, so the snapshot is the real state
    /// and not the empty start.
    pub async fn listed(&self) {
        let mut listed = self.listed.clone();
        let _ = listed.wait_for(|done| *done).await;
    }

    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.rx.clone()
    }

    pub fn current(&self) -> Snapshot {
        self.rx.borrow().clone()
    }
}

/// List, then apply watch events until the stream ends or fails.
async fn follow(
    kine: &Kine,
    prefix: &str,
    tx: &watch::Sender<Snapshot>,
    listed: &watch::Sender<bool>,
) -> Result<(), ClusterError> {
    let (entries, revision) = kine.list(prefix).await?;
    let mut map: BTreeMap<String, Entry> =
        entries.into_iter().map(|e| (e.key.clone(), e)).collect();
    tx.send_replace(Arc::new(map.clone()));
    listed.send_replace(true);

    let mut stream = kine.watch(prefix, revision + 1).await?;
    while let Some(resp) = stream.message().await? {
        if resp.canceled() {
            return Err(ClusterError::Protocol(format!(
                "watch canceled (compacted at {})",
                resp.compact_revision()
            )));
        }
        let mut changed = false;
        for event in resp.events() {
            let Some(kv) = event.kv() else { continue };
            let Ok(key) = kv.key_str() else { continue };
            match event.event_type() {
                EventType::Put => {
                    let entry = Entry {
                        key: key.to_string(),
                        value: kv.value().to_vec(),
                        mod_revision: kv.mod_revision(),
                    };
                    // A rewrite that keeps a key alive changes only its revision.
                    let same_value = map.get(key).is_some_and(|e| e.value == entry.value);
                    map.insert(key.to_string(), entry);
                    changed |= !same_value;
                }
                EventType::Delete => changed |= map.remove(key).is_some(),
            }
        }
        if changed {
            tx.send_replace(Arc::new(map.clone()));
        }
        if tx.is_closed() {
            return Ok(());
        }
    }
    Err(ClusterError::Protocol("watch stream closed".into()))
}
