//! Join-paced dialing on Postgres (GAPS.md DIAL-9, decision D-10).
//!
//! - `ingest` reads the new samples of a pool (the attempts whose agent start
//!   ended), runs them through the pure `ad_pacing::observe` under the pool
//!   row lock, and saves the state. Each attempt teaches once (`paced_at`).
//! - The claim (`PgAttemptRepo::claim_paced`) takes the same lock, reads the
//!   limit, and counts the window, so two campaigns of one pool cannot both
//!   fill its last place.
//! - The API changes the settings of a pool and the mode of a campaign at run
//!   time; the next claim reads them.

use ad_attempt::{AgentLeg, Attempt, Outcome};
use ad_core::CampaignId;
use ad_pacing::{Join, Limits, Mode, Sample, Settings, SettingsError, State};
use chrono::{DateTime, Duration, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};

/// The samples read in one `ingest`.
const INGEST_LIMIT: i64 = 1000;
/// The joins that the view reports.
const RECENT: i64 = 50;

#[derive(Debug, thiserror::Error)]
pub enum PacingError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

fn err(e: impl std::fmt::Display) -> PacingError {
    PacingError::Storage(e.to_string())
}

/// The pacing of a claim: the pool of the campaign's agent and the
/// campaign's mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaceClaim {
    pub pool: String,
    pub mode: Mode,
    /// The abandonment rule's limit. The default target and back-off come
    /// from it.
    pub connect_within_ms: u32,
}

