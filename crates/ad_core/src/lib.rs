//! Foundation types shared across AgentDialer crates.
//!
//! Deliberately tiny and dependency-light. Anything here is used by multiple
//! crates; crate-specific types live in their own crate.

use std::fmt;

pub mod ids;
pub mod time;

pub use ids::{CampaignId, LeadId, OrgId, RunId};
pub use time::Clock;

/// A crate-agnostic error wrapper for boundaries that aggregate sub-errors.
/// Domain crates define their own precise error enums; this is only for the
/// outer binary/service seams.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

/// Build/version string surfaced by the binary and health endpoints.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Marker for values that are safe to show in logs (no PII). Used to make the
/// "never echo secrets/PII" rule visible at the type level over time.
pub trait NonSensitive: fmt::Debug {}
