use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use ad_core::{AttemptId, Clock};
use ad_db::{OutcomeError, Recorded, Session};
use ad_disposition::{check, AgentOutcome, AnsweredBy, CallbackRequest, Catalog, Flags, Report};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::pb::{self, agent_message::Body as In, dialer_message::Body as Out};
use crate::token::digest;

/// Where the sessions and the outcomes are kept: Postgres in the api role.
pub trait Store: Send + Sync + 'static {
    fn session(
        &self,
        attempt_id: AttemptId,
        digest: [u8; 32],
    ) -> impl Future<Output = Result<Session, OutcomeError>> + Send;

    fn record(
        &self,
        session: &Session,
        digest: [u8; 32],
        outcome: &AgentOutcome,
    ) -> impl Future<Output = Result<Recorded, OutcomeError>> + Send;
}

impl Store for ad_db::PgOutcomeRepo {
    async fn session(
        &self,
        attempt_id: AttemptId,
        digest: [u8; 32],
    ) -> Result<Session, OutcomeError> {
        ad_db::PgOutcomeRepo::session(self, attempt_id, &digest).await
    }

    async fn record(
        &self,
        session: &Session,
        digest: [u8; 32],
        outcome: &AgentOutcome,
    ) -> Result<Recorded, OutcomeError> {
        ad_db::PgOutcomeRepo::record(self, session, &digest, outcome).await
    }
}

pub struct SessionService<S> {
    store: Arc<S>,
    clock: Arc<dyn Clock>,
}

/// The gRPC service. `clock` dates the check of a callback time.
pub fn service<S: Store>(
    store: S,
    clock: Arc<dyn Clock>,
) -> pb::session_server::SessionServer<SessionService<S>> {
    pb::session_server::SessionServer::new(SessionService {
        store: Arc::new(store),
        clock,
    })
}

