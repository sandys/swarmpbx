//! A call that the node carries itself (PLAN.md §0.0, decision D-9): our SIP
//! user agent (`ad_sip`) places it through the SIP edge, our RTP leg
//! (`ad_media`) carries its audio, and our room participant brings that audio
//! to the agent. No `livekit/sip`.
//!
//! The participant joins the room only after the answer, so the agent's audio
//! never queues while the phone rings. The call ends when the callee sends
//! BYE, when the agent leaves the room, or when no RTP arrives for the media
//! timeout. We send BYE on every other end.
//!
//! Customer first, the agent joins the room after the answer: `run` hangs up
//! when no agent is in the room by its deadline (`CallEnd::NoAgent`).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use ad_core::Clock;
use ad_dispatch::Placement;
use ad_livekit::Signer;
use ad_media::{MediaConfig, MediaOut, RtpLeg, SAMPLE_RATE};
use ad_sip::{Ended, Incoming, Invite, Outcome, SipUa, TrunkAuth};
use tokio::sync::{mpsc, watch, Notify};

use crate::bridge::{
    AudioFrame, BridgeError, MediaBridge, RoomConnection, RoomParticipant, RtpSink, RtpSource,
};
use crate::webrtc_participant::{mint_token, LiveKitParticipant};
use crate::{GatewayConfig, RING_TIMEOUT};

/// Where the calls go.
#[derive(Debug, Clone)]
pub struct SipTrunk {
    /// The carrier's host and port: the Request-URI is `sip:<number>@<host>`.
    pub host: String,
    pub auth: Option<TrunkAuth>,
}

/// One call to place.
#[derive(Debug, Clone)]
pub struct SipDial<'a> {
    pub room: &'a str,
    /// The callee's identity in the room (`sip-attempt-<id>`).
    pub identity: &'a str,
    pub phone_number: &'a str,
    /// The SIP Call-ID; it names the node.
    pub call_id: String,
    /// Notified at the answer, before the room join: customer first starts
    /// the agent then.
    pub on_answer: Option<&'a Notify>,
}

/// The result of a dial.
pub enum Dialed {
    Answered(LiveCall),
    /// The call did not connect.
    Failed(Placement),
}

/// Why a connected call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEnd {
    CalleeHangup,
    AgentLeft,
    MediaTimeout,
    /// No agent was in the room by the deadline (customer first).
    NoAgent,
    /// The dialog ended for another reason.
    Other(String),
}

/// Places calls with our SIP user agent and RTP legs.
#[derive(Clone)]
pub struct SipCaller {
    ua: SipUa,
    media_ip: IpAddr,
    public_ip: Option<IpAddr>,
    trunk: SipTrunk,
    livekit: GatewayConfig,
    signer: Signer,
}

impl SipCaller {
    /// `media_ip` is the local address of the RTP sockets; `public_ip` (the
    /// `public` media mode) is the address that the SDP names instead.
    pub fn new(
        ua: SipUa,
        media_ip: IpAddr,
        public_ip: Option<IpAddr>,
        trunk: SipTrunk,
        livekit: GatewayConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            signer: Signer::new(&livekit.api_key, &livekit.api_secret, clock),
            ua,
            media_ip,
            public_ip,
            trunk,
            livekit,
        }
    }

    /// Place the call and wait for its final response. On an answer the call
    /// is connected to the room and runs in [`LiveCall::run`].
    pub async fn dial(&self, req: SipDial<'_>) -> Dialed {
        let mut media = MediaConfig::new(self.media_ip);
        media.external_ip = self.public_ip;
        let (leg, offer) = match RtpLeg::offer(&media).await {
            Ok(v) => v,
            Err(e) => return Dialed::Failed(Placement::NotSent(format!("rtp: {e}"))),
        };
        let outcome = self
            .ua
            .invite(Invite {
                target: format!("sip:{}@{}", req.phone_number, self.trunk.host),
                offer,
                ring_timeout: RING_TIMEOUT,
                auth: self.trunk.auth.clone(),
                call_id: Some(req.call_id),
            })
            .await;
        let mut call = match outcome {
            Ok(Outcome::Answered(call)) => call,
            Ok(Outcome::Rejected { code, reason }) => {
                return Dialed::Failed(Placement::Rejected {
                    sip_code: Some(code),
                    detail: reason,
                })
            }
            Ok(Outcome::NoResponse(detail)) => {
                return Dialed::Failed(Placement::NoResponse(detail))
            }
            Ok(Outcome::NotSent(detail)) => return Dialed::Failed(Placement::NotSent(detail)),
            Err(e) => return Dialed::Failed(Placement::NotSent(e.to_string())),
        };
        let answered = Instant::now();
        if let Some(n) = req.on_answer {
            n.notify_one();
        }
        if let Err(e) = leg.accept(&call.remote_sdp).await {
            call.hangup().await;
            return Dialed::Failed(Placement::Rejected {
                sip_code: None,
                detail: format!("bad SDP answer: {e}"),
            });
        }
        self.connect(call, leg, req.room, req.identity, answered)
            .await
    }

    /// The node's SIP user agent.
    pub fn ua(&self) -> &SipUa {
        &self.ua
    }

    /// Answer an expected inbound call (click-to-call) and connect it to the
    /// room, as an answered dial.
    pub async fn answer(
        &self,
        incoming: Incoming,
        room: &str,
        identity: &str,
        on_answer: Option<&Notify>,
    ) -> Dialed {
        let mut media = MediaConfig::new(self.media_ip);
        media.external_ip = self.public_ip;
        let (leg, answer) = match RtpLeg::answer(&media, &incoming.offer).await {
            Ok(v) => v,
            Err(e) => {
                incoming.reject(488);
                return Dialed::Failed(Placement::Rejected {
                    sip_code: None,
                    detail: format!("bad SDP offer: {e}"),
                });
            }
        };
        let answered = Instant::now();
        match incoming.accept(answer) {
            Ok(call) => {
                if let Some(n) = on_answer {
                    n.notify_one();
                }
                self.connect(call, leg, room, identity, answered).await
            }
            Err(e) => Dialed::Failed(Placement::Rejected {
                sip_code: None,
                detail: e.to_string(),
            }),
        }
    }

    /// Start the media of a connected call and join the room.
    async fn connect(
        &self,
        mut call: ad_sip::Call,
        leg: RtpLeg,
        room: &str,
        identity: &str,
        answered: Instant,
    ) -> Dialed {
        let media = match leg.start() {
            Ok(m) => m,
            Err(e) => {
                call.hangup().await;
                return Dialed::Failed(Placement::Rejected {
                    sip_code: None,
                    detail: e.to_string(),
                });
            }
        };
        let session = match self.join(room, identity).await {
            Ok(s) => s,
            Err(e) => {
                call.hangup().await;
                return Dialed::Failed(Placement::Rejected {
                    sip_code: None,
                    detail: format!("room: {e}"),
                });
            }
        };
        Dialed::Answered(LiveCall {
            call,
            media,
            session,
            answered,
        })
    }

    async fn join(
        &self,
        room: &str,
        identity: &str,
    ) -> Result<crate::webrtc_participant::LiveKitSession, BridgeError> {
        let token = mint_token(&self.signer, room, identity)?;
        LiveKitParticipant {
            sample_rate: SAMPLE_RATE,
            channels: 1,
        }
        .join(RoomConnection {
            url: self.livekit.livekit_url.clone(),
            token,
            room: room.to_string(),
            identity: identity.to_string(),
        })
        .await
    }
}

