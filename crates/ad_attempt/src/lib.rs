//! One call attempt as a statechart (PLAN.md §0.10.7a, decision D-2).
//!
//! The attempt is plain data that serializes to JSON. `apply` is pure: it takes
//! the attempt and one event, and it returns the next attempt. It returns
//! `Ignored` when the event does not apply in the current state, for example a
//! late or repeated webhook. The database saves each applied event with its
//! sequence number and the new snapshot in one transaction, so `replay` of the
//! saved events gives the saved snapshot.
//!
//! The attempt has two regions. `leg` is the customer SIP leg. `agent` is the
//! AI agent (a LiveKit agent or a pipecat bot) in the same room. The dial
//! starts only when the agent is ready, so no callee answers to an empty room.
//! A live attempt (any leg state except `Ended`) holds one unit of campaign
//! capacity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod sip;
pub use sip::classify_sip;

/// The state of the customer SIP leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Leg {
    /// Claimed. Not dialed yet.
    Pending,
    /// The dial request went to the provider.
    Dialing { since: DateTime<Utc> },
    /// The dial request may have reached the provider, but no result came back.
    /// The attempt keeps its capacity until a reconcile finds the result.
    Uncertain {
        since: DateTime<Utc>,
        detail: String,
    },
    /// The callee answered.
    Connected { since: DateTime<Utc> },
    /// Terminal.
    Ended { at: DateTime<Utc>, outcome: Outcome },
}

impl Leg {
    /// The stored state name (`call_attempts.state`).
    pub fn name(&self) -> &'static str {
        match self {
            Leg::Pending => "pending",
            Leg::Dialing { .. } => "dialing",
            Leg::Uncertain { .. } => "uncertain",
            Leg::Connected { .. } => "connected",
            Leg::Ended { .. } => "ended",
        }
    }

    pub fn is_live(&self) -> bool {
        !matches!(self, Leg::Ended { .. })
    }
}

/// The state of the agent in the room.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentLeg {
    /// The campaign has no agent.
    #[default]
    None,
    /// The dialer must start the agent before the dial.
    Wanted,
    /// The runtime accepted the start request; the agent is not in the room yet.
    Starting { since: DateTime<Utc> },
    /// The agent is in the room.
    Ready { since: DateTime<Utc> },
    /// The agent left the room.
    Left { at: DateTime<Utc> },
    /// The agent did not start or did not join in time.
    Failed { at: DateTime<Utc>, detail: String },
}

impl AgentLeg {
    pub fn name(&self) -> &'static str {
        match self {
            AgentLeg::None => "none",
            AgentLeg::Wanted => "wanted",
            AgentLeg::Starting { .. } => "starting",
            AgentLeg::Ready { .. } => "ready",
            AgentLeg::Left { .. } => "left",
            AgentLeg::Failed { .. } => "failed",
        }
    }
}

/// Why an attempt ended without a dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotDialedReason {
    /// Outside the calling window, a holiday, or an invalid window set.
    Window,
    /// The number is on a do-not-call list.
    Dnc,
    /// The local time of the lead is unknown (no IANA timezone).
    UnknownTimezone,
    /// A rule or a limit excludes the lead.
    Excluded,
    /// The dialer gave the lead back before the dial (for example after a
    /// restart).
    Released,
    /// The agent did not start, did not join in time, or left before the dial.
    NoAgent,
}

/// The result of an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Outcome {
    /// The callee answered and the call ended.
    Completed {
        detail: String,
    },
    Busy {
        sip_code: u16,
    },
    NoAnswer {
        sip_code: u16,
    },
    Declined {
        sip_code: u16,
    },
    InvalidNumber {
        sip_code: u16,
    },
    /// The provider or the carrier failed the call.
    Failed {
        sip_code: Option<u16>,
        detail: String,
    },
    /// The dial request did not reach the provider. No call exists.
    SetupError {
        detail: String,
    },
    /// The dialer never learned the result.
    Uncertain {
        detail: String,
    },
    NotDialed {
        reason: NotDialedReason,
        detail: String,
    },
}

impl Outcome {
    /// The stored class name (`call_attempts.outcome`).
    pub fn class(&self) -> &'static str {
        match self {
            Outcome::Completed { .. } => "completed",
            Outcome::Busy { .. } => "busy",
            Outcome::NoAnswer { .. } => "no_answer",
            Outcome::Declined { .. } => "declined",
            Outcome::InvalidNumber { .. } => "invalid_number",
            Outcome::Failed { .. } => "failed",
            Outcome::SetupError { .. } => "setup_error",
            Outcome::Uncertain { .. } => "uncertain",
            Outcome::NotDialed { .. } => "not_dialed",
        }
    }

    pub fn sip_code(&self) -> Option<u16> {
        match self {
            Outcome::Busy { sip_code }
            | Outcome::NoAnswer { sip_code }
            | Outcome::Declined { sip_code }
            | Outcome::InvalidNumber { sip_code } => Some(*sip_code),
            Outcome::Failed { sip_code, .. } => *sip_code,
            _ => None,
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Outcome::Completed { detail }
            | Outcome::Failed { detail, .. }
            | Outcome::SetupError { detail }
            | Outcome::Uncertain { detail }
            | Outcome::NotDialed { detail, .. } => detail,
            _ => "",
        }
    }

    /// Whether the circuit breaker counts this outcome as a failure. A busy or
    /// unanswered call is a normal result, not a failure.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Outcome::Failed { .. } | Outcome::SetupError { .. } | Outcome::Uncertain { .. }
        )
    }
}

