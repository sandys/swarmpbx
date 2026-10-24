//! The SIP user agent of a node (PLAN.md §0.0, decision D-9).
//!
//! The vendored `rsipstack` does the SIP work: messages, the transaction
//! timers, ACK for every 2xx (also a resent one), CANCEL only after a 1xx, ACK
//! then BYE for a 2xx that comes after CANCEL, and digest auth. This crate
//! drives one outbound call: the INVITE (through the SIP edge, Kamailio, when
//! a proxy is set), a ring timeout that cancels, and the end of the call.
//!
//! The node only places calls. It answers OPTIONS (the edge's health check)
//! and refuses a new INVITE.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{DialogState, DialogStateReceiver, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{self, StatusCodeKind};
use rsipstack::transaction::TransactionReceiver;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{SipAddr, TransportLayer};
use rsipstack::EndpointBuilder;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum SipError {
    #[error("sip: {0}")]
    Stack(String),
    #[error("bad SIP URI {0}")]
    Uri(String),
}

fn stack<E: std::fmt::Display>(e: E) -> SipError {
    SipError::Stack(e.to_string())
}

/// The node's SIP socket and how it presents itself.
#[derive(Debug, Clone)]
pub struct SipConfig {
    /// The local UDP socket, for example `0.0.0.0:5060`.
    pub bind: SocketAddr,
    /// The address in Via and Contact: an address that the SIP edge reaches
    /// (the pod IP).
    pub advertise: IpAddr,
    /// The SIP edge (Kamailio). Every INVITE goes to it. Kamailio then keeps
    /// itself on the call path with `Record-Route`.
    pub proxy: Option<SocketAddr>,
    /// The user part of our Contact, and of our From when there is no
    /// caller ID.
    pub user: String,
    /// The caller ID: the user part of our From. Twilio requires an E.164
    /// number of the account here (or a verified caller ID).
    pub caller_id: Option<String>,
}

/// Digest credentials for a trunk.
#[derive(Debug, Clone)]
pub struct TrunkAuth {
    pub username: String,
    pub password: String,
}

/// One outbound call.
#[derive(Debug, Clone)]
pub struct Invite {
    /// The Request-URI, for example `sip:+15551234567@carrier.example:5060`.
    pub target: String,
    /// The SDP offer.
    pub offer: String,
    /// CANCEL when no final response comes in this time.
    pub ring_timeout: Duration,
    pub auth: Option<TrunkAuth>,
    /// The Call-ID. A node puts its name in it, so a log line names the node.
    pub call_id: Option<String>,
}

/// How an INVITE ended.
pub enum Outcome {
    /// 2xx. The call runs until [`Call::ended`] returns.
    Answered(Call),
    /// A final response that is not 2xx. A ring timeout gives 487.
    Rejected { code: u16, reason: String },
    /// No final response came back. A call may exist at the carrier.
    NoResponse(String),
    /// The INVITE never left (no address, no route). No call exists.
    NotSent(String),
}

/// Why a connected call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The callee sent BYE.
    RemoteHangup,
    /// We sent BYE.
    LocalHangup,
    /// The dialog ended for another reason (for example a transaction timeout).
    Other(String),
}

/// The node's user agent. Clone it freely; the endpoint stops when the last
/// clone drops.
#[derive(Clone)]
pub struct SipUa {
    inner: Arc<Inner>,
}

