//! The session stream over a real gRPC connection, with a store in memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ad_core::time::FixedClock;
use ad_core::{AttemptId, CampaignId, LeadId};
use ad_db::{OutcomeError, Recorded, Session};
use ad_disposition::{AgentOutcome, Disposition, Flags};
use chrono::{TimeZone, Utc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};

use super::*;
use crate::pb::{agent_message::Body as In, dialer_message::Body as Out};

#[derive(Default)]
struct Memory {
    sessions: Mutex<HashMap<i64, ([u8; 32], Session)>>,
    outcomes: Mutex<HashMap<i64, AgentOutcome>>,
}

impl Store for Arc<Memory> {
    async fn session(&self, id: AttemptId, d: [u8; 32]) -> Result<Session, OutcomeError> {
        match self.sessions.lock().unwrap().get(&id.0) {
            Some((stored, s)) if *stored == d => Ok(s.clone()),
            _ => Err(OutcomeError::Session),
        }
    }

    async fn record(
        &self,
        s: &Session,
        _d: [u8; 32],
        o: &AgentOutcome,
    ) -> Result<Recorded, OutcomeError> {
        let mut all = self.outcomes.lock().unwrap();
        match all.get(&s.attempt_id.0) {
            Some(prev) if prev == o => Ok(Recorded::Duplicate),
            Some(_) => Err(OutcomeError::Conflict),
            None => {
                all.insert(s.attempt_id.0, o.clone());
                Ok(Recorded::New {
                    cancelled: u64::from(o.dnc.is_some()),
                    settled: false,
                })
            }
        }
    }
}

const TOKEN: &str = "token-of-attempt-7";

