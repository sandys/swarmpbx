//! The SIP user agent of a node (PLAN.md §0.0, decision D-9).
//!
//! The vendored `rsipstack` does the SIP work: messages, the transaction
//! timers, ACK for every 2xx (also a resent one), CANCEL only after a 1xx, ACK
//! then BYE for a 2xx that comes after CANCEL, and digest auth. This crate
//! drives one outbound call: the INVITE (through the SIP edge, Kamailio, when
//! a proxy is set), a ring timeout that cancels, and the end of the call.
//!
//! It also answers one kind of inbound call: an INVITE that the node expects
//! (click-to-call: the provider calls the phone, then calls us). The INVITE
//! names the call in the [`CALL_HEADER`] header with the token of
//! [`SipUa::expect`]. Any other new INVITE is refused. The node answers
//! OPTIONS (the edge's health check).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{self, StatusCodeKind};
use rsipstack::transaction::key::TransactionRole;
use rsipstack::transaction::{Transaction, TransactionReceiver};
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{SipAddr, TransportLayer};
use rsipstack::EndpointBuilder;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// The header of an inbound INVITE that names the call the node expects. A
/// provider copies it from the SIP URI it dials
/// (`sip:...?x-ad-call=<token>`).
pub const CALL_HEADER: &str = "X-AD-Call";

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

/// The inbound calls that the node expects, by token.
type Waiters = Mutex<HashMap<String, oneshot::Sender<Incoming>>>;

struct Inner {
    dialogs: Arc<DialogLayer>,
    contact: sip::Uri,
    caller: sip::Uri,
    /// The SIP edge as a preloaded Route.
    proxy: Option<sip::typed::Route>,
    local: SocketAddr,
    /// The address in Via and Contact.
    external: SocketAddr,
    waiters: Arc<Waiters>,
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
        let waiters = Arc::new(Waiters::default());

        let serving = endpoint;
        tokio::spawn(async move { serving.serve().await });
        tokio::spawn(answer_requests(
            dialogs.clone(),
            incoming,
            waiters.clone(),
            contact.clone(),
        ));
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
                external,
                waiters,
                stop,
            }),
        })
    }

    /// The bound SIP socket (useful when the port was 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local
    }

    /// The address in our Via and Contact: where the SIP edge reaches us.
    pub fn contact_addr(&self) -> SocketAddr {
        self.inner.external
    }

    /// Wait for the inbound INVITE that carries `token` in [`CALL_HEADER`].
    /// While the returned value lives, that INVITE goes to it. Afterwards,
    /// and for any other token, an INVITE gets 404.
    pub fn expect(&self, token: &str) -> Expected {
        let (tx, rx) = oneshot::channel();
        self.inner
            .waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.to_string(), tx);
        Expected {
            token: token.to_string(),
            rx,
            waiters: self.inner.waiters.clone(),
        }
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
            remote_sdp: String::from_utf8_lossy(response.body()).into_owned(),
            dialog,
            states,
            dialogs: self.inner.dialogs.clone(),
            ended: None,
        }))
    }
}

/// An inbound call that the node waits for. Dropping it stops the wait.
pub struct Expected {
    token: String,
    rx: oneshot::Receiver<Incoming>,
    waiters: Arc<Waiters>,
}

impl Expected {
    /// The INVITE, when it arrives. `None` when the user agent stopped.
    pub async fn arrived(&mut self) -> Option<Incoming> {
        (&mut self.rx).await.ok()
    }
}

impl Drop for Expected {
    fn drop(&mut self) {
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
    }
}

/// An expected INVITE, not answered yet. Dropping it rejects the call (480).
pub struct Incoming {
    /// The caller's SDP offer.
    pub offer: String,
    pub call_id: String,
    headers: Vec<(String, String)>,
    dialog: Option<InviteDialog>,
    states: Option<DialogStateReceiver>,
    dialogs: Arc<DialogLayer>,
}

impl Incoming {
    /// The value of a header of the INVITE (the name ignores case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Answer with 200 OK and our SDP. The caller's ACK confirms the call.
    pub fn accept(mut self, answer: String) -> Result<Call, SipError> {
        let (Some(dialog), Some(states)) = (self.dialog.take(), self.states.take()) else {
            return Err(SipError::Stack("the call was answered already".into()));
        };
        let sdp = vec![sip::Header::ContentType("application/sdp".into())];
        if let Err(e) = dialog.accept(Some(sdp), Some(answer.into_bytes())) {
            self.dialogs.remove_dialog(&dialog.id());
            return Err(stack(e));
        }
        Ok(Call {
            remote_sdp: std::mem::take(&mut self.offer),
            dialog,
            states,
            dialogs: self.dialogs.clone(),
            ended: None,
        })
    }

