//! The cluster's clock: the time of Postgres, plus the monotonic time of this
//! process since the last read of it.
//!
//! The OS wall clock of a node can be wrong, or step during a call (NTP, the
//! clock sync of a VM). With this clock, every node and Postgres (`now()` in
//! SQL) agree on the time, and a wrong OS clock changes no decision: not the
//! safe hours, not a due time, not a join time. The OS clock still dates the
//! log lines and the LiveKit tokens, so a large offset is logged.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ad_core::time::Clock;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// The reads of one sync. The read with the shortest round trip is used.
const READS: usize = 3;
/// A read with a longer round trip is not used: the error of a read is up to
/// half of its round trip.
pub const MAX_RTT: Duration = Duration::from_millis(500);
/// How often the clock reads Postgres again.
pub const SYNC_EVERY: Duration = Duration::from_secs(30);
/// An OS clock further than this from Postgres is logged as a warning.
pub const WARN_OS_OFFSET: Duration = Duration::from_millis(250);

/// One read of the Postgres clock, between two monotonic times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Read {
    pub sent: Instant,
    pub received: Instant,
    pub db: DateTime<Utc>,
}

impl Read {
    pub fn rtt(&self) -> Duration {
        self.received.saturating_duration_since(self.sent)
    }

    /// The Postgres time at the middle of the round trip.
    fn anchor(&self) -> Anchor {
        Anchor {
            db: self.db,
            mono: self.sent + self.rtt() / 2,
        }
    }
}

/// A Postgres time and the monotonic time at which it was true.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    db: DateTime<Utc>,
    mono: Instant,
}

impl Anchor {
    pub fn at(&self, mono: Instant) -> DateTime<Utc> {
        let d = |d: Duration| chrono::Duration::from_std(d).unwrap_or_default();
        match mono.checked_duration_since(self.mono) {
            Some(after) => self.db + d(after),
            None => self.db - d(self.mono - mono),
        }
    }
}

/// The read to use: the shortest round trip, if it is at most `MAX_RTT`.
pub fn best(reads: &[Read]) -> Option<Read> {
    reads
        .iter()
        .copied()
        .filter(|r| r.rtt() <= MAX_RTT)
        .min_by_key(Read::rtt)
}

/// What one sync found.
#[derive(Debug, Clone, Copy)]
pub struct Synced {
    pub rtt: Duration,
    /// How far the new time moved this clock, in milliseconds.
    pub step_ms: i64,
    /// The OS clock minus the Postgres time, in milliseconds.
    pub os_offset_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ClockError {
    #[error("cannot read the Postgres clock: {0}")]
    Read(String),
    #[error("no read of the Postgres clock came back within {MAX_RTT:?}")]
    Slow,
}

pub struct PgClock {
    pool: PgPool,
    anchor: RwLock<Anchor>,
}

impl PgClock {
    /// Read the Postgres clock until a read succeeds, with backoff, then read
    /// it again every `SYNC_EVERY`. A role does no work before this returns,
    /// so it never uses an unknown time.
    pub async fn start(pool: PgPool) -> Arc<Self> {
        let mut wait = Duration::from_millis(250);
        let anchor = loop {
            match read_best(&pool).await {
                Ok(r) => break r.anchor(),
                Err(e) => tracing::warn!(error = %e, "clock: waiting for the Postgres clock"),
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(5));
        };
        let clock = Arc::new(Self {
            pool,
            anchor: RwLock::new(anchor),
        });
        let mut logged = os_offset_ms(&clock.now_utc());
        if far(logged) {
            tracing::warn!(os_offset_ms = logged, "clock: synced to Postgres; {OS_OFF}");
        } else {
            tracing::info!(os_offset_ms = logged, "clock: synced to Postgres");
        }
        let c = clock.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SYNC_EVERY).await;
                match c.sync().await {
                    Ok(s) => {
                        if s.step_ms.abs() >= 50 {
                            tracing::info!(
                                step_ms = s.step_ms,
                                rtt_ms = s.rtt.as_millis() as u64,
                                "clock: re-synced to Postgres"
                            );
                        }
                        // Log a new offset once, not at each sync.
                        if far(s.os_offset_ms - logged) {
                            logged = s.os_offset_ms;
                            if far(logged) {
                                tracing::warn!(os_offset_ms = logged, "clock: {OS_OFF}");
                            } else {
                                tracing::info!(
                                    os_offset_ms = logged,
                                    "clock: the OS clock agrees with Postgres again"
                                );
                            }
                        }
                    }
                    // The clock goes on from the last read; the monotonic
                    // clock drifts by milliseconds in an hour.
                    Err(e) => tracing::warn!(error = %e, "clock: re-sync failed"),
                }
            }
        });
        clock
    }

    /// Read the Postgres clock now, and use the new time.
    pub async fn sync(&self) -> Result<Synced, ClockError> {
        let read = read_best(&self.pool).await?;
        Ok(self.apply(read))
    }

    fn apply(&self, read: Read) -> Synced {
        let new = read.anchor();
        let mut anchor = self.anchor.write().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let step_ms = (new.at(now) - anchor.at(now)).num_milliseconds();
        *anchor = new;
        Synced {
            rtt: read.rtt(),
            step_ms,
            os_offset_ms: os_offset_ms(&new.at(now)),
        }
    }

    /// A clock at a given anchor, with no task. For tests.
    pub fn at_anchor(pool: PgPool, read: Read) -> Self {
        Self {
            pool,
            anchor: RwLock::new(read.anchor()),
        }
    }
}

