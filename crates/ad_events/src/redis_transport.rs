//! Redis transport for `ad_events` (feature `redis`). Pub/sub on the
//! `CAMPAIGN_EVENTS` channel for signals; a per-campaign sorted set
//! (`ad:timers:{campaign_id}`, score = due-epoch-ms) for due-timers.

use futures_util::StreamExt;
use redis::AsyncCommands;

use crate::{CampaignEvent, Consumer, DueTimers, EventError, Publisher, CAMPAIGN_EVENTS_CHANNEL};

fn tx(e: redis::RedisError) -> EventError {
    EventError::Transport(e.to_string())
}

fn timers_key(campaign_id: i64) -> String {
    format!("ad:timers:{campaign_id}")
}

/// Publishes events and schedules due-timers over a multiplexed connection.
#[derive(Clone)]
pub struct RedisPublisher {
    conn: redis::aio::MultiplexedConnection,
}

impl RedisPublisher {
    pub async fn connect(url: &str) -> Result<Self, EventError> {
        let client = redis::Client::open(url).map_err(tx)?;
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(tx)?;
        Ok(Self { conn })
    }
}

impl Publisher for RedisPublisher {
    async fn publish(&self, event: &CampaignEvent) -> Result<(), EventError> {
        let mut conn = self.conn.clone();
        let _: i64 = conn
            .publish(CAMPAIGN_EVENTS_CHANNEL, event.to_json())
            .await
            .map_err(tx)?;
        Ok(())
    }
}

/// Due-timer schedule backed by a Redis sorted set per campaign.
#[derive(Clone)]
pub struct RedisTimers {
    conn: redis::aio::MultiplexedConnection,
}

impl RedisTimers {
    pub async fn connect(url: &str) -> Result<Self, EventError> {
        let client = redis::Client::open(url).map_err(tx)?;
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(tx)?;
        Ok(Self { conn })
    }
}

impl DueTimers for RedisTimers {
    async fn schedule(
        &self,
        campaign_id: i64,
        lead_id: i64,
        due_epoch_ms: i64,
    ) -> Result<(), EventError> {
        let mut conn = self.conn.clone();
        let _: i64 = conn
            .zadd(timers_key(campaign_id), lead_id, due_epoch_ms)
            .await
            .map_err(tx)?;
        Ok(())
    }

    async fn pop_due(&self, campaign_id: i64, now_epoch_ms: i64) -> Result<Vec<i64>, EventError> {
        let mut conn = self.conn.clone();
        let key = timers_key(campaign_id);
        // Members with score <= now are due. Read them, then remove exactly
        // those (ZREMRANGEBYSCORE removes the same window).
        let due: Vec<i64> = conn
            .zrangebyscore(&key, "-inf", now_epoch_ms)
            .await
            .map_err(tx)?;
        if !due.is_empty() {
            let _: i64 = conn
                .zrembyscore(&key, "-inf", now_epoch_ms)
                .await
                .map_err(tx)?;
        }
        Ok(due)
    }
}

/// Subscribes to the campaign-events channel and yields typed events.
pub struct RedisConsumer {
    rx: tokio::sync::mpsc::Receiver<String>,
    _task: tokio::task::JoinHandle<()>,
}

impl RedisConsumer {
    pub async fn connect(url: &str) -> Result<Self, EventError> {
        let client = redis::Client::open(url).map_err(tx)?;
        let mut pubsub = client.get_async_pubsub().await.map_err(tx)?;
        pubsub
            .subscribe(CAMPAIGN_EVENTS_CHANNEL)
            .await
            .map_err(tx)?;

        // A background task owns the pub/sub connection and forwards payloads
        // into a channel. This keeps messages buffered between next_event calls
        // (the earlier per-call on_message() re-creation dropped them).
        let (sender, rx) = tokio::sync::mpsc::channel::<String>(1024);
        let task = tokio::spawn(async move {
            let mut stream = pubsub.into_on_message();
            while let Some(msg) = stream.next().await {
                if let Ok(payload) = msg.get_payload::<String>() {
                    if sender.send(payload).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Self { rx, _task: task })
    }
}

impl Consumer for RedisConsumer {
    async fn next_event(&mut self) -> Result<Option<CampaignEvent>, EventError> {
        match self.rx.recv().await {
            Some(payload) => Ok(Some(CampaignEvent::from_json(&payload)?)),
            None => Ok(None),
        }
    }
}
