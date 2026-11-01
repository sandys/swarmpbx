//! The retry decision (PLAN.md §0.10.8). Pure. The orchestrator persists the
//! result on the lead (`next_eligible_at`) in the same transaction as the call
//! outcome, so a retry survives a restart and never depends on an event.

use ad_campaign::Retry;
use ad_events::RetryReason;
use chrono::{DateTime, Duration, Utc};

/// Retry timing policy. The interval depends on why the call failed, matching
/// VICIdial per-status retry rules. The waits and the call limit are the
/// campaign's settings (`ad_campaign::Retry`).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub busy: Duration,
    pub no_answer: Duration,
    pub voicemail: Duration,
    pub failed: Duration,
    pub error: Duration,
    pub abandoned: Duration,
    /// The wait before a lead that the calling window held is checked again.
    pub window_recheck: Duration,
    /// The wait before a lead whose agent did not join is tried again.
    pub agent_recheck: Duration,
    /// No retry once the lead has this many attempts.
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::of(&Retry::default())
    }
}

impl RetryPolicy {
    /// The policy of a campaign with these retry settings.
    pub fn of(retry: &Retry) -> Self {
        let s = |secs: u32| Duration::seconds(secs.into());
        RetryPolicy {
            busy: s(retry.busy_s),
            no_answer: s(retry.no_answer_s),
            voicemail: s(retry.voicemail_s),
            failed: s(retry.failed_s),
            error: s(retry.error_s),
            abandoned: s(retry.abandoned_s),
            window_recheck: Duration::minutes(15),
            agent_recheck: Duration::minutes(1),
            max_attempts: retry.max_calls,
        }
    }

    pub fn interval(&self, reason: RetryReason) -> Duration {
        match reason {
            RetryReason::Busy => self.busy,
            RetryReason::NoAnswer => self.no_answer,
            RetryReason::Voicemail => self.voicemail,
            RetryReason::Failed => self.failed,
            RetryReason::Error => self.error,
            RetryReason::Abandoned => self.abandoned,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Dial the lead again at this time.
    RetryAt(DateTime<Utc>),
    /// The attempt cap is reached; do not retry.
    Exhausted,
}

/// Decide whether a lead retries after an attempt that ended for `reason`.
/// `attempts_made` counts every attempt, including the one that just ended.
pub fn retry_decision(
    reason: RetryReason,
    attempts_made: u32,
    policy: &RetryPolicy,
    now: DateTime<Utc>,
) -> RetryDecision {
    if attempts_made >= policy.max_attempts {
        RetryDecision::Exhausted
    } else {
        RetryDecision::RetryAt(now + policy.interval(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn retry_is_scheduled_at_the_policy_interval() {
        let p = RetryPolicy::default();
        assert_eq!(
            retry_decision(RetryReason::NoAnswer, 1, &p, now()),
            RetryDecision::RetryAt(now() + Duration::minutes(30))
        );
        assert_eq!(
            retry_decision(RetryReason::Busy, 1, &p, now()),
            RetryDecision::RetryAt(now() + Duration::minutes(5))
        );
    }

    #[test]
    fn retry_reason_changes_the_interval() {
        let p = RetryPolicy::default();
        let busy = retry_decision(RetryReason::Busy, 1, &p, now());
        let vm = retry_decision(RetryReason::Voicemail, 1, &p, now());
        match (busy, vm) {
            (RetryDecision::RetryAt(b), RetryDecision::RetryAt(v)) => {
                assert!(v > b, "voicemail backs off longer than busy")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_campaign_settings_set_the_waits_and_the_cap() {
        let retry = Retry {
            busy_s: 7,
            no_answer_s: 11,
            max_calls: 2,
            ..Retry::default()
        };
        let p = RetryPolicy::of(&retry);
        assert_eq!(
            retry_decision(RetryReason::Busy, 1, &p, now()),
            RetryDecision::RetryAt(now() + Duration::seconds(7))
        );
        assert_eq!(
            retry_decision(RetryReason::NoAnswer, 1, &p, now()),
            RetryDecision::RetryAt(now() + Duration::seconds(11))
        );
        assert_eq!(
            retry_decision(RetryReason::Busy, 2, &p, now()),
            RetryDecision::Exhausted
        );
    }

    #[test]
    fn retry_stops_exactly_at_the_attempt_cap() {
        let p = RetryPolicy::default(); // max_attempts = 3
        assert!(matches!(
            retry_decision(RetryReason::Failed, 2, &p, now()),
            RetryDecision::RetryAt(_)
        ));
        assert_eq!(
            retry_decision(RetryReason::Failed, 3, &p, now()),
            RetryDecision::Exhausted
        );
        assert_eq!(
            retry_decision(RetryReason::Failed, 7, &p, now()),
            RetryDecision::Exhausted
        );
    }
}
