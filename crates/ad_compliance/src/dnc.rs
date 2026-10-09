//! Do-not-call scrubbing. Pure: the DNC data is supplied by the caller (loaded
//! from the DB elsewhere); this module only decides a match. Scopes mirror
//! VICIdial: system-wide, per-campaign, and area-code lists.

use std::collections::HashSet;

use crate::{Decision, DenyReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DncScope {
    System,
    Campaign,
    AreaCode,
}

/// A loaded DNC data set to check a number against. Numbers are expected to be
/// normalized (digits only) by the caller; we normalize defensively too.
#[derive(Debug, Clone, Default)]
pub struct DncSet {
    pub system: HashSet<String>,
    pub campaign: HashSet<String>,
    pub area_codes: HashSet<String>,
}

impl DncSet {
    /// Check a phone number against all scopes. First match wins, in order of
    /// precedence (system, campaign, area code).
    pub fn check(&self, phone_number: &str) -> Decision {
        let digits = normalize(phone_number);
        if digits.is_empty() {
            // No usable number => cannot prove it's callable => fail closed.
            return Decision::Denied(DenyReason::Dnc(DncScope::System));
        }
        if self.system.contains(&digits) {
            return Decision::Denied(DenyReason::Dnc(DncScope::System));
        }
        if self.campaign.contains(&digits) {
            return Decision::Denied(DenyReason::Dnc(DncScope::Campaign));
        }
        if let Some(ac) = area_code(&digits) {
            if self.area_codes.contains(&ac) {
                return Decision::Denied(DenyReason::Dnc(DncScope::AreaCode));
            }
        }
        Decision::Allowed
    }
}

/// Keep only ASCII digits.
pub fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Extract a NANP-style 3-digit area code from a normalized number.
/// For an 11-digit number starting with country code 1, area code is digits
/// 1..4; for a 10-digit number it's digits 0..3. Other lengths => None.
fn area_code(digits: &str) -> Option<String> {
    match digits.len() {
        11 if digits.starts_with('1') => Some(digits[1..4].to_string()),
        10 => Some(digits[0..3].to_string()),
        _ => None,
    }
}
