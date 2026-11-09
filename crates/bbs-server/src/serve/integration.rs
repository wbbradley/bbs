//! Hub's integration stream (`bbs.integration.v1`) on its own listener,
//! `bbs serve --integration-listen`. The adapter connects with an integration
//! credential, sends its hello, and the stream carries both directions: BBS
//! publishes its catalog and hands off outbound work; the adapter offers
//! incoming messages, reports results and asks directory queries. Rules and
//! storage are in `service::integration`; this runs the stream.
//!
//! One stream per integration: a new connection supersedes the older one,
//! which ends. Outbound work is claimed (marked attempted and committed)
//! before it is put on the stream, and is never offered again (D072).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use super::{BoxStream, CancellableStream, State};
use crate::service::integration::{self, InvalidFrame, TOKEN_METADATA};
use bbs_core::hub_proto::integration as pb;
use pb::adapter_frame::Payload as In;
use pb::bbs_frame::Payload as Out;
use pb::integration_server::{Integration as IntegrationRpc, IntegrationServer};

/// How long a new stream may take to send its hello.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often an open stream rechecks its credential and the catalog, which
/// local administration can change without the server noticing.
const RECHECK: Duration = Duration::from_secs(5);
/// Frames queued for the adapter before senders wait.
const OUTBOUND_FRAMES: usize = 16;

/// Open streams by integration ID; setting one's flag supersedes it.
type Sessions = Arc<Mutex<HashMap<String, Arc<watch::Sender<bool>>>>>;

#[derive(Clone)]
pub(super) struct Integration {
    state: Arc<State>,
    sessions: Sessions,
}

/// The integration service, bounded to Hub's 4 MiB frames each way.
pub(super) fn service(state: Arc<State>) -> IntegrationServer<Integration> {
    IntegrationServer::new(Integration {
        state,
        sessions: Arc::default(),
    })
    .max_decoding_message_size(integration::MAX_FRAME_BYTES)
    .max_encoding_message_size(integration::MAX_FRAME_BYTES)
}

/// Statuses for errors outside `State::call`: authentication (a hello for
/// another integration) is PermissionDenied, protocol violations
/// InvalidArgument, the rest FailedPrecondition.
fn status(err: anyhow::Error) -> Status {
    if err
        .downcast_ref::<crate::auth::AuthenticationError>()
        .is_some()
    {
        Status::permission_denied(err.to_string())
    } else if err.downcast_ref::<InvalidFrame>().is_some() {
        Status::invalid_argument(err.to_string())
    } else {
        Status::failed_precondition(format!("{err:#}"))
    }
}

fn frame(payload: Out) -> Result<pb::BbsFrame, Status> {
    Ok(pb::BbsFrame {
        payload: Some(payload),
    })
}

impl Integration {
    /// Register a new stream for `id`, ending any older one.
    fn supersede(&self, id: &str) -> Arc<watch::Sender<bool>> {
        let mine = Arc::new(watch::Sender::new(false));
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = sessions.insert(id.to_string(), mine.clone()) {
            old.send_replace(true);
        }
        mine
    }

    fn release(&self, id: &str, mine: &Arc<watch::Sender<bool>>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if sessions.get(id).is_some_and(|s| Arc::ptr_eq(s, mine)) {
            sessions.remove(id);
        }
    }
}

#[tonic::async_trait]
impl IntegrationRpc for Integration {
    type ConnectStream = BoxStream<pb::BbsFrame>;

    async fn connect(
        &self,
        req: Request<Streaming<pb::AdapterFrame>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let state = self.state.clone();
        let peer = req.remote_addr().map(|addr| addr.ip());
        let token = req
            .metadata()
            .get(TOKEN_METADATA)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // A limited address still gets through with a valid credential.
        let limited = state.check_auth_rate(peer).err();
        let t = token.clone();
        let id = match state
            .call(move |conn| integration::authenticate(conn, t.as_deref()))
            .await
        {
            Ok(id) => id,
            Err(status) => {
                state.record_auth_failure(peer, &status);
                tracing::warn!(?peer, "integration stream refused: {}", status.message());
                return Err(limited.unwrap_or(status));
            }
        };
        let token = token.unwrap_or_default();
        let mut inbound = req.into_inner();
        let first = tokio::time::timeout(HELLO_WAIT, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no hello within 10 seconds"))??;
        let hello = match first.and_then(|f| f.payload) {
            Some(In::Hello(hello)) => hello,
            _ => return Err(Status::invalid_argument("the first frame must be hello")),
        };
        integration::check_hello(&id, &hello).map_err(status)?;
        let i = id.clone();
        let bbs_hello = state.call(move |conn| integration::hello(conn, &i)).await?;
        let superseded = self.supersede(&id);
        tracing::info!(
            integration = id,
            build = hello.build,
            ?peer,
            "integration connected"
        );

        let (tx, rx) = mpsc::channel(OUTBOUND_FRAMES);
        // Room for the hello is certain: the channel is new.
        let _ = tx.try_send(frame(Out::Hello(bbs_hello)));
        let (terminal, terminal_rx) = oneshot::channel();
        let this = self.clone();
        tokio::spawn(async move {
            let mut ended = superseded.subscribe();
            let session = Session {
                state: state.clone(),
                id: id.clone(),
                tx,
            };
            let result = tokio::select! {
                r = session.read(inbound) => r,
                r = session.write(&token) => r,
                _ = state.stopping() => Err(Status::unavailable("server is shutting down")),
                _ = ended.wait_for(|s| *s) => {
                    Err(Status::aborted("superseded by a newer stream for this integration"))
                }
                _ = session.tx.closed() => Ok(()),
            };
            match result {
                Ok(()) => tracing::info!(integration = id, "integration disconnected"),
                Err(status) => {
                    tracing::warn!(
                        integration = id,
                        "integration stream ended: {}",
                        status.message()
                    );
                    let _ = terminal.send(status);
                }
            }
            this.release(&id, &superseded);
        });
        Ok(Response::new(Box::pin(CancellableStream {
            messages: ReceiverStream::new(rx),
            terminal: Some(terminal_rx),
            done: false,
            protocol: self.state.protocol.clone(),
        }) as BoxStream<pb::BbsFrame>))
    }
}

/// One open stream.
struct Session {
    state: Arc<State>,
    id: String,
    tx: mpsc::Sender<Result<pb::BbsFrame, Status>>,
}

impl Session {
    /// Queue a frame; false when the stream has gone.
    async fn send(&self, payload: Out) -> bool {
        self.tx.send(frame(payload)).await.is_ok()
    }

