//! Campaign aggregate and its lifecycle state machine (PLAN.md §0.10.2).
//!
//! The state machine is pure. `Transition::apply` returns the next state or a
//! `TransitionError`. It does no I/O. The repository (see `repo`) persists the
//! aggregate; the state machine only decides whether a change is legal.

pub mod repo;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use ad_core::{CampaignId, OrgId};

/// Lifecycle state. Mirrors PLAN.md §0.10.2:
/// draft → syncing → running ⇄ paused → completed/failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignState {
    Draft,
    Syncing,
    Running,
    Paused,
    Completed,
    Failed,
}

impl CampaignState {
    /// A terminal state accepts no further transitions.
    pub fn is_terminal(&self) -> bool {
        matches!(self, CampaignState::Completed | CampaignState::Failed)
    }

    /// Stable wire/storage string. Used by the database layer.
    pub fn as_str(&self) -> &'static str {
        match self {
            CampaignState::Draft => "draft",
            CampaignState::Syncing => "syncing",
            CampaignState::Running => "running",
            CampaignState::Paused => "paused",
            CampaignState::Completed => "completed",
            CampaignState::Failed => "failed",
        }
    }

    /// Parse a stored state string. Returns None for an unknown value.
    pub fn from_db_str(s: &str) -> Option<CampaignState> {
        Some(match s {
            "draft" => CampaignState::Draft,
            "syncing" => CampaignState::Syncing,
            "running" => CampaignState::Running,
            "paused" => CampaignState::Paused,
            "completed" => CampaignState::Completed,
            "failed" => CampaignState::Failed,
            _ => return None,
        })
    }
}

/// An operator- or system-initiated lifecycle action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    /// Start a draft campaign. It begins syncing its source.
    Start,
    /// Source sync finished. The campaign begins dialing.
    SyncComplete,
    /// Pause a running campaign. In-flight calls complete.
    Pause,
    /// Resume a paused campaign.
    Resume,
    /// All work is done.
    Complete,
    /// A fatal error stopped the campaign.
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal transition {transition:?} from state {from:?}")]
pub struct TransitionError {
    pub from: CampaignState,
    pub transition: Transition,
}

/// Decide the next state for a transition, or reject it. Pure.
pub fn next_state(
    from: CampaignState,
    transition: Transition,
) -> Result<CampaignState, TransitionError> {
    use CampaignState as S;
    use Transition as T;

    let next = match (from, transition) {
        (S::Draft, T::Start) => S::Syncing,
        (S::Syncing, T::SyncComplete) => S::Running,
        (S::Running, T::Pause) => S::Paused,
        (S::Paused, T::Resume) => S::Running,
        // Completion and failure may happen from any active (non-terminal) state.
        (s, T::Complete) if !s.is_terminal() => S::Completed,
        (s, T::Fail) if !s.is_terminal() => S::Failed,
        _ => return Err(TransitionError { from, transition }),
    };
    Ok(next)
}

/// Row counters tracked over a campaign run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub total_rows: u64,
    pub processed_rows: u64,
    pub failed_rows: u64,
}

/// The default `concurrency_cap`.
pub const DEFAULT_CONCURRENCY_CAP: u32 = 10;

/// The campaign aggregate. Fields grow as later slices land (schedule, pacing,
/// retry, rulesets). It carries identity, lifecycle, counters, and the
/// concurrency cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Campaign {
    pub id: CampaignId,
    pub org_id: OrgId,
    pub name: String,
    pub state: CampaignState,
    pub counters: Counters,
    /// The most live call attempts at one time (GAPS.md DIAL-2).
    pub concurrency_cap: u32,
    /// The agent of the campaign as data (`runtime` names the adapter), or
    /// `None` for no agent. `ad_agent::AgentSpec` reads it.
    pub agent: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Campaign {
    /// Create a new campaign in the `Draft` state.
    pub fn new(id: CampaignId, org_id: OrgId, name: String, now: DateTime<Utc>) -> Self {
        Campaign {
            id,
            org_id,
            name,
            state: CampaignState::Draft,
            counters: Counters::default(),
            concurrency_cap: DEFAULT_CONCURRENCY_CAP,
            agent: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Apply a lifecycle transition in place, or reject it. Pure except for the
    /// `updated_at` stamp supplied by the caller.
    pub fn apply(
        &mut self,
        transition: Transition,
        now: DateTime<Utc>,
    ) -> Result<(), TransitionError> {
        let next = next_state(self.state, transition)?;
        self.state = next;
        self.updated_at = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn new_campaign_starts_in_draft() {
        let c = Campaign::new(CampaignId(1), OrgId(1), "test".into(), t0());
        assert_eq!(c.state, CampaignState::Draft);
        assert_eq!(c.counters, Counters::default());
    }

    #[test]
    fn happy_path_lifecycle() {
        let mut c = Campaign::new(CampaignId(1), OrgId(1), "t".into(), t0());
        assert!(c.apply(Transition::Start, t0()).is_ok());
        assert_eq!(c.state, CampaignState::Syncing);
        assert!(c.apply(Transition::SyncComplete, t0()).is_ok());
        assert_eq!(c.state, CampaignState::Running);
        assert!(c.apply(Transition::Pause, t0()).is_ok());
        assert_eq!(c.state, CampaignState::Paused);
        assert!(c.apply(Transition::Resume, t0()).is_ok());
        assert_eq!(c.state, CampaignState::Running);
        assert!(c.apply(Transition::Complete, t0()).is_ok());
        assert_eq!(c.state, CampaignState::Completed);
    }

    #[test]
    fn cannot_pause_a_draft() {
        let mut c = Campaign::new(CampaignId(1), OrgId(1), "t".into(), t0());
        let err = c.apply(Transition::Pause, t0()).unwrap_err();
        assert_eq!(err.from, CampaignState::Draft);
        assert_eq!(err.transition, Transition::Pause);
        // State is unchanged after a rejected transition.
        assert_eq!(c.state, CampaignState::Draft);
    }

    #[test]
    fn terminal_states_reject_all_transitions() {
        for terminal in [CampaignState::Completed, CampaignState::Failed] {
            for t in [
                Transition::Start,
                Transition::SyncComplete,
                Transition::Pause,
                Transition::Resume,
                Transition::Complete,
                Transition::Fail,
            ] {
                assert!(
                    next_state(terminal, t).is_err(),
                    "{terminal:?} must reject {t:?}"
                );
            }
        }
    }

    #[test]
    fn fail_allowed_from_any_active_state() {
        for active in [
            CampaignState::Draft,
            CampaignState::Syncing,
            CampaignState::Running,
            CampaignState::Paused,
        ] {
            assert_eq!(
                next_state(active, Transition::Fail),
                Ok(CampaignState::Failed)
            );
        }
    }

    #[test]
    fn cannot_resume_a_running_campaign() {
        assert!(next_state(CampaignState::Running, Transition::Resume).is_err());
    }

    #[test]
    fn state_string_roundtrip() {
        for s in [
            CampaignState::Draft,
            CampaignState::Syncing,
            CampaignState::Running,
            CampaignState::Paused,
            CampaignState::Completed,
            CampaignState::Failed,
        ] {
            assert_eq!(CampaignState::from_db_str(s.as_str()), Some(s));
        }
        assert_eq!(CampaignState::from_db_str("bogus"), None);
    }
}