/// The recent joins of a pool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recent {
    pub joins: u32,
    pub p50_ms: Option<u32>,
    pub p95_ms: Option<u32>,
    /// Joins above the back-off.
    pub slow: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolView {
    pub pool: String,
    pub settings: Settings,
    pub limits: Limits,
    /// The saved state, or the first state when the pool has none yet.
    pub state: State,
    /// The pool's attempts whose agent is not in the room yet.
    pub window: i64,
    pub recent: Recent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Listed {
    Pool(PoolView),
    /// The saved settings do not read. Claims of this pool take a limit of 1.
    Invalid {
        pool: String,
        error: SettingsError,
    },
}

#[derive(Clone)]
pub struct PgPacingRepo {
    pool: PgPool,
}

impl PgPacingRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Learn from the new samples of a pool, and save the state.
    pub async fn ingest(&self, pool: &str, connect_within_ms: u32) -> Result<State, PacingError> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        let (settings, state) = lock(&mut tx, pool).await.map_err(err)?;
        let limits = read_settings(settings)?.resolve(connect_within_ms)?;
        let mut state = match state {
            Some(v) => serde_json::from_value(v).map_err(err)?,
            None => State::new(&limits),
        };
        let rows = sqlx::query(
            "SELECT a.id, a.snapshot, a.created_at, a.pool_window, a.pool_limit, \
                    (SELECT count(*) FROM call_attempts b \
                      WHERE b.pool = a.pool \
                        AND b.created_at >= s.t - interval '15 minutes' \
                        AND COALESCE(CASE WHEN b.snapshot ? 'agent_within_ms' THEN b.answered_at END, \
                                     b.agent_started_at) <= s.t \
                        AND COALESCE(b.agent_ready_at, b.ended_at, 'infinity'::timestamptz) > s.t \
                    ) AS concurrent \
             FROM call_attempts a, \
                  LATERAL (SELECT COALESCE(CASE WHEN a.snapshot ? 'agent_within_ms' THEN a.answered_at END, \
                                           a.agent_started_at, a.created_at) AS t) s \
             WHERE a.pool = $1 AND a.paced_at IS NULL \
               AND (a.agent_ready_at IS NOT NULL OR a.state = 'ended') \
             ORDER BY COALESCE(a.agent_ready_at, a.ended_at), a.id \
             LIMIT $2",
        )
        .bind(pool)
        .bind(INGEST_LIMIT)
        .fetch_all(&mut *tx)
        .await
        .map_err(err)?;
        if rows.is_empty() {
            return Ok(state);
        }
        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            ids.push(row.try_get::<i64, _>("id").map_err(err)?);
            state = ad_pacing::observe(&state, &limits, &sample_of(row).map_err(err)?);
        }
        sqlx::query("UPDATE agent_pools SET state = $2, updated_at = now() WHERE pool = $1")
            .bind(pool)
            .bind(serde_json::to_value(&state).map_err(err)?)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        sqlx::query("UPDATE call_attempts SET paced_at = now() WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        tracing::info!(
            pool,
            samples = ids.len(),
            k = state.k,
            slow_start = state.slow_start,
            proven = state.proven,
            slow = state.slow,
            "pacing: learned from agent joins"
        );
        Ok(state)
    }

    /// The pools that have samples to learn from.
    pub async fn pools_with_samples(&self) -> Result<Vec<String>, PacingError> {
        sqlx::query_scalar(
            "SELECT DISTINCT pool FROM call_attempts \
             WHERE pool IS NOT NULL AND paced_at IS NULL \
               AND (agent_ready_at IS NOT NULL OR state = 'ended')",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(err)
    }

    /// The mode of a campaign. A value that does not read is `strict`: the
    /// smaller window. `None` when the campaign does not exist.
    pub async fn mode(&self, campaign_id: CampaignId) -> Result<Option<Mode>, PacingError> {
        let v: Option<Option<String>> =
            sqlx::query_scalar("SELECT pacing->>'mode' FROM campaigns WHERE id = $1")
                .bind(campaign_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(err)?;
        Ok(v.map(|m| match m.as_deref() {
            None | Some("adaptive") => Mode::Adaptive,
            Some(_) => Mode::Strict,
        }))
    }

    /// Set the mode of a campaign. `false` when the campaign does not exist.
    pub async fn set_mode(&self, campaign_id: CampaignId, mode: Mode) -> Result<bool, PacingError> {
        let done = sqlx::query(
            "UPDATE campaigns SET pacing = jsonb_set(pacing, '{mode}', to_jsonb($2::text)), \
             updated_at = now() WHERE id = $1",
        )
        .bind(campaign_id.0)
        .bind(mode.name())
        .execute(&self.pool)
        .await
        .map_err(err)?;
        Ok(done.rows_affected() == 1)
    }

    /// Replace the settings of a pool. `reset` starts the pool again at its
    /// first limit, in slow start.
    pub async fn set_settings(
        &self,
        pool: &str,
        settings: &Settings,
        reset: bool,
    ) -> Result<(), PacingError> {
        sqlx::query(
            "INSERT INTO agent_pools (pool, settings) VALUES ($1, $2) \
             ON CONFLICT (pool) DO UPDATE SET settings = $2, \
                 state = CASE WHEN $3 THEN NULL ELSE agent_pools.state END, updated_at = now()",
        )
        .bind(pool)
        .bind(serde_json::to_value(settings).map_err(err)?)
        .bind(reset)
        .execute(&self.pool)
        .await
        .map_err(err)?;
        Ok(())
    }

    /// The settings, the state, the window, and the recent joins of a pool.
    pub async fn view(&self, pool: &str, connect_within_ms: u32) -> Result<PoolView, PacingError> {
        let row = sqlx::query("SELECT settings, state FROM agent_pools WHERE pool = $1")
            .bind(pool)
            .fetch_optional(&self.pool)
            .await
            .map_err(err)?;
        let (settings, state) = match row {
            Some(r) => (
                read_settings(r.try_get("settings").map_err(err)?)?,
                r.try_get::<Option<serde_json::Value>, _>("state")
                    .map_err(err)?,
            ),
            None => (Settings::default(), None),
        };
        let limits = settings.resolve(connect_within_ms)?;
        let state = match state {
            Some(v) => serde_json::from_value(v).map_err(err)?,
            None => State::new(&limits),
        };
        let window = window(&self.pool, pool).await.map_err(err)?;
        let rows = sqlx::query(
            "SELECT a.id, a.snapshot, a.created_at, a.pool_window, a.pool_limit, 1::bigint AS concurrent \
             FROM call_attempts a \
             WHERE a.pool = $1 AND a.paced_at IS NOT NULL AND a.agent_ready_at IS NOT NULL \
             ORDER BY a.paced_at DESC, a.id DESC LIMIT $2",
        )
        .bind(pool)
        .bind(RECENT)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        let mut joins = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Join::Joined { ms } = sample_of(row).map_err(err)?.join {
                joins.push(ms);
            }
        }
        Ok(PoolView {
            pool: pool.to_string(),
            recent: recent(joins, &limits),
            settings,
            limits,
            state,
            window,
        })
    }

    /// Every pool that has a row: one that a claim, a sample, or the API
    /// touched. A pool whose settings do not read comes back as its error.
    pub async fn list(&self, connect_within_ms: u32) -> Result<Vec<Listed>, PacingError> {
        let pools: Vec<String> = sqlx::query_scalar("SELECT pool FROM agent_pools ORDER BY pool")
            .fetch_all(&self.pool)
            .await
            .map_err(err)?;
        let mut views = Vec::with_capacity(pools.len());
        for p in pools {
            match self.view(&p, connect_within_ms).await {
                Ok(v) => views.push(Listed::Pool(v)),
                Err(PacingError::Settings(e)) => views.push(Listed::Invalid { pool: p, error: e }),
                Err(e) => return Err(e),
            }
        }
        Ok(views)
    }
}

