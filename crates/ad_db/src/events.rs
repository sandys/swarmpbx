//! The orchestrator event layer on Postgres (decision D-3): an event table plus
//! LISTEN/NOTIFY. There is no Redis.
//!
//! Publish: insert the event row and call `pg_notify(channel, id)` in one
//! transaction. Postgres sends the notification only after the commit, so a
//! listener never sees an id it cannot read.
//!
//! Consume: LISTEN on the channel and read rows with an id after the last seen
//! id. The consumer reads on every notification and on a sweep timer. A lost
//! notification (a reconnect, a listener that was not connected) delays an
//! event by at most one sweep interval. It never loses it.
//!
//! Ordering: publishers take a transaction-level advisory lock, so event ids
//! commit in id order. Without it, a slow transaction could commit a lower id
//! after the consumer has read past it, and that event would be skipped.

use std::collections::VecDeque;
use std::time::Duration;

use ad_events::{CampaignEvent, Consumer, EventError, Publisher, CAMPAIGN_EVENTS_CHANNEL};
use sqlx::postgres::PgListener;
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Advisory lock key that serializes event publishers ("AD_EVT").
const PUBLISH_LOCK_KEY: i64 = 0x4144_5f45_5654;

/// Rows read per fetch.
const FETCH_LIMIT: i64 = 500;

fn tx(e: sqlx::Error) -> EventError {
    EventError::Transport(e.to_string())
}

fn decode_row(row: &sqlx::postgres::PgRow) -> Result<(i64, serde_json::Value), EventError> {
    let id: i64 = row.try_get("id").map_err(tx)?;
    let payload: serde_json::Value = row.try_get("payload").map_err(tx)?;
    Ok((id, payload))
}

fn decode_event(id: i64, payload: serde_json::Value) -> Result<CampaignEvent, EventError> {
    serde_json::from_value(payload)
        .map_err(|e| EventError::Decode(format!("campaign_events id {id}: {e}")))
}

/// Writes events to the `campaign_events` table and notifies listeners.
#[derive(Clone)]
pub struct PgEventLog {
    pool: PgPool,
}

impl PgEventLog {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The events of a campaign, oldest first, as (id, event).
    pub async fn list(
        &self,
        campaign_id: i64,
        limit: i64,
    ) -> Result<Vec<(i64, CampaignEvent)>, EventError> {
        let rows = sqlx::query(
            "SELECT id, payload FROM campaign_events WHERE campaign_id = $1 ORDER BY id LIMIT $2",
        )
        .bind(campaign_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(tx)?;
        rows.iter()
            .map(|r| {
                let (id, payload) = decode_row(r)?;
                Ok((id, decode_event(id, payload)?))
            })
            .collect()
    }
}

impl Publisher for PgEventLog {
    async fn publish(&self, event: &CampaignEvent) -> Result<(), EventError> {
        let payload = serde_json::to_value(event).map_err(|e| EventError::Decode(e.to_string()))?;
        let mut t = self.pool.begin().await.map_err(tx)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(PUBLISH_LOCK_KEY)
            .execute(&mut *t)
            .await
            .map_err(tx)?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO campaign_events (campaign_id, kind, payload) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(event.campaign_id())
        .bind(event.kind())
        .bind(payload)
        .fetch_one(&mut *t)
        .await
        .map_err(tx)?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CAMPAIGN_EVENTS_CHANNEL)
            .bind(id.to_string())
            .execute(&mut *t)
            .await
            .map_err(tx)?;
        t.commit().await.map_err(tx)?;
        Ok(())
    }
}

/// Aborts the listener task when the consumer drops.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Listens on the event channel and yields events in id order.
pub struct PgEventConsumer {
    pool: PgPool,
    cursor: i64,
    pending: VecDeque<CampaignEvent>,
    sweep: Duration,
    /// One wake-up per batch of notifications, or the listener error.
    wake: mpsc::Receiver<Result<(), String>>,
    _listener: AbortOnDrop,
}

impl PgEventConsumer {
    /// Start listening. Delivery starts after the newest event at connect time.
    /// Work that is already persisted is found by the orchestrator's reconcile
    /// pass, not by replaying old events.
    pub async fn connect(pool: &PgPool, sweep: Duration) -> Result<Self, EventError> {
        let mut listener = PgListener::connect_with(pool).await.map_err(tx)?;
        listener.listen(CAMPAIGN_EVENTS_CHANNEL).await.map_err(tx)?;
        let cursor: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM campaign_events")
            .fetch_one(pool)
            .await
            .map_err(tx)?;

        // The task owns the listener, so a sweep timeout never cancels a
        // socket read. The channel holds one wake-up: when it is full, a new
        // notification adds nothing, because one fetch reads every new row.
        let (wake_tx, wake) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            loop {
                match listener.recv().await {
                    Ok(_) => {
                        if let Err(mpsc::error::TrySendError::Closed(_)) = wake_tx.try_send(Ok(()))
                        {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = wake_tx.send(Err(e.to_string())).await;
                        return;
                    }
                }
            }
        });
        Ok(Self {
            pool: pool.clone(),
            cursor,
            pending: VecDeque::new(),
            sweep,
            wake,
            _listener: AbortOnDrop(task),
        })
    }

    /// The id of the last event read.
    pub fn cursor(&self) -> i64 {
        self.cursor
    }

    /// Read the events after the cursor into the pending queue. On a row that
    /// does not decode, the cursor moves past it and the error is returned; the
    /// next call continues after that row.
    async fn fetch_new(&mut self) -> Result<(), EventError> {
        let rows = sqlx::query(
            "SELECT id, payload FROM campaign_events WHERE id > $1 ORDER BY id LIMIT $2",
        )
        .bind(self.cursor)
        .bind(FETCH_LIMIT)
        .fetch_all(&self.pool)
        .await
        .map_err(tx)?;
        for row in &rows {
            let (id, payload) = decode_row(row)?;
            self.cursor = id;
            self.pending.push_back(decode_event(id, payload)?);
        }
        Ok(())
    }
}

impl Consumer for PgEventConsumer {
    async fn next_event(&mut self) -> Result<Option<CampaignEvent>, EventError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            self.fetch_new().await?;
            if !self.pending.is_empty() {
                continue;
            }
            // Wait for a notification, or sweep when the interval passes.
            match tokio::time::timeout(self.sweep, self.wake.recv()).await {
                Ok(Some(Ok(()))) | Err(_) => {}
                Ok(Some(Err(e))) => return Err(EventError::Transport(e)),
                Ok(None) => {
                    return Err(EventError::Transport("event listener stopped".into()));
                }
            }
        }
    }
}
