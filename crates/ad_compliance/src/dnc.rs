//! Do-not-call scrubbing. Pure: the caller loads the DNC data; this module
//! decides a match. The scopes follow VICIdial: system, campaign, and area
//! code. `ad_contact` reads the numbers, so a list entry matches a lead in
//! any form of the same number.

use std::collections::HashSet;

use ad_contact::E164;

use crate::{Decision, DenyReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DncScope {
    System,
    Campaign,
    AreaCode,
}

/// A loaded DNC data set to check a number against.
#[derive(Debug, Clone, Default)]
pub struct DncSet {
    pub system: HashSet<E164>,
    pub campaign: HashSet<E164>,
    pub area_codes: HashSet<String>,
}

/// A list entry that does not read. The set refuses it: without it, the set
/// could let a call to that number through.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("DNC entry '{entry}': {reason}")]
pub struct DncEntryError {
    pub entry: String,
    pub reason: String,
}

impl DncSet {
    /// A set from lists of numbers in any form with their country code, and
    /// of three-digit area codes.
    pub fn from_lists(
        system: &[String],
        campaign: &[String],
        area_codes: &[String],
    ) -> Result<Self, DncEntryError> {
        let keys = |list: &[String]| {
            list.iter()
                .map(|entry| {
                    ad_contact::phone(entry, None)
                        .map(|p| p.e164)
                        .map_err(|e| DncEntryError {
                            entry: entry.clone(),
                            reason: e.to_string(),
                        })
                })
                .collect::<Result<HashSet<_>, _>>()
        };
        let area_codes = area_codes
            .iter()
            .map(|a| {
                let a = a.trim();
                if a.len() == 3 && a.bytes().all(|b| b.is_ascii_digit()) {
                    Ok(a.to_string())
                } else {
                    Err(DncEntryError {
                        entry: a.to_string(),
                        reason: "an area code has three digits".into(),
                    })
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(DncSet {
            system: keys(system)?,
            campaign: keys(campaign)?,
            area_codes,
        })
    }

    /// Check a phone number against all scopes. First match wins, in order of
    /// precedence (system, campaign, area code).
    pub fn check(&self, phone_number: &str) -> Decision {
        let Some(key) = ad_contact::phone_key(phone_number) else {
            // No usable number => cannot prove it's callable => fail closed.
            return Decision::Denied(DenyReason::InvalidNumber);
        };
        if self.system.contains(&key) {
            return Decision::Denied(DenyReason::Dnc(DncScope::System));
        }
        if self.campaign.contains(&key) {
            return Decision::Denied(DenyReason::Dnc(DncScope::Campaign));
        }
        if let Some(ac) = ad_contact::area_code(&key) {
            if self.area_codes.contains(&ac) {
                return Decision::Denied(DenyReason::Dnc(DncScope::AreaCode));
            }
        }
        Decision::Allowed
    }
}