struct Inner {
    dialogs: Arc<DialogLayer>,
    contact: sip::Uri,
    caller: sip::Uri,
    /// The SIP edge as a preloaded Route.
    proxy: Option<sip::typed::Route>,
    local: SocketAddr,
    stop: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl SipUa {
    /// Bind the SIP socket and start the endpoint.
    pub async fn start(cfg: SipConfig) -> Result<Self, SipError> {
        let stop = CancellationToken::new();
        let transports = TransportLayer::new(stop.clone());
        // Bind first, so Via and Contact name the port we really got.
        let socket = tokio::net::UdpSocket::bind(cfg.bind).await.map_err(stack)?;
        let local = socket.local_addr().map_err(stack)?;
        let external = SocketAddr::new(cfg.advertise, local.port());
        let socket: Arc<dyn rsipstack::platform::net::UdpSocket> = Arc::new(socket);
        let udp = UdpConnection::from_socket(socket, Some(external), Some(stop.child_token()))
            .await
            .map_err(stack)?;
        transports.add_transport(udp.into());
        let endpoint = EndpointBuilder::new()
            .with_cancel_token(stop.clone())
            .with_transport_layer(transports)
            .build();
        let incoming = endpoint.incoming_transactions().map_err(stack)?;
        let dialogs = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        let uri = |user: &str| -> Result<sip::Uri, SipError> {
            format!("sip:{user}@{external}")
                .try_into()
                .map_err(|_| SipError::Uri(user.to_string()))
        };
        let contact = uri(&cfg.user)?;
        let caller = uri(cfg.caller_id.as_deref().unwrap_or(&cfg.user))?;

        let serving = endpoint;
        tokio::spawn(async move { serving.serve().await });
        tokio::spawn(answer_requests(dialogs.clone(), incoming));
        Ok(Self {
            inner: Arc::new(Inner {
                dialogs,
                contact,
                caller,
                proxy: match cfg.proxy {
                    Some(p) => Some(sip::typed::Route {
                        display_name: None,
                        uri: format!("sip:{p};lr")
                            .try_into()
                            .map_err(|_| SipError::Uri(p.to_string()))?,
                        params: vec![],
                    }),
                    None => None,
                },
                local,
                stop,
            }),
        })
    }

    /// The bound SIP socket (useful when the port was 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local
    }

    /// The address of the Request-URI host that our socket can send to.
    async fn trunk_address(&self, uri: &sip::Uri) -> Result<SocketAddr, String> {
        let raw = uri.host_with_port.host.to_string();
        let host = raw.trim_start_matches('[').trim_end_matches(']');
        let port = uri.host_with_port.port.as_ref().map_or(5060, |p| p.value());
        let candidates: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host(format!("{host}:{port}"))
                .await
                .map_err(|e| format!("{host}: {e}"))?
                .collect(),
        };
        let v4 = self.inner.local.is_ipv4();
        candidates
            .into_iter()
            .find(|a| a.is_ipv4() == v4)
            .ok_or_else(|| format!("{host} has no address that our socket reaches"))
    }

    /// Place one call and wait for its final response.
    pub async fn invite(&self, call: Invite) -> Result<Outcome, SipError> {
        let callee: sip::Uri = call
            .target
            .clone()
            .try_into()
            .map_err(|_| SipError::Uri(call.target.clone()))?;
        // Decide where the INVITE goes before it exists. Through the SIP edge:
        // a preloaded Route (RFC 3261 §8.1.2), which the CANCEL copies, so the
        // CANCEL follows the INVITE. Straight to the trunk: its address.
        // rsipstack retries a failed lookup until Timer B, so a trunk that we
        // cannot reach is decided here: no call.
        let (destination, route_set) = match &self.inner.proxy {
            Some(route) => (None, vec![route.clone()]),
            None => match self.trunk_address(&callee).await {
                Ok(addr) => (
                    Some(SipAddr {
                        r#type: Some(sip::transport::Transport::Udp),
                        ..SipAddr::from(addr)
                    }),
                    vec![],
                ),
                Err(why) => return Ok(Outcome::NotSent(why)),
            },
        };
        let (states_tx, states) = self.inner.dialogs.new_dialog_state_channel();
        let option = InviteOption {
            caller: self.inner.caller.clone(),
            callee,
            contact: self.inner.contact.clone(),
            destination,
            route_set,
            content_type: Some("application/sdp".into()),
            offer: Some(call.offer.into_bytes()),
            credential: call.auth.map(|a| Credential {
                username: a.username,
                password: a.password,
                realm: None,
                auth_username: None,
            }),
            call_id: call.call_id,
            ..Default::default()
        };
        let (dialog, running) = self
            .inner
            .dialogs
            .do_invite_async(option, states_tx)
            .map_err(stack)?;
        let mut running = running;
        let result = tokio::select! {
            r = &mut running => r,
            _ = tokio::time::sleep(call.ring_timeout) => {
                tracing::info!(dialog = %dialog.id(), "ring timeout; CANCEL");
                if let Err(e) = dialog.cancel().await {
                    tracing::debug!(error = %e, "cancel failed");
                }
                running.await
            }
        };
        let response = match result {
            Ok(Ok((_, Some(response)))) => response,
            Ok(Ok((_, None))) => return Ok(Outcome::NoResponse("no final response".into())),
            Ok(Err(e)) => return Ok(unsent_or_lost(e)),
            Err(e) => return Ok(Outcome::NoResponse(e.to_string())),
        };
        // rsipstack reports Timer B (and Timer C) as a 408 that it builds from
        // our own request, so it has no To tag. Every real response but 100
        // has one (RFC 3261 §8.2.6.2). No response came back: the INVITE may
        // have reached the carrier, so the call is uncertain, not "no answer".
        let to_tag = response
            .to_header()
            .ok()
            .and_then(|to| to.tag().ok().flatten());
        if response.status_code.code() == 408 && to_tag.is_none() {
            return Ok(Outcome::NoResponse("no response from the next hop".into()));
        }
        if response.status_code.kind() != StatusCodeKind::Successful {
            return Ok(Outcome::Rejected {
                code: response.status_code.code(),
                reason: response.status_code.to_string(),
            });
        }
        if dialog.cancel_token().is_cancelled() {
            // The 2xx came after our CANCEL: rsipstack sent ACK and BYE.
            return Ok(Outcome::Rejected {
                code: 487,
                reason: "answered after the ring timeout".into(),
            });
        }
        Ok(Outcome::Answered(Call {
            answer: String::from_utf8_lossy(response.body()).into_owned(),
            dialog,
            states,
            dialogs: self.inner.dialogs.clone(),
            ended: None,
        }))
    }
}

