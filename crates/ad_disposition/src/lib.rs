//! Dispositions: the business result of a call, as the agent reports it.
//! Pure: the functions take data and a time, and do no I/O.
//!
//! - `catalog`: the codes an agent can report, with the VICIdial flags
//!   (GAPS.md LEAD-10). The system codes apply to every campaign; a campaign
//!   adds its own.
//! - `outcome`: the agent's report (schema version 1) and its check against
//!   the catalog (GAPS.md AGNT-2). A checked outcome carries the callback
//!   (LEAD-5) and the DNC request (COMP-7).
//! - `callback`: the local time of a callback in UTC, with an explicit rule
//!   for the daylight saving changes.

pub mod callback;
pub mod catalog;
pub mod outcome;

pub use callback::local_to_utc;
pub use catalog::{Catalog, CatalogError, Disposition, Flags};
pub use outcome::{
    check, AgentOutcome, AnsweredBy, Callback, CallbackRequest, DncScope, Rejection, Report,
    SCHEMA_VERSION,
};
