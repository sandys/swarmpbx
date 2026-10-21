//! A client for the etcd API that kine serves on Postgres.
//!
//! kine supports only the transaction shapes that the Kubernetes API server
//! sends. This wrapper offers only those shapes, and every compare is on the
//! key that the transaction writes:
//!   - create: `If(mod(key) == 0) Then(Put)`;
//!   - update: `If(mod(key) == N) Then(Put) Else(Get key)`;
//!   - delete: `Then(Get key, Delete key)`.
//!
//! kine has no lease KeepAlive or Revoke. A lease ID is the TTL in seconds, and
//! kine deletes a key TTL seconds after its last write. A key stays alive only
//! when its owner writes it again before that.

use std::time::Duration;

use etcd_client::{
    Client, Compare, CompareOp, ConnectOptions, GetOptions, PutOptions, Txn, TxnOp, TxnOpResponse,
    WatchOptions, WatchStream,
};

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("kine: {0}")]
    Kine(#[from] etcd_client::Error),
    #[error("kine: {0}")]
    Protocol(String),
}

/// One key with its value and the revision of its last write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Vec<u8>,
    pub mod_revision: i64,
}

impl Entry {
    fn from_kv(kv: &etcd_client::KeyValue) -> Result<Self, ClusterError> {
        Ok(Entry {
            key: kv
                .key_str()
                .map_err(|e| ClusterError::Protocol(e.to_string()))?
                .to_string(),
            value: kv.value().to_vec(),
            mod_revision: kv.mod_revision(),
        })
    }
}

/// The result of a conditional write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// The write applied. `mod_revision` is the key's new revision.
    Done { mod_revision: i64 },
    /// The condition failed. `current` is the key now, if it exists.
    Conflict { current: Option<Entry> },
}

#[derive(Clone)]
pub struct Kine {
    client: Client,
}

impl Kine {
    pub async fn connect(endpoints: &[String]) -> Result<Self, ClusterError> {
        // kine drops a client that pings too often or pings while idle.
        let options = ConnectOptions::new()
            .with_connect_timeout(Duration::from_secs(5))
            .with_timeout(Duration::from_secs(10));
        Ok(Self {
            client: Client::connect(endpoints, Some(options)).await?,
        })
    }

    /// Connect, and wait until kine answers a read. kine (and the Postgres
    /// behind it) may start after this process or restart under it, as a
    /// managed service can; so retry with backoff. `None` waits without limit.
    pub async fn connect_ready(
        endpoints: &[String],
        limit: Option<Duration>,
    ) -> Result<Self, ClusterError> {
        let start = std::time::Instant::now();
        let mut backoff = Duration::from_millis(250);
        loop {
            let attempt = async {
                let kine = Self::connect(endpoints).await?;
                kine.get(crate::keys::ROOT).await?;
                Ok::<_, ClusterError>(kine)
            };
            match attempt.await {
                Ok(kine) => return Ok(kine),
                Err(e) if limit.is_some_and(|l| start.elapsed() >= l) => return Err(e),
                Err(e) => tracing::warn!(error = %e, "kine not reachable yet; retrying"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    /// Write `key` only if it does not exist. A `ttl_s` makes the key expire
    /// that many seconds after this write.
    pub async fn create(
        &self,
        key: &str,
        value: &[u8],
        ttl_s: Option<i64>,
    ) -> Result<Write, ClusterError> {
        let txn = Txn::new()
            .when([Compare::mod_revision(key, CompareOp::Equal, 0)])
            .and_then([TxnOp::put(key, value, put_options(ttl_s))]);
        let resp = self.client.clone().txn(txn).await?;
        if resp.succeeded() {
            self.revision_after_write(key, resp.header().map(|h| h.revision()))
                .await
        } else {
            Ok(Write::Conflict {
                current: self.get(key).await?,
            })
        }
    }

    /// Write `key` only if its last write was at `mod_revision`.
    pub async fn update(
        &self,
        key: &str,
        value: &[u8],
        ttl_s: Option<i64>,
        mod_revision: i64,
    ) -> Result<Write, ClusterError> {
        let txn = Txn::new()
            .when([Compare::mod_revision(key, CompareOp::Equal, mod_revision)])
            .and_then([TxnOp::put(key, value, put_options(ttl_s))])
            .or_else([TxnOp::get(key, None)]);
        let resp = self.client.clone().txn(txn).await?;
        if resp.succeeded() {
            return self
                .revision_after_write(key, resp.header().map(|h| h.revision()))
                .await;
        }
        let mut current = None;
        for op in resp.op_responses() {
            if let TxnOpResponse::Get(get) = op {
                current = get.kvs().first().map(Entry::from_kv).transpose()?;
            }
        }
        Ok(Write::Conflict { current })
    }

    /// Delete `key`. Deleting a missing key is not an error. kine (v0.13.17)
    /// cannot tell whether the key existed: a repeated delete also succeeds,
    /// with the delete marker in `prev_kvs`. Read the key to know.
    pub async fn delete(&self, key: &str) -> Result<(), ClusterError> {
        let txn = Txn::new().and_then([TxnOp::get(key, None), TxnOp::delete(key, None)]);
        let resp = self.client.clone().txn(txn).await?;
        if resp.succeeded() {
            Ok(())
        } else {
            Err(ClusterError::Protocol(format!(
                "delete of {key} did not apply"
            )))
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Entry>, ClusterError> {
        let resp = self.client.clone().get(key, None).await?;
        resp.kvs().first().map(Entry::from_kv).transpose()
    }

    /// Every key under `prefix`, and the store revision of the read.
    pub async fn list(&self, prefix: &str) -> Result<(Vec<Entry>, i64), ClusterError> {
        let resp = self
            .client
            .clone()
            .get(prefix, Some(GetOptions::new().with_prefix()))
            .await?;
        let revision = resp.header().map(|h| h.revision()).unwrap_or(0);
        let entries = resp
            .kvs()
            .iter()
            .map(Entry::from_kv)
            .collect::<Result<_, _>>()?;
        Ok((entries, revision))
    }

    /// Watch every key under `prefix`, from `from_revision` on.
    pub async fn watch(
        &self,
        prefix: &str,
        from_revision: i64,
    ) -> Result<WatchStream, ClusterError> {
        let options = WatchOptions::new()
            .with_prefix()
            .with_start_revision(from_revision);
        Ok(self.client.clone().watch(prefix, Some(options)).await?)
    }

    /// The revision of a key we just wrote: from the response header, or from
    /// a read when the header has none.
    async fn revision_after_write(
        &self,
        key: &str,
        header_revision: Option<i64>,
    ) -> Result<Write, ClusterError> {
        if let Some(mod_revision) = header_revision.filter(|r| *r > 0) {
            return Ok(Write::Done { mod_revision });
        }
        match self.get(key).await? {
            Some(entry) => Ok(Write::Done {
                mod_revision: entry.mod_revision,
            }),
            None => Err(ClusterError::Protocol(format!(
                "{key} is missing right after a successful write"
            ))),
        }
    }
}

fn put_options(ttl_s: Option<i64>) -> Option<PutOptions> {
    ttl_s.map(|ttl| PutOptions::new().with_lease(ttl))
}
