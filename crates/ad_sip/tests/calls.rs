//! Our user agent against a real rsipstack callee on loopback.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ad_sip::{Ended, Invite, Outcome, SipConfig, SipUa};
use rsipstack::dialog::dialog::{Dialog, DialogState, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{self};
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::TransportLayer;
use rsipstack::EndpointBuilder;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const ANSWER: &str = "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                      t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const OFFER: &str = "v=0\r\no=dialer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 41000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\n";

#[derive(Clone, Copy)]
enum Script {
    /// Answer, then send BYE after this time.
    AnswerThenHangUp(Duration),
    /// Answer and wait for our BYE.
    Answer,
    Busy,
    /// Ring and never answer.
    Ring,
}

/// A callee that follows `script` for every call. Each dialog end reaches
/// the returned channel.
async fn callee(script: Script) -> (SocketAddr, mpsc::UnboundedReceiver<TerminatedReason>) {
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
    let contact: sip::Uri = format!("sip:callee@{addr}").try_into().unwrap();
    let (ends_tx, ends) = mpsc::unbounded_channel();
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
            let (states_tx, mut states) = dialogs.new_dialog_state_channel();
            let dialog = dialogs
                .get_or_create_server_invite(&tx, states_tx, None, Some(contact.clone()))
                .unwrap();
            let mut handled = Dialog::Invite(dialog.clone());
            tokio::spawn(async move { handled.handle(&mut tx).await });
            let ends_tx = ends_tx.clone();
            let dialogs = dialogs.clone();
            tokio::spawn(async move {
                while let Some(state) = states.recv().await {
                    match state {
                        DialogState::Calling(_) => match script {
                            Script::Busy => {
                                dialog.reject(Some(sip::StatusCode::BusyHere), None).ok();
                            }
                            Script::Ring => {
                                dialog.ringing(None, None).ok();
                            }
                            Script::Answer | Script::AnswerThenHangUp(_) => {
                                dialog.ringing(None, None).ok();
                                let sdp = vec![sip::Header::ContentType("application/sdp".into())];
                                dialog.accept(Some(sdp), Some(ANSWER.into())).ok();
                            }
                        },
                        DialogState::Confirmed(..) => {
                            if let Script::AnswerThenHangUp(after) = script {
                                let d = dialog.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(after).await;
                                    d.hangup().await.ok();
                                });
                            }
                        }
                        DialogState::Terminated(id, reason) => {
                            dialogs.remove_dialog(&id);
                            let _ = ends_tx.send(reason);
                            return;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (addr, ends)
}

async fn ua() -> SipUa {
    SipUa::start(SipConfig {
        bind: SocketAddr::new(LOOPBACK, 0),
        advertise: LOOPBACK,
        proxy: None,
        user: "agentdialer".into(),
    })
    .await
    .unwrap()
}

fn invite(to: SocketAddr, ring: Duration) -> Invite {
    Invite {
        target: format!("sip:+15550001111@{to}"),
        offer: OFFER.into(),
        ring_timeout: ring,
        auth: None,
        call_id: Some("test-1@node-a".into()),
    }
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("in time")
}

#[tokio::test]
async fn an_answered_call_ends_when_the_callee_hangs_up() {
    let (callee, mut ends) = callee(Script::AnswerThenHangUp(Duration::from_millis(300))).await;
    let ua = ua().await;
    let Outcome::Answered(mut call) = ua
        .invite(invite(callee, Duration::from_secs(5)))
        .await
        .unwrap()
    else {
        panic!("the callee answers");
    };
    assert!(call.answer.contains("m=audio 40000"), "{}", call.answer);
    assert_eq!(within(call.ended()).await, Ended::RemoteHangup);
    assert!(matches!(
        within(ends.recv()).await,
        Some(TerminatedReason::UasBye)
    ));
}

#[tokio::test]
async fn our_hangup_sends_bye() {
    let (callee, mut ends) = callee(Script::Answer).await;
    let ua = ua().await;
    let Outcome::Answered(mut call) = ua
        .invite(invite(callee, Duration::from_secs(5)))
        .await
        .unwrap()
    else {
        panic!("the callee answers");
    };
    within(call.hangup()).await;
    assert_eq!(within(call.ended()).await, Ended::LocalHangup);
    assert!(matches!(
        within(ends.recv()).await,
        Some(TerminatedReason::UacBye)
    ));
}

#[tokio::test]
async fn busy_is_a_rejection_with_its_code() {
    let (callee, _ends) = callee(Script::Busy).await;
    let ua = ua().await;
    match within(ua.invite(invite(callee, Duration::from_secs(5))))
        .await
        .unwrap()
    {
        Outcome::Rejected { code, .. } => assert_eq!(code, 486),
        _ => panic!("486 is a rejection"),
    }
}

#[tokio::test]
async fn the_ring_timeout_cancels_and_gives_487() {
    let (callee, mut ends) = callee(Script::Ring).await;
    let ua = ua().await;
    match within(ua.invite(invite(callee, Duration::from_millis(500))))
        .await
        .unwrap()
    {
        Outcome::Rejected { code, .. } => assert_eq!(code, 487),
        _ => panic!("a cancelled call is a rejection"),
    }
    assert!(matches!(
        within(ends.recv()).await,
        Some(TerminatedReason::UacCancel)
    ));
}

#[tokio::test]
async fn a_callee_that_does_not_exist_gives_no_response() {
    // Nothing listens on this port: the INVITE times out (Timer B).
    let silent = std::net::UdpSocket::bind((LOOPBACK, 0)).unwrap();
    let to = silent.local_addr().unwrap();
    let ua = ua().await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        ua.invite(invite(to, Duration::from_secs(60))),
    )
    .await
    .expect("Timer B ends the INVITE in 32 s")
    .unwrap();
    // Not "no answer" (a 408): the INVITE may have reached the carrier.
    assert!(
        matches!(outcome, Outcome::NoResponse(_)),
        "a timeout with no response is uncertain"
    );
}

#[tokio::test]
async fn an_invite_that_cannot_leave_is_not_sent() {
    let ua = ua().await;
    let mut call = invite(SocketAddr::new(LOOPBACK, 5060), Duration::from_secs(5));
    // Our socket is IPv4; a trunk with only an IPv6 address is out of reach.
    call.target = "sip:+15550001111@[::1]:5060".into();
    let outcome = within(ua.invite(call)).await.unwrap();
    assert!(
        matches!(outcome, Outcome::NotSent(_)),
        "an INVITE that cannot leave is not a call"
    );
}
