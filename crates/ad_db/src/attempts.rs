//! Call attempts on Postgres (PLAN.md §0.10.7a, GAPS.md BETTER-3, BETTER-4).
//!
//! - `claim` reads the due leads straight from `leads` (the eligibility index)
//!   and creates one attempt per lead. A live attempt holds one unit of the
//!   campaign's `concurrency_cap`, so the claim takes only the free capacity.
//! - `apply` runs one event through the pure `ad_attempt::apply` under the row
//!   lock. The event row, the new snapshot, and (when the attempt ends) the
//!   `attempt_ended` campaign event commit together.
//! - `settle` updates the lead from an ended attempt once, keyed by the
//!   attempt's `settled_at`.

use ad_attempt::{Attempt, AttemptEvent, Ignored, Outcome};
use ad_core::{AttemptId, CampaignId, LeadId};
use ad_events::CampaignEvent;
use ad_lead::{Lead, LeadStatus};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::events::publish_in;
use crate::leads::{row_to_lead, COLS as LEAD_COLS};

#[derive(Debug, thiserror::Error)]
pub enum AttemptError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("attempt {0} not found")]
    NotFound(AttemptId),
}

fn err(e: impl std::fmt::Display) -> AttemptError {
    AttemptError::Storage(e.to_string())
}

/// A new attempt and its lead, ready for the dispatch gate.
#[derive(Debug, Clone)]
pub struct ClaimedAttempt {
    pub attempt_id: AttemptId,
    pub lead: Lead,
    pub room: String,
    pub participant_identity: String,
    /// The agent of the campaign (`campaigns.agent`), or `None`.
    pub agent: Option<serde_json::Value>,
}

/// A live attempt, for the reconcile.
#[derive(Debug, Clone)]
pub struct LiveAttempt {
    pub attempt_id: AttemptId,
    pub campaign_id: CampaignId,
    pub room: String,
    pub participant_identity: String,
    pub attempt: Attempt,
    pub checked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// The node that runs it, if any.
    pub node: Option<String>,
    pub assign_gen: i32,
}

/// The node that owns an attempt, and the generation of its assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub node: String,
    pub gen: i32,
}

/// A live attempt in a room, with the identities of its two parties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomAttempt {
    pub attempt_id: AttemptId,
    pub callee_identity: String,
    pub agent_identity: Option<String>,
}

/// The scheduling facts of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptBrief {
    pub attempt_id: AttemptId,
    pub campaign_id: CampaignId,
    pub node: Option<String>,
    pub assign_gen: i32,
    pub room: String,
    pub live: bool,
    pub settled: bool,
}

/// One attempt with its campaign agent and lead context.
#[derive(Debug, Clone)]
pub struct AttemptRow {
    pub attempt_id: AttemptId,
    pub campaign_id: CampaignId,
    pub lead_id: LeadId,
    pub phone_number: String,
    pub room: String,
    pub participant_identity: String,
    pub attempt: Attempt,
    pub node: Option<String>,
    pub assign_gen: i32,
    pub agent_identity: Option<String>,
    /// `campaigns.agent`.
    pub agent: Option<serde_json::Value>,
    /// `leads.context`.
    pub lead_context: serde_json::Value,
}

/// What `apply` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Changed(Attempt),
    Ignored(Ignored),
    /// Another node owns the attempt now. Nothing changed.
    Fenced,
}

/// The input of a settlement rule.
#[derive(Debug, Clone)]
pub struct SettleInput<'a> {
    pub campaign_id: CampaignId,
    pub lead_id: LeadId,
    pub outcome: &'a Outcome,
    /// `leads.called_count` before this attempt.
    pub called_count: u32,
}

/// The lead update that a settlement writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadUpdate {
    pub status: LeadStatus,
    pub counts_as_call: bool,
    pub next_eligible_at: Option<DateTime<Utc>>,
    pub wait_reason: Option<&'static str>,
}

#[derive(Clone)]
pub struct PgAttemptRepo {
    pool: PgPool,
}

