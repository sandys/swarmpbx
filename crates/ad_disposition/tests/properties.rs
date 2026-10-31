//! Invariants of the outcome check and the callback time rule.

use ad_disposition::{
    check, local_to_utc, CallbackRequest, Catalog, DncScope, Report, SCHEMA_VERSION,
};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use proptest::prelude::*;

const ZONES: &[&str] = &[
    "America/New_York",
    "America/Los_Angeles",
    "Europe/London",
    "Australia/Lord_Howe",
    "Asia/Kolkata",
];

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 15, 0, 0).unwrap()
}

proptest! {
    // INVARIANT: every local time has a UTC time, and the UTC time is never
    // earlier than the local time asked for minus one zone offset day.
    #[test]
    fn every_local_time_resolves(
        zone in prop::sample::select(ZONES),
        day in 0i64..366,
        minute in 0u32..1440,
    ) {
        let tz: chrono_tz::Tz = zone.parse().unwrap();
        let local = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
            + Duration::days(day) + Duration::minutes(minute.into());
        let at = local_to_utc(local, tz);
        prop_assert!(at.is_some(), "{zone} {local}");
        // The local time of the result is the local time asked for, or the
        // first time after a gap (at most 2 hours later).
        let back = at.unwrap().with_timezone(&tz).naive_local();
        prop_assert!(back >= local && back - local <= Duration::hours(2), "{zone} {local} -> {back}");
    }

    // INVARIANT: an accepted callback is inside the allowed range.
    #[test]
    fn an_accepted_callback_is_in_range(
        zone in prop::sample::select(ZONES),
        minutes in -10_000i64..200_000,
    ) {
        let tz: chrono_tz::Tz = zone.parse().unwrap();
        let local = (now() + Duration::minutes(minutes)).with_timezone(&tz).naive_local();
        let r = Report {
            schema_version: SCHEMA_VERSION,
            disposition: Some("CALLBK".into()),
            callback: Some(CallbackRequest { at: local.format("%Y-%m-%dT%H:%M").to_string(), zone: Some(zone.into()) }),
            ..Report::default()
        };
        if let Ok(o) = check(&r, &Catalog::default(), None, now()) {
            let at = o.callback.unwrap().at_utc;
            prop_assert!(at >= now() + Duration::minutes(1));
            prop_assert!(at <= now() + Duration::days(90));
        }
    }

    // INVARIANT (fail safe): a DNC code or a DNC request always gives a DNC
    // scope, whatever else the report says.
    #[test]
    fn a_dnc_request_is_never_lost(
        code in prop::sample::select(vec!["DNC", "SALE", "NI", "DONE", "WN"]),
        ask in prop::option::of(prop::option::of(prop::sample::select(vec!["system", "campaign"]))),
        summary in prop::option::of(".{0,50}"),
    ) {
        let r = Report {
            schema_version: SCHEMA_VERSION,
            disposition: Some(code.into()),
            dnc: ask.map(|o| o.map(String::from)),
            summary,
            ..Report::default()
        };
        let wants = code == "DNC" || ask.is_some();
        let checked = check(&r, &Catalog::default(), Some("America/New_York"), now());
        prop_assert!(checked.is_ok(), "{checked:?}");
        let o = checked.unwrap();
        prop_assert_eq!(o.dnc.is_some(), wants);
        if ask == Some(Some("campaign")) {
            prop_assert_eq!(o.dnc, Some(DncScope::Campaign));
        }
    }
}
