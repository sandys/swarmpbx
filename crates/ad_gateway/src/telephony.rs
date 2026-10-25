//! The telephony providers of a node: how a dial reaches the phone. A
//! registry: adding a provider is its module, one variant, and one match arm
//! here, and its settings in the orchestrator's configuration.
//!
//! - `sip_trunk`: the node sends the INVITE to a SIP trunk (LiveKit Cloud
//!   SIP, Twilio Elastic SIP Trunking) through the SIP edge.
//! - `twilio_click_to_call`: Twilio Programmable Voice calls the phone, then
//!   sends an INVITE to the node (`click_to_call`).

use crate::click_to_call::ClickToCall;
use crate::sip_call::{Dialed, SipCaller, SipDial};

#[derive(Clone)]
pub enum Telephony {
    SipTrunk(SipCaller),
    TwilioClickToCall(ClickToCall),
}

impl Telephony {
    /// The provider names of `AD_DIAL_PROVIDER`.
    pub const NAMES: &'static [&'static str] = &["sip_trunk", "twilio_click_to_call"];

    pub fn name(&self) -> &'static str {
        match self {
            Telephony::SipTrunk(_) => "sip_trunk",
            Telephony::TwilioClickToCall(_) => "twilio_click_to_call",
        }
    }

    /// Place one call and wait until it connects or fails.
    pub async fn dial(&self, req: SipDial<'_>) -> Dialed {
        match self {
            Telephony::SipTrunk(caller) => caller.dial(req).await,
            Telephony::TwilioClickToCall(c2c) => c2c.dial(req).await,
        }
    }
}
