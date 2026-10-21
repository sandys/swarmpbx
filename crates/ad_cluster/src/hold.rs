//! Hold a key with a TTL: write it, then write it again every third of the TTL
//! so kine does not expire it. Node membership holds the node key; leader
//! election holds the leader key.
//!
//! The holder reports `true` on its status channel while it holds the key. It
//! reports `false` at once when a renewal finds the key changed or gone, and
//! when no renewal has succeeded for two thirds of the TTL. By then the key
//! may have expired, and another holder may have it.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::kine::{Kine, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Take the key even when it exists: it is ours (a node key after a restart).
    Own,
    /// Take the key only when it is free or already holds our value (election).
    Claim,
}

pub struct Hold {
    status: watch::Receiver<bool>,
    task: JoinHandle<()>,
    kine: Kine,
    key: String,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Hold {
    /// Hold `key` as its owner (for example `/agentdialer/nodes/{node}`).
    pub fn own(kine: Kine, key: String, value: Vec<u8>, ttl: Duration) -> Self {
        Self::start(kine, key, value, ttl, Mode::Own)
    }

    /// Compete for `key` (for example `/agentdialer/leader`).
    pub fn claim(kine: Kine, key: String, value: Vec<u8>, ttl: Duration) -> Self {
        Self::start(kine, key, value, ttl, Mode::Claim)
    }

    fn start(kine: Kine, key: String, value: Vec<u8>, ttl: Duration, mode: Mode) -> Self {
        let (tx, status) = watch::channel(false);
        let task = tokio::spawn(run(kine.clone(), key.clone(), value, ttl, mode, tx));
        Self {
            status,
            task,
            kine,
            key,
        }
    }

    /// `true` while this process holds the key.
    pub fn status(&self) -> watch::Receiver<bool> {
        self.status.clone()
    }

    pub fn holds(&self) -> bool {
        *self.status.borrow()
    }

    /// Stop renewing and delete the key, so others need not wait for the TTL.
    pub async fn release(self) {
        self.task.abort();
        if *self.status.borrow() {
            if let Err(e) = self.kine.delete(&self.key).await {
                tracing::warn!(key = self.key, error = %e, "release failed; the key expires by TTL");
            }
        }
    }
}

async fn run(
    kine: Kine,
    key: String,
    value: Vec<u8>,
    ttl: Duration,
    mode: Mode,
    status: watch::Sender<bool>,
) {
    let ttl_s = ttl.as_secs().max(1) as i64;
    let interval = ttl / 3;
    loop {
        let Some(mut revision) = acquire(&kine, &key, &value, ttl_s, mode).await else {
            status.send_replace(false);
            tokio::time::sleep(interval).await;
            continue;
        };
        status.send_replace(true);
        tracing::info!(key, "holding key");
        let mut last_ok = Instant::now();
        loop {
            tokio::time::sleep(interval).await;
            match kine.update(&key, &value, Some(ttl_s), revision).await {
                Ok(Write::Done { mod_revision }) => {
                    revision = mod_revision;
                    last_ok = Instant::now();
                }
                Ok(Write::Conflict { .. }) => {
                    tracing::warn!(key, "key changed or expired; no longer held");
                    break;
                }
                Err(e) if last_ok.elapsed() >= ttl * 2 / 3 => {
                    tracing::warn!(key, error = %e, "renewal failed too long; no longer held");
                    break;
                }
                Err(e) => tracing::warn!(key, error = %e, "renewal failed; retrying"),
            }
        }
        status.send_replace(false);
    }
}

/// Write the key, and return its revision when we hold it.
async fn acquire(kine: &Kine, key: &str, value: &[u8], ttl_s: i64, mode: Mode) -> Option<i64> {
    let current = match kine.create(key, value, Some(ttl_s)).await {
        Ok(Write::Done { mod_revision }) => return Some(mod_revision),
        Ok(Write::Conflict { current }) => current?,
        Err(e) => {
            tracing::warn!(key, error = %e, "kine write failed");
            return None;
        }
    };
    if mode == Mode::Claim && current.value != value {
        return None;
    }
    match kine
        .update(key, value, Some(ttl_s), current.mod_revision)
        .await
    {
        Ok(Write::Done { mod_revision }) => Some(mod_revision),
        Ok(Write::Conflict { .. }) => None,
        Err(e) => {
            tracing::warn!(key, error = %e, "kine write failed");
            None
        }
    }
}