async fn start() -> (Arc<Memory>, String) {
    let memory = Arc::new(Memory::default());
    memory.sessions.lock().unwrap().insert(
        7,
        (
            digest(TOKEN),
            Session {
                attempt_id: AttemptId(7),
                campaign_id: CampaignId(3),
                lead_id: LeadId(5),
                phone_number: "+12025550123".into(),
                lead_timezone: Some("America/New_York".into()),
                dispositions: vec![Disposition {
                    code: "APPT".into(),
                    name: "Appointment".into(),
                    flags: Flags {
                        human_answered: true,
                        sale: true,
                        ..Flags::default()
                    },
                }],
                live: true,
                settled: false,
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let clock = Arc::new(FixedClock(
        Utc.with_ymd_and_hms(2026, 3, 1, 15, 0, 0).unwrap(),
    ));
    let svc = service(memory.clone(), clock);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    (memory, format!("http://{addr}"))
}

/// An open session: send messages, read the answers.
struct Agent {
    tx: mpsc::Sender<pb::AgentMessage>,
    rx: tonic::Streaming<pb::DialerMessage>,
}

impl Agent {
    async fn open(url: &str, attempt_id: i64, token: &str) -> Result<(Agent, Out), tonic::Status> {
        let mut client = pb::session_client::SessionClient::connect(url.to_string())
            .await
            .unwrap();
        let (tx, out) = mpsc::channel(8);
        tx.send(pb::AgentMessage {
            body: Some(In::Hello(pb::Hello {
                attempt_id,
                token: token.into(),
                agent: "test".into(),
            })),
        })
        .await
        .unwrap();
        let mut rx = client.open(ReceiverStream::new(out)).await?.into_inner();
        let first = rx.message().await?.unwrap().body.unwrap();
        Ok((Agent { tx, rx }, first))
    }

    async fn send(&mut self, body: In) -> Out {
        self.tx
            .send(pb::AgentMessage { body: Some(body) })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.rx.message())
            .await
            .expect("an answer in time")
            .unwrap()
            .unwrap()
            .body
            .unwrap()
    }
}

fn outcome(code: &str) -> pb::Outcome {
    pb::Outcome {
        schema_version: 1,
        disposition: code.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_welcome_lists_the_catalog_of_the_campaign() {
    let (_, url) = start().await;
    let (_, first) = Agent::open(&url, 7, TOKEN).await.unwrap();
    let Out::Welcome(w) = first else {
        panic!("{first:?}")
    };
    assert_eq!((w.attempt_id, w.campaign_id, w.schema_version), (7, 3, 1));
    let codes: Vec<_> = w.dispositions.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"SALE") && codes.contains(&"APPT"),
        "{codes:?}"
    );
    let dnc = w.dispositions.iter().find(|d| d.code == "DNC").unwrap();
    assert!(dnc.flags.contains(&"dnc".to_string()));
}

#[tokio::test]
async fn a_wrong_token_or_a_missing_hello_is_refused() {
    let (_, url) = start().await;
    let e = Agent::open(&url, 7, "guess").await.err().unwrap();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);
    let e = Agent::open(&url, 8, TOKEN).await.err().unwrap();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);

    let mut client = pb::session_client::SessionClient::connect(url)
        .await
        .unwrap();
    let log = pb::AgentMessage {
        body: Some(In::Log(pb::Log {
            level: "info".into(),
            message: "hi".into(),
        })),
    };
    let e = client
        .open(tokio_stream::iter(vec![log]))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn an_outcome_is_checked_stored_once_and_acked() {
    let (memory, url) = start().await;
    let (mut agent, _) = Agent::open(&url, 7, TOKEN).await.unwrap();
    // Logs and events get no answer; the next answer is for the outcome.
    agent
        .tx
        .send(pb::AgentMessage {
            body: Some(In::Event(pb::Event {
                name: "speech".into(),
                json: "{}".into(),
            })),
        })
        .await
        .unwrap();
    let Out::Rejected(r) = agent.send(In::Outcome(outcome("XYZ"))).await else {
        panic!()
    };
    assert!(r.reason.contains("XYZ"), "{}", r.reason);
    let Out::Ack(a) = agent.send(In::Outcome(outcome("APPT"))).await else {
        panic!()
    };
    assert!(!a.duplicate);
    let Out::Ack(a) = agent.send(In::Outcome(outcome("APPT"))).await else {
        panic!()
    };
    assert!(a.duplicate);
    let Out::Rejected(r) = agent.send(In::Outcome(outcome("SALE"))).await else {
        panic!()
    };
    assert!(r.reason.contains("another outcome"));
    let stored = memory.outcomes.lock().unwrap()[&7].clone();
    assert_eq!(stored.disposition.unwrap().code, "APPT");
}

#[tokio::test]
async fn a_callback_and_a_dnc_request_reach_the_store() {
    let (memory, url) = start().await;
    let (mut agent, _) = Agent::open(&url, 7, TOKEN).await.unwrap();
    let callback = pb::Outcome {
        callback: Some(pb::Callback {
            local_time: "2026-03-02T09:30".into(),
            zone: String::new(),
        }),
        ..outcome("CALLBK")
    };
    let Out::Ack(_) = agent.send(In::Outcome(callback)).await else {
        panic!()
    };
    let cb = memory.outcomes.lock().unwrap()[&7]
        .callback
        .clone()
        .unwrap();
    // The zone of the lead: 09:30 EST is 14:30 UTC.
    assert_eq!(
        cb.at_utc,
        Utc.with_ymd_and_hms(2026, 3, 2, 14, 30, 0).unwrap()
    );

    memory.outcomes.lock().unwrap().clear();
    let dnc = pb::Outcome {
        schema_version: 1,
        dnc: Some(pb::Dnc {
            scope: String::new(),
        }),
        summary: "please stop calling".into(),
        ..Default::default()
    };
    let Out::Ack(a) = agent.send(In::Outcome(dnc)).await else {
        panic!()
    };
    assert_eq!(a.cancelled, 1);
    let stored = memory.outcomes.lock().unwrap()[&7].clone();
    assert_eq!(stored.dnc, Some(ad_disposition::DncScope::System));
    assert_eq!(
        serde_json::to_value(&stored).unwrap()["summary"],
        "please stop calling"
    );
}

#[test]
fn tokens_are_random_and_the_digest_is_sha256() {
    let (a, b) = (new_token(), new_token());
    assert_ne!(a, b);
    assert_eq!(a.len(), 64);
    // SHA-256 of "abc".
    assert_eq!(
        digest("abc")[..4],
        [0xba, 0x78, 0x16, 0xbf],
        "the digest is SHA-256"
    );
}
