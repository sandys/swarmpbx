//! Twilio compatibility: our user agent against an in-process callee that
//! behaves like Twilio Elastic SIP Trunking termination. Twilio is a carrier
//! that the dialer supports, so these run in every test tier.
//!
//! Twilio challenges each new INVITE with 407 (digest, Proxy-Authenticate),
//! needs an E.164 Request-URI, a caller ID of the account in From, and
//! Max-Forwards 70 ("Send Max-Forwards with a value of 70").

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ad_sip::{Invite, Outcome, SipConfig, SipUa, TrunkAuth};
use rsipstack::dialog::authenticate::verify_digest;
use rsipstack::dialog::dialog::{Dialog, DialogState};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{self, Header};
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::TransportLayer;
use rsipstack::EndpointBuilder;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const REALM: &str = "sip.twilio.com";
const USER: &str = "agentdialer";
const PASSWORD: &str = "TrunkPassw0rd2026";
const CALLER_ID: &str = "+15550001000";
const PLAY: &str = "+16504894546";
const OFFER: &str = "v=0\r\no=dialer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 41000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=twilio 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                      t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// What the callee saw in one INVITE.
#[derive(Debug, Clone)]
struct Seen {
    request_user: Option<String>,
    from: String,
    max_forwards: Option<String>,
    /// None: no credentials; Some(valid).
    digest: Option<bool>,
}

fn header_value(req: &sip::Request, name: &str) -> Option<String> {
    req.headers.iter().find_map(|h| match (name, h) {
        ("Proxy-Authorization", Header::ProxyAuthorization(v)) => Some(v.value().to_string()),
        ("Max-Forwards", Header::MaxForwards(v)) => Some(v.value().to_string()),
        _ => None,
    })
}

