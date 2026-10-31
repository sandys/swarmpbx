use chrono::{NaiveDate, TimeZone};

use super::*;
use crate::catalog::{CatalogError, Flags};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 15, 0, 0).unwrap()
}

fn report(disposition: &str) -> Report {
    Report {
        schema_version: SCHEMA_VERSION,
        disposition: Some(disposition.into()),
        ..Report::default()
    }
}

fn callback_at(at: &str, zone: Option<&str>) -> Option<CallbackRequest> {
    Some(CallbackRequest {
        at: at.into(),
        zone: zone.map(Into::into),
    })
}

fn ny() -> Option<&'static str> {
    Some("America/New_York")
}

#[test]
fn a_system_code_passes_with_its_flags() {
    let o = check(&report("SALE"), &Catalog::default(), ny(), now()).unwrap();
    let d = o.disposition.unwrap();
    assert_eq!(d.code, "SALE");
    assert!(d.flags.sale && d.flags.human_answered && d.flags.completed);
    assert_eq!((o.dnc, o.callback), (None, None));
}

#[test]
fn an_unknown_code_or_version_is_rejected() {
    assert_eq!(
        check(&report("XYZ"), &Catalog::default(), ny(), now()),
        Err(Rejection::UnknownDisposition("XYZ".into()))
    );
    let old = Report {
        schema_version: 2,
        ..report("SALE")
    };
    assert_eq!(
        check(&old, &Catalog::default(), ny(), now()),
        Err(Rejection::Version(2))
    );
}

#[test]
fn a_campaign_code_adds_to_the_system_codes() {
    let catalog = Catalog::new(vec![Disposition {
        code: "APPT".into(),
        name: "Appointment set".into(),
        flags: Flags {
            human_answered: true,
            sale: true,
            completed: true,
            ..Flags::default()
        },
    }])
    .unwrap();
    assert!(check(&report("APPT"), &catalog, ny(), now()).is_ok());
    assert!(check(&report("SALE"), &catalog, ny(), now()).is_ok());
    assert!(check(&report("APPT"), &Catalog::default(), ny(), now()).is_err());
}

#[test]
fn the_catalog_refuses_bad_codes_and_conflicting_flags() {
    let one = |code: &str, flags: Flags| {
        Catalog::new(vec![Disposition {
            code: code.into(),
            name: "x".into(),
            flags,
        }])
    };
    assert_eq!(
        one("SALE", Flags::default()),
        Err(CatalogError::System("SALE".into()))
    );
    assert!(matches!(
        one("sale", Flags::default()),
        Err(CatalogError::Code(_))
    ));
    assert!(matches!(
        one("", Flags::default()),
        Err(CatalogError::Code(_))
    ));
    let both = Flags {
        dnc: true,
        scheduled_callback: true,
        ..Flags::default()
    };
    assert!(matches!(one("X", both), Err(CatalogError::Flags { .. })));
    let machine_sale = Flags {
        answering_machine: true,
        sale: true,
        ..Flags::default()
    };
    assert!(matches!(
        one("X", machine_sale),
        Err(CatalogError::Flags { .. })
    ));
    let twice = vec![
        Disposition {
            code: "A".into(),
            name: "a".into(),
            flags: Flags::default(),
        };
        2
    ];
    assert_eq!(
        Catalog::new(twice),
        Err(CatalogError::Duplicate("A".into()))
    );
}

#[test]
fn the_dnc_flag_asks_for_the_system_list() {
    let o = check(&report("DNC"), &Catalog::default(), ny(), now()).unwrap();
    assert_eq!(o.dnc, Some(DncScope::System));
    let campaign = Report {
        dnc: Some(Some("campaign".into())),
        ..report("NI")
    };
    let o = check(&campaign, &Catalog::default(), ny(), now()).unwrap();
    assert_eq!(o.dnc, Some(DncScope::Campaign));
    let bare = Report {
        disposition: None,
        dnc: Some(None),
        ..report("")
    };
    let o = check(&bare, &Catalog::default(), ny(), now()).unwrap();
    assert_eq!((o.dnc, o.disposition), (Some(DncScope::System), None));
    let wrong = Report {
        dnc: Some(Some("org".into())),
        ..report("NI")
    };
    assert_eq!(
        check(&wrong, &Catalog::default(), ny(), now()),
        Err(Rejection::DncScope("org".into()))
    );
}