impl PgAttemptRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claim up to `limit` due leads of a running campaign, within its free
    /// capacity. Each claimed lead becomes `in_call` with a new `pending`
    /// attempt. The campaign row lock makes the capacity check and the insert
    /// one step; `SKIP LOCKED` keeps concurrent claims disjoint.
    pub async fn claim(
        &self,
        campaign_id: CampaignId,
        worker: &str,
        limit: i64,
        room_prefix: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ClaimedAttempt>, AttemptError> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        let Some(row) = sqlx::query(
            "SELECT state, concurrency_cap, agent FROM campaigns WHERE id = $1 FOR UPDATE",
        )
        .bind(campaign_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?
        else {
            return Ok(Vec::new());
        };
        let state: String = row.try_get("state").map_err(err)?;
        let cap: i32 = row.try_get("concurrency_cap").map_err(err)?;
        let agent: Option<serde_json::Value> = row.try_get("agent").map_err(err)?;
        if state != "running" {
            return Ok(Vec::new());
        }
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM call_attempts WHERE campaign_id = $1 AND state <> 'ended'",
        )
        .bind(campaign_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(err)?;
        let take = limit.min(i64::from(cap) - live);
        if take <= 0 {
            return Ok(Vec::new());
        }

        let sql = format!(
            "SELECT {LEAD_COLS} FROM leads \
             WHERE campaign_id = $1 AND status IN ('new', 'queued') \
               AND (next_eligible_at IS NULL OR next_eligible_at <= $2) \
             ORDER BY called_count, id LIMIT $3 FOR UPDATE SKIP LOCKED"
        );
        let rows = sqlx::query(&sql)
            .bind(campaign_id.0)
            .bind(now)
            .bind(take)
            .fetch_all(&mut *tx)
            .await
            .map_err(err)?;

        let first = if agent.is_some() {
            Attempt::with_agent()
        } else {
            Attempt::default()
        };
        let snapshot = serde_json::to_value(&first).map_err(err)?;
        let mut claimed = Vec::with_capacity(rows.len());
        for row in &rows {
            let lead = row_to_lead(row).map_err(err)?;
            sqlx::query(
                "UPDATE leads SET status = 'in_call', next_eligible_at = NULL, \
                 retry_reason = NULL, updated_at = now() WHERE id = $1",
            )
            .bind(lead.id.0)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
            let row = sqlx::query(
                "WITH n AS (SELECT nextval(pg_get_serial_sequence('call_attempts', 'id')) AS id) \
                 INSERT INTO call_attempts (id, campaign_id, lead_id, phone_number, room, \
                     participant_identity, worker, state, snapshot, agent_state) \
                 SELECT n.id, $1, $2, $3, $4 || '-' || n.id, 'sip-attempt-' || n.id, $5, 'pending', $6, $7 \
                 FROM n RETURNING id, room, participant_identity",
            )
            .bind(campaign_id.0)
            .bind(lead.id.0)
            .bind(&lead.phone_number)
            .bind(room_prefix)
            .bind(worker)
            .bind(&snapshot)
            .bind(first.agent.name())
            .fetch_one(&mut *tx)
            .await
            .map_err(err)?;
            claimed.push(ClaimedAttempt {
                attempt_id: AttemptId(row.try_get("id").map_err(err)?),
                participant_identity: row.try_get("participant_identity").map_err(err)?,
                room: row.try_get("room").map_err(err)?,
                agent: agent.clone(),
                lead,
            });
        }
        tx.commit().await.map_err(err)?;
        Ok(claimed)
    }

    /// Apply one event to an attempt, from a source that does not own it (a
    /// webhook, a test).
    pub async fn apply(
        &self,
        attempt_id: AttemptId,
        event: &AttemptEvent,
    ) -> Result<Applied, AttemptError> {
        self.apply_inner(attempt_id, event, None).await
    }

    /// Apply one event as the node that owns the attempt. The event applies
    /// only while the row still names this node with this generation.
    pub async fn apply_owned(
        &self,
        attempt_id: AttemptId,
        event: &AttemptEvent,
        owner: &Owner,
    ) -> Result<Applied, AttemptError> {
        self.apply_inner(attempt_id, event, Some(owner)).await
    }

    async fn apply_inner(
        &self,
        attempt_id: AttemptId,
        event: &AttemptEvent,
        owner: Option<&Owner>,
    ) -> Result<Applied, AttemptError> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        let row = sqlx::query(
            "SELECT campaign_id, snapshot, node, assign_gen FROM call_attempts \
             WHERE id = $1 FOR UPDATE",
        )
        .bind(attempt_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?
        .ok_or(AttemptError::NotFound(attempt_id))?;
        if let Some(owner) = owner {
            let node: Option<String> = row.try_get("node").map_err(err)?;
            let gen: i32 = row.try_get("assign_gen").map_err(err)?;
            if node.as_deref() != Some(owner.node.as_str()) || gen != owner.gen {
                return Ok(Applied::Fenced);
            }
        }
        let campaign_id: i64 = row.try_get("campaign_id").map_err(err)?;
        let current: Attempt =
            serde_json::from_value(row.try_get("snapshot").map_err(err)?).map_err(err)?;
        let next = match ad_attempt::apply(&current, event) {
            Ok(next) => next,
            Err(ignored) => return Ok(Applied::Ignored(ignored)),
        };

        sqlx::query(
            "INSERT INTO attempt_events (attempt_id, seq, kind, payload) VALUES ($1, $2, $3, $4)",
        )
        .bind(attempt_id.0)
        .bind(next.seq as i32)
        .bind(event.kind())
        .bind(serde_json::to_value(event).map_err(err)?)
        .execute(&mut *tx)
        .await
        .map_err(err)?;

        let outcome = next.outcome();
        let reconciled = matches!(
            event,
            AttemptEvent::Present { .. } | AttemptEvent::Absent { .. }
        );
        let agent_identity = match event {
            AttemptEvent::AgentReady { identity, .. } => Some(identity.as_str()),
            _ => None,
        };
        sqlx::query(
            "UPDATE call_attempts SET state = $2, snapshot = $3, seq = $4, outcome = $5, \
                 sip_code = $6, detail = $7, failure = $8, dialed_at = $9, answered_at = $10, \
                 ended_at = $11, checked_at = CASE WHEN $12 THEN $13 ELSE checked_at END, \
                 agent_state = $14, agent_identity = COALESCE($15, agent_identity), \
                 updated_at = now() \
             WHERE id = $1",
        )
        .bind(attempt_id.0)
        .bind(next.leg.name())
        .bind(serde_json::to_value(&next).map_err(err)?)
        .bind(next.seq as i32)
        .bind(outcome.map(|o| o.class()))
        .bind(outcome.and_then(|o| o.sip_code()).map(i32::from))
        .bind(outcome.map(|o| o.detail()))
        .bind(outcome.is_some_and(|o| o.is_failure()))
        .bind(next.dialed_at)
        .bind(next.answered_at)
        .bind(next.ended_at())
        .bind(reconciled)
        .bind(event.at())
        .bind(next.agent.name())
        .bind(agent_identity)
        .execute(&mut *tx)
        .await
        .map_err(err)?;

        if !next.leg.is_live() {
            publish_in(
                &mut tx,
                &[CampaignEvent::AttemptEnded {
                    campaign_id,
                    attempt_id: attempt_id.0,
                }],
            )
            .await
            .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(Applied::Changed(next))
    }

    /// Give a live attempt to a node. Returns the new generation, or `None` when
    /// the attempt has ended. A node that holds an older generation can no
    /// longer change the attempt.
    pub async fn assign(
        &self,
        attempt_id: AttemptId,
        node: &str,
    ) -> Result<Option<Owner>, AttemptError> {
        let gen: Option<i32> = sqlx::query_scalar(
            "UPDATE call_attempts SET node = $2, assign_gen = assign_gen + 1, updated_at = now() \
             WHERE id = $1 AND state <> 'ended' RETURNING assign_gen",
        )
        .bind(attempt_id.0)
        .bind(node)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?;
        Ok(gen.map(|gen| Owner {
            node: node.to_string(),
            gen,
        }))
    }

    /// Everything a node needs to run an attempt.
    pub async fn get(&self, attempt_id: AttemptId) -> Result<AttemptRow, AttemptError> {
        let row = sqlx::query(
            "SELECT a.campaign_id, a.lead_id, a.phone_number, a.room, a.participant_identity, \
                    a.snapshot, a.node, a.assign_gen, a.agent_identity, c.agent, l.context \
             FROM call_attempts a JOIN campaigns c ON c.id = a.campaign_id \
                  JOIN leads l ON l.id = a.lead_id \
             WHERE a.id = $1",
        )
        .bind(attempt_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?
        .ok_or(AttemptError::NotFound(attempt_id))?;
        Ok(AttemptRow {
            attempt_id,
            campaign_id: CampaignId(row.try_get("campaign_id").map_err(err)?),
            lead_id: LeadId(row.try_get("lead_id").map_err(err)?),
            phone_number: row.try_get("phone_number").map_err(err)?,
            room: row.try_get("room").map_err(err)?,
            participant_identity: row.try_get("participant_identity").map_err(err)?,
            attempt: serde_json::from_value(row.try_get("snapshot").map_err(err)?).map_err(err)?,
            node: row.try_get("node").map_err(err)?,
            assign_gen: row.try_get("assign_gen").map_err(err)?,
            agent_identity: row.try_get("agent_identity").map_err(err)?,
            agent: row.try_get("agent").map_err(err)?,
            lead_context: row.try_get("context").map_err(err)?,
        })
    }

    /// Record that a reconcile found a connected call in its room.
    pub async fn mark_checked(
        &self,
        attempt_id: AttemptId,
        at: DateTime<Utc>,
    ) -> Result<(), AttemptError> {
        sqlx::query("UPDATE call_attempts SET checked_at = $2 WHERE id = $1")
            .bind(attempt_id.0)
            .bind(at)
            .execute(&self.pool)
            .await
            .map_err(err)?;
        Ok(())
    }

    /// Update the lead from an ended attempt, once. `rule` decides the update
    /// and the campaign events, which commit with it. Returns `None` when the
    /// attempt is live or already settled.
    pub async fn settle<F>(
        &self,
        attempt_id: AttemptId,
        rule: F,
    ) -> Result<Option<LeadUpdate>, AttemptError>
    where
        F: FnOnce(&SettleInput<'_>) -> (LeadUpdate, Vec<CampaignEvent>),
    {
        let mut tx = self.pool.begin().await.map_err(err)?;
        let row = sqlx::query(
            "SELECT a.campaign_id, a.lead_id, a.snapshot, a.settled_at, l.called_count \
             FROM call_attempts a JOIN leads l ON l.id = a.lead_id \
             WHERE a.id = $1 FOR UPDATE OF a, l",
        )
        .bind(attempt_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(err)?
        .ok_or(AttemptError::NotFound(attempt_id))?;
        let settled_at: Option<DateTime<Utc>> = row.try_get("settled_at").map_err(err)?;
        let attempt: Attempt =
            serde_json::from_value(row.try_get("snapshot").map_err(err)?).map_err(err)?;
        let Some(outcome) = attempt.outcome() else {
            return Ok(None);
        };
        if settled_at.is_some() {
            return Ok(None);
        }
        let lead_id = LeadId(row.try_get("lead_id").map_err(err)?);
        let (update, events) = rule(&SettleInput {
            campaign_id: CampaignId(row.try_get("campaign_id").map_err(err)?),
            lead_id,
            outcome,
            called_count: row.try_get::<i32, _>("called_count").map_err(err)? as u32,
        });

        sqlx::query(
            "UPDATE leads SET status = $2, called_count = called_count + $3, \
                 last_local_call_time = CASE WHEN $3 = 1 THEN $4 ELSE last_local_call_time END, \
                 next_eligible_at = $5, retry_reason = $6, updated_at = now() \
             WHERE id = $1",
        )
        .bind(lead_id.0)
        .bind(update.status.as_str())
        .bind(i32::from(update.counts_as_call))
        .bind(attempt.dialed_at)
        .bind(update.next_eligible_at)
        .bind(update.wait_reason)
        .execute(&mut *tx)
        .await
        .map_err(err)?;
        sqlx::query("UPDATE call_attempts SET settled_at = now() WHERE id = $1")
            .bind(attempt_id.0)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        publish_in(&mut tx, &events).await.map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(Some(update))
    }

    /// Ended attempts that have not updated their lead yet, oldest first.
    pub async fn unsettled(&self, limit: i64) -> Result<Vec<AttemptId>, AttemptError> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM call_attempts WHERE state = 'ended' AND settled_at IS NULL \
             ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        Ok(ids.into_iter().map(AttemptId).collect())
    }

    /// Live attempts, oldest first.
    pub async fn live(&self, limit: i64) -> Result<Vec<LiveAttempt>, AttemptError> {
        let rows = sqlx::query(
            "SELECT id, campaign_id, room, participant_identity, snapshot, checked_at, created_at, \
                    node, assign_gen \
             FROM call_attempts WHERE state <> 'ended' ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        rows.iter()
            .map(|r| {
                Ok(LiveAttempt {
                    attempt_id: AttemptId(r.try_get("id").map_err(err)?),
                    campaign_id: CampaignId(r.try_get("campaign_id").map_err(err)?),
                    room: r.try_get("room").map_err(err)?,
                    participant_identity: r.try_get("participant_identity").map_err(err)?,
                    attempt: serde_json::from_value(r.try_get("snapshot").map_err(err)?)
                        .map_err(err)?,
                    checked_at: r.try_get("checked_at").map_err(err)?,
                    created_at: r.try_get("created_at").map_err(err)?,
                    node: r.try_get("node").map_err(err)?,
                    assign_gen: r.try_get("assign_gen").map_err(err)?,
                })
            })
            .collect()
    }

    /// The state of each given attempt, for the leader's repair pass. Missing
    /// ids are left out.
    pub async fn briefs(&self, ids: &[i64]) -> Result<Vec<AttemptBrief>, AttemptError> {
        let rows = sqlx::query(
            "SELECT id, campaign_id, node, assign_gen, room, state <> 'ended' AS live, \
                    settled_at IS NOT NULL AS settled \
             FROM call_attempts WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        rows.iter()
            .map(|r| {
                Ok(AttemptBrief {
                    attempt_id: AttemptId(r.try_get("id").map_err(err)?),
                    campaign_id: CampaignId(r.try_get("campaign_id").map_err(err)?),
                    node: r.try_get("node").map_err(err)?,
                    assign_gen: r.try_get("assign_gen").map_err(err)?,
                    room: r.try_get("room").map_err(err)?,
                    live: r.try_get("live").map_err(err)?,
                    settled: r.try_get("settled").map_err(err)?,
                })
            })
            .collect()
    }

    /// The live attempts in a room.
    pub async fn live_in_room(&self, room: &str) -> Result<Vec<RoomAttempt>, AttemptError> {
        let rows = sqlx::query(
            "SELECT id, participant_identity, agent_identity FROM call_attempts \
             WHERE room = $1 AND state <> 'ended' ORDER BY id",
        )
        .bind(room)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        rows.iter()
            .map(|r| {
                Ok(RoomAttempt {
                    attempt_id: AttemptId(r.try_get("id").map_err(err)?),
                    callee_identity: r.try_get("participant_identity").map_err(err)?,
                    agent_identity: r.try_get("agent_identity").map_err(err)?,
                })
            })
            .collect()
    }

    /// The number of live attempts of a campaign: its used capacity.
    pub async fn live_count(&self, campaign_id: CampaignId) -> Result<i64, AttemptError> {
        sqlx::query_scalar(
            "SELECT count(*) FROM call_attempts WHERE campaign_id = $1 AND state <> 'ended'",
        )
        .bind(campaign_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(err)
    }

    /// The saved snapshot of an attempt.
    pub async fn snapshot(&self, attempt_id: AttemptId) -> Result<Attempt, AttemptError> {
        let value: serde_json::Value =
            sqlx::query_scalar("SELECT snapshot FROM call_attempts WHERE id = $1")
                .bind(attempt_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(err)?
                .ok_or(AttemptError::NotFound(attempt_id))?;
        serde_json::from_value(value).map_err(err)
    }

    /// The saved events of an attempt, in order.
    pub async fn events(&self, attempt_id: AttemptId) -> Result<Vec<AttemptEvent>, AttemptError> {
        let values: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT payload FROM attempt_events WHERE attempt_id = $1 ORDER BY seq",
        )
        .bind(attempt_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        values
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(err))
            .collect()
    }
}
