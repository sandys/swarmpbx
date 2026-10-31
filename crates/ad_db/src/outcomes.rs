//! The agent session of an attempt and its outcome on Postgres (GAPS.md
//! AGNT-2, decision D-6), and the disposition catalog of a campaign
//! (LEAD-10).
//!
//! - The node stores the SHA-256 of a random session token when it starts
//!   the agent. The agent shows the token on the session stream.
//! - `record` stores the first checked outcome of an attempt. A DNC request
//!   takes effect in the same transaction (`dnc::add_in`). The
//!   `agent_outcome` campaign event lets the leader settle the attempt.

use ad_core::{AttemptId, CampaignId, LeadId};
use ad_disposition::{AgentOutcome, Disposition, DncScope};
use ad_events::CampaignEvent;
use sqlx::{PgPool, Row};

use crate::attempts::Owner;
use crate::dnc::{add_in, DncError, NewDnc};
use crate::events::publish_in;

#[derive(Debug, thiserror::Error)]
pub enum OutcomeError {
    #[error("storage error: {0}")]
    Storage(String),
    /// No attempt has this id and token.
    #[error("unknown session")]
    Session,
    /// The attempt has another outcome. The first outcome stays.
    #[error("the attempt has another outcome")]
    Conflict,
    #[error("the stored catalog does not read: {0}")]
    Catalog(String),
}

fn err(e: impl std::fmt::Display) -> OutcomeError {
    OutcomeError::Storage(e.to_string())
}

impl From<DncError> for OutcomeError {
    fn from(e: DncError) -> Self {
        OutcomeError::Storage(e.to_string())
    }
}

/// An attempt as its agent session sees it.
#[derive(Debug, Clone)]
pub struct Session {
    pub attempt_id: AttemptId,
    pub campaign_id: CampaignId,
    pub lead_id: LeadId,
    pub phone_number: String,
    pub lead_timezone: Option<String>,
    /// The campaign codes of the catalog (`campaigns.dispositions`).
    pub dispositions: Vec<Disposition>,
    pub live: bool,
    pub settled: bool,
}

/// What `record` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    New {
        /// The waiting leads that the DNC request stopped.
        cancelled: u64,
        /// The attempt settled before the outcome: only a DNC request still
        /// changes the leads.
        settled: bool,
    },
    /// The same outcome again.
    Duplicate,
}

#[derive(Clone)]
pub struct PgOutcomeRepo {
    pool: PgPool,
}

