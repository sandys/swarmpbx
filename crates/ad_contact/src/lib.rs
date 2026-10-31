//! Phone numbers and postal codes: the one place where the dialer reads them.
//!
//! Every crate that reads a phone number or a postal code calls the
//! functions of this crate: the lead load, the DNC keys, the gate, and the
//! API. Each function asks a backend, and `phones()` and `postal_codes()`
//! select the backend. To use another library, implement `PhoneBackend` or
//! `PostalBackend`, and change the line in that function. All callers then
//! use it.

pub mod phone;
pub mod postal;

pub use phone::{Phone, PhoneBackend, PhoneError, E164};
pub use postal::{PostalBackend, PostalError};

/// The phone number backend of the dialer.
pub fn phones() -> &'static dyn PhoneBackend {
    &phone::Libphonenumber
}

/// The postal code backend of the dialer.
pub fn postal_codes() -> &'static dyn PostalBackend {
    &postal::Rules
}

/// Read a number in any common form, for example `+1 (202) 555-0123`.
/// `region` (ISO 3166, for example `US`) is the country of a number without
/// a country code. The number can be one that the numbering plan does not
/// assign (`Phone::valid`).
pub fn phone(raw: &str, region: Option<&str>) -> Result<Phone, PhoneError> {
    phones().parse(raw, region)
}

/// A number that the dialer can call: it reads, and its numbering plan
/// assigns it. The lead load takes numbers with this.
pub fn dialable(raw: &str, region: Option<&str>) -> Result<Phone, PhoneError> {
    let p = phone(raw, region)?;
    if p.valid {
        Ok(p)
    } else {
        Err(PhoneError::NotAssigned(p.e164.to_string()))
    }
}

/// The key that compares two numbers, for example in a DNC list: the E.164
/// form. `None` when the number does not read. A number without a country
/// code does not read.
pub fn phone_key(raw: &str) -> Option<E164> {
    phone(raw, None).ok().map(|p| p.e164)
}

/// The area code (NPA) of a North American number, for area-code DNC lists.
pub fn area_code(number: &E164) -> Option<String> {
    phones().area_code(number)
}

/// A postal code in its standard form. `region` is the ISO 3166 code of the
/// country, for example the region of the lead's phone number.
pub fn postal_code(raw: &str, region: Option<&str>) -> Result<String, PostalError> {
    postal_codes().normalize(raw, region)
}

#[cfg(test)]
mod tests;
