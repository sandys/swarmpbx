//! Postgres-backed lead repository. Implements `ad_lead::repo::LeadRepo`.

use std::collections::BTreeMap;

use ad_events::CampaignEvent;

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

    /// The earliest time after `after` at which a waiting lead of the
    /// campaign becomes due (a retry, a callback, or a hold).
    pub async fn next_due(
        &self,
        campaign_id: CampaignId,
        after: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, RepoError> {
        sqlx::query_scalar(
            "SELECT min(next_eligible_at) FROM leads \
             WHERE campaign_id = $1 AND status IN ('new', 'queued') AND next_eligible_at > $2",
        )
        .bind(campaign_id.0)
        .bind(after)
        .fetch_one(&self.pool)
        .await
        .map_err(to_repo_err)
    }

    /// `next_due` of each running campaign that has a lead due after `after`.
    pub async fn next_due_running(
        &self,
        after: DateTime<Utc>,
    ) -> Result<Vec<(CampaignId, DateTime<Utc>)>, RepoError> {
        let rows: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT l.campaign_id, min(l.next_eligible_at) FROM leads l \
             JOIN campaigns c ON c.id = l.campaign_id AND c.state = 'running' \
             WHERE l.status IN ('new', 'queued') AND l.next_eligible_at > $1 \
             GROUP BY l.campaign_id",
        )
        .bind(after)
        .fetch_all(&self.pool)
        .await
        .map_err(to_repo_err)?;
        Ok(rows
            .into_iter()
            .map(|(c, at)| (CampaignId(c), at))
            .collect())
    }
}

fn to_repo_err(e: sqlx::Error) -> RepoError {
    RepoError::Storage(e.to_string())
}

pub(crate) const COLS: &str =
    "id, campaign_id, phone_number, timezone, state, postal_code, status, \
     called_count, priority, next_eligible_at, retry_reason, last_local_call_time, \
     context, created_at, updated_at";

pub(crate) fn row_to_lead(row: &PgRow) -> Result<Lead, RepoError> {
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
        priority: row.try_get("priority").map_err(to_repo_err)?,
        next_eligible_at: row.try_get("next_eligible_at").map_err(to_repo_err)?,
        retry_reason: row.try_get("retry_reason").map_err(to_repo_err)?,
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

/// A `leads_added` event for each campaign of the new leads, so the leader
/// dials them without a wait for its sweep.
async fn added(tx: &mut sqlx::PgConnection, leads: &[Lead]) -> Result<(), RepoError> {
    let mut counts = std::collections::BTreeMap::<i64, u32>::new();
    for lead in leads {
        *counts.entry(lead.campaign_id.0).or_default() += 1;
    }
    let events: Vec<CampaignEvent> = counts
        .into_iter()
        .map(|(campaign_id, count)| CampaignEvent::LeadsAdded { campaign_id, count })
        .collect();
    crate::events::publish_in(tx, &events)
        .await
        .map_err(to_repo_err)
}

impl LeadRepo for PgLeadRepo {
    async fn create(&self, new: NewLead) -> Result<Lead, RepoError> {
        let mut tx = self.pool.begin().await.map_err(to_repo_err)?;
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
            .fetch_one(&mut *tx)
            .await
            .map_err(to_repo_err)?;
        let lead = row_to_lead(&row)?;
        added(&mut tx, std::slice::from_ref(&lead)).await?;
        tx.commit().await.map_err(to_repo_err)?;
        Ok(lead)
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
        added(&mut tx, &out).await?;
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
