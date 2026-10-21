//! Lead aggregate (PLAN.md §0.10.3). A lead belongs to a campaign and carries
//! the compliance-bearing fields the dialer and `ad_compliance` need:
//! phone number, timezone, state/postal, status, call counters, and an
//! arbitrary `context` map (CSV extra columns → agent template variables).

pub mod repo;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use ad_core::{CampaignId, LeadId};

/// Dialing status of a lead. Stored as a stable string (discriminator in data),
/// so adding a status needs no migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadStatus {
    /// Never dialed.
    New,
    /// Waiting for a dial. `next_eligible_at` holds the due time of a retry or
    /// a hold; without it the lead is due now.
    Queued,
    /// Has a live call attempt.
    InCall,
    /// Dialed. Only a recycle, retry, or callback rule makes it dialable again.
    Dispositioned,
    /// Terminal: do not call again (completed or suppressed).
    Done,
    /// On a do-not-call list.
    Dnc,
    /// Cannot be dialed until its data changes (for example, it has no
    /// timezone). The last attempt holds the reason.
    Blocked,
}

impl LeadStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LeadStatus::New => "new",
            LeadStatus::Queued => "queued",
            LeadStatus::InCall => "in_call",
            LeadStatus::Dispositioned => "dispositioned",
            LeadStatus::Done => "done",
            LeadStatus::Dnc => "dnc",
            LeadStatus::Blocked => "blocked",
        }
    }

    pub fn from_db_str(s: &str) -> Option<LeadStatus> {
        Some(match s {
            "new" => LeadStatus::New,
            "queued" => LeadStatus::Queued,
            "in_call" => LeadStatus::InCall,
            "dispositioned" => LeadStatus::Dispositioned,
            "done" => LeadStatus::Done,
            "dnc" => LeadStatus::Dnc,
            "blocked" => LeadStatus::Blocked,
            _ => return None,
        })
    }
}

/// The lead aggregate. Compliance-bearing fields mirror VICIdial `vicidial_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lead {
    pub id: LeadId,
    pub campaign_id: CampaignId,
    pub phone_number: String,
    /// IANA timezone (e.g. "America/New_York"). Compliance fails closed if absent.
    pub timezone: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub status: LeadStatus,
    pub called_count: u32,
    pub last_local_call_time: Option<DateTime<Utc>>,
    /// CSV extra columns → agent template variables.
    pub context: BTreeMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_string_roundtrip() {
        for s in [
            LeadStatus::New,
            LeadStatus::Queued,
            LeadStatus::InCall,
            LeadStatus::Dispositioned,
            LeadStatus::Done,
            LeadStatus::Dnc,
            LeadStatus::Blocked,
        ] {
            assert_eq!(LeadStatus::from_db_str(s.as_str()), Some(s));
        }
        assert_eq!(LeadStatus::from_db_str("nope"), None);
    }
}