/// A callee that answers like Twilio's termination edge.
async fn twilio_like() -> (SocketAddr, mpsc::UnboundedReceiver<Seen>) {
    let stop = CancellationToken::new();
    let transports = TransportLayer::new(stop.clone());
    let udp = UdpConnection::create_connection(SocketAddr::new(LOOPBACK, 0), None, None)
        .await
        .unwrap();
    let addr: SocketAddr = udp.get_addr().addr.to_owned().try_into().unwrap();
    transports.add_transport(udp.into());
    let endpoint = EndpointBuilder::new()
        .with_cancel_token(stop.clone())
        .with_transport_layer(transports)
        .build();
    let mut incoming = endpoint.incoming_transactions().unwrap();
    let dialogs = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let contact: sip::Uri = format!("sip:twilio@{addr}").try_into().unwrap();
    let (seen_tx, seen) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _stop = stop;
        endpoint.serve().await
    });
    tokio::spawn(async move {
        while let Some(mut tx) = incoming.recv().await {
            let in_dialog = tx
                .original
                .to_header()
                .ok()
                .and_then(|to| to.tag().ok().flatten())
                .is_some();
            if in_dialog || tx.original.method != sip::Method::Invite {
                if let Some(mut d) = dialogs.match_dialog(&tx) {
                    tokio::spawn(async move { d.handle(&mut tx).await });
                }
                continue;
            }
            let req = tx.original.clone();
            let raw = header_value(&req, "Proxy-Authorization");
            let digest = raw.as_deref().map(|raw| {
                sip::typed::Authorization::parse(raw)
                    .map(|auth| {
                        auth.username == USER
                            && auth.realm == REALM
                            && verify_digest(&auth, PASSWORD, &sip::Method::Invite, raw)
                    })
                    .unwrap_or(false)
            });
            let from = req
                .from_header()
                .map(|f| f.value().to_string())
                .unwrap_or_default();
            let _ = seen_tx.send(Seen {
                request_user: req.uri.user().map(str::to_string),
                from: from.clone(),
                max_forwards: header_value(&req, "Max-Forwards"),
                digest,
            });
            match digest {
                None => {
                    let challenge = format!(
                        "Digest realm=\"{REALM}\", nonce=\"{}\", algorithm=MD5",
                        req.call_id_header().map(|c| c.value().len()).unwrap_or(0)
                    );
                    let _ = tx
                        .reply_with(
                            sip::StatusCode::ProxyAuthenticationRequired,
                            vec![Header::ProxyAuthenticate(challenge.into())],
                            None,
                        )
                        .await;
                    continue;
                }
                Some(false) => {
                    let _ = tx.reply(sip::StatusCode::Forbidden).await;
                    continue;
                }
                Some(true) if !from.contains(&format!("sip:{CALLER_ID}@")) => {
                    let _ = tx.reply(sip::StatusCode::Forbidden).await;
                    continue;
                }
                Some(true) => {}
            }
            let (states_tx, mut states) = dialogs.new_dialog_state_channel();
            let dialog = dialogs
                .get_or_create_server_invite(&tx, states_tx, None, Some(contact.clone()))
                .unwrap();
            let mut handled = Dialog::Invite(dialog.clone());
            tokio::spawn(async move { handled.handle(&mut tx).await });
            let dialogs = dialogs.clone();
            tokio::spawn(async move {
                while let Some(state) = states.recv().await {
                    match state {
                        DialogState::Calling(_) => {
                            let sdp = vec![Header::ContentType("application/sdp".into())];
                            dialog.accept(Some(sdp), Some(ANSWER.into())).ok();
                        }
                        DialogState::Terminated(id, _) => {
                            dialogs.remove_dialog(&id);
                            return;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (addr, seen)
}

async fn ua(caller_id: Option<&str>) -> SipUa {
    SipUa::start(SipConfig {
        bind: SocketAddr::new(LOOPBACK, 0),
        advertise: LOOPBACK,
        proxy: None,
        user: USER.into(),
        caller_id: caller_id.map(str::to_string),
    })
    .await
    .unwrap()
}

fn invite(to: SocketAddr, password: &str) -> Invite {
    Invite {
        target: format!("sip:{PLAY}@{to}"),
        offer: OFFER.into(),
        ring_timeout: Duration::from_secs(5),
        auth: Some(TrunkAuth {
            username: USER.into(),
            password: password.into(),
        }),
        call_id: Some("attempt-1-1@node-a".into()),
    }
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("in time")
}

#[tokio::test]
async fn we_answer_twilios_407_with_digest_and_send_the_caller_id() {
    let (twilio, mut seen) = twilio_like().await;
    let ua = ua(Some(CALLER_ID)).await;
    let outcome = within(ua.invite(invite(twilio, PASSWORD))).await.unwrap();
    let Outcome::Answered(mut call) = outcome else {
        panic!("Twilio answers an authenticated call");
    };
    let first = seen.recv().await.unwrap();
    let second = seen.recv().await.unwrap();
    assert_eq!(first.digest, None, "the first INVITE has no credentials");
    assert_eq!(
        second.digest,
        Some(true),
        "the retry carries a valid digest"
    );
    for s in [&first, &second] {
        assert_eq!(
            s.request_user.as_deref(),
            Some(PLAY),
            "an E.164 Request-URI"
        );
        assert!(
            s.from.contains(&format!("sip:{CALLER_ID}@")),
            "the caller ID is in From: {}",
            s.from
        );
        assert_eq!(s.max_forwards.as_deref(), Some("70"));
    }
    within(call.hangup()).await;
}

#[tokio::test]
async fn bad_trunk_credentials_fail_once_and_do_not_loop() {
    let (twilio, mut seen) = twilio_like().await;
    let ua = ua(Some(CALLER_ID)).await;
    match within(ua.invite(invite(twilio, "WrongPassw0rd99")))
        .await
        .unwrap()
    {
        Outcome::Rejected { code, .. } => assert_eq!(code, 403),
        _ => panic!("bad credentials are a rejection"),
    }
    assert!(seen.recv().await.is_some());
    assert!(seen.recv().await.is_some());
    assert!(seen.try_recv().is_err(), "two INVITEs only: no retry loop");
}

#[tokio::test]
async fn without_a_caller_id_twilio_refuses_the_call() {
    let (twilio, _seen) = twilio_like().await;
    let ua = ua(None).await;
    match within(ua.invite(invite(twilio, PASSWORD))).await.unwrap() {
        Outcome::Rejected { code, .. } => assert_eq!(code, 403),
        _ => panic!("From must carry a caller ID of the account"),
    }
}
