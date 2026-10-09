//! Postgres data layer. Implements the repository traits defined by the domain
//! crates (for now `ad_campaign::repo::CampaignRepo`), using sqlx.
//!
//! Queries run at runtime (no compile-time macro checks), so the crate builds
//! without a live database. Integration tests exercise it against a real
//! Postgres started by `docker-compose.yml`.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

pub mod leads;
pub use leads::PgLeadRepo;

use ad_campaign::repo::{CampaignRepo, NewCampaign, RepoError};
use ad_campaign::{Campaign, CampaignState, Counters};
use ad_core::{CampaignId, OrgId};

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

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Postgres-backed campaign repository.
#[derive(Clone)]
pub struct PgCampaignRepo {
    pool: PgPool,
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
        created_at,
        updated_at,
    })
}

const SELECT_COLS: &str =
    "id, org_id, name, state, total_rows, processed_rows, failed_rows, created_at, updated_at";

impl CampaignRepo for PgCampaignRepo {
    async fn create(&self, new: NewCampaign) -> Result<Campaign, RepoError> {
        let sql =
            format!("INSERT INTO campaigns (org_id, name) VALUES ($1, $2) RETURNING {SELECT_COLS}");
        let row = sqlx::query(&sql)
            .bind(new.org_id.0)
            .bind(&new.name)
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
             processed_rows = $5, failed_rows = $6, updated_at = $7 \
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
            .fetch_optional(&self.pool)
            .await
            .map_err(to_repo_err)?;
        match row {
            Some(r) => row_to_campaign(&r),
            None => Err(RepoError::NotFound(campaign.id)),
        }
    }
}
