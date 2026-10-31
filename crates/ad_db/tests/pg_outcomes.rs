//! Integration tests for the agent session, the outcome record, the DNC list,
//! and the catalog on Postgres. Gated on DATABASE_URL.

mod common;

use std::collections::BTreeMap;

use ad_core::{AttemptId, CampaignId};
use ad_db::{Db, LeadUpdate, NewDnc, OutcomeError, Owner, Recorded};
use ad_disposition::{
    check, AgentOutcome, CallbackRequest, Catalog, Disposition, DncScope, Flags, Report,
    SCHEMA_VERSION,
};
use ad_lead::repo::{LeadRepo, NewLead};
use ad_lead::{Lead, LeadStatus};
use chrono::{Duration, Utc};
use common::{claim, complete, running_campaign};

/// A number of its own for each campaign, so that a system DNC entry of one
/// test stops no lead of another test.
fn number(c: CampaignId, n: u32) -> String {
    format!("+1202{:05}{n:02}", c.0 % 100_000)
}

async fn lead_with(db: &Db, c: CampaignId, phone: &str) -> Lead {
    db.leads()
        .create(NewLead {
            campaign_id: c,
            phone_number: phone.into(),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            context: BTreeMap::new(),
        })
        .await
        .unwrap()
}

/// A live attempt of a lead with `phone`, owned by node `n1`, with a session.
async fn attempt_with_session(db: &Db, c: CampaignId, phone: &str) -> (AttemptId, Owner) {
    let lead = lead_with(db, c, phone).await;
    let a = claim(db, c, 10)
        .await
        .into_iter()
        .find(|a| a.lead.id == lead.id)
        .unwrap();
    let owner = db
        .attempts()
        .assign(a.attempt_id, "n1")
        .await
        .unwrap()
        .unwrap();
    assert!(db
        .outcomes()
        .set_session(a.attempt_id, &owner, b"digest-1")
        .await
        .unwrap());
    (a.attempt_id, owner)
}

fn outcome(code: Option<&str>, f: impl FnOnce(&mut Report)) -> AgentOutcome {
    let mut r = Report {
        schema_version: SCHEMA_VERSION,
        disposition: code.map(Into::into),
        ..Report::default()
    };
    f(&mut r);
    check(
        &r,
        &Catalog::default(),
        Some("America/New_York"),
        Utc::now(),
    )
    .unwrap()
}

async fn status(db: &Db, lead: &Lead) -> LeadStatus {
    db.leads().get(lead.id).await.unwrap().status
}

#[tokio::test]
async fn the_session_needs_the_token_and_the_owner() {
    let Some(db) = common::db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let c = running_campaign(&db, 10).await;
    let (a, owner) = attempt_with_session(&db, c, &number(c, 1)).await;
    let s = db.outcomes().session(a, b"digest-1").await.unwrap();
    assert_eq!((s.campaign_id, s.live, s.settled), (c, true, false));
    assert_eq!(s.lead_timezone.as_deref(), Some("America/New_York"));
    assert!(matches!(
        db.outcomes().session(a, b"digest-2").await,
        Err(OutcomeError::Session)
    ));
    // A node that lost the call cannot set the session.
    let stale = Owner {
        node: owner.node.clone(),
        gen: owner.gen - 1,
    };
    assert!(!db.outcomes().set_session(a, &stale, b"x").await.unwrap());
    let _ = db.attempts().assign(a, "n2").await.unwrap();
    assert!(!db.outcomes().set_session(a, &owner, b"x").await.unwrap());
}

