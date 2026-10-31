use proptest::prelude::*;

use super::*;

#[test]
fn common_forms_of_one_number_give_one_e164() {
    for raw in [
        "+12025550123",
        "+1 (202) 555-0123",
        " +1-202-555-0123 ",
        "+1.202.555.0123",
    ] {
        let p = phone(raw, None).unwrap();
        assert_eq!(p.e164.as_str(), "+12025550123", "{raw}");
        assert_eq!(p.region.as_deref(), Some("US"));
        assert!(p.valid);
    }
    let uk = phone("+44 20 7946 0958", None).unwrap();
    assert_eq!(
        (uk.e164.as_str(), uk.region.as_deref()),
        ("+442079460958", Some("GB"))
    );
}

#[test]
fn a_number_without_a_country_code_reads_only_with_a_region() {
    assert!(matches!(
        phone("202-555-0123", None),
        Err(PhoneError::Unreadable(_))
    ));
    assert_eq!(
        phone("(202) 555-0123", Some("US")).unwrap().e164.as_str(),
        "+12025550123"
    );
    assert_eq!(
        phone("020 7946 0958", Some("gb")).unwrap().e164.as_str(),
        "+442079460958"
    );
    // A country code wins over the region.
    assert_eq!(
        phone("+44 20 7946 0958", Some("US")).unwrap().e164.as_str(),
        "+442079460958"
    );
    assert_eq!(
        phone("202-555-0123", Some("XX")),
        Err(PhoneError::Region("XX".into()))
    );
    assert_eq!(phone("  ", None), Err(PhoneError::Empty));
    assert!(phone("not a number", Some("US")).is_err());
    assert_eq!(phone_key("202-555-0123"), None);
}

#[test]
fn dialable_needs_a_number_that_the_plan_assigns() {
    assert!(dialable("+12025550123", None).is_ok());
    // 555 is not an area code.
    let p = phone("+15550000001", None).unwrap();
    assert!(!p.valid);
    assert_eq!(
        dialable("+15550000001", None),
        Err(PhoneError::NotAssigned("+15550000001".into()))
    );
    // The key does not need an assigned number.
    assert_eq!(
        phone_key("+1 555 000 0001").unwrap().as_str(),
        "+15550000001"
    );
}

#[test]
fn a_number_with_a_wrong_length_for_its_country_does_not_read() {
    assert_eq!(
        phone("+1555", None),
        Err(PhoneError::Length("+1555".into()))
    );
    assert_eq!(phone_key("+1 202 555 01234"), None);
    assert_eq!(
        phone_key("+44 20 7946 0958").unwrap().as_str(),
        "+442079460958"
    );
}

#[test]
fn a_number_with_an_extension_is_refused() {
    assert_eq!(
        phone("+1 202 555 0123 ext. 7", None),
        Err(PhoneError::Extension)
    );
}

#[test]
fn area_codes_come_from_north_american_numbers() {
    let k = phone_key("+1 212 867 5309").unwrap();
    assert_eq!(area_code(&k).as_deref(), Some("212"));
    let uk = phone_key("+442079460958").unwrap();
    assert_eq!(area_code(&uk), None);
}

#[test]
fn e164_serializes_as_a_plain_string() {
    let k = phone_key("+12025550123").unwrap();
    assert_eq!(serde_json::to_string(&k).unwrap(), "\"+12025550123\"");
}

#[test]
fn us_and_canadian_postal_codes_take_their_standard_form() {
    assert_eq!(postal_code(" 20500 ", Some("US")).unwrap(), "20500");
    assert_eq!(postal_code("205000003", Some("US")).unwrap(), "20500-0003");
    assert_eq!(postal_code("20500-0003", Some("US")).unwrap(), "20500-0003");
    assert!(postal_code("2050", Some("US")).is_err());
    assert!(postal_code("K1A0B1", Some("US")).is_err());
    assert_eq!(postal_code("k1a0b1", Some("CA")).unwrap(), "K1A 0B1");
    assert_eq!(postal_code("K1A 0B1", Some("CA")).unwrap(), "K1A 0B1");
    assert!(postal_code("12345", Some("CA")).is_err());
}

#[test]
fn other_postal_codes_keep_their_form_in_upper_case() {
    assert_eq!(postal_code("sw1a  1aa", Some("GB")).unwrap(), "SW1A 1AA");
    assert_eq!(postal_code("10115", None).unwrap(), "10115");
    assert_eq!(postal_code("", None), Err(PostalError::Empty));
    assert!(postal_code("a/b", None).is_err());
    assert!(postal_code("1234567890123", None).is_err());
}

proptest! {
    // INVARIANT: the key of a key is the key: E.164 is a fixed point.
    #[test]
    fn the_e164_form_reads_back_as_itself(n in 2_000_000_000u64..9_999_999_999) {
        if let Some(k) = phone_key(&format!("+1{n}")) {
            prop_assert_eq!(phone_key(k.as_str()), Some(k.clone()));
        }
    }

    // INVARIANT: no text makes the backend panic.
    #[test]
    fn any_text_reads_or_fails_without_a_panic(s in ".{0,40}") {
        let _ = phone(&s, None);
        let _ = phone(&s, Some("US"));
        let _ = postal_code(&s, Some("US"));
        let _ = postal_code(&s, None);
    }
}