    /// Refuse the call with a final response.
    pub fn reject(mut self, code: u16) {
        if let Some(dialog) = self.dialog.take() {
            dialog.reject(Some(code.into()), None).ok();
            self.dialogs.remove_dialog(&dialog.id());
        }
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            dialog
                .reject(Some(sip::StatusCode::TemporarilyUnavailable), None)
                .ok();
            self.dialogs.remove_dialog(&dialog.id());
        }
    }
}

/// A connected call. Dropping it hangs up.
pub struct Call {
    /// The other side's SDP: the callee's answer to our INVITE, or the
    /// caller's offer of an inbound call.
    pub remote_sdp: String,
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
                Some(DialogState::Terminated(_, reason)) => {
                    break end_of(reason, self.dialog.role())
                }
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

/// The reason names the side that sent BYE (UAC or UAS); our side is the UAC
/// of a call we placed and the UAS of an inbound call.
fn end_of(reason: TerminatedReason, role: TransactionRole) -> Ended {
    match (reason, role) {
        (TerminatedReason::UasBye, TransactionRole::Client)
        | (TerminatedReason::UacBye, TransactionRole::Server) => Ended::RemoteHangup,
        (TerminatedReason::UacBye, TransactionRole::Client)
        | (TerminatedReason::UasBye, TransactionRole::Server) => Ended::LocalHangup,
        (other, _) => Ended::Other(format!("{other:?}")),
    }
}

/// Requests that reach the node: in-dialog requests go to their dialog (the
/// other side's BYE ends the call); OPTIONS gets 200; an expected INVITE
/// goes to its waiter; any other new INVITE is refused.
async fn answer_requests(
    dialogs: Arc<DialogLayer>,
    mut incoming: TransactionReceiver,
    waiters: Arc<Waiters>,
    contact: sip::Uri,
) {
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
            sip::Method::Invite => {
                let token = tx
                    .original
                    .headers
                    .iter()
                    .find(|h| h.name().eq_ignore_ascii_case(CALL_HEADER))
                    .map(|h| h.value().trim().to_string());
                let Some(token) = token else {
                    let _ = tx.reply(sip::StatusCode::Forbidden).await;
                    continue;
                };
                let waiter = waiters
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&token);
                match waiter {
                    Some(waiter) => {
                        hand_over(&dialogs, tx, waiter, &contact);
                        continue;
                    }
                    None => sip::StatusCode::NotFound,
                }
            }
            sip::Method::Ack => continue,
            _ => sip::StatusCode::MethodNotAllowed,
        };
        let _ = tx.reply(status).await;
    }
}

/// Give an expected INVITE to its waiter as a server dialog that is not
/// answered yet.
fn hand_over(
    dialogs: &Arc<DialogLayer>,
    mut tx: Transaction,
    waiter: oneshot::Sender<Incoming>,
    contact: &sip::Uri,
) {
    let (states_tx, states) = dialogs.new_dialog_state_channel();
    let dialog =
        match dialogs.get_or_create_server_invite(&tx, states_tx, None, Some(contact.clone())) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "cannot take the inbound call");
                tokio::spawn(async move { tx.reply(sip::StatusCode::ServerInternalError).await });
                return;
            }
        };
    let incoming = Incoming {
        offer: String::from_utf8_lossy(tx.original.body()).into_owned(),
        call_id: tx
            .original
            .call_id_header()
            .map(|c| c.value().to_string())
            .unwrap_or_default(),
        headers: tx
            .original
            .headers
            .iter()
            .map(|h| (h.name().to_string(), h.value().to_string()))
            .collect(),
        dialog: Some(dialog.clone()),
        states: Some(states),
        dialogs: dialogs.clone(),
    };
    let mut serving = Dialog::Invite(dialog);
    tokio::spawn(async move {
        if let Err(e) = serving.handle(&mut tx).await {
            tracing::debug!(error = %e, "inbound INVITE failed");
        }
    });
    // A waiter that stopped drops the call, which rejects it.
    let _ = waiter.send(incoming);
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