fn read_settings(v: serde_json::Value) -> Result<Settings, PacingError> {
    serde_json::from_value(v).map_err(|e| PacingError::Settings(SettingsError(e.to_string())))
}

/// Lock the row of a pool, and make it first when it does not exist.
async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    pool: &str,
) -> Result<(serde_json::Value, Option<serde_json::Value>), sqlx::Error> {
    sqlx::query("INSERT INTO agent_pools (pool) VALUES ($1) ON CONFLICT (pool) DO NOTHING")
        .bind(pool)
        .execute(&mut **tx)
        .await?;
    let row = sqlx::query("SELECT settings, state FROM agent_pools WHERE pool = $1 FOR UPDATE")
        .bind(pool)
        .fetch_one(&mut **tx)
        .await?;
    Ok((row.try_get("settings")?, row.try_get("state")?))
}

/// Lock the pool, and return the window limit of the claim and the window
/// now. Fail closed: settings or a state that do not read give a limit of 1.
pub(crate) async fn lock_limit(
    tx: &mut Transaction<'_, Postgres>,
    p: &PaceClaim,
) -> Result<(u32, i64), sqlx::Error> {
    let (settings, state) = lock(tx, &p.pool).await?;
    let limit = (|| {
        let limits = serde_json::from_value::<Settings>(settings)
            .ok()?
            .resolve(p.connect_within_ms)
            .ok()?;
        let state = match state {
            Some(v) => serde_json::from_value(v).ok()?,
            None => State::new(&limits),
        };
        Some(ad_pacing::limit(&state, &limits, p.mode))
    })()
    .unwrap_or(1);
    let window = window(&mut **tx, &p.pool).await?;
    Ok((limit, window))
}

async fn window<'e>(
    e: impl sqlx::Executor<'e, Database = Postgres>,
    pool: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM call_attempts \
         WHERE pool = $1 AND state <> 'ended' AND agent_ready_at IS NULL",
    )
    .bind(pool)
    .fetch_one(e)
    .await
}

/// One sample from an attempt row (`id`, `snapshot`, `created_at`,
/// `pool_window`, `pool_limit`, `concurrent`).
fn sample_of(row: &PgRow) -> Result<Sample, sqlx::Error> {
    let attempt: Attempt = serde_json::from_value(row.try_get("snapshot")?)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let window: Option<i32> = row.try_get("pool_window")?;
    let limit: Option<i32> = row.try_get("pool_limit")?;
    let concurrent: i64 = row.try_get("concurrent")?;
    // Customer first counts from the answer, as the abandonment limit does.
    let started_at = attempt
        .agent_within_ms
        .and(attempt.answered_at)
        .or(attempt.agent_started_at)
        .unwrap_or(created_at);
    let join = match attempt.agent_ready_at {
        Some(ready) => Join::Joined {
            ms: (ready - started_at)
                .num_milliseconds()
                .clamp(0, u32::MAX.into()) as u32,
        },
        None if matches!(attempt.agent, AgentLeg::Failed { .. }) => Join::Failed,
        // No agent by the limit. A call that ended before the limit (the
        // node stopped, or the callee hung up) says nothing about the pool.
        None if matches!(attempt.outcome(), Some(Outcome::Abandoned { .. }))
            && attempt
                .agent_within_ms
                .zip(attempt.ended_at())
                .is_some_and(|(ms, end)| end - started_at >= Duration::milliseconds(ms.into())) =>
        {
            Join::Failed
        }
        None => Join::Unknown,
    };
    let to_u32 = |v: i64| u32::try_from(v.max(0)).unwrap_or(u32::MAX);
    Ok(Sample {
        started_at,
        at: attempt
            .agent_ready_at
            .or(attempt.ended_at())
            .unwrap_or(started_at),
        join,
        window: window.map_or(0, |w| to_u32(w.into())),
        // An attempt without a recorded limit never counts as a full window.
        limit: limit.map_or(u32::MAX, |l| to_u32(l.into())),
        concurrent: to_u32(concurrent).max(1),
    })
}

/// Nearest-rank percentiles of the joins, and how many were slow.
fn recent(mut joins: Vec<u32>, limits: &Limits) -> Recent {
    joins.sort_unstable();
    let rank = |p: usize| -> Option<u32> {
        let n = joins.len();
        (n > 0).then(|| joins[(n * p).div_ceil(100).clamp(1, n) - 1])
    };
    Recent {
        joins: joins.len() as u32,
        p50_ms: rank(50),
        p95_ms: rank(95),
        slow: joins.iter().filter(|&&ms| ms > limits.backoff_ms).count() as u32,
    }
}
