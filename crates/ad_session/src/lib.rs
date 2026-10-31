//! The agent session stream (decision D-6, GAPS.md AGNT-2).
//!
//! The api role serves `agentdialer.session.v1.Session` over gRPC
//! (`proto/agentdialer/session/v1/session.proto`). The node makes a token for
//! each agent start (`new_token`) and stores only its digest; the agent shows
//! the token in `Hello`. The service checks each `Outcome` against the
//! catalog of the campaign (`ad_disposition::check`) and stores the first one
//! that passes. Events and logs go to the dialer log.

pub mod pb {
    #![allow(clippy::all)]
    tonic::include_proto!("agentdialer.session.v1");
}

mod server;
mod token;

pub use server::{report, serve, service, SessionService, Store};
pub use token::{digest, new_token};

#[cfg(test)]
mod tests;