#[test]
fn a_callback_takes_the_zone_of_the_lead() {
    let r = Report {
        callback: callback_at("2026-03-02T09:30", None),
        ..report("CALLBK")
    };
    let o = check(&r, &Catalog::default(), ny(), now()).unwrap();
    let cb = o.callback.unwrap();
    // 09:30 EST is 14:30 UTC.
    assert_eq!(
        cb.at_utc,
        Utc.with_ymd_and_hms(2026, 3, 2, 14, 30, 0).unwrap()
    );
    assert_eq!(cb.zone, "America/New_York");

    let la = Report {
        callback: callback_at("2026-03-02T09:30:00", Some("America/Los_Angeles")),
        ..report("CALLBK")
    };
    let cb = check(&la, &Catalog::default(), ny(), now())
        .unwrap()
        .callback
        .unwrap();
    assert_eq!(
        cb.at_utc,
        Utc.with_ymd_and_hms(2026, 3, 2, 17, 30, 0).unwrap()
    );
}

#[test]
fn callback_rules() {
    let cat = Catalog::default();
    let cb = |at: &str| Report {
        callback: callback_at(at, None),
        ..report("CALLBK")
    };
    assert_eq!(
        check(&report("CALLBK"), &cat, ny(), now()),
        Err(Rejection::CallbackMissing("CALLBK".into()))
    );
    assert_eq!(
        check(
            &Report {
                callback: callback_at("2026-03-02T09:30", None),
                ..report("SALE")
            },
            &cat,
            ny(),
            now()
        ),
        Err(Rejection::CallbackNotAllowed("SALE".into()))
    );
    assert_eq!(
        check(&cb("2026-03-02T09:30"), &cat, None, now()),
        Err(Rejection::CallbackNoZone)
    );
    assert_eq!(
        check(&cb("tomorrow"), &cat, ny(), now()),
        Err(Rejection::CallbackTime("tomorrow".into()))
    );
    // In the past, and more than 90 days ahead.
    assert_eq!(
        check(&cb("2026-02-28T09:30"), &cat, ny(), now()),
        Err(Rejection::CallbackRange)
    );
    assert_eq!(
        check(&cb("2026-06-30T09:30"), &cat, ny(), now()),
        Err(Rejection::CallbackRange)
    );
    let zone = Report {
        callback: callback_at("2026-03-02T09:30", Some("Mars/Base")),
        ..report("CALLBK")
    };
    assert_eq!(
        check(&zone, &cat, ny(), now()),
        Err(Rejection::CallbackZone("Mars/Base".into()))
    );
    let both = Report {
        callback: callback_at("2026-03-02T09:30", None),
        dnc: Some(None),
        ..report("CALLBK")
    };
    assert_eq!(
        check(&both, &cat, ny(), now()),
        Err(Rejection::CallbackAndDnc)
    );
}

#[test]
fn a_callback_in_the_spring_gap_moves_to_the_end_of_the_gap() {
    // 2026-03-08: New York goes from 02:00 EST to 03:00 EDT.
    let local = NaiveDate::from_ymd_opt(2026, 3, 8)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    let at = local_to_utc(local, chrono_tz::America::New_York).unwrap();
    // 03:00 EDT is 07:00 UTC.
    assert_eq!(at, Utc.with_ymd_and_hms(2026, 3, 8, 7, 0, 0).unwrap());
}

#[test]
fn a_callback_in_the_autumn_overlap_takes_the_earlier_time() {
    // 2026-11-01: 01:30 occurs in EDT (05:30 UTC) and in EST (06:30 UTC).
    let local = NaiveDate::from_ymd_opt(2026, 11, 1)
        .unwrap()
        .and_hms_opt(1, 30, 0)
        .unwrap();
    let at = local_to_utc(local, chrono_tz::America::New_York).unwrap();
    assert_eq!(at, Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap());
}

#[test]
fn a_machine_answer_comes_from_the_flag_or_the_answer_class() {
    let am = check(&report("AM"), &Catalog::default(), ny(), now()).unwrap();
    assert!(am.machine());
    let bare = Report {
        disposition: None,
        answered_by: AnsweredBy::Machine,
        ..report("")
    };
    assert!(check(&bare, &Catalog::default(), ny(), now())
        .unwrap()
        .machine());
    // The flag of the code wins over the answer class.
    let sale = Report {
        answered_by: AnsweredBy::Machine,
        ..report("SALE")
    };
    assert!(!check(&sale, &Catalog::default(), ny(), now())
        .unwrap()
        .machine());
}

#[test]
fn texts_are_trimmed_and_limited() {
    let r = Report {
        summary: Some("  bought the plan  ".into()),
        transfer_to: Some("   ".into()),
        disclosures: vec!["recording_notice".into()],
        ..report("SALE")
    };
    let o = check(&r, &Catalog::default(), ny(), now()).unwrap();
    assert_eq!(o.summary.as_deref(), Some("bought the plan"));
    assert_eq!(o.transfer_to, None);
    assert_eq!(o.disclosures, vec!["recording_notice".to_string()]);
    let long = Report {
        summary: Some("x".repeat(4001)),
        ..report("SALE")
    };
    assert_eq!(
        check(&long, &Catalog::default(), ny(), now()),
        Err(Rejection::TooLong("summary"))
    );
}
