//! Hopper selection tests. These exercise the whole selection-through-routing
//! path: status gate, call-count cap, DNC, safe-hours, routing exclusion, and
//! ordering — the reason routing and the hopper are one slice.

use super::*;

use std::collections::BTreeMap;

use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::{self, CallWindowSet};
use ad_core::time::FixedClock;
use ad_core::{CampaignId, LeadId};
use ad_lead::{Lead, LeadStatus};
use ad_routing::{Detail, Ruleset};
use chrono::{DateTime, Utc};

fn now_utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    safe_hours::utc(y, mo, d, h, mi)
}

fn lead(id: i64, status: LeadStatus, called: u32, tz: Option<&str>, phone: &str) -> Lead {
    let t = now_utc(2026, 1, 1, 0, 0);
    Lead {
        id: LeadId(id),
        campaign_id: CampaignId(1),
        phone_number: phone.into(),
        timezone: tz.map(|s| s.into()),
        state: Some("NY".into()),
        postal_code: Some("10001".into()),
        status,
        called_count: called,
        last_local_call_time: None,
        context: BTreeMap::new(),
        created_at: t,
        updated_at: t,
    }
}

fn windows() -> CallWindowSet {
    CallWindowSet::uniform(900, 2100).unwrap() // 9am-9pm local
}

/// 15:00 UTC = 11:00 America/New_York (EDT) in summer — inside 9-21.
fn clock_inside() -> FixedClock {
    FixedClock(now_utc(2026, 6, 1, 15, 0))
}

#[test]
fn eligible_lead_inside_window_is_selected() {
    let input = SelectionInput {
        leads: vec![lead(
            1,
            LeadStatus::New,
            0,
            Some("America/New_York"),
            "+13125550100",
        )],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: None,
    };
    let sel = select(&clock_inside(), input);
    assert_eq!(sel.eligible.len(), 1);
    assert!(sel.excluded.is_empty());
}

#[test]
fn done_and_dnc_status_excluded_by_status_gate() {
    let input = SelectionInput {
        leads: vec![
            lead(1, LeadStatus::Done, 0, Some("America/New_York"), "+1a"),
            lead(2, LeadStatus::Dnc, 0, Some("America/New_York"), "+1b"),
        ],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: None,
    };
    let sel = select(&clock_inside(), input);
    assert!(sel.eligible.is_empty());
    assert_eq!(sel.excluded.len(), 2);
    assert!(sel
        .excluded
        .iter()
        .all(|e| e.reason == ExcludeReason::IneligibleStatus));
}

#[test]
fn call_count_cap_excludes() {
    let input = SelectionInput {
        leads: vec![lead(1, LeadStatus::New, 3, Some("America/New_York"), "+1a")],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: Some(3),
        order: Order::UpCount,
        ruleset: None,
    };
    let sel = select(&clock_inside(), input);
    assert_eq!(sel.excluded[0].reason, ExcludeReason::CallCountExceeded);
}

#[test]
fn unknown_timezone_excluded_fail_closed() {
    let input = SelectionInput {
        leads: vec![lead(1, LeadStatus::New, 0, None, "+1a")],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: None,
    };
    let sel = select(&clock_inside(), input);
    assert!(matches!(
        sel.excluded[0].reason,
        ExcludeReason::Compliance(ad_compliance::DenyReason::UnknownLocalTime)
    ));
}

#[test]
fn dnc_number_excluded() {
    let mut dnc = DncSet::default();
    dnc.system.insert("18005551234".into());
    let input = SelectionInput {
        leads: vec![lead(
            1,
            LeadStatus::New,
            0,
            Some("America/New_York"),
            "1-800-555-1234",
        )],
        windows: &windows(),
        dnc: &dnc,
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: None,
    };
    let sel = select(&clock_inside(), input);
    assert!(matches!(
        sel.excluded[0].reason,
        ExcludeReason::Compliance(_)
    ));
}