impl PgOutcomeRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Store the digest of the session token of a live attempt that `owner`
    /// holds. `false` when another node owns the attempt now.
    pub async fn set_session(
        &self,
        attempt_id: AttemptId,
        owner: &Owner,
        token_sha256: &[u8],
    ) -> Result<bool, OutcomeError> {
        let n = sqlx::query(
            "UPDATE call_attempts SET session_token_sha256 = $2, updated_at = now() \
             WHERE id = $1 AND node = $3 AND assign_gen = $4 AND state <> 'ended'",
        )
        .bind(attempt_id.0)
        .bind(token_sha256)
        .bind(&owner.node)
        .bind(owner.gen)
        .execute(&self.pool)
        .await
        .map_err(err)?
        .rows_affected();
        Ok(n == 1)
    }

    /// The session of an attempt, when the token matches.
    pub async fn session(
        &self,
        attempt_id: AttemptId,
        token_sha256: &[u8],
    ) -> Result<Session, OutcomeError> {
        let row = sqlx::query(
            "SELECT a.campaign_id, a.lead_id, a.phone_number, a.state, a.settled_at, \
                    l.timezone, c.dispositions \
             FROM call_attempts a JOIN leads l ON l.id = a.lead_id \
                  JOIN campaigns c ON c.id = a.campaign_id \
             WHERE a.id = $1 AND a.session_token_sha256 = $2",
        )
        .bind(attempt_id.0)
        .bind(token_sha256)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?
        .ok_or(OutcomeError::Session)?;
        let state: String = row.try_get("state").map_err(err)?;
        Ok(Session {
            attempt_id,
            campaign_id: CampaignId(row.try_get("campaign_id").map_err(err)?),
            lead_id: LeadId(row.try_get("lead_id").map_err(err)?),
            phone_number: row.try_get("phone_number").map_err(err)?,
            lead_timezone: row.try_get("timezone").map_err(err)?,
            dispositions: serde_json::from_value(row.try_get("dispositions").map_err(err)?)
                .map_err(|e| OutcomeError::Catalog(e.to_string()))?,
            live: state != "ended",
            settled: row
                .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("settled_at")
                .map_err(err)?
                .is_some(),
        })
    }

    /// Store the checked outcome of a session, once.
    pub async fn record(
        &self,
        session: &Session,
        token_sha256: &[u8],
        outcome: &AgentOutcome,
    ) -> Result<Recorded, OutcomeError> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        let row = sqlx::query(
            "SELECT agent_outcome, settled_at IS NOT NULL AS settled FROM call_attempts \
             WHERE id = $1 AND session_token_sha256 = $2 FOR UPDATE",
        )
        .bind(session.attempt_id.0)
        .bind(token_sha256)
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?
        .ok_or(OutcomeError::Session)?;
        let value = serde_json::to_value(outcome).map_err(err)?;
        if let Some(stored) = row
            .try_get::<Option<serde_json::Value>, _>("agent_outcome")
            .map_err(err)?
        {
            return if stored == value {
                Ok(Recorded::Duplicate)
            } else {
                Err(OutcomeError::Conflict)
            };
        }
        let settled: bool = row.try_get("settled").map_err(err)?;
        let code = outcome.disposition.as_ref().map(|d| d.code.clone());
        sqlx::query(
            "UPDATE call_attempts SET agent_outcome = $2, disposition = $3, outcome_at = now(), \
                 updated_at = now() WHERE id = $1",
        )
        .bind(session.attempt_id.0)
        .bind(&value)
        .bind(&code)
        .execute(&mut *tx)
        .await
        .map_err(err)?;

        let mut cancelled = 0;
        if let Some(scope) = outcome.dnc {
            let Some(number) = ad_contact::phone_key(&session.phone_number) else {
                // The lead's number does not read, so the gate denies it.
                return Err(OutcomeError::Storage(format!(
                    "the number of attempt {} does not read",
                    session.attempt_id.0
                )));
            };
            let added = add_in(
                &mut tx,
                &NewDnc {
                    phone_number: number,
                    scope,
                    campaign_id: (scope == DncScope::Campaign).then_some(session.campaign_id),
                    source: "agent",
                    attempt_id: Some(session.attempt_id.0),
                    evidence: serde_json::json!({
                        "disposition": code,
                        "summary": outcome.summary,
                    }),
                },
            )
            .await?;
            cancelled = added.cancelled;
        }
        publish_in(
            &mut tx,
            &[CampaignEvent::AgentOutcome {
                campaign_id: session.campaign_id.0,
                attempt_id: session.attempt_id.0,
                disposition: code,
                dnc: outcome.dnc.is_some(),
            }],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(Recorded::New { cancelled, settled })
    }

    /// The campaign codes of a campaign's catalog.
    pub async fn catalog(&self, campaign_id: CampaignId) -> Result<Vec<Disposition>, OutcomeError> {
        let value: serde_json::Value =
            sqlx::query_scalar("SELECT dispositions FROM campaigns WHERE id = $1")
                .bind(campaign_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(err)?
                .ok_or(OutcomeError::Session)?;
        serde_json::from_value(value).map_err(|e| OutcomeError::Catalog(e.to_string()))
    }

    /// Replace the campaign codes of a campaign's catalog. The caller checks
    /// them (`ad_disposition::Catalog::new`). `false` when the campaign does
    /// not exist.
    pub async fn put_catalog(
        &self,
        campaign_id: CampaignId,
        codes: &[Disposition],
    ) -> Result<bool, OutcomeError> {
        let n =
            sqlx::query("UPDATE campaigns SET dispositions = $2, updated_at = now() WHERE id = $1")
                .bind(campaign_id.0)
                .bind(serde_json::to_value(codes).map_err(err)?)
                .execute(&self.pool)
                .await
                .map_err(err)?
                .rows_affected();
        Ok(n == 1)
    }
}
