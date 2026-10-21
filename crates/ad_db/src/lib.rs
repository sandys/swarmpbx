//! Postgres data layer. Implements the repository traits defined by the domain
//! crates, the call attempts (`attempts`), and the orchestrator event layer
//! (`events`: an event table plus LISTEN/NOTIFY, and `breaker`: the
//! circuit-breaker window), using sqlx.
//!
//! Queries run at runtime (no compile-time macro checks), so the crate builds
//! without a live database. Integration tests exercise it against a real
//! Postgres started by `docker-compose.yml`.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

pub mod attempts;
pub mod breaker;
pub mod events;
pub mod leads;
pub use attempts::{
    Applied, AttemptBrief, AttemptError, AttemptRow, ClaimedAttempt, LeadUpdate, LiveAttempt,
    Owner, PgAttemptRepo, RoomAttempt, SettleInput,
};
pub use breaker::PgBreakerStore;
pub use events::{PgEventConsumer, PgEventLog};
pub use leads::PgLeadRepo;

use ad_campaign::repo::{CampaignRepo, NewCampaign, RepoError};
use ad_campaign::{Campaign, CampaignState, Counters, Transition, TransitionError};
use ad_core::{CampaignId, OrgId};
use ad_events::CampaignEvent;

/// Embedded migrations from `crates/ad_db/migrations`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A connection pool plus the repositories built on it.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect and build a pool. Does not run migrations.
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Connect, and wait until Postgres answers. A managed Postgres (for
    /// example RDS) may be unreachable while this process starts, or restart
    /// under it; so retry with backoff. `None` waits without limit.
    pub async fn connect_ready(
        database_url: &str,
        limit: Option<std::time::Duration>,
    ) -> anyhow::Result<Self> {
        let start = std::time::Instant::now();
        let mut backoff = std::time::Duration::from_millis(250);
        loop {
            match Self::connect(database_url).await {
                Ok(db) => return Ok(db),
                Err(e) if limit.is_some_and(|l| start.elapsed() >= l) => return Err(e),
                Err(e) => tracing::warn!(error = %e, "Postgres not reachable yet; retrying"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
        }
    }

    /// Apply all pending migrations.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    pub fn campaigns(&self) -> PgCampaignRepo {
        PgCampaignRepo {
            pool: self.pool.clone(),
        }
    }

    pub fn leads(&self) -> PgLeadRepo {
        PgLeadRepo::new(self.pool.clone())
    }

    pub fn attempts(&self) -> PgAttemptRepo {
        PgAttemptRepo::new(self.pool.clone())
    }

    pub fn events(&self) -> PgEventLog {
        PgEventLog::new(self.pool.clone())
    }

    pub fn breaker(&self) -> PgBreakerStore {
        PgBreakerStore::new(self.pool.clone())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Postgres-backed campaign repository.
#[derive(Clone)]
pub struct PgCampaignRepo {
    pool: PgPool,
}

impl PgCampaignRepo {
    /// All campaigns currently in the `running` state, across all orgs. The
    /// orchestrator iterates these each tick.
    pub async fn list_running(&self) -> Result<Vec<Campaign>, RepoError> {
        let sql =
            format!("SELECT {SELECT_COLS} FROM campaigns WHERE state = 'running' ORDER BY id ASC");
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(to_repo_err)?;
        rows.iter().map(row_to_campaign).collect()
    }

    /// Apply a lifecycle transition under the row lock, so a concurrent
    /// orchestrator write (pause, complete) is never overwritten. When the
    /// campaign enters `running`, a `campaign_running` event commits with it. A
    /// resume also resets the circuit breaker.
    pub async fn transition(
        &self,
        id: CampaignId,
        transition: Transition,
        now: DateTime<Utc>,
    ) -> Result<Result<Campaign, TransitionError>, RepoError> {
        let mut tx = self.pool.begin().await.map_err(to_repo_err)?;
        let sql = format!("SELECT {SELECT_COLS} FROM campaigns WHERE id = $1 FOR UPDATE");
        let row = sqlx::query(&sql)
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_repo_err)?
            .ok_or(RepoError::NotFound(id))?;
        let mut campaign = row_to_campaign(&row)?;
        if let Err(e) = campaign.apply(transition, now) {
            return Ok(Err(e));
        }
        let sql = format!(
            "UPDATE campaigns SET state = $2, updated_at = $3, \
             breaker_reset_at = CASE WHEN $4 THEN $3 ELSE breaker_reset_at END \
             WHERE id = $1 RETURNING {SELECT_COLS}"
        );
        let row = sqlx::query(&sql)
            .bind(id.0)
            .bind(campaign.state.as_str())
            .bind(now)
            .bind(transition == Transition::Resume)
            .fetch_one(&mut *tx)
            .await
            .map_err(to_repo_err)?;
        let saved = row_to_campaign(&row)?;
        if saved.state == CampaignState::Running {
            events::publish_in(
                &mut tx,
                &[CampaignEvent::CampaignRunning { campaign_id: id.0 }],
            )
            .await
            .map_err(to_repo_err)?;
        }
        tx.commit().await.map_err(to_repo_err)?;
        Ok(Ok(saved))
    }

    /// Complete a running campaign when no work is left: no lead waits for a
    /// dial now or later, and every attempt has ended and updated its lead
    /// (GAPS.md ORCH-1). The `campaign_completed` event commits with the
    /// state. Returns `true` when this call completed the campaign.
    pub async fn complete_if_idle(&self, id: CampaignId) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await.map_err(to_repo_err)?;
        let done = sqlx::query(
            "UPDATE campaigns SET state = 'completed', updated_at = now() \
             WHERE id = $1 AND state = 'running' \
               AND NOT EXISTS (SELECT 1 FROM leads \
                   WHERE campaign_id = $1 AND status IN ('new', 'queued', 'in_call')) \
               AND NOT EXISTS (SELECT 1 FROM call_attempts \
                   WHERE campaign_id = $1 AND (state <> 'ended' OR settled_at IS NULL))",
        )
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(to_repo_err)?
        .rows_affected()
            == 1;
        if done {
            events::publish_in(
                &mut tx,
                &[CampaignEvent::CampaignCompleted { campaign_id: id.0 }],
            )
            .await
            .map_err(to_repo_err)?;
        }
        tx.commit().await.map_err(to_repo_err)?;
        Ok(done)
    }

    /// Pause a running campaign because its breaker is open (GAPS.md DIAL-8).
    /// The `circuit_breaker_tripped` event commits with the state, so it is
    /// written once per trip. Returns `true` when this call paused it.
    pub async fn pause_for_breaker(
        &self,
        id: CampaignId,
        failure_rate: f64,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await.map_err(to_repo_err)?;
        let paused = sqlx::query(
            "UPDATE campaigns SET state = 'paused', updated_at = now() \
             WHERE id = $1 AND state = 'running'",
        )
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(to_repo_err)?
        .rows_affected()
            == 1;
        if paused {
            events::publish_in(
                &mut tx,
                &[CampaignEvent::CircuitBreakerTripped {
                    campaign_id: id.0,
                    failure_rate,
                }],
            )
            .await
            .map_err(to_repo_err)?;
        }
        tx.commit().await.map_err(to_repo_err)?;
        Ok(paused)
    }
}

fn to_repo_err(e: sqlx::Error) -> RepoError {
    RepoError::Storage(e.to_string())
}

/// Map a row to the aggregate. An unknown state string is a storage error:
/// the engine must never silently accept data it cannot interpret.
fn row_to_campaign(row: &PgRow) -> Result<Campaign, RepoError> {
    let state_str: String = row.try_get("state").map_err(to_repo_err)?;
    let state = CampaignState::from_db_str(&state_str)
        .ok_or_else(|| RepoError::Storage(format!("unknown campaign state '{state_str}'")))?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(to_repo_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(to_repo_err)?;
    Ok(Campaign {
        id: CampaignId(row.try_get::<i64, _>("id").map_err(to_repo_err)?),
        org_id: OrgId(row.try_get::<i64, _>("org_id").map_err(to_repo_err)?),
        name: row.try_get("name").map_err(to_repo_err)?,
        state,
        counters: Counters {
            total_rows: row.try_get::<i64, _>("total_rows").map_err(to_repo_err)? as u64,
            processed_rows: row
                .try_get::<i64, _>("processed_rows")
                .map_err(to_repo_err)? as u64,
            failed_rows: row.try_get::<i64, _>("failed_rows").map_err(to_repo_err)? as u64,
        },
        concurrency_cap: row
            .try_get::<i32, _>("concurrency_cap")
            .map_err(to_repo_err)? as u32,
        agent: row.try_get("agent").map_err(to_repo_err)?,
        created_at,
        updated_at,
    })
}

const SELECT_COLS: &str = "id, org_id, name, state, total_rows, processed_rows, failed_rows, \
     concurrency_cap, agent, created_at, updated_at";

impl CampaignRepo for PgCampaignRepo {
    async fn create(&self, new: NewCampaign) -> Result<Campaign, RepoError> {
        let sql = format!(
            "INSERT INTO campaigns (org_id, name, concurrency_cap, agent) VALUES ($1, $2, $3, $4) \
             RETURNING {SELECT_COLS}"
        );
        let row = sqlx::query(&sql)
            .bind(new.org_id.0)
            .bind(&new.name)
            .bind(new.concurrency_cap as i32)
            .bind(&new.agent)
            .fetch_one(&self.pool)
            .await
            .map_err(to_repo_err)?;
        row_to_campaign(&row)
    }
    async fn get(&self, id: CampaignId) -> Result<Campaign, RepoError> {
        let sql = format!("SELECT {SELECT_COLS} FROM campaigns WHERE id = $1");
        let row = sqlx::query(&sql)
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_repo_err)?;
        match row {
            Some(r) => row_to_campaign(&r),
            None => Err(RepoError::NotFound(id)),
        }
    }

    async fn list(&self, org_id: OrgId) -> Result<Vec<Campaign>, RepoError> {
        let sql = format!("SELECT {SELECT_COLS} FROM campaigns WHERE org_id = $1 ORDER BY id ASC");
        let rows = sqlx::query(&sql)
            .bind(org_id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(to_repo_err)?;
        rows.iter().map(row_to_campaign).collect()
    }

    async fn save(&self, campaign: Campaign) -> Result<Campaign, RepoError> {
        // Persist the mutable fields. id/org/created_at are immutable here.
        let sql = format!(
            "UPDATE campaigns SET name = $2, state = $3, total_rows = $4, \
             processed_rows = $5, failed_rows = $6, updated_at = $7, concurrency_cap = $8, \
             agent = $9 \
             WHERE id = $1 RETURNING {SELECT_COLS}"
        );
        let row = sqlx::query(&sql)
            .bind(campaign.id.0)
            .bind(&campaign.name)
            .bind(campaign.state.as_str())
            .bind(campaign.counters.total_rows as i64)
            .bind(campaign.counters.processed_rows as i64)
            .bind(campaign.counters.failed_rows as i64)
            .bind(campaign.updated_at)
            .bind(campaign.concurrency_cap as i32)
            .bind(&campaign.agent)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_repo_err)?;
        match row {
            Some(r) => row_to_campaign(&r),
            None => Err(RepoError::NotFound(campaign.id)),
        }
    }
}
