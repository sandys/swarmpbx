//! Lead selection — the hopper (PLAN.md §0.10.4). The hopper picks which leads
//! to dial next. It is pure over supplied data: given a campaign's leads, the
//! current time (a `Clock`), the safe-hours windows + DNC data, and an optional
//! routing ruleset, it returns the ordered eligible leads and the excluded ones
//! with a reason.
//!
//! This is why `ad_routing` and `ad_hopper` are one slice: the hopper runs the
//! routing engine per lead to let operators express custom exclusion/skip
//! rules, on top of the hard compliance gate.

use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::{self, CallWindowSet, LeadLocation};
use ad_compliance::{Decision, DenyReason};
use ad_core::time::Clock;
use ad_lead::{Lead, LeadStatus};
use ad_routing::{evaluate, Context, Ruleset};

/// Why a lead was excluded from the hopper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExcludeReason {
    /// Not an eligible status (e.g. Done, Dnc, InCall).
    IneligibleStatus,
    /// Exceeded the per-lead call-count cap.
    CallCountExceeded,
    /// Compliance denied the call (safe-hours / DNC / etc.).
    Compliance(DenyReason),
    /// A routing rule emitted a `skip`/`hangup`/`add_dnc` action for this lead.
    RoutingExcluded(String),
}

/// Lead-ordering strategy for the hopper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// Lowest call-count first (fresh leads before retried ones), then id.
    #[default]
    UpCount,
    /// By id ascending (load order).
    Oldest,
}

/// Everything the hopper needs to decide, supplied by the caller (loaded from
/// the DB elsewhere — the engine stays pure).
pub struct SelectionInput<'a> {
    pub leads: Vec<Lead>,
    pub windows: &'a CallWindowSet,
    pub dnc: &'a DncSet,
    pub call_count_limit: Option<u32>,
    pub order: Order,
    /// Optional per-lead routing ruleset (exclusion/decoration). If a rule
    /// emits a `skip`, `hangup`, or `add_dnc` action, the lead is excluded.
    pub ruleset: Option<&'a Ruleset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub lead: Lead,
    pub reason: ExcludeReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    /// Eligible leads, in dial order.
    pub eligible: Vec<Lead>,
    /// Excluded leads with the reason (useful for the preview API and reports).
    pub excluded: Vec<Excluded>,
}

/// Statuses the hopper will consider for dialing.
fn is_dialable_status(s: LeadStatus) -> bool {
    matches!(s, LeadStatus::New | LeadStatus::Queued)
}

/// Build the routing context from a lead so rules can test its fields.
fn lead_context(lead: &Lead) -> Context {
    let mut c = Context::new();
    c.set("phone_number", lead.phone_number.clone());
    c.set("status", lead.status.as_str());
    c.set("called_count", lead.called_count.to_string());
    if let Some(tz) = &lead.timezone {
        c.set("timezone", tz.clone());
    }
    if let Some(st) = &lead.state {
        c.set("state", st.clone());
    }
    if let Some(pc) = &lead.postal_code {
        c.set("postal_code", pc.clone());
    }
    for (k, v) in &lead.context {
        c.set(k.clone(), v.clone());
    }
    c
}

/// Run the hopper. Pure except for reading the injected clock.
pub fn select(clock: &dyn Clock, input: SelectionInput<'_>) -> Selection {
    let mut selection = Selection::default();

    for lead in input.leads {
        // 1) Status gate.
        if !is_dialable_status(lead.status) {
            selection.excluded.push(Excluded {
                lead,
                reason: ExcludeReason::IneligibleStatus,
            });
            continue;
        }

        // 2) Call-count cap.
        if let Some(limit) = input.call_count_limit {
            if lead.called_count >= limit {
                selection.excluded.push(Excluded {
                    lead,
                    reason: ExcludeReason::CallCountExceeded,
                });
                continue;
            }
        }

        // 3) DNC scrub (compliance, fail-closed on empty number).
        if let Decision::Denied(r) = input.dnc.check(&lead.phone_number) {
            selection.excluded.push(Excluded {
                lead,
                reason: ExcludeReason::Compliance(r),
            });
            continue;
        }

        // 4) Safe-hours gate (compliance, fail-closed on unknown tz).
        let loc = LeadLocation {
            timezone: lead.timezone.clone(),
        };
        if let Decision::Denied(r) = safe_hours::check(clock, &loc, input.windows) {
            selection.excluded.push(Excluded {
                lead,
                reason: ExcludeReason::Compliance(r),
            });
            continue;
        }

        // 5) Optional routing ruleset: operator-defined exclusion.
        if let Some(rs) = input.ruleset {
            let actions = evaluate(rs, &lead_context(&lead));
            if let Some(a) = actions
                .iter()
                .find(|a| matches!(a.verb.as_str(), "skip" | "hangup" | "add_dnc"))
            {
                selection.excluded.push(Excluded {
                    lead,
                    reason: ExcludeReason::RoutingExcluded(a.verb.clone()),
                });
                continue;
            }
        }

        selection.eligible.push(lead);
    }

    // 6) Order the eligible leads.
    match input.order {
        Order::UpCount => selection.eligible.sort_by(|a, b| {
            a.called_count
                .cmp(&b.called_count)
                .then(a.id.0.cmp(&b.id.0))
        }),
        Order::Oldest => selection.eligible.sort_by_key(|l| l.id.0),
    }

    selection
}

#[cfg(test)]
mod tests;
