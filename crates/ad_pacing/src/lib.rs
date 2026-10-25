//! Join-paced dialing (GAPS.md DIAL-9, decision D-10). Pure: no I/O, no clock.
//!
//! An agent pool is the agents that share one capacity: a LiveKit
//! `agent_name`, or a pipecat `start_url`. The window of a pool counts its
//! attempts whose agent is not in the room yet. A ringing call counts, because
//! it can answer at any time. The leader claims a call only while the window
//! is below the pool's limit, so at most that many agents start at once.
//!
//! `observe` learns the limit `k` from the time that each agent takes to join,
//! as TCP congestion control learns its window: slow start doubles `k` after
//! each round of fast joins, then each round adds one, and a slow join or a
//! failed start halves it. The target and the back-off are below the
//! abandonment limit, so the pool slows down before a call is abandoned.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a campaign uses the limit of its pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The learned limit. When few calls answer, it can grow above the number
    /// of agents that ever started at once.
    #[default]
    Adaptive,
    /// At most one agent start more than the pool proved: the largest number
    /// of starts at once that all joined by the target.
    Strict,
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Adaptive => "adaptive",
            Mode::Strict => "strict",
        }
    }
}

/// The settings of a pool, as the API stores them. An absent value takes its
/// default from the abandonment rule's limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// A join at or below this time is fast. Default: half the rule's limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ms: Option<u32>,
    /// A join above this time halves `k`. Default: three quarters of the
    /// rule's limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u32>,
    /// The `k` of a new or reset pool. Default: 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_k: Option<u32>,
    /// The largest `k`. Default: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_k: Option<u32>,
}

/// The settings with every default filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Limits {
    pub target_ms: u32,
    pub backoff_ms: u32,
    pub first_k: u32,
    pub max_k: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid pacing settings: {0}")]
pub struct SettingsError(pub String);

impl Settings {
    /// Fill in the defaults and check the values against the rule's limit.
    pub fn resolve(&self, connect_within_ms: u32) -> Result<Limits, SettingsError> {
        let limits = Limits {
            target_ms: self.target_ms.unwrap_or(connect_within_ms / 2),
            backoff_ms: self.backoff_ms.unwrap_or(connect_within_ms / 4 * 3),
            first_k: self.first_k.unwrap_or(1),
            max_k: self.max_k,
        };
        let bad = |m: String| Err(SettingsError(m));
        if limits.target_ms == 0 {
            return bad("target_ms must be at least 1".into());
        }
        if limits.backoff_ms <= limits.target_ms {
            return bad(format!(
                "backoff_ms ({}) must be above target_ms ({})",
                limits.backoff_ms, limits.target_ms
            ));
        }
        if limits.backoff_ms > connect_within_ms {
            return bad(format!(
                "backoff_ms ({}) must be at most the abandonment limit ({connect_within_ms} ms)",
                limits.backoff_ms
            ));
        }
        if limits.first_k == 0 {
            return bad("first_k must be at least 1".into());
        }
        if limits.max_k.is_some_and(|m| m < limits.first_k) {
            return bad("max_k must be at least first_k".into());
        }
        Ok(limits)
    }
}

/// The controller state of one pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub k: u32,
    /// Slow start doubles `k` after each round; the first join above the
    /// target ends it.
    pub slow_start: bool,
    /// Fast samples that filled the window, since `k` last grew.
    pub acks: u32,
    /// The largest number of agent starts at once that all joined by the
    /// target. A slow join lowers it below its own number.
    pub proven: u32,
    /// A slow sample that started before this time does not halve `k` again:
    /// the leader claimed it under the limit before the halving.
    pub halved_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub samples: u64,
    #[serde(default)]
    pub slow: u64,
}

impl State {
    pub fn new(limits: &Limits) -> Self {
        State {
            k: cap(limits.first_k, limits),
            slow_start: true,
            acks: 0,
            proven: 0,
            halved_at: None,
            samples: 0,
            slow: 0,
        }
    }
}

/// How one agent start ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// The agent was in the room this many milliseconds after the start: the
    /// answer (customer first) or the start request (agent first).
    Joined { ms: u32 },
    /// The start was refused, the agent did not join in time, or the call was
    /// abandoned.
    Failed,
    /// The call ended before the agent joined, for a reason that says nothing
    /// about the pool (for example the callee hung up).
    Unknown,
}

/// One agent start of the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub started_at: DateTime<Utc>,
    /// When the result was known: the join or the end.
    pub at: DateTime<Utc>,
    pub join: Join,
    /// The window of the pool with this attempt, at its claim.
    pub window: u32,
    /// The limit of the campaign at the claim.
    pub limit: u32,
    /// The agent starts in progress at `started_at`, this one included.
    pub concurrent: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Fast,
    /// Above the target, at or below the back-off.
    Mid,
    Slow,
}

/// The class of a join, or `None` for a sample that says nothing.
pub fn classify(join: Join, limits: &Limits) -> Option<Class> {
    match join {
        Join::Joined { ms } if ms <= limits.target_ms => Some(Class::Fast),
        Join::Joined { ms } if ms <= limits.backoff_ms => Some(Class::Mid),
        Join::Joined { .. } | Join::Failed => Some(Class::Slow),
        Join::Unknown => None,
    }
}

/// Learn from one sample.
pub fn observe(state: &State, limits: &Limits, sample: &Sample) -> State {
    let mut next = state.clone();
    let Some(class) = classify(sample.join, limits) else {
        return next;
    };
    next.samples += 1;
    match class {
        Class::Fast => {
            next.proven = next.proven.max(sample.concurrent);
            // A quiet pool proves nothing about a burst: only a full window
            // counts toward a larger `k`.
            if sample.window.saturating_add(1) >= sample.limit {
                next.acks += 1;
                if next.acks >= next.k {
                    next.k = if next.slow_start {
                        next.k.saturating_mul(2)
                    } else {
                        next.k.saturating_add(1)
                    };
                    next.acks = 0;
                }
            }
        }
        Class::Mid => next.slow_start = false,
        Class::Slow => {
            next.slow += 1;
            next.slow_start = false;
            next.proven = next.proven.min(sample.concurrent.saturating_sub(1));
            if next.halved_at.is_none_or(|h| sample.started_at >= h) {
                next.k /= 2;
                next.acks = 0;
                next.halved_at = Some(sample.at);
            }
        }
    }
    next.k = cap(next.k, limits);
    next
}

/// The window limit of a campaign of the pool.
pub fn limit(state: &State, limits: &Limits, mode: Mode) -> u32 {
    let k = cap(state.k, limits);
    match mode {
        Mode::Adaptive => k,
        Mode::Strict => k.min(state.proven.saturating_add(1)),
    }
}

fn cap(k: u32, limits: &Limits) -> u32 {
    limits.max_k.map_or(k, |m| k.min(m)).max(1)
}

#[cfg(test)]
mod tests;
