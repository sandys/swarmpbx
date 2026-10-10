//! Postgres-backed lead repository. Implements `ad_lead::repo::LeadRepo`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use ad_core::{CampaignId, LeadId};
use ad_lead::repo::{LeadRepo, NewLead, RepoError};
use ad_lead::{Lead, LeadStatus};

#[derive(Clone)]
pub struct PgLeadRepo {
    pool: PgPool,
}

impl PgLeadRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Write back a terminal call outcome, idempotently. Inserts one
    /// `call_outcomes` row keyed by (lead_id, attempt_at); on a replayed event
    /// the insert is a no-op and the lead is NOT updated again. Returns `true`
    /// when this call recorded a new outcome (so the caller can publish /
    /// schedule retries exactly once).
    ///
    /// `next_status` is the lead status to set when the outcome is new
    /// (e.g. `dispositioned` for answered, `queued` when a retry is due,
    /// `done`/`dnc` when terminal).
    pub async fn record_outcome(
        &self,
        campaign_id: CampaignId,
        lead_id: LeadId,
        outcome: &str,
        answered: bool,
        attempt_at: DateTime<Utc>,
        next_status: LeadStatus,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await.map_err(to_repo_err)?;

        // Insert the log row; ON CONFLICT means a replay inserts nothing.
        let inserted = sqlx::query(
            "INSERT INTO call_outcomes (campaign_id, lead_id, outcome, answered, attempt_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (lead_id, attempt_at) DO NOTHING",
        )
        .bind(campaign_id.0)
        .bind(lead_id.0)
        .bind(outcome)
        .bind(answered)
        .bind(attempt_at)
        .execute(&mut *tx)
        .await
        .map_err(to_repo_err)?;

        let is_new = inserted.rows_affected() == 1;
        if is_new {
            // Advance the lead exactly once for this attempt.
            sqlx::query(
                "UPDATE leads \
                 SET called_count = called_count + 1, \
                     last_local_call_time = $2, \
                     status = $3, \
                     updated_at = now() \
                 WHERE id = $1",
            )
            .bind(lead_id.0)
            .bind(attempt_at)
            .bind(next_status.as_str())
            .execute(&mut *tx)
            .await
            .map_err(to_repo_err)?;
        }

        tx.commit().await.map_err(to_repo_err)?;
        Ok(is_new)
    }
}

fn to_repo_err(e: sqlx::Error) -> RepoError {
    RepoError::Storage(e.to_string())
}

const COLS: &str = "id, campaign_id, phone_number, timezone, state, postal_code, status, \
     called_count, last_local_call_time, context, created_at, updated_at";

fn row_to_lead(row: &PgRow) -> Result<Lead, RepoError> {
    let status_str: String = row.try_get("status").map_err(to_repo_err)?;
    let status = LeadStatus::from_db_str(&status_str)
        .ok_or_else(|| RepoError::Storage(format!("unknown lead status '{status_str}'")))?;
    let context_json: serde_json::Value = row.try_get("context").map_err(to_repo_err)?;
    let context: BTreeMap<String, String> = match context_json {
        serde_json::Value::Object(m) => m
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    v.as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| v.to_string()),
                )
            })
            .collect(),
        _ => BTreeMap::new(),
    };
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(to_repo_err)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(to_repo_err)?;
    let last_local_call_time: Option<DateTime<Utc>> =
        row.try_get("last_local_call_time").map_err(to_repo_err)?;
    Ok(Lead {
        id: LeadId(row.try_get::<i64, _>("id").map_err(to_repo_err)?),
        campaign_id: CampaignId(row.try_get::<i64, _>("campaign_id").map_err(to_repo_err)?),
        phone_number: row.try_get("phone_number").map_err(to_repo_err)?,
        timezone: row.try_get("timezone").map_err(to_repo_err)?,
        state: row.try_get("state").map_err(to_repo_err)?,
        postal_code: row.try_get("postal_code").map_err(to_repo_err)?,
        status,
        called_count: row.try_get::<i32, _>("called_count").map_err(to_repo_err)? as u32,
        last_local_call_time,
        context,
        created_at,
        updated_at,
    })
}

fn context_to_json(context: &BTreeMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        context
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    )
}

impl LeadRepo for PgLeadRepo {
    async fn create(&self, new: NewLead) -> Result<Lead, RepoError> {
        let sql = format!(
            "INSERT INTO leads (campaign_id, phone_number, timezone, state, postal_code, context) \
             VALUES ($1,$2,$3,$4,$5,$6) RETURNING {COLS}"
        );
        let row = sqlx::query(&sql)
            .bind(new.campaign_id.0)
            .bind(&new.phone_number)
            .bind(&new.timezone)
            .bind(&new.state)
            .bind(&new.postal_code)
            .bind(context_to_json(&new.context))
            .fetch_one(&self.pool)
            .await
            .map_err(to_repo_err)?;
        row_to_lead(&row)
    }

    async fn bulk_create(&self, new: Vec<NewLead>) -> Result<Vec<Lead>, RepoError> {
        // One transaction for the whole batch: all-or-nothing load.
        let mut tx = self.pool.begin().await.map_err(to_repo_err)?;
        let sql = format!(
            "INSERT INTO leads (campaign_id, phone_number, timezone, state, postal_code, context) \
             VALUES ($1,$2,$3,$4,$5,$6) RETURNING {COLS}"
        );
        let mut out = Vec::with_capacity(new.len());
        for n in new {
            let row = sqlx::query(&sql)
                .bind(n.campaign_id.0)
                .bind(&n.phone_number)
                .bind(&n.timezone)
                .bind(&n.state)
                .bind(&n.postal_code)
                .bind(context_to_json(&n.context))
                .fetch_one(&mut *tx)
                .await
                .map_err(to_repo_err)?;
            out.push(row_to_lead(&row)?);
        }
        tx.commit().await.map_err(to_repo_err)?;
        Ok(out)
    }

    async fn get(&self, id: LeadId) -> Result<Lead, RepoError> {
        let sql = format!("SELECT {COLS} FROM leads WHERE id = $1");
        let row = sqlx::query(&sql)
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_repo_err)?;
        match row {
            Some(r) => row_to_lead(&r),
            None => Err(RepoError::NotFound(id)),
        }
    }

    async fn list_by_campaign(&self, campaign_id: CampaignId) -> Result<Vec<Lead>, RepoError> {
        let sql = format!("SELECT {COLS} FROM leads WHERE campaign_id = $1 ORDER BY id ASC");
        let rows = sqlx::query(&sql)
            .bind(campaign_id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(to_repo_err)?;
        rows.iter().map(row_to_lead).collect()
    }
}
