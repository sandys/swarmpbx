//! SIP final response codes to outcome classes (GAPS.md BETTER-7). The class
//! sets the default policy: retry, suppress the number, or count a failure.

use crate::Outcome;

/// Map a SIP final response that is not an answer to an outcome.
pub fn classify_sip(code: u16, detail: String) -> Outcome {
    match code {
        486 | 600 => Outcome::Busy { sip_code: code },
        408 | 480 | 487 => Outcome::NoAnswer { sip_code: code },
        603 | 607 => Outcome::Declined { sip_code: code },
        404 | 410 | 484 | 485 | 604 => Outcome::InvalidNumber { sip_code: code },
        _ => Outcome::Failed {
            sip_code: Some(code),
            detail,
        },
    }
}