/// The snapshot that the database saves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub leg: Leg,
    #[serde(default)]
    pub agent: AgentLeg,
    /// The number of applied events.
    pub seq: u32,
    pub dialed_at: Option<DateTime<Utc>>,
    pub answered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub agent_ready_at: Option<DateTime<Utc>>,
}

impl Default for Attempt {
    fn default() -> Self {
        Attempt {
            leg: Leg::Pending,
            agent: AgentLeg::None,
            seq: 0,
            dialed_at: None,
            answered_at: None,
            agent_ready_at: None,
        }
    }
}

impl Attempt {
    /// A new attempt whose campaign has an agent: the dial waits for it.
    pub fn with_agent() -> Self {
        Attempt {
            agent: AgentLeg::Wanted,
            ..Attempt::default()
        }
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        match &self.leg {
            Leg::Ended { outcome, .. } => Some(outcome),
            _ => None,
        }
    }

    pub fn ended_at(&self) -> Option<DateTime<Utc>> {
        match &self.leg {
            Leg::Ended { at, .. } => Some(*at),
            _ => None,
        }
    }

    /// The dial may start: the leg waits, and the agent is ready or not needed.
    pub fn can_dial(&self) -> bool {
        matches!(self.leg, Leg::Pending)
            && matches!(self.agent, AgentLeg::None | AgentLeg::Ready { .. })
    }
}

/// An input to the statechart. Every event carries its own time, so `apply`
/// reads no clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptEvent {
    /// The dispatch gate denied the call, or the dialer released the lead.
    NotDialed {
        at: DateTime<Utc>,
        reason: NotDialedReason,
        detail: String,
    },
    /// The agent runtime accepted the start request.
    AgentStarting { at: DateTime<Utc>, detail: String },
    /// The agent is in the room.
    AgentReady { at: DateTime<Utc>, identity: String },
    /// The agent did not start, or did not join in time.
    AgentFailed { at: DateTime<Utc>, detail: String },
    /// The agent left the room.
    AgentLeft { at: DateTime<Utc> },
    /// The dialer is about to send the dial request.
    DialStarted { at: DateTime<Utc> },
    /// The callee answered.
    Answered { at: DateTime<Utc> },
    /// The provider returned a final result that is not an answer.
    Rejected {
        at: DateTime<Utc>,
        sip_code: Option<u16>,
        detail: String,
    },
    /// The dial request did not reach the provider.
    NotSent { at: DateTime<Utc>, detail: String },
    /// The dial request went out, but no result came back.
    NoResponse { at: DateTime<Utc>, detail: String },
    /// The customer leg left the room.
    Hangup { at: DateTime<Utc>, detail: String },
    /// A reconcile found the customer leg in the room.
    Present { at: DateTime<Utc> },
    /// A reconcile did not find the customer leg in the room.
    Absent { at: DateTime<Utc> },
}

impl AttemptEvent {
    /// The stored event name (`attempt_events.kind`).
    pub fn kind(&self) -> &'static str {
        match self {
            AttemptEvent::NotDialed { .. } => "not_dialed",
            AttemptEvent::AgentStarting { .. } => "agent_starting",
            AttemptEvent::AgentReady { .. } => "agent_ready",
            AttemptEvent::AgentFailed { .. } => "agent_failed",
            AttemptEvent::AgentLeft { .. } => "agent_left",
            AttemptEvent::DialStarted { .. } => "dial_started",
            AttemptEvent::Answered { .. } => "answered",
            AttemptEvent::Rejected { .. } => "rejected",
            AttemptEvent::NotSent { .. } => "not_sent",
            AttemptEvent::NoResponse { .. } => "no_response",
            AttemptEvent::Hangup { .. } => "hangup",
            AttemptEvent::Present { .. } => "present",
            AttemptEvent::Absent { .. } => "absent",
        }
    }

    pub fn at(&self) -> DateTime<Utc> {
        match self {
            AttemptEvent::NotDialed { at, .. }
            | AttemptEvent::AgentStarting { at, .. }
            | AttemptEvent::AgentReady { at, .. }
            | AttemptEvent::AgentFailed { at, .. }
            | AttemptEvent::AgentLeft { at }
            | AttemptEvent::DialStarted { at }
            | AttemptEvent::Answered { at }
            | AttemptEvent::Rejected { at, .. }
            | AttemptEvent::NotSent { at, .. }
            | AttemptEvent::NoResponse { at, .. }
            | AttemptEvent::Hangup { at, .. }
            | AttemptEvent::Present { at }
            | AttemptEvent::Absent { at } => *at,
        }
    }
}

