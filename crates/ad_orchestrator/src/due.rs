//! The times at which the leader has work, so it wakes at each time and does
//! not poll. Pure: the leader puts the times in, and takes the due work out.
//!
//! - A campaign is due when one of its waiting leads becomes due (a retry, a
//!   callback, or a hold). The leader then dispatches the campaign.
//! - An attempt is due when its wait for the agent's outcome ends. The leader
//!   then settles it without the outcome.
//!
//! Each campaign and each attempt keeps only its earliest time, so the size
//! stays at the number of campaigns plus the number of waiting attempts.

use std::collections::HashMap;

use ad_core::{AttemptId, CampaignId};
use chrono::{DateTime, Utc};

#[derive(Debug, Default)]
pub struct DueTimes {
    campaigns: HashMap<CampaignId, DateTime<Utc>>,
    attempts: HashMap<AttemptId, (CampaignId, DateTime<Utc>)>,
}

/// The work that is due now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Due {
    pub campaigns: Vec<CampaignId>,
    /// Each attempt with its campaign, which dispatches after the settlement.
    pub attempts: Vec<(AttemptId, CampaignId)>,
}

impl DueTimes {
    /// Dispatch the campaign at `at`, or at its earlier time.
    pub fn campaign_at(&mut self, campaign: CampaignId, at: DateTime<Utc>) {
        let t = self.campaigns.entry(campaign).or_insert(at);
        *t = (*t).min(at);
    }

    /// Settle the attempt at `at`, or at its earlier time.
    pub fn attempt_at(&mut self, attempt: AttemptId, campaign: CampaignId, at: DateTime<Utc>) {
        let t = self.attempts.entry(attempt).or_insert((campaign, at));
        t.1 = t.1.min(at);
    }

    /// The earliest time of all, or `None` when nothing waits.
    pub fn next(&self) -> Option<DateTime<Utc>> {
        let campaigns = self.campaigns.values().copied();
        let attempts = self.attempts.values().map(|(_, at)| *at);
        campaigns.chain(attempts).min()
    }

    /// Remove and return the work that is due at `now`.
    pub fn take_due(&mut self, now: DateTime<Utc>) -> Due {
        let mut due = Due::default();
        self.campaigns.retain(|c, at| {
            let keep = *at > now;
            if !keep {
                due.campaigns.push(*c);
            }
            keep
        });
        self.attempts.retain(|a, (c, at)| {
            let keep = *at > now;
            if !keep {
                due.attempts.push((*a, *c));
            }
            keep
        });
        due.campaigns.sort();
        due.attempts.sort();
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn t(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).unwrap() + Duration::seconds(s)
    }

    #[test]
    fn the_next_time_is_the_earliest_of_all() {
        let mut d = DueTimes::default();
        assert_eq!(d.next(), None);
        d.campaign_at(CampaignId(1), t(30));
        d.attempt_at(AttemptId(9), CampaignId(2), t(20));
        assert_eq!(d.next(), Some(t(20)));
        d.campaign_at(CampaignId(1), t(10));
        assert_eq!(d.next(), Some(t(10)));
    }

    #[test]
    fn each_campaign_and_attempt_keeps_its_earliest_time() {
        let mut d = DueTimes::default();
        d.campaign_at(CampaignId(1), t(10));
        d.campaign_at(CampaignId(1), t(50));
        d.attempt_at(AttemptId(9), CampaignId(2), t(40));
        d.attempt_at(AttemptId(9), CampaignId(2), t(60));
        assert_eq!(d.next(), Some(t(10)));
        assert_eq!(
            d.take_due(t(10)),
            Due {
                campaigns: vec![CampaignId(1)],
                attempts: vec![],
            }
        );
        assert_eq!(
            d.next(),
            Some(t(40)),
            "the later time of campaign 1 is gone"
        );
    }

    #[test]
    fn only_the_work_that_is_due_comes_out() {
        let mut d = DueTimes::default();
        d.campaign_at(CampaignId(2), t(5));
        d.campaign_at(CampaignId(1), t(5));
        d.campaign_at(CampaignId(3), t(6));
        d.attempt_at(AttemptId(8), CampaignId(3), t(5));
        d.attempt_at(AttemptId(7), CampaignId(3), t(7));
        assert_eq!(d.take_due(t(4)), Due::default());
        assert_eq!(
            d.take_due(t(5)),
            Due {
                campaigns: vec![CampaignId(1), CampaignId(2)],
                attempts: vec![(AttemptId(8), CampaignId(3))],
            }
        );
        assert_eq!(d.take_due(t(5)), Due::default(), "the work comes out once");
        assert_eq!(d.next(), Some(t(6)));
    }
}
