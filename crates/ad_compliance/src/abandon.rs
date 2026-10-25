//! Abandonment-rate accounting.
//!
//! The regulator's definition is NOT failed/dialed. It is:
//!   abandon_rate = answered_by_human_but_no_agent / answered_by_human
//! measured over a rolling window per campaign. Jurisdictions differ in the
//! window and the cap, so rules live behind a registry (add a jurisdiction =
//! one variant + one match arm; no schema change).

use chrono::{DateTime, Duration, Utc};

/// One recorded answered-by-human event and whether it was abandoned (no agent
/// available to take it). Calls NOT answered by a human are not ledger events —
/// they are not part of the denominator.
#[derive(Debug, Clone, Copy)]
pub struct AnswerEvent {
    pub at: DateTime<Utc>,
    pub abandoned: bool,
}

/// A time-ordered ledger of answered-by-human events for one campaign.
#[derive(Debug, Default, Clone)]
pub struct AbandonLedger {
    events: Vec<AnswerEvent>,
}

impl AbandonLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, at: DateTime<Utc>, abandoned: bool) {
        self.events.push(AnswerEvent { at, abandoned });
    }

    /// (abandoned, answered_by_human) counts within [now-window, now].
    fn counts_in_window(&self, now: DateTime<Utc>, window: Duration) -> (u64, u64) {
        let cutoff = now - window;
        let mut abandoned = 0u64;
        let mut total = 0u64;
        for e in &self.events {
            if e.at >= cutoff && e.at <= now {
                total += 1;
                if e.abandoned {
                    abandoned += 1;
                }
            }
        }
        (abandoned, total)
    }

    /// The regulator rate in [0.0, 1.0] over the rule's window. Returns None if
    /// there are no answered-by-human calls in the window (rate undefined).
    pub fn rate(&self, now: DateTime<Utc>, rule: &JurisdictionRule) -> Option<f64> {
        let (abandoned, total) = self.counts_in_window(now, rule.window());
        if total == 0 {
            None
        } else {
            Some(abandoned as f64 / total as f64)
        }
    }

    /// Whether the campaign is within the jurisdiction cap. An undefined rate
    /// (no calls yet) is treated as compliant.
    pub fn is_compliant(&self, now: DateTime<Utc>, rule: &JurisdictionRule) -> bool {
        match self.rate(now, rule) {
            None => true,
            Some(r) => r <= rule.cap(),
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// The answered and abandoned calls of one campaign in a rule's window, as the
/// store counts them (`call_attempts`). The store treats every answer as a
/// human answer: there is no answering-machine detection yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AbandonCounts {
    pub answered: u64,
    pub abandoned: u64,
}

impl AbandonCounts {
    /// The regulator rate, or `None` with no answered call (rate undefined).
    pub fn rate(&self) -> Option<f64> {
        (self.answered > 0).then(|| self.abandoned as f64 / self.answered as f64)
    }
}

/// Per-jurisdiction abandonment rule. Registry: add a variant + arms below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JurisdictionRule {
    /// US FCC/FTC Telemarketing Sales Rule: 3% over a rolling 30 days / campaign.
    UsTsr,
    /// UK Ofcom: <3% over a rolling 24h / campaign. (experimental — verify)
    UkOfcom,
    /// India TRAI: network-drop penalties; Feb-2025 amendments. (experimental —
    /// MUST be verified against primary TRAI sources before production use.)
    IndiaTrai,
}

impl JurisdictionRule {
    /// The rule of every campaign until a campaign names its jurisdiction
    /// (GAPS.md COMP-10).
    pub const CAMPAIGN: JurisdictionRule = JurisdictionRule::UsTsr;

    /// The stored and reported name.
    pub fn name(&self) -> &'static str {
        match self {
            JurisdictionRule::UsTsr => "us_tsr",
            JurisdictionRule::UkOfcom => "uk_ofcom",
            JurisdictionRule::IndiaTrai => "india_trai",
        }
    }

    /// The longest time from the answer to an agent on the call. A later
    /// connection abandons the call. US TSR (16 CFR 310.4(b)(4)(i)) and
    /// Ofcom: two seconds after the called person's greeting. India TRAI is
    /// not verified (experimental) and uses the same two seconds. We measure
    /// from the answer, which is earlier, so the limit is never longer than
    /// the rule's.
    pub fn connect_within(&self) -> Duration {
        match self {
            JurisdictionRule::UsTsr | JurisdictionRule::UkOfcom | JurisdictionRule::IndiaTrai => {
                Duration::seconds(2)
            }
        }
    }

    /// `connect_within` in milliseconds, or `None` when it does not fit.
    pub fn connect_within_ms(&self) -> Option<u32> {
        u32::try_from(self.connect_within().num_milliseconds()).ok()
    }

    /// Whether the next calls may risk abandonment (the dialer starts the
    /// agent on the answer): the rate in the window is at or under the cap.
    /// An undefined rate (no answered call yet) allows it, as `is_compliant`
    /// does. Above the cap, the dialer starts the agent before the dial,
    /// which cannot abandon a call.
    pub fn allows_customer_first(&self, counts: &AbandonCounts) -> bool {
        match counts.rate() {
            None => true,
            Some(r) => r <= self.cap(),
        }
    }

    /// Maximum permitted abandonment rate (fraction).
    pub fn cap(&self) -> f64 {
        match self {
            JurisdictionRule::UsTsr => 0.03,
            JurisdictionRule::UkOfcom => 0.03,
            JurisdictionRule::IndiaTrai => 0.02,
        }
    }

    /// The rolling measurement window.
    pub fn window(&self) -> Duration {
        match self {
            JurisdictionRule::UsTsr => Duration::days(30),
            JurisdictionRule::UkOfcom => Duration::hours(24),
            JurisdictionRule::IndiaTrai => Duration::hours(24),
        }
    }

    /// Whether this rule is production-ready or still needs source verification.
    pub fn is_experimental(&self) -> bool {
        matches!(
            self,
            JurisdictionRule::UkOfcom | JurisdictionRule::IndiaTrai
        )
    }
}