/// A connected call. Dropping it hangs up, stops the media, and leaves the
/// room.
pub struct LiveCall {
    call: ad_sip::Call,
    media: ad_media::Media,
    session: crate::webrtc_participant::LiveKitSession,
    answered: Instant,
}

impl LiveCall {
    /// When the callee answered (the 200 OK, or our 200 OK to an inbound
    /// call).
    pub fn answered(&self) -> Instant {
        self.answered
    }

    /// The identity of the agent once it is in the room.
    pub fn agent_joined(&self) -> watch::Receiver<Option<String>> {
        self.session.others_joined()
    }

    /// Carry the audio until the call ends. With `agent_by`, hang up at that
    /// time if no agent is in the room.
    pub async fn run(self, agent_by: Option<Instant>) -> CallEnd {
        let LiveCall {
            mut call,
            mut media,
            session,
            ..
        } = self;
        let Some(from_callee) = media.take_inbound() else {
            call.hangup().await;
            return CallEnd::Other("no inbound audio".into());
        };
        let mut agent_left = session.others_left();
        let mut joined = session.others_joined();
        let no_agent = async move {
            let Some(by) = agent_by else {
                return std::future::pending().await;
            };
            let deadline = tokio::time::Instant::from_std(by);
            if tokio::time::timeout_at(deadline, joined.wait_for(|j| j.is_some()))
                .await
                .is_ok()
            {
                std::future::pending::<()>().await;
            }
        };
        let (stop, stopped) = watch::channel(false);
        let bridge = MediaBridge::run(
            session,
            CalleeAudio {
                rx: from_callee,
                stopped,
            },
            ToCallee(media.sender()),
        );
        tokio::pin!(bridge);
        let end = tokio::select! {
            ended = call.ended() => match ended {
                Ended::RemoteHangup => CallEnd::CalleeHangup,
                Ended::LocalHangup => CallEnd::Other("we hung up".into()),
                Ended::Other(why) => CallEnd::Other(why),
            },
            _ = async { let _ = agent_left.wait_for(|left| *left).await; } => {
                call.hangup().await;
                CallEnd::AgentLeft
            }
            _ = no_agent => {
                call.hangup().await;
                CallEnd::NoAgent
            }
            stats = &mut bridge => {
                // The callee's audio stopped: the media timeout.
                call.hangup().await;
                log_media(&stats);
                return CallEnd::MediaTimeout;
            }
        };
        // Stop the bridge: it leaves the room on its way out.
        let _ = stop.send(true);
        log_media(&bridge.await);
        drop(media);
        end
    }
}

/// The frames that crossed the bridge in each direction (20 ms from the
/// callee, 10 ms from the agent).
fn log_media(stats: &Result<crate::bridge::BridgeStats, BridgeError>) {
    match stats {
        Ok(s) => tracing::info!(
            from_callee = s.outbound_frames,
            to_callee = s.inbound_frames,
            "call media"
        ),
        Err(e) => tracing::warn!(error = %e, "call media failed"),
    }
}

/// The callee's audio, until the call ends.
struct CalleeAudio {
    rx: mpsc::Receiver<Vec<i16>>,
    stopped: watch::Receiver<bool>,
}

impl RtpSource for CalleeAudio {
    async fn recv(&mut self) -> Option<AudioFrame> {
        tokio::select! {
            samples = self.rx.recv() => samples.map(|s| AudioFrame::new(s, SAMPLE_RATE, 1)),
            _ = async { let _ = self.stopped.wait_for(|s| *s).await; } => None,
        }
    }
}

/// The agent's audio toward the callee.
struct ToCallee(MediaOut);

impl RtpSink for ToCallee {
    async fn send(&self, frame: AudioFrame) -> Result<(), BridgeError> {
        self.0.send(frame.samples);
        Ok(())
    }
}
