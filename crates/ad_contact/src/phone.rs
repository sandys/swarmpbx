//! Phone numbers. `Libphonenumber` reads them with the `phonenumber` crate
//! (libphonenumber in Rust).

use serde::{Deserialize, Serialize};

/// A number in E.164 form: `+`, the country code, and the national number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct E164(String);

impl E164 {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for E164 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A number that reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phone {
    pub e164: E164,
    /// The ISO 3166 code of the region that the number belongs to, for
    /// example `US`. `None` for a number of no single region.
    pub region: Option<String>,
    /// The numbering plan of the region assigns this number.
    pub valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhoneError {
    #[error("the number is empty")]
    Empty,
    #[error("the number does not read: {0}")]
    Unreadable(String),
    #[error("the number has an extension; the dialer cannot dial it")]
    Extension,
    #[error("{0} has a wrong length for its country")]
    Length(String),
    #[error("the numbering plan does not assign {0}")]
    NotAssigned(String),
    #[error("'{0}' is not an ISO 3166 region code")]
    Region(String),
}

/// A library that reads phone numbers.
pub trait PhoneBackend: Send + Sync {
    /// Read a number. `region` is the country of a number without a
    /// country code. A number with a wrong length for its country does not
    /// read.
    fn parse(&self, raw: &str, region: Option<&str>) -> Result<Phone, PhoneError>;
    /// The area code (NPA) of a North American number.
    fn area_code(&self, number: &E164) -> Option<String>;
}

/// libphonenumber, through the `phonenumber` crate.
pub struct Libphonenumber;

impl PhoneBackend for Libphonenumber {
    fn parse(&self, raw: &str, region: Option<&str>) -> Result<Phone, PhoneError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(PhoneError::Empty);
        }
        let country = match region {
            Some(r) => Some(
                r.to_ascii_uppercase()
                    .parse::<phonenumber::country::Id>()
                    .map_err(|_| PhoneError::Region(r.to_string()))?,
            ),
            None => None,
        };
        let number =
            phonenumber::parse(country, raw).map_err(|e| PhoneError::Unreadable(e.to_string()))?;
        if number.extension().is_some() {
            return Err(PhoneError::Extension);
        }
        let e164 = number.format().mode(phonenumber::Mode::E164).to_string();
        if !possible_length(&number) {
            return Err(PhoneError::Length(e164));
        }
        Ok(Phone {
            e164: E164(e164),
            region: number.country().id().map(|id| id.as_ref().to_string()),
            valid: phonenumber::is_valid(&number),
        })
    }

    fn area_code(&self, number: &E164) -> Option<String> {
        let digits = number.as_str().strip_prefix("+1")?;
        (digits.len() == 10 && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits[..3].to_string())
    }
}

/// The national number has a length that the plan of its country code uses
/// for a dialable type. The library does not export this check, and its
/// general descriptors have no lengths.
fn possible_length(number: &phonenumber::PhoneNumber) -> bool {
    let len = number.national().to_string().len() as u16;
    let Some(plans) = phonenumber::metadata::DATABASE.by_code(&number.country().code()) else {
        return false;
    };
    plans.iter().any(|m| {
        let d = m.descriptors();
        [
            d.fixed_line(),
            d.mobile(),
            d.toll_free(),
            d.premium_rate(),
            d.shared_cost(),
            d.personal_number(),
            d.voip(),
            d.pager(),
            d.uan(),
            d.voicemail(),
        ]
        .into_iter()
        .flatten()
        .chain([d.general()])
        .any(|desc| desc.possible_length().contains(&len))
    })
}