#[tokio::test]
async fn the_first_outcome_stays() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    let (a, _) = attempt_with_session(&db, c, &number(c, 1)).await;
    let s = db.outcomes().session(a, b"digest-1").await.unwrap();
    let sale = outcome(Some("SALE"), |r| r.summary = Some("bought".into()));
    assert_eq!(
        db.outcomes().record(&s, b"digest-1", &sale).await.unwrap(),
        Recorded::New {
            cancelled: 0,
            settled: false
        }
    );
    assert_eq!(
        db.outcomes().record(&s, b"digest-1", &sale).await.unwrap(),
        Recorded::Duplicate
    );
    assert!(matches!(
        db.outcomes()
            .record(&s, b"digest-1", &outcome(Some("NI"), |_| {}))
            .await,
        Err(OutcomeError::Conflict)
    ));
    assert!(matches!(
        db.outcomes().record(&s, b"wrong", &sale).await,
        Err(OutcomeError::Session)
    ));
    let view = db.attempts().page(c, a.0 - 1, 1).await.unwrap().remove(0);
    assert_eq!(view.disposition.as_deref(), Some("SALE"));
    assert!(view.outcome_at.is_some());
    let events = db.events().list(c.0, 100).await.unwrap();
    assert!(events.iter().any(|(_, e)| matches!(
        e,
        ad_events::CampaignEvent::AgentOutcome { attempt_id, disposition: Some(d), dnc: false, .. }
            if *attempt_id == a.0 && d == "SALE"
    )));
}

#[tokio::test]
async fn a_dnc_request_stops_the_waiting_leads_of_the_number_at_once() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    let other = running_campaign(&db, 10).await;
    let phone = number(c, 1);
    let (a, _) = attempt_with_session(&db, c, &phone).await;
    // The same number waits in another campaign, also for a callback.
    let waiting = lead_with(&db, other, &phone).await;
    let callback = lead_with(&db, other, &phone).await;
    sqlx::query(
        "UPDATE leads SET status = 'queued', next_eligible_at = now() + interval '1 day', \
             retry_reason = 'callback', priority = 50 WHERE id = $1",
    )
    .bind(callback.id.0)
    .execute(db.pool())
    .await
    .unwrap();
    let done = lead_with(&db, other, &phone).await;
    sqlx::query("UPDATE leads SET status = 'dispositioned' WHERE id = $1")
        .bind(done.id.0)
        .execute(db.pool())
        .await
        .unwrap();

    let s = db.outcomes().session(a, b"digest-1").await.unwrap();
    let dnc = outcome(Some("DNC"), |r| r.summary = Some("asked to stop".into()));
    assert_eq!(
        db.outcomes().record(&s, b"digest-1", &dnc).await.unwrap(),
        Recorded::New {
            cancelled: 2,
            settled: false
        }
    );
    assert_eq!(status(&db, &waiting).await, LeadStatus::Dnc);
    let cb = db.leads().get(callback.id).await.unwrap();
    assert_eq!(cb.status, LeadStatus::Dnc);
    assert_eq!(status(&db, &done).await, LeadStatus::Dispositioned);

    let entries = db.dnc().list(Some(c), 1000).await.unwrap();
    let e = entries.iter().find(|e| e.phone_number == phone).unwrap();
    assert_eq!((e.scope.as_str(), e.source.as_str()), ("system", "agent"));
    assert_eq!(e.attempt_id, Some(a.0));
    assert_eq!(e.evidence["summary"], "asked to stop");
    // The gate of any campaign now denies the number.
    let key = ad_contact::phone_key(&phone).unwrap();
    let set = db
        .dnc()
        .set_for(other, std::slice::from_ref(&key))
        .await
        .unwrap();
    assert!(set.system.contains(&key));
}

#[tokio::test]
async fn a_campaign_entry_stops_only_its_campaign() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    let other = running_campaign(&db, 10).await;
    let phone = number(c, 2);
    let here = lead_with(&db, c, &phone).await;
    let there = lead_with(&db, other, &phone).await;
    let key = ad_contact::phone_key(&phone).unwrap();
    let new = NewDnc {
        phone_number: key.clone(),
        scope: DncScope::Campaign,
        campaign_id: Some(c),
        source: "api",
        attempt_id: None,
        evidence: serde_json::json!({"note": "letter"}),
    };
    let added = db.dnc().add(&new).await.unwrap();
    assert!(added.created);
    assert_eq!(added.cancelled, 1);
    assert!(
        !db.dnc().add(&new).await.unwrap().created,
        "one entry per scope"
    );
    assert_eq!(status(&db, &here).await, LeadStatus::Dnc);
    assert_eq!(status(&db, &there).await, LeadStatus::New);
    assert!(db
        .dnc()
        .set_for(c, std::slice::from_ref(&key))
        .await
        .unwrap()
        .campaign
        .contains(&key));
    assert!(db
        .dnc()
        .set_for(other, std::slice::from_ref(&key))
        .await
        .unwrap()
        .campaign
        .is_empty());
    // A scope without its campaign is refused.
    let wrong = NewDnc {
        campaign_id: None,
        ..new.clone()
    };
    assert!(db.dnc().add(&wrong).await.is_err());
    // Other tests add system entries at the same time: take this entry.
    let id = db
        .dnc()
        .list(Some(c), 1000)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.campaign_id == Some(c.0) && e.phone_number == key.as_str())
        .unwrap()
        .id;
    assert!(db.dnc().remove(id).await.unwrap());
    assert!(db
        .dnc()
        .set_for(c, &[key])
        .await
        .unwrap()
        .campaign
        .is_empty());
}