    /// Handle the adapter's frames until it ends the stream.
    async fn read(&self, mut inbound: Streaming<pb::AdapterFrame>) -> Result<(), Status> {
        while let Some(f) = inbound.message().await? {
            let id = self.id.clone();
            let reply = match f.payload {
                None => {
                    return Err(Status::invalid_argument(
                        "a frame has no payload, or one this server does not know",
                    ));
                }
                Some(In::Hello(_)) => return Err(Status::invalid_argument("repeated hello")),
                Some(In::Incoming(offer)) => {
                    let result = self
                        .state
                        .call(move |conn| Ok(integration::import(conn, &id, &offer)))
                        .await?;
                    if result.disposition == pb::HandoffDisposition::Received as i32 {
                        self.state.changed();
                    }
                    Some(Out::IncomingResult(result))
                }
                Some(In::OutgoingResult(result)) => {
                    self.state
                        .call(move |conn| integration::record_result(conn, &id, &result))
                        .await?;
                    None
                }
                Some(In::Observed(observed)) => {
                    self.state
                        .call(move |conn| integration::observe(conn, &observed))
                        .await?;
                    None
                }
                Some(In::LocalDirectoryQuery(query)) => Some(Out::LocalDirectoryResult(
                    self.state
                        .call(move |conn| integration::local_directory(conn, &id, &query))
                        .await?,
                )),
                Some(In::RemoteDirectoryResult(result)) => {
                    integration::check_remote_directory(&result).map_err(status)?;
                    None
                }
                Some(In::Ping(ping)) => Some(Out::Pong(pb::Pong { nonce: ping.nonce })),
                Some(In::Pong(_)) => None,
            };
            if let Some(reply) = reply
                && !self.send(reply).await
            {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Publish the catalog whenever its revision changes and hand off pending
    /// work, after every committed change and every [`RECHECK`], which also
    /// rechecks the credential.
    async fn write(&self, token: &str) -> Result<(), Status> {
        let mut changes = self.state.notify.subscribe();
        let mut recheck = tokio::time::interval(RECHECK);
        recheck.tick().await;
        let mut published = None;
        loop {
            changes.borrow_and_update();
            let id = self.id.clone();
            let catalog = self
                .state
                .call(move |conn| integration::catalog(conn, &id))
                .await?;
            if published != Some(catalog.revision) {
                published = Some(catalog.revision);
                if !self.send(Out::Published(catalog)).await {
                    return Ok(());
                }
            }
            if !self.hand_off().await? {
                return Ok(());
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = recheck.tick() => {
                    let (id, token) = (self.id.clone(), token.to_string());
                    self.state
                        .call(move |conn| integration::recheck(conn, &id, &token))
                        .await?;
                }
            }
        }
    }

    /// Hand off pending work, one handoff at a time: wait for room on the
    /// stream, then in one database call claim it (committed as attempted),
    /// put it on the stream and record `sent`. The call finishes even if the
    /// stream ends meanwhile, so a claim is never left half done. Returns
    /// false when the stream has gone.
    async fn hand_off(&self) -> Result<bool, Status> {
        loop {
            let Ok(permit) = self.tx.clone().reserve_owned().await else {
                return Ok(false);
            };
            let id = self.id.clone();
            let sent = self
                .state
                .call(move |conn| {
                    let Some((outbox, offer)) = integration::claim(conn, &id)? else {
                        return Ok(false);
                    };
                    let handoff = offer.handoff_id.clone();
                    let tx = permit.send(frame(Out::Outgoing(offer)));
                    // The handoff boundary: the frame is on the stream, unless
                    // it closed and dropped it.
                    let sent = !tx.is_closed();
                    integration::handed_off(conn, outbox, sent)?;
                    tracing::info!(integration = id, handoff, sent, "handed off message");
                    Ok(true)
                })
                .await?;
            if !sent {
                return Ok(true);
            }
        }
    }
}

#[cfg(test)]
mod tests;
