//! Compliance engine — the highest-risk *product* subsystem (PLAN.md §9).
//!
//! Design rules, enforced here:
//!   * PURE and DETERMINISTIC: given inputs + a clock, output is a function of
//!     them. No I/O inside decision functions (DNC data etc. is passed in).
//!   * FAIL CLOSED: if we cannot prove a call is allowed, we DO NOT allow it.
//!     This is the deliberate inversion of permissive scheduling — a missing
//!     timezone or an unparseable window means "do not call", never "call".
//!
//! Submodules:
//!   * `safe_hours` — TCPA-style local-time calling-window gate.
//!   * `dnc`        — do-not-call scrubbing.
//!   * `abandon`    — abandonment-rate ledger + per-jurisdiction rules.

pub mod abandon;
pub mod dnc;
pub mod safe_hours;

/// The outcome of a compliance check. `Denied` always carries a machine-usable
/// reason so dispatch can record *why* a lead was not called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Denied(DenyReason),
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allowed)
    }
    pub fn deny_reason(&self) -> Option<&DenyReason> {
        match self {
            Decision::Denied(r) => Some(r),
            Decision::Allowed => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// Local time could not be determined for the lead (fail-closed).
    UnknownLocalTime,
    /// Current local time is outside the permitted calling window.
    OutsideCallingWindow,
    /// Today is a configured holiday with no calling allowed.
    Holiday,
    /// Phone number is on a do-not-call list.
    Dnc(dnc::DncScope),
    /// A referenced calling-window definition was missing or invalid (fail-closed).
    InvalidWindowConfig,
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenyReason::UnknownLocalTime => write!(f, "unknown local time (fail-closed)"),
            DenyReason::OutsideCallingWindow => write!(f, "outside calling window"),
            DenyReason::Holiday => write!(f, "holiday"),
            DenyReason::Dnc(scope) => write!(f, "DNC ({scope:?})"),
            DenyReason::InvalidWindowConfig => write!(f, "invalid window config (fail-closed)"),
        }
    }
}