/// A connected call. Dropping it hangs up.
pub struct Call {
    /// The callee's SDP answer.
    pub answer: String,
    dialog: InviteDialog,
    states: DialogStateReceiver,
    dialogs: Arc<DialogLayer>,
    ended: Option<Ended>,
}

impl Call {
    /// Wait until the call ends.
    pub async fn ended(&mut self) -> Ended {
        if let Some(e) = &self.ended {
            return e.clone();
        }
        let end = loop {
            match self.states.recv().await {
                Some(DialogState::Terminated(_, reason)) => break end_of(reason),
                Some(_) => {}
                None => break Ended::Other("the dialog closed".into()),
            }
        };
        self.dialogs.remove_dialog(&self.dialog.id());
        self.ended = Some(end.clone());
        end
    }

    /// Send BYE (through the route set, so through the SIP edge).
    pub async fn hangup(&mut self) {
        if self.ended.is_some() {
            return;
        }
        if let Err(e) = self.dialog.hangup().await {
            tracing::debug!(error = %e, "BYE failed");
        }
        self.dialogs.remove_dialog(&self.dialog.id());
        self.ended = Some(Ended::LocalHangup);
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if self.ended.is_some() {
            return;
        }
        let dialog = self.dialog.clone();
        let dialogs = self.dialogs.clone();
        tokio::spawn(async move {
            let _ = dialog.hangup().await;
            dialogs.remove_dialog(&dialog.id());
        });
    }
}

fn end_of(reason: TerminatedReason) -> Ended {
    match reason {
        TerminatedReason::UasBye => Ended::RemoteHangup,
        TerminatedReason::UacBye => Ended::LocalHangup,
        other => Ended::Other(format!("{other:?}")),
    }
}

/// Requests that reach the node: in-dialog requests go to their dialog (the
/// callee's BYE ends the call); OPTIONS gets 200; a new INVITE is refused.
async fn answer_requests(dialogs: Arc<DialogLayer>, mut incoming: TransactionReceiver) {
    while let Some(mut tx) = incoming.recv().await {
        let in_dialog = tx
            .original
            .to_header()
            .ok()
            .and_then(|to| to.tag().ok().flatten())
            .is_some();
        if in_dialog {
            match dialogs.match_dialog(&tx) {
                Some(mut dialog) => {
                    tokio::spawn(async move {
                        if let Err(e) = dialog.handle(&mut tx).await {
                            tracing::debug!(error = %e, "in-dialog request failed");
                        }
                    });
                }
                None => {
                    let _ = tx.reply(sip::StatusCode::CallTransactionDoesNotExist).await;
                }
            }
            continue;
        }
        let status = match tx.original.method {
            sip::Method::Options => sip::StatusCode::OK,
            sip::Method::Invite => sip::StatusCode::Forbidden,
            sip::Method::Ack => continue,
            _ => sip::StatusCode::MethodNotAllowed,
        };
        let _ = tx.reply(status).await;
    }
}

/// A failure before the INVITE left means no call exists; any other failure
/// may follow a call that the carrier has.
fn unsent_or_lost(e: rsipstack::Error) -> Outcome {
    use rsipstack::Error as E;
    match e {
        E::DnsResolutionError(_)
        | E::TransportLayerError(..)
        | E::IoError(_)
        | E::AddrParseError(_) => Outcome::NotSent(e.to_string()),
        other => Outcome::NoResponse(other.to_string()),
    }
}