#[test]
fn routing_rule_excludes_a_lead() {
    // A ruleset that skips any lead in state TX.
    let rs = Ruleset::new(vec![
        Detail::condition("state", "^TX$", 0, 0),
        Detail::action("skip", "", 0, 10),
    ]);
    let mut tx_lead = lead(2, LeadStatus::New, 0, Some("America/New_York"), "+1tx");
    tx_lead.state = Some("TX".into());
    let input = SelectionInput {
        leads: vec![
            lead(1, LeadStatus::New, 0, Some("America/New_York"), "+1ny"), // NY -> eligible
            tx_lead,                                                       // TX -> skipped
        ],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: Some(&rs),
    };
    let sel = select(&clock_inside(), input);
    assert_eq!(sel.eligible.len(), 1);
    assert_eq!(sel.eligible[0].id.0, 1);
    assert_eq!(sel.excluded.len(), 1);
    assert_eq!(
        sel.excluded[0].reason,
        ExcludeReason::RoutingExcluded("skip".into())
    );
}

#[test]
fn up_count_ordering_fresh_first() {
    let input = SelectionInput {
        leads: vec![
            lead(1, LeadStatus::New, 2, Some("America/New_York"), "+1a"),
            lead(2, LeadStatus::New, 0, Some("America/New_York"), "+1b"),
            lead(3, LeadStatus::New, 1, Some("America/New_York"), "+1c"),
        ],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: None,
    };
    let sel = select(&clock_inside(), input);
    let ids: Vec<i64> = sel.eligible.iter().map(|l| l.id.0).collect();
    assert_eq!(ids, vec![2, 3, 1]); // called_count 0,1,2
}

#[test]
fn up_count_ties_break_by_id_ascending() {
    // All same call_count -> stable tie-break by id ascending.
    let input = SelectionInput {
        leads: vec![
            lead(3, LeadStatus::New, 5, Some("America/New_York"), "+1c"),
            lead(1, LeadStatus::New, 5, Some("America/New_York"), "+1a"),
            lead(2, LeadStatus::New, 5, Some("America/New_York"), "+1b"),
        ],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: None,
    };
    let ids: Vec<i64> = select(&clock_inside(), input)
        .eligible
        .iter()
        .map(|l| l.id.0)
        .collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn oldest_order_is_id_ascending_ignoring_call_count() {
    let input = SelectionInput {
        leads: vec![
            lead(2, LeadStatus::New, 0, Some("America/New_York"), "+1b"),
            lead(1, LeadStatus::New, 9, Some("America/New_York"), "+1a"),
        ],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::Oldest,
        ruleset: None,
    };
    let ids: Vec<i64> = select(&clock_inside(), input)
        .eligible
        .iter()
        .map(|l| l.id.0)
        .collect();
    assert_eq!(ids, vec![1, 2]); // by id, not by call_count
}

#[test]
fn routing_rule_that_does_not_match_leaves_lead_eligible() {
    // Rule only skips TX; an NY lead passes through eligible.
    let rs = Ruleset::new(vec![
        Detail::condition("state", "^TX$", 0, 0),
        Detail::action("skip", "", 0, 10),
    ]);
    let input = SelectionInput {
        leads: vec![lead(
            1,
            LeadStatus::New,
            0,
            Some("America/New_York"),
            "+1ny",
        )],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: Some(&rs),
    };
    let sel = select(&clock_inside(), input);
    assert_eq!(sel.eligible.len(), 1);
    assert!(sel.excluded.is_empty());
}

#[test]
fn every_lead_appears_exactly_once_across_eligible_and_excluded() {
    let mut tx_lead = lead(4, LeadStatus::New, 0, Some("America/New_York"), "+1tx");
    tx_lead.state = Some("TX".into());
    let rs = Ruleset::new(vec![
        Detail::condition("state", "^TX$", 0, 0),
        Detail::action("skip", "", 0, 10),
    ]);
    let input = SelectionInput {
        leads: vec![
            lead(1, LeadStatus::New, 0, Some("America/New_York"), "+1ok"), // eligible
            lead(2, LeadStatus::Done, 0, Some("America/New_York"), "+1done"), // status
            lead(3, LeadStatus::New, 0, None, "+1notz"),                   // compliance
            tx_lead,                                                       // routing
        ],
        windows: &windows(),
        dnc: &DncSet::default(),
        call_count_limit: None,
        order: Order::UpCount,
        ruleset: Some(&rs),
    };
    let sel = select(&clock_inside(), input);
    assert_eq!(sel.eligible.len(), 1);
    assert_eq!(sel.excluded.len(), 3);
    // No lead lost or duplicated: 1 + 3 == 4 inputs.
    let total = sel.eligible.len() + sel.excluded.len();
    assert_eq!(total, 4);
}