#[tokio::test]
async fn settlement_can_wait_and_reads_the_outcome() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 10).await;
    let (a, _) = attempt_with_session(&db, c, &number(c, 3)).await;
    complete(&db, a, Utc::now(), 0).await;

    // No outcome yet: the rule waits, and nothing changes.
    let waited = db
        .attempts()
        .settle_or_wait(a, |input| {
            assert!(input.session && input.agent_outcome.is_none());
            None
        })
        .await
        .unwrap();
    assert_eq!(waited, None);
    assert!(db.attempts().unsettled(1000).await.unwrap().contains(&a));

    let s = db.outcomes().session(a, b"digest-1").await.unwrap();
    let at = (Utc::now() + Duration::days(1))
        .with_timezone(&chrono_tz::America::New_York)
        .naive_local()
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let cb = outcome(Some("CALLBK"), |r| {
        r.callback = Some(CallbackRequest { at, zone: None })
    });
    let due = cb.callback.as_ref().unwrap().at_utc;
    db.outcomes().record(&s, b"digest-1", &cb).await.unwrap();
    let update = db
        .attempts()
        .settle_or_wait(a, |input| {
            let o = input.agent_outcome.unwrap();
            assert_eq!(o.callback.as_ref().unwrap().at_utc, due);
            Some((
                LeadUpdate {
                    status: LeadStatus::Queued,
                    counts_as_call: true,
                    next_eligible_at: Some(due),
                    wait_reason: Some("callback"),
                    priority: 50,
                },
                Vec::new(),
            ))
        })
        .await
        .unwrap();
    assert!(update.is_some());
    let lead: (i16, String) = sqlx::query_as(
        "SELECT l.priority, l.retry_reason FROM leads l JOIN call_attempts a ON a.lead_id = l.id \
         WHERE a.id = $1",
    )
    .bind(a.0)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(lead, (50, "callback".to_string()));
}

#[tokio::test]
async fn the_claim_takes_a_due_callback_first() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 1).await;
    let first = lead_with(&db, c, &number(c, 4)).await;
    let callback = lead_with(&db, c, &number(c, 5)).await;
    sqlx::query(
        "UPDATE leads SET status = 'queued', called_count = 1, next_eligible_at = now() - \
             interval '1 second', retry_reason = 'callback', priority = 50 WHERE id = $1",
    )
    .bind(callback.id.0)
    .execute(db.pool())
    .await
    .unwrap();
    let claimed = claim(&db, c, 1).await;
    assert_eq!(claimed[0].lead.id, callback.id, "not {:?}", first.id);
}

#[tokio::test]
async fn the_catalog_of_a_campaign_is_stored() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 1).await;
    assert!(db.outcomes().catalog(c).await.unwrap().is_empty());
    let codes = vec![Disposition {
        code: "APPT".into(),
        name: "Appointment".into(),
        flags: Flags {
            human_answered: true,
            sale: true,
            ..Flags::default()
        },
    }];
    assert!(db.outcomes().put_catalog(c, &codes).await.unwrap());
    assert_eq!(db.outcomes().catalog(c).await.unwrap(), codes);
    assert!(!db
        .outcomes()
        .put_catalog(CampaignId(i64::MAX), &codes)
        .await
        .unwrap());
    let (a, _) = attempt_with_session(&db, c, &number(c, 6)).await;
    let s = db.outcomes().session(a, b"digest-1").await.unwrap();
    assert_eq!(s.dispositions, codes);
}