/// The event does not apply in the current state. The attempt is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ignored {
    pub state: &'static str,
    pub event: &'static str,
}

/// Apply one event. Pure.
pub fn apply(attempt: &Attempt, event: &AttemptEvent) -> Result<Attempt, Ignored> {
    use AgentLeg as A;
    use AttemptEvent as E;
    use Leg as L;

    let ignored = || Ignored {
        state: attempt.leg.name(),
        event: event.kind(),
    };
    // An ended attempt never changes again, so it never takes capacity back.
    if !attempt.leg.is_live() {
        return Err(ignored());
    }
    let ended = |at: DateTime<Utc>, outcome: Outcome| L::Ended { at, outcome };
    let no_agent = |at: DateTime<Utc>, detail: &str| {
        ended(
            at,
            Outcome::NotDialed {
                reason: NotDialedReason::NoAgent,
                detail: detail.into(),
            },
        )
    };
    let mut next = attempt.clone();
    match (&attempt.leg, &attempt.agent, event) {
        // The agent region.
        (L::Pending, A::Wanted, E::AgentStarting { at, .. }) => {
            next.agent = A::Starting { since: *at };
        }
        (L::Pending, A::Starting { .. }, E::AgentReady { at, .. }) => {
            next.agent = A::Ready { since: *at };
            next.agent_ready_at = Some(*at);
        }
        (L::Pending, A::Wanted | A::Starting { .. }, E::AgentFailed { at, detail }) => {
            next.agent = A::Failed {
                at: *at,
                detail: detail.clone(),
            };
            next.leg = no_agent(*at, detail);
        }
        (leg, A::Ready { .. }, E::AgentLeft { at }) => {
            next.agent = A::Left { at: *at };
            match leg {
                L::Pending => next.leg = no_agent(*at, "the agent left before the dial"),
                L::Connected { .. } => {
                    next.leg = ended(
                        *at,
                        Outcome::Completed {
                            detail: "the agent ended the call".into(),
                        },
                    )
                }
                // The dial result or the reconcile ends the leg; the dialer
                // closes the room, so the call cannot connect to no one.
                L::Dialing { .. } | L::Uncertain { .. } | L::Ended { .. } => {}
            }
        }

        // The customer leg.
        (L::Pending, _, E::NotDialed { at, reason, detail }) => {
            next.leg = ended(
                *at,
                Outcome::NotDialed {
                    reason: *reason,
                    detail: detail.clone(),
                },
            );
        }
        (L::Pending, A::None | A::Ready { .. }, E::DialStarted { at }) => {
            next.dialed_at = Some(*at);
            next.leg = L::Dialing { since: *at };
        }
        (L::Dialing { .. } | L::Uncertain { .. }, _, E::Answered { at } | E::Present { at }) => {
            next.answered_at = Some(*at);
            next.leg = L::Connected { since: *at };
        }
        (
            L::Dialing { .. } | L::Uncertain { .. },
            _,
            E::Rejected {
                at,
                sip_code,
                detail,
            },
        ) => {
            let outcome = match sip_code {
                Some(code) => classify_sip(*code, detail.clone()),
                None => Outcome::Failed {
                    sip_code: None,
                    detail: detail.clone(),
                },
            };
            next.leg = ended(*at, outcome);
        }
        (L::Dialing { .. }, _, E::NotSent { at, detail }) => {
            next.leg = ended(
                *at,
                Outcome::SetupError {
                    detail: detail.clone(),
                },
            );
        }
        (L::Dialing { .. }, _, E::NoResponse { at, detail }) => {
            next.leg = L::Uncertain {
                since: *at,
                detail: detail.clone(),
            };
        }
        (L::Dialing { .. }, _, E::Absent { at }) => {
            next.leg = ended(
                *at,
                Outcome::Uncertain {
                    detail: "no dial result, and the call is not in the room".into(),
                },
            );
        }
        (L::Uncertain { detail, .. }, _, E::Absent { at } | E::Hangup { at, .. }) => {
            next.leg = ended(
                *at,
                Outcome::Uncertain {
                    detail: detail.clone(),
                },
            );
        }
        (L::Connected { .. }, _, E::Hangup { at, detail }) => {
            next.leg = ended(
                *at,
                Outcome::Completed {
                    detail: detail.clone(),
                },
            );
        }
        (L::Connected { .. }, _, E::Absent { at }) => {
            next.leg = ended(
                *at,
                Outcome::Completed {
                    detail: "the call left the room, and no end event came".into(),
                },
            );
        }
        _ => return Err(ignored()),
    }
    next.seq = attempt.seq + 1;
    Ok(next)
}

/// The replay failed: an event did not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayError {
    pub index: usize,
    pub ignored: Ignored,
}

/// Rebuild an attempt from its first snapshot and its saved events.
pub fn replay(start: Attempt, events: &[AttemptEvent]) -> Result<Attempt, ReplayError> {
    events.iter().enumerate().try_fold(start, |a, (index, e)| {
        apply(&a, e).map_err(|ignored| ReplayError { index, ignored })
    })
}

#[cfg(test)]
mod tests;