impl Clock for PgClock {
    fn now_utc(&self) -> DateTime<Utc> {
        self.anchor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .at(Instant::now())
    }
}

async fn read_best(pool: &PgPool) -> Result<Read, ClockError> {
    let mut reads = Vec::with_capacity(READS);
    let mut last_err = None;
    for _ in 0..READS {
        let sent = Instant::now();
        match sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
            .fetch_one(pool)
            .await
        {
            Ok(db) => reads.push(Read {
                sent,
                received: Instant::now(),
                db,
            }),
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    match (best(&reads), last_err) {
        (Some(r), _) => Ok(r),
        (None, Some(e)) if reads.is_empty() => Err(ClockError::Read(e)),
        (None, _) => Err(ClockError::Slow),
    }
}

/// The OS clock minus `now`, in milliseconds.
fn os_offset_ms(now: &DateTime<Utc>) -> i64 {
    (Utc::now() - *now).num_milliseconds()
}

const OS_OFF: &str = "the OS clock is off (the logs and the LiveKit tokens use it)";

fn far(offset_ms: i64) -> bool {
    offset_ms.unsigned_abs() > WARN_OS_OFFSET.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(sent: Instant, rtt_ms: u64, db: DateTime<Utc>) -> Read {
        Read {
            sent,
            received: sent + Duration::from_millis(rtt_ms),
            db,
        }
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn the_time_is_the_postgres_time_plus_the_monotonic_time() {
        let base = Instant::now();
        let a = read(base, 10, t("2030-01-01T00:00:00Z")).anchor();
        // The Postgres time was true at the middle of the round trip.
        assert_eq!(
            a.at(base + Duration::from_millis(5)),
            t("2030-01-01T00:00:00Z")
        );
        assert_eq!(
            a.at(base + Duration::from_millis(1505)),
            t("2030-01-01T00:00:01.500Z")
        );
        assert_eq!(
            a.at(base),
            t("2029-12-31T23:59:59.995Z"),
            "a time before the anchor"
        );
    }

    #[test]
    fn the_shortest_round_trip_wins_and_a_slow_read_is_not_used() {
        let base = Instant::now();
        let reads = [
            read(base, 40, t("2030-01-01T00:00:00Z")),
            read(base, 4, t("2030-01-01T00:00:01Z")),
            read(base, 600, t("2030-01-01T00:00:02Z")),
        ];
        assert_eq!(best(&reads).unwrap().db, t("2030-01-01T00:00:01Z"));
        assert_eq!(best(&reads[2..]), None, "600 ms is above MAX_RTT");
        assert_eq!(best(&[]), None);
    }
}
