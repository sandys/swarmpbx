//! Inbound calls that the node expects (click-to-call): a real rsipstack
//! caller on loopback sends the INVITE with the call's token.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ad_sip::{Ended, SipConfig, SipUa, CALL_HEADER};
use rsipstack::dialog::dialog::{DialogState, DialogStateReceiver, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{SipAddr, TransportLayer};
use rsipstack::EndpointBuilder;
use tokio_util::sync::CancellationToken;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const OFFER: &str = "v=0\r\no=provider 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 41000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=dialer 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                      t=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// The provider's side: a user agent that sends INVITEs to the node.
struct Caller {
    dialogs: Arc<DialogLayer>,
    contact: sip::Uri,
    _stop: CancellationToken,
}

/// The result of one INVITE from the provider.
struct Sent {
    code: u16,
    body: String,
    dialog: InviteDialog,
    states: DialogStateReceiver,
}

impl Caller {
    async fn start() -> Self {
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
        tokio::spawn(async move { endpoint.serve().await });
        let in_dialog = dialogs.clone();
        tokio::spawn(async move {
            while let Some(mut tx) = incoming.recv().await {
                if let Some(mut d) = in_dialog.match_dialog(&tx) {
                    tokio::spawn(async move { d.handle(&mut tx).await });
                }
            }
        });
        Caller {
            dialogs,
            contact: format!("sip:provider@{addr}").try_into().unwrap(),
            _stop: stop,
        }
    }

    /// INVITE the node, with `token` in the call header when it is set.
    async fn invite(&self, node: SocketAddr, token: Option<&str>) -> Sent {
        let (states_tx, states) = self.dialogs.new_dialog_state_channel();
        let option = InviteOption {
            caller: "sip:+15550002000@provider.example".try_into().unwrap(),
            callee: format!("sip:agentdialer@{node}").try_into().unwrap(),
            contact: self.contact.clone(),
            destination: Some(SipAddr {
                r#type: Some(sip::transport::Transport::Udp),
                ..SipAddr::from(node)
            }),
            content_type: Some("application/sdp".into()),
            offer: Some(OFFER.as_bytes().to_vec()),
            headers: token.map(|t| vec![sip::Header::Other(CALL_HEADER.into(), t.into())]),
            ..Default::default()
        };
        let (dialog, running) = self.dialogs.do_invite_async(option, states_tx).unwrap();
        let response = within(running)
            .await
            .unwrap()
            .unwrap()
            .1
            .expect("a final response");
        Sent {
            code: response.status_code.code(),
            body: String::from_utf8_lossy(response.body()).into_owned(),
            dialog,
            states,
        }
    }
}

async fn ua() -> SipUa {
    SipUa::start(SipConfig {
        bind: SocketAddr::new(LOOPBACK, 0),
        advertise: LOOPBACK,
        proxy: None,
        user: "agentdialer".into(),
        caller_id: None,
    })
    .await
    .unwrap()
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("in time")
}

async fn terminated(states: &mut DialogStateReceiver) -> TerminatedReason {
    loop {
        match within(states.recv()).await {
            Some(DialogState::Terminated(_, reason)) => return reason,
            Some(_) => {}
            None => panic!("the dialog closed without an end"),
        }
    }
}

/// The node answers the INVITE that it expects, from a task, as the gateway
/// does: the provider's INVITE waits for the final response.
async fn answer_expected(ua: &SipUa, token: &str) -> tokio::task::JoinHandle<ad_sip::Call> {
    let mut expected = ua.expect(token);
    tokio::spawn(async move {
        let incoming = within(expected.arrived())
            .await
            .expect("the INVITE arrives");
        assert!(
            incoming.offer.contains("m=audio 41000"),
            "{}",
            incoming.offer
        );
        assert_eq!(incoming.header("x-ad-call"), Some("tok-1"));
        incoming.accept(ANSWER.into()).unwrap()
    })
}

#[tokio::test]
async fn an_expected_invite_is_answered_and_ends_on_the_callers_bye() {
    let ua = ua().await;
    let caller = Caller::start().await;
    let answering = answer_expected(&ua, "tok-1").await;
    let mut sent = caller.invite(ua.local_addr(), Some("tok-1")).await;
    assert_eq!(sent.code, 200);
    assert!(sent.body.contains("m=audio 40000"), "{}", sent.body);
    let mut call = within(answering).await.unwrap();
    assert!(call.remote_sdp.contains("m=audio 41000"));

    within(sent.dialog.hangup()).await.unwrap();
    assert_eq!(within(call.ended()).await, Ended::RemoteHangup);
    assert!(matches!(
        terminated(&mut sent.states).await,
        TerminatedReason::UacBye
    ));
}

#[tokio::test]
async fn our_hangup_of_an_inbound_call_sends_bye() {
    let ua = ua().await;
    let caller = Caller::start().await;
    let answering = answer_expected(&ua, "tok-1").await;
    let mut sent = caller.invite(ua.local_addr(), Some("tok-1")).await;
    assert_eq!(sent.code, 200);
    let mut call = within(answering).await.unwrap();
    // The provider ACKs the 200 before our BYE, as in a real call.
    loop {
        match within(sent.states.recv()).await {
            Some(DialogState::Confirmed(..)) => break,
            Some(_) => {}
            None => panic!("the provider's dialog closed"),
        }
    }

    within(call.hangup()).await;
    assert_eq!(within(call.ended()).await, Ended::LocalHangup);
    assert!(matches!(
        terminated(&mut sent.states).await,
        TerminatedReason::UasBye
    ));
}

#[tokio::test]
async fn an_invite_with_an_unknown_token_gets_404() {
    let ua = ua().await;
    let caller = Caller::start().await;
    let _other = ua.expect("tok-2");
    assert_eq!(
        caller.invite(ua.local_addr(), Some("tok-1")).await.code,
        404
    );
}

#[tokio::test]
async fn an_invite_after_the_wait_stopped_gets_404() {
    let ua = ua().await;
    let caller = Caller::start().await;
    drop(ua.expect("tok-1"));
    assert_eq!(
        caller.invite(ua.local_addr(), Some("tok-1")).await.code,
        404
    );
}

#[tokio::test]
async fn an_invite_without_the_call_header_gets_403() {
    let ua = ua().await;
    let caller = Caller::start().await;
    assert_eq!(caller.invite(ua.local_addr(), None).await.code, 403);
}

#[tokio::test]
async fn an_expected_invite_that_the_node_drops_gets_480() {
    let ua = ua().await;
    let caller = Caller::start().await;
    let mut expected = ua.expect("tok-1");
    let dropping = tokio::spawn(async move {
        drop(
            within(expected.arrived())
                .await
                .expect("the INVITE arrives"),
        );
    });
    assert_eq!(
        caller.invite(ua.local_addr(), Some("tok-1")).await.code,
        480
    );
    dropping.await.unwrap();
}