/// Serve the session stream on `addr` until the process stops.
pub async fn serve<S: Store>(
    addr: SocketAddr,
    store: S,
    clock: Arc<dyn Clock>,
) -> Result<(), tonic::transport::Error> {
    tracing::info!(%addr, "session stream listening");
    tonic::transport::Server::builder()
        .add_service(service(store, clock))
        .serve(addr)
        .await
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The report of an `Outcome` message.
pub fn report(o: &pb::Outcome) -> Report {
    Report {
        schema_version: o.schema_version,
        disposition: non_empty(&o.disposition),
        answered_by: match o.answered_by() {
            pb::AnsweredBy::Human => AnsweredBy::Human,
            pb::AnsweredBy::Machine => AnsweredBy::Machine,
            pb::AnsweredBy::Unknown => AnsweredBy::Unknown,
        },
        callback: o.callback.as_ref().map(|c| CallbackRequest {
            at: c.local_time.clone(),
            zone: non_empty(&c.zone),
        }),
        dnc: o.dnc.as_ref().map(|d| non_empty(&d.scope)),
        transfer_to: non_empty(&o.transfer_to),
        summary: non_empty(&o.summary),
        disclosures: o.disclosures.clone(),
    }
}

fn flag_names(f: &Flags) -> Vec<String> {
    [
        (f.human_answered, "human_answered"),
        (f.sale, "sale"),
        (f.dnc, "dnc"),
        (f.customer_contact, "customer_contact"),
        (f.not_interested, "not_interested"),
        (f.unworkable, "unworkable"),
        (f.scheduled_callback, "scheduled_callback"),
        (f.completed, "completed"),
        (f.answering_machine, "answering_machine"),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .map(|(_, name)| name.to_string())
    .collect()
}

fn reply(body: Out) -> Result<pb::DialerMessage, Status> {
    Ok(pb::DialerMessage { body: Some(body) })
}

fn rejected(reason: impl Into<String>) -> Result<pb::DialerMessage, Status> {
    reply(Out::Rejected(pb::Rejected {
        reason: reason.into(),
    }))
}

fn status(e: OutcomeError) -> Status {
    match e {
        OutcomeError::Session => Status::unauthenticated("unknown session"),
        other => Status::unavailable(other.to_string()),
    }
}

async fn outcome<S: Store>(
    store: &S,
    session: &Session,
    digest: [u8; 32],
    catalog: &Catalog,
    message: &pb::Outcome,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<pb::DialerMessage, Status> {
    let checked = match check(
        &report(message),
        catalog,
        session.lead_timezone.as_deref(),
        now,
    ) {
        Ok(o) => o,
        Err(e) => return rejected(e.to_string()),
    };
    match store.record(session, digest, &checked).await {
        Ok(Recorded::New { cancelled, settled }) => {
            tracing::info!(
                attempt = session.attempt_id.0,
                disposition = checked.disposition.as_ref().map(|d| d.code.as_str()),
                dnc = checked.dnc.map(|s| s.as_str()),
                callback = checked.callback.as_ref().map(|c| c.at_utc.to_rfc3339()),
                cancelled,
                settled,
                "agent outcome"
            );
            reply(Out::Ack(pb::Ack {
                duplicate: false,
                settled,
                cancelled,
            }))
        }
        Ok(Recorded::Duplicate) => reply(Out::Ack(pb::Ack {
            duplicate: true,
            ..Default::default()
        })),
        Err(OutcomeError::Conflict) => rejected("the attempt has another outcome"),
        Err(e) => Err(status(e)),
    }
}

#[tonic::async_trait]
impl<S: Store> pb::session_server::Session for SessionService<S> {
    type OpenStream = ReceiverStream<Result<pb::DialerMessage, Status>>;

    async fn open(
        &self,
        request: Request<Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::OpenStream>, Status> {
        let mut inbound = request.into_inner();
        let Some(In::Hello(hello)) = inbound.message().await?.and_then(|m| m.body) else {
            return Err(Status::invalid_argument("send hello first"));
        };
        let digest = digest(&hello.token);
        let session = self
            .store
            .session(AttemptId(hello.attempt_id), digest)
            .await
            .map_err(status)?;
        let catalog = Catalog::new(session.dispositions.clone())
            .map_err(|e| Status::failed_precondition(format!("the catalog: {e}")))?;
        tracing::info!(
            attempt = session.attempt_id.0,
            agent = hello.agent,
            "agent session open"
        );

        let (tx, rx) = mpsc::channel(8);
        let welcome = pb::Welcome {
            attempt_id: session.attempt_id.0,
            campaign_id: session.campaign_id.0,
            schema_version: ad_disposition::SCHEMA_VERSION,
            dispositions: catalog
                .all()
                .iter()
                .map(|d| pb::Disposition {
                    code: d.code.clone(),
                    name: d.name.clone(),
                    flags: flag_names(&d.flags),
                })
                .collect(),
        };
        let _ = tx.send(reply(Out::Welcome(welcome))).await;

        let store = self.store.clone();
        let clock = self.clock.clone();
        tokio::spawn(async move {
            let attempt = session.attempt_id.0;
            loop {
                let message = match inbound.message().await {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(attempt, error = %e, "agent session read failed");
                        break;
                    }
                };
                let answer = match message.body {
                    Some(In::Outcome(o)) => Some(
                        outcome(&*store, &session, digest, &catalog, &o, clock.now_utc()).await,
                    ),
                    Some(In::Event(e)) => {
                        tracing::info!(attempt, name = e.name, data = e.json, "agent event");
                        None
                    }
                    Some(In::Log(l)) => {
                        tracing::info!(attempt, level = l.level, "agent log: {}", l.message);
                        None
                    }
                    Some(In::Hello(_)) => Some(rejected("the session has its hello")),
                    None => Some(rejected("the message is empty")),
                };
                if let Some(answer) = answer {
                    if tx.send(answer).await.is_err() {
                        break;
                    }
                }
            }
            tracing::info!(attempt, "agent session closed");
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
