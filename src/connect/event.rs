//! Everything the app reacts to, merged into one channel: key presses and
//! resizes, a clock tick, and replies from the server.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use super::theme::Role;
use crate::backend::Backend;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

pub enum Event {
    Key(KeyEvent),
    /// Text the terminal reported as one bracketed paste.
    Paste(String),
    Resize,
    Tick,
    Data(Data),
}

/// Work for the server, done off the drawing path.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Agents,
    AgentsAt {
        effort_revision: u64,
    },
    Groups,
    /// Stream a feed, replacing any stream with the same id.
    Watch {
        id: u64,
        req: proto::FeedRequest,
    },
    /// Stream agent activity, replacing any stream with the same id. Without
    /// `after_id`, open on the newest `backlog` records, follow from there,
    /// and deliver older retained history once, whole (`ActivityBackfill`).
    WatchActivity {
        id: u64,
        req: proto::ActivityRequest,
    },
    /// Stop a stream.
    Unwatch(u64),
    /// Send a message; `id` identifies this send independently of the screen.
    Send {
        id: u64,
        req: proto::SendRequest,
    },
    /// A control send has a server request ID; it is not a composer send.
    SetEffort {
        id: u64,
        agent: String,
        value: String,
    },
    /// One owned agent's side state (side-questions.v1), for the screen `id`.
    SideReport {
        id: u64,
        agent: String,
    },
    /// The owner's side question (`/btw`) to an agent, for the screen `id`.
    AskSide {
        id: u64,
        agent: String,
        question: String,
        /// Continue the side conversation whose latest question this is.
        follows: Option<i64>,
    },
    /// Cancel the owner's open side question `side`, for the screen `id`;
    /// with `end`, also end its side conversation.
    CancelSide {
        id: u64,
        side: i64,
        end: bool,
    },
    /// The owner's reply to an agent's questions, for the screen `id`.
    AnswerOwnerInput {
        id: u64,
        req: proto::AnswerOwnerInputRequest,
    },
    /// Change a group for the screen `id`; the group list follows a success.
    Admin {
        id: u64,
        op: Admin,
    },
    /// Mark read the human's mail from this sender. The feed shows the result.
    MarkRead(String),
    /// Mark seen an agent's activity through an id. Summaries show the result.
    MarkSeen {
        agent: String,
        through_id: i64,
    },
    /// Stream per-agent summaries, replacing any stream with the same id.
    WatchSummary(u64),
    /// The human's personal blocker queue (blockers-mine.v1), for app `id`.
    Blockers {
        id: u64,
    },
    /// A human's answer to one exact blocker version, for the screen `id`.
    AnswerBlocker {
        id: u64,
        req: proto::AnswerBlockerRequest,
    },
    /// Cancel one exact blocker version, for the screen `id`.
    CancelBlocker {
        id: u64,
        req: proto::CancelBlockerRequest,
    },
    /// A page of the human's agent notes changed after a revision (agent-notes.v1).
    ListNotes {
        id: u64,
        after: u64,
    },
    /// Store the human's note on an agent; never retried here.
    PutNote {
        id: u64,
        agent_id: i64,
        body: String,
    },
}

/// One group change, as the CLI's `bbs group` commands make it.
#[derive(Clone, Debug, PartialEq)]
pub enum Admin {
    Create(proto::GroupRegisterRequest),
    Describe(proto::GroupDescribeRequest),
    Delete(proto::GroupRequest),
    Add(proto::GroupAddRequest),
    Remove(proto::GroupRemoveRequest),
    Writer(proto::SetGroupWriterRequest),
}

/// A successful admin reply carries typed categories, resolved on the UI thread.
#[derive(Clone, Debug)]
pub struct Notice(pub Vec<(String, Role)>);

impl From<&str> for Notice {
    fn from(text: &str) -> Self {
        Self(vec![(text.to_string(), Role::Text)])
    }
}

impl Notice {
    fn text(mut self, text: &str) -> Self {
        self.0.push((text.to_string(), Role::Text));
        self
    }

    fn group(mut self, name: &str) -> Self {
        self.0.push((format!("@{name}"), Role::Group));
        self
    }

    fn agent(mut self, name: &str) -> Self {
        self.0.push((name.to_string(), Role::Agent));
        self
    }

    fn agents(mut self, names: &[String]) -> Self {
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                self = self.text(", ");
            }
            self = self.agent(name);
        }
        self
    }
}

/// Preserve definite refusal separately from a reply lost after delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffortSendError {
    Rejected(String),
    Unknown(String),
}

/// Why a side call (agent-interactions.v1) failed, by the server's status
/// code rather than its message text.
#[derive(Clone, Debug, PartialEq)]
pub enum SideError {
    /// The server lacks the RPC or the capability: never ask again.
    Unsupported(String),
    /// A definite refusal (not the owner, no such agent, invalid answer).
    Refused(String),
    /// Possibly transient; a poll retries, a reply is not known to have landed.
    Transient(String),
}

/// How long a side call (agent-interactions.v1) may take.
const SIDE_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// A side call, bounded by SIDE_CALL_TIMEOUT, with its error typed.
async fn bounded<T>(
    call: impl std::future::Future<Output = anyhow::Result<T>>,
    local: bool,
) -> Result<T, SideError> {
    match tokio::time::timeout(SIDE_CALL_TIMEOUT, call).await {
        Ok(result) => result.map_err(|e| side_error(local, e)),
        Err(_) => Err(SideError::Transient(format!(
            "no reply within {} seconds",
            SIDE_CALL_TIMEOUT.as_secs()
        ))),
    }
}

fn side_error(local: bool, error: anyhow::Error) -> SideError {
    use tonic::Code;
    let text = format!("{error:#}");
    // A local board's momentary writer is not a refusal.
    let busy = error.chain().any(|cause| {
        cause.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
            matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
        })
    });
    if busy {
        return SideError::Transient(text);
    }
    match error
        .downcast_ref::<tonic::Status>()
        .map(tonic::Status::code)
    {
        Some(Code::Unimplemented | Code::FailedPrecondition) => SideError::Unsupported(text),
        Some(
            Code::InvalidArgument
            | Code::NotFound
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::OutOfRange,
        ) => SideError::Refused(text),
        // A local board answers from its own database: no transport to fail.
        _ if local => SideError::Refused(text),
        _ => SideError::Transient(text),
    }
}

/// A failed blocker poll. The server reports an ordinary refusal (a retired
/// agent, no such group) with the same code as a missing capability, so a
/// failed precondition asks once what the server offers; only a server
/// without the requested scope capability, or without the RPC, is an old server.
async fn blocker_poll_error(
    backend: &mut Backend,
    local: bool,
    error: anyhow::Error,
    capability: &str,
) -> SideError {
    let precondition = error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|s| s.code() == tonic::Code::FailedPrecondition);
    match side_error(local, error) {
        SideError::Unsupported(text) if precondition => {
            match bounded(backend.server_info(), local).await {
                Ok(info) if info.capabilities.iter().any(|c| c == capability) => {
                    SideError::Refused(text)
                }
                Ok(_) => SideError::Unsupported(text),
                // Unknown: the next tick asks again.
                Err(_) => SideError::Transient(text),
            }
        }
        other => other,
    }
}

/// A blocker answer or cancel is never how a missing feature shows: a
/// refusal stays a refusal, and the panel stays usable.
fn blocker_write_error(error: SideError) -> SideError {
    match error {
        SideError::Unsupported(text) => SideError::Refused(text),
        other => other,
    }
}

fn effort_send_error(local: bool, error: anyhow::Error) -> EffortSendError {
    use tonic::Code;
    let rejected = local
        || error.downcast_ref::<tonic::Status>().is_some_and(|s| {
            matches!(
                s.code(),
                Code::InvalidArgument
                    | Code::NotFound
                    | Code::AlreadyExists
                    | Code::PermissionDenied
                    | Code::Unauthenticated
                    | Code::FailedPrecondition
                    | Code::OutOfRange
                    | Code::Unimplemented
            )
        });
    let text = format!("{error:#}");
    if rejected {
        EffortSendError::Rejected(text)
    } else {
        EffortSendError::Unknown(text)
    }
}

/// A server reply; errors are messages to show.
pub enum Data {
    Agents(Result<proto::ListAgentsResponse, String>),
    AgentsAt {
        effort_revision: u64,
        result: Result<proto::ListAgentsResponse, String>,
    },
    Groups(Result<proto::GroupListResponse, String>),
    /// A snapshot of the feed stream `id`. Transient remote failures reconnect automatically.
    Feed {
        id: u64,
        result: Result<Vec<proto::Message>, String>,
    },
    /// A batch of the activity stream `id`. Transient remote failures reconnect automatically.
    Activity {
        id: u64,
        result: Result<Vec<proto::Activity>, String>,
    },
    /// History watermark established before subscribing to activity replay,
    /// with the newest activity record when there is one.
    ActivityBaseline {
        id: u64,
        through_id: i64,
        record: Option<String>,
    },
    /// The activity stream `id`'s retained history older than its opening
    /// window, oldest first, delivered once when complete.
    ActivityBackfill {
        id: u64,
        result: Result<Vec<proto::Activity>, String>,
    },
    /// Summaries from the stream `id`. Transient remote failures reconnect automatically.
    Summary {
        id: u64,
        result: Result<Vec<proto::AgentSummary>, String>,
    },
    /// The outcome of the send `id`, even if its screen has closed.
    Sent {
        id: u64,
        result: Result<(), String>,
    },
    EffortSent {
        id: u64,
        agent: String,
        result: Result<Option<i64>, EffortSendError>,
    },
    SideReport {
        id: u64,
        result: Result<Box<proto::SideReportResponse>, SideError>,
    },
    OwnerAnswered {
        id: u64,
        request_id: i64,
        result: Result<(), SideError>,
    },
    /// The new side question's id.
    SideAsked {
        id: u64,
        result: Result<i64, SideError>,
    },
    SideCancelled {
        id: u64,
        side: i64,
        result: Result<(), SideError>,
    },
    /// Completion of an actual read request, for the local summary fence.
    MarkedRead {
        agent: String,
        result: Result<(), String>,
    },
    /// What a group change did, for the screen `id`.
    Admin {
        id: u64,
        result: Result<Notice, String>,
    },
    /// The first page of the screen `id`'s open blockers.
    Blockers {
        id: u64,
        result: Result<Box<proto::ListBlockersResponse>, SideError>,
    },
    /// The outcome of the blocker write `key`, for the screen `id`.
    BlockerWritten {
        id: u64,
        key: String,
        result: Result<Box<proto::BlockerMutationResponse>, SideError>,
    },
    /// The newest agent-note revision a summary stream announced; `None`
    /// from a server without agent notes.
    NotesRevision {
        id: u64,
        revision: Option<u64>,
    },
    /// The note page `id`.
    NotesListed {
        id: u64,
        result: Result<proto::ListAgentNotesResponse, SideError>,
    },
    /// The note as stored by the save `id`.
    NoteSaved {
        id: u64,
        result: Result<proto::AgentNote, SideError>,
    },
}

/// What a new server connection needs: each feed stream holds one.
#[derive(Clone)]
pub struct Opener {
    pub config: Config,
    pub dir: PathBuf,
    pub caller: Caller,
}

/// The app's event for a terminal event, if it has one.
fn input(event: event::Event) -> Option<Event> {
    match event {
        event::Event::Key(key) if key.kind == KeyEventKind::Press => Some(Event::Key(key)),
        event::Event::Paste(text) => Some(Event::Paste(text)),
        event::Event::Resize(..) => Some(Event::Resize),
        _ => None,
    }
}

/// Read only from the UI loop: no reader may outlive terminal ownership.
pub fn poll_input() -> std::io::Result<Option<Event>> {
    if event::poll(Duration::ZERO)? {
        return event::read().map(input);
    }
    Ok(None)
}

/// A tick each second, for the status bar's clock.
pub fn spawn_ticks(tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if tx.send(Event::Tick).is_err() {
                return;
            }
        }
    });
}

/// Answer requests one at a time with one server connection, and run each
/// feed stream as a task with a connection of its own.
pub async fn serve(
    mut backend: Backend,
    opener: Opener,
    mut requests: UnboundedReceiver<Request>,
    tx: UnboundedSender<Event>,
) {
    let mut streams: HashMap<u64, JoinHandle<()>> = HashMap::new();
    while let Some(request) = requests.recv().await {
        let error = |e: anyhow::Error| format!("{e:#}");
        let data = match request {
            Request::Agents => Data::Agents(
                backend
                    .list_agents(proto::ListAgentsRequest { all: false })
                    .await
                    .map_err(error),
            ),
            Request::AgentsAt { effort_revision } => Data::AgentsAt {
                effort_revision,
                result: backend
                    .list_agents(proto::ListAgentsRequest { all: false })
                    .await
                    .map_err(error),
            },
            Request::Groups => Data::Groups(backend.group_list().await.map_err(error)),
            Request::Send { id, req } => Data::Sent {
                id,
                result: backend.send(req).await.map(drop).map_err(error),
            },
            Request::SetEffort { id, agent, value } => {
                let local = matches!(backend, Backend::Local(_));
                Data::EffortSent {
                    id,
                    result: backend
                        .send(proto::SendRequest {
                            to: agent.clone(),
                            body: value,
                            control: Some(proto::Control::SetEffort as i32),
                        })
                        .await
                        .map(|r| r.id)
                        .map_err(|e| effort_send_error(local, e)),
                    agent,
                }
            }
            // Side calls are bounded, so a stalled connection cannot hold
            // ordinary sends behind a background poll; a timeout is
            // transient, and an answer is never retried.
            Request::SideReport { id, agent } => {
                let local = matches!(backend, Backend::Local(_));
                let call = backend.side_report(proto::SideReportRequest {
                    agent,
                    generation: None,
                });
                Data::SideReport {
                    id,
                    result: bounded(call, local).await.map(Box::new),
                }
            }
            Request::AnswerOwnerInput { id, req } => {
                let local = matches!(backend, Backend::Local(_));
                let request_id = req.request_id;
                Data::OwnerAnswered {
                    id,
                    request_id,
                    result: bounded(backend.answer_owner_input(req), local)
                        .await
                        .map(|_| ()),
                }
            }
            Request::AskSide {
                id,
                agent,
                question,
                follows,
            } => {
                let local = matches!(backend, Backend::Local(_));
                let call = backend.ask_side(proto::AskSideRequest {
                    agent,
                    question,
                    follows,
                });
                Data::SideAsked {
                    id,
                    result: bounded(call, local).await.map(|r| r.id),
                }
            }
            Request::CancelSide { id, side, end } => {
                let local = matches!(backend, Backend::Local(_));
                let call = backend.cancel_side(proto::CancelSideRequest { id: side, end });
                Data::SideCancelled {
                    id,
                    side,
                    result: bounded(call, local).await.map(|_| ()),
                }
            }
            Request::Admin { id, op } => {
                let result = admin(&mut backend, op).await.map_err(error);
                let changed = result.is_ok();
                if tx.send(Event::Data(Data::Admin { id, result })).is_err() {
                    break;
                }
                if !changed {
                    continue;
                }
                Data::Groups(backend.group_list().await.map_err(error))
            }
            Request::Blockers { id } => {
                let local = matches!(backend, Backend::Local(_));
                let capability = crate::backend::BLOCKERS_MINE;
                let req = proto::ListBlockersRequest {
                    mine: true,
                    ..Default::default()
                };
                let result =
                    match tokio::time::timeout(SIDE_CALL_TIMEOUT, backend.list_blockers(req)).await
                    {
                        Ok(Ok(list)) => Ok(Box::new(list)),
                        Ok(Err(e)) => {
                            Err(blocker_poll_error(&mut backend, local, e, capability).await)
                        }
                        Err(_) => Err(SideError::Transient(format!(
                            "no reply within {} seconds",
                            SIDE_CALL_TIMEOUT.as_secs()
                        ))),
                    };
                Data::Blockers { id, result }
            }
            // Bounded like side calls; a timeout leaves the write uncertain,
            // and only the screen retries it, with the same key.
            Request::AnswerBlocker { id, req } => {
                let local = matches!(backend, Backend::Local(_));
                let key = req.request_key.clone();
                Data::BlockerWritten {
                    id,
                    key,
                    result: bounded(backend.answer_blocker(req), local)
                        .await
                        .map(Box::new)
                        .map_err(blocker_write_error),
                }
            }
            Request::CancelBlocker { id, req } => {
                let local = matches!(backend, Backend::Local(_));
                let key = req.request_key.clone();
                Data::BlockerWritten {
                    id,
                    key,
                    result: bounded(backend.cancel_blocker(req), local)
                        .await
                        .map(Box::new)
                        .map_err(blocker_write_error),
                }
            }
            Request::ListNotes { id, after } => {
                let local = matches!(backend, Backend::Local(_));
                let req = proto::ListAgentNotesRequest {
                    after_revision: after,
                    limit: 0,
                };
                let result =
                    match tokio::time::timeout(SIDE_CALL_TIMEOUT, backend.list_agent_notes(req))
                        .await
                    {
                        Ok(Ok(list)) => Ok(list),
                        Ok(Err(e)) => Err(blocker_poll_error(
                            &mut backend,
                            local,
                            e,
                            crate::backend::AGENT_NOTES,
                        )
                        .await),
                        Err(_) => Err(SideError::Transient(format!(
                            "no reply within {} seconds",
                            SIDE_CALL_TIMEOUT.as_secs()
                        ))),
                    };
                Data::NotesListed { id, result }
            }
            // Bounded; a timeout leaves the save uncertain, and only the human
            // retries it.
            Request::PutNote { id, agent_id, body } => {
                let local = matches!(backend, Backend::Local(_));
                let call = backend.put_agent_note(proto::PutAgentNoteRequest { agent_id, body });
                Data::NoteSaved {
                    id,
                    result: bounded(call, local).await.and_then(|r| {
                        r.note
                            .ok_or_else(|| SideError::Refused("the server stored no note".into()))
                    }),
                }
            }
            Request::MarkRead(from) => Data::MarkedRead {
                agent: from.clone(),
                result: backend
                    .mark_read(proto::MarkReadRequest { from })
                    .await
                    .map(drop)
                    .map_err(error),
            },
            Request::MarkSeen { agent, through_id } => {
                let req = proto::MarkSeenRequest { agent, through_id };
                if let Err(err) = backend.mark_seen(req).await {
                    tracing::warn!("marking activity seen: {err:#}");
                }
                continue;
            }
            Request::WatchSummary(id) => {
                let task = tokio::spawn(watch_summary(opener.clone(), id, tx.clone()));
                if let Some(old) = streams.insert(id, task) {
                    old.abort();
                }
                continue;
            }
            Request::Watch { id, req } => {
                let task = tokio::spawn(watch(opener.clone(), id, req, tx.clone()));
                if let Some(old) = streams.insert(id, task) {
                    old.abort();
                }
                continue;
            }
            Request::WatchActivity { id, req } => {
                let task = tokio::spawn(watch_activity(opener.clone(), id, req, tx.clone()));
                if let Some(old) = streams.insert(id, task) {
                    old.abort();
                }
                continue;
            }
            Request::Unwatch(id) => {
                if let Some(task) = streams.remove(&id) {
                    task.abort();
                }
                continue;
            }
        };
        if tx.send(Event::Data(data)).is_err() {
            break;
        }
    }
    for task in streams.into_values() {
        task.abort();
    }
}

/// Make a group change and describe what it did.
async fn admin(backend: &mut Backend, op: Admin) -> anyhow::Result<Notice> {
    Ok(match op {
        Admin::Create(req) => {
            let group = backend.group_register(req).await?.group;
            Notice::from("Created ").group(&group).text(".")
        }
        Admin::Describe(req) => {
            let cleared = req.description.is_none();
            let group = backend.group_describe(req).await?.group;
            match cleared {
                true => Notice::from("Cleared the description of ")
                    .group(&group)
                    .text("."),
                false => Notice::from("Described ").group(&group).text("."),
            }
        }
        Admin::Delete(req) => {
            let group = backend.group_delete(req).await?.group;
            Notice::from("Deleted ").group(&group).text(".")
        }
        Admin::Add(req) => {
            let r = backend.group_add(req).await?;
            let mut notice = Notice::from("");
            if !r.added.is_empty() {
                notice = notice
                    .text("Added ")
                    .agents(&r.added)
                    .text(" to ")
                    .group(&r.group)
                    .text(".");
            }
            if !r.already.is_empty() {
                if !r.added.is_empty() {
                    notice = notice.text(" ");
                }
                notice = notice
                    .text("Already in ")
                    .group(&r.group)
                    .text(": ")
                    .agents(&r.already)
                    .text(".");
            }
            notice
        }
        Admin::Remove(req) => {
            let r = backend.group_remove(req).await?;
            Notice::from("Removed ")
                .agent(&r.agent)
                .text(" from ")
                .group(&r.group)
                .text(".")
        }
        Admin::Writer(req) => {
            let grant = req.grant;
            let r = backend.set_group_writer(req).await?;
            let verb = match (grant, r.changed) {
                (true, true) => " can now write to ",
                (true, false) => " could already write to ",
                (false, true) => " can no longer write to ",
                (false, false) => " could not write to ",
            };
            Notice::from("")
                .agent(&r.agent)
                .text(verb)
                .group(&r.group)
                .text(".")
        }
    })
}

/// Retry only remote read subscriptions. Writes remain single attempts in serve().
struct Reconnect {
    remote: bool,
    delay: Duration,
}

fn transient_connection(error: &anyhow::Error) -> bool {
    use tonic::Code;
    if let Some(status) = error.downcast_ref::<tonic::Status>() {
        return matches!(
            status.code(),
            Code::Unavailable
                | Code::DeadlineExceeded
                | Code::Unknown
                | Code::Internal
                | Code::Cancelled
        );
    }
    error.downcast_ref::<tonic::transport::Error>().is_some()
        && !error
            .chain()
            .any(|cause| cause.is::<tonic::codegen::http::uri::InvalidUri>())
}

impl Reconnect {
    fn new(opener: &Opener) -> Self {
        Self {
            remote: opener.config.server.is_some(),
            delay: Duration::from_secs(1),
        }
    }

    fn received(&mut self) {
        self.delay = Duration::from_secs(1);
    }

    async fn after_error(
        &mut self,
        error: anyhow::Error,
        tx: &UnboundedSender<Event>,
        data: impl FnOnce(String) -> Data,
    ) -> bool {
        let retry = self.remote && transient_connection(&error);
        let text = if retry {
            format!("Connection interrupted; reconnecting… {error:#}")
        } else {
            format!("{error:#}")
        };
        if tx.send(Event::Data(data(text))).is_err() || !retry {
            return false;
        }
        let delay = self.delay;
        self.delay = (self.delay * 2).min(Duration::from_secs(30));
        tokio::select! {
            () = tx.closed() => false,
            () = tokio::time::sleep(delay) => true,
        }
    }
}

/// Refresh feed snapshots after a remote interruption, until closed or refused.
async fn watch(opener: Opener, id: u64, mut req: proto::FeedRequest, tx: UnboundedSender<Event>) {
    let mut reconnect = Reconnect::new(&opener);
    loop {
        let result: anyhow::Result<()> = async {
            let mut backend =
                Backend::open(&opener.config, &opener.dir, opener.caller.clone()).await?;
            let mut feed = backend.watch_feed(req.clone(), false).await?;
            loop {
                let messages = feed.next().await?;
                let newest = messages.iter().map(|m| m.id).max();
                if tx
                    .send(Event::Data(Data::Feed {
                        id,
                        result: Ok(messages),
                    }))
                    .is_err()
                {
                    return Ok(());
                }
                if req.after_id.is_some()
                    && let Some(newest) = newest
                {
                    req.after_id = Some(newest);
                }
                reconnect.received();
            }
        }
        .await;
        let Err(error) = result else { return };
        if !reconnect
            .after_error(error, &tx, |text| Data::Feed {
                id,
                result: Err(text),
            })
            .await
        {
            return;
        }
    }
}

/// Reopen summaries after a remote interruption; each reply replaces the snapshot.
async fn watch_summary(opener: Opener, id: u64, tx: UnboundedSender<Event>) {
    let mut reconnect = Reconnect::new(&opener);
    loop {
        let result: anyhow::Result<()> = async {
            let mut backend =
                Backend::open(&opener.config, &opener.dir, opener.caller.clone()).await?;
            let mut summaries = backend.watch_agent_summary().await?;
            loop {
                let snapshot = summaries.next_snapshot().await?;
                let revision = snapshot.notes_revision;
                let result = Ok(snapshot.agents);
                if tx.send(Event::Data(Data::Summary { id, result })).is_err()
                    || tx
                        .send(Event::Data(Data::NotesRevision { id, revision }))
                        .is_err()
                {
                    return Ok(());
                }
                reconnect.received();
            }
        }
        .await;
        let Err(error) = result else { return };
        if !reconnect
            .after_error(error, &tx, |text| Data::Summary {
                id,
                result: Err(text),
            })
            .await
        {
            return;
        }
    }
}

/// The newest stored activity row for the agent, when there is one.
async fn activity_baseline(
    backend: &mut Backend,
    req: &proto::ActivityRequest,
) -> anyhow::Result<Option<proto::Activity>> {
    let mut latest = backend
        .watch_activity(proto::ActivityRequest {
            backlog: 1,
            after_id: None,
            agent: req.agent.clone(),
        })
        .await?;
    Ok(latest.next().await?.into_iter().next())
}

/// Follow an agent's activity: opened without `after_id`, from its newest
/// window, with older retained history fetched alongside on its own
/// connection. Both end with this task, so closing or reloading the
/// conversation cancels them together.
async fn watch_activity(
    opener: Opener,
    id: u64,
    req: proto::ActivityRequest,
    tx: UnboundedSender<Event>,
) {
    let (cutoff_tx, cutoff) = tokio::sync::oneshot::channel();
    let agent = req.agent.clone();
    tokio::join!(
        follow_activity(opener.clone(), id, req, tx.clone(), cutoff_tx),
        backfill_activity(opener, id, agent, tx, cutoff),
    );
}

/// Resume activity after the last delivered id, with a fresh history
/// watermark; without one yet, start at the newest window and report its
/// oldest id to the backfill when older history may exist.
async fn follow_activity(
    opener: Opener,
    id: u64,
    mut req: proto::ActivityRequest,
    tx: UnboundedSender<Event>,
    cutoff: tokio::sync::oneshot::Sender<i64>,
) {
    let mut cutoff = Some(cutoff);
    let mut reconnect = Reconnect::new(&opener);
    loop {
        let result: anyhow::Result<()> = async {
            let mut backend =
                Backend::open(&opener.config, &opener.dir, opener.caller.clone()).await?;
            if req.after_id.is_none() {
                // The newest window answers once, then the cursor follows
                // from its last id, so nothing after it is missed.
                let tail = backend.watch_activity(req.clone()).await?.next().await?;
                let last = tail.last();
                if tx
                    .send(Event::Data(Data::ActivityBaseline {
                        id,
                        through_id: last.map_or(0, |row| row.id),
                        record: last.map(|row| row.record.clone()),
                    }))
                    .is_err()
                {
                    return Ok(());
                }
                req.after_id = Some(last.map_or(0, |row| row.id));
                // A full window may have older history before it, sent after
                // the window itself.
                let first = (tail.len() >= req.backlog as usize)
                    .then(|| tail.first().map(|row| row.id))
                    .flatten();
                if tx
                    .send(Event::Data(Data::Activity {
                        id,
                        result: Ok(tail),
                    }))
                    .is_err()
                {
                    return Ok(());
                }
                if let (Some(first), Some(cutoff)) = (first, cutoff.take()) {
                    let _ = cutoff.send(first);
                }
                cutoff = None;
                reconnect.received();
            } else {
                let baseline = activity_baseline(&mut backend, &req).await?;
                if tx
                    .send(Event::Data(Data::ActivityBaseline {
                        id,
                        through_id: baseline.as_ref().map_or(0, |row| row.id),
                        record: baseline.map(|row| row.record),
                    }))
                    .is_err()
                {
                    return Ok(());
                }
            }
            let mut activity = backend.watch_activity(req.clone()).await?;
            loop {
                let events = activity.next().await?;
                let newest = events.last().map(|row| row.id);
                if tx
                    .send(Event::Data(Data::Activity {
                        id,
                        result: Ok(events),
                    }))
                    .is_err()
                {
                    return Ok(());
                }
                if let Some(newest) = newest {
                    req.after_id = Some(newest);
                }
                reconnect.received();
            }
        }
        .await;
        let Err(error) = result else { return };
        if !reconnect
            .after_error(error, &tx, |text| Data::Activity {
                id,
                result: Err(text),
            })
            .await
        {
            return;
        }
    }
}

/// Fetch older activity once, without delaying or interrupting live activity.
/// A failure leaves the recent window usable; reloading retries the history.
async fn backfill_activity(
    opener: Opener,
    id: u64,
    agent: Option<String>,
    tx: UnboundedSender<Event>,
    cutoff: tokio::sync::oneshot::Receiver<i64>,
) {
    let Ok(cutoff) = cutoff.await else { return };
    let result = tokio::select! {
        () = tx.closed() => return,
        result = tokio::time::timeout(HISTORY_LOAD_TIMEOUT, older_activity(opener, agent, cutoff)) => {
            match result {
                Ok(result) => result.map_err(|e| format!("{e:#}")),
                Err(_) => Err("did not finish within 30 seconds".to_string()),
            }
        }
    };
    let result = result.map_err(|e| format!("Older activity unavailable; r retries: {e}"));
    let _ = tx.send(Event::Data(Data::ActivityBackfill { id, result }));
}

async fn older_activity(
    opener: Opener,
    agent: Option<String>,
    cutoff: i64,
) -> anyhow::Result<Vec<proto::Activity>> {
    let mut backend = Backend::open(&opener.config, &opener.dir, opener.caller).await?;
    let mut pages = backend
        .watch_activity(proto::ActivityRequest {
            backlog: BACKFILL_PAGE,
            after_id: Some(0),
            agent,
        })
        .await?;
    let mut older = VecDeque::new();
    loop {
        let page = pages.next().await?;
        // The first batch can be empty. Later empty cursor batches are
        // suppressed, so also stop on reaching the sampled opening window.
        let done =
            page.len() < BACKFILL_PAGE as usize || page.last().is_some_and(|row| row.id >= cutoff);
        for row in page.into_iter().take_while(|row| row.id < cutoff) {
            if older.len() == super::timeline::MAX_RECORDS {
                older.pop_front();
            }
            older.push_back(row);
        }
        if done {
            return Ok(older.into_iter().collect());
        }
    }
}

const HISTORY_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Records per backfill page, as the server's cursor pages them.
const BACKFILL_PAGE: u32 = 200;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocker_writes_never_read_as_an_old_server() {
        let write =
            |s: tonic::Status| blocker_write_error(side_error(false, anyhow::Error::new(s)));
        // An answer or cancel refused for any reason stays a refusal.
        for status in [
            tonic::Status::failed_precondition("blocker #1 is not open"),
            tonic::Status::unimplemented("AnswerBlocker"),
            tonic::Status::permission_denied("not yours"),
        ] {
            assert!(matches!(write(status), SideError::Refused(_)));
        }
        assert!(matches!(
            write(tonic::Status::unavailable("reset")),
            SideError::Transient(_)
        ));
    }

    /// A local board refuses an answer to a blocker that is not there; the
    /// refusal is a refusal, and the next poll still works.
    #[tokio::test]
    async fn a_refused_blocker_answer_leaves_polling_working() {
        let (_dir, conn, opener, _) = board();
        // A board-wide poll would expose this unrelated group's blocker.
        conn.execute_batch(
            "INSERT INTO bbs_groups (id, name, created_at) VALUES (1, 'elsewhere', 't');
             INSERT INTO blockers (group_id, creator_id, body, created_at, request_key)
             VALUES (1, 1, 'not in the personal queue', 't', 'unrelated')",
        )
        .unwrap();
        let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
        let poll = || Request::Blockers { id: 60 };
        tx.send(poll()).unwrap();
        tx.send(Request::AnswerBlocker {
            id: 60,
            req: proto::AnswerBlockerRequest {
                id: 999,
                answer: "yes".into(),
                request_key: "k1".into(),
            },
        })
        .unwrap();
        tx.send(poll()).unwrap();
        drop(tx);
        serve(backend, opener, rx, events).await;
        let mut next = || match replies.try_recv() {
            Ok(Event::Data(data)) => data,
            _ => panic!("expected data"),
        };
        assert!(
            matches!(next(), Data::Blockers { id: 60, result: Ok(list) } if list.active_count == 0)
        );
        assert!(matches!(
            next(),
            Data::BlockerWritten { id: 60, ref key, result: Err(SideError::Refused(_)) } if key == "k1"
        ));
        assert!(matches!(
            next(),
            Data::Blockers {
                id: 60,
                result: Ok(_)
            }
        ));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "no mail"
        );
    }

    #[test]
    fn side_errors_are_classified_by_status_code_not_text() {
        let status = |s: tonic::Status| side_error(false, anyhow::Error::new(s));
        assert!(matches!(
            status(tonic::Status::unimplemented("x")),
            SideError::Unsupported(_)
        ));
        // The server reports a missing required capability this way.
        assert!(matches!(
            status(tonic::Status::failed_precondition("anything")),
            SideError::Unsupported(_)
        ));
        for refused in [
            tonic::Status::permission_denied("x"),
            tonic::Status::not_found("x"),
            tonic::Status::invalid_argument("x"),
        ] {
            assert!(matches!(status(refused), SideError::Refused(_)));
        }
        // Text that merely mentions the capability is not a capability error.
        assert!(matches!(
            status(tonic::Status::unavailable(
                "agent-interactions.v1 is unavailable"
            )),
            SideError::Transient(_)
        ));
        assert!(matches!(
            side_error(false, anyhow::anyhow!("reset")),
            SideError::Transient(_)
        ));
        assert!(matches!(
            side_error(true, anyhow::anyhow!("not the owner")),
            SideError::Refused(_)
        ));
        // A local board's busy or locked database is momentary.
        for code in [5, 6] {
            let busy = anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
            .context("reading side state");
            assert!(
                matches!(side_error(true, busy), SideError::Transient(_)),
                "{code}"
            );
        }
    }
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn reconnect_classifies_codes_without_matching_error_text() {
        use tonic::Code;
        for code in [
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::Unknown,
            Code::Internal,
            Code::Cancelled,
        ] {
            assert!(transient_connection(&anyhow::Error::new(
                tonic::Status::new(code, "permission denied")
            )));
        }
        for code in [
            Code::Unauthenticated,
            Code::PermissionDenied,
            Code::FailedPrecondition,
            Code::Unimplemented,
            Code::NotFound,
            Code::InvalidArgument,
            Code::ResourceExhausted,
            Code::DataLoss,
        ] {
            assert!(!transient_connection(&anyhow::Error::new(
                tonic::Status::new(code, "h2 connection lost")
            )));
        }
        assert!(!transient_connection(&anyhow::anyhow!(
            "h2 connection lost"
        )));
        let invalid =
            tonic::transport::Endpoint::from_shared("http://bad host".to_string()).unwrap_err();
        assert!(!transient_connection(&anyhow::Error::new(invalid)));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_backoff_is_capped_resettable_and_cancelled_when_closed() {
        let (_dir, _conn, mut opener, _) = board();
        opener.config.server = Some("localhost:1".into());
        let mut reconnect = Reconnect::new(&opener);
        let (tx, mut rx) = unbounded_channel();
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            let before = tokio::time::Instant::now();
            assert!(
                reconnect
                    .after_error(tonic::Status::unavailable("offline").into(), &tx, |e| {
                        Data::Summary {
                            id: 1,
                            result: Err(e),
                        }
                    })
                    .await
            );
            assert_eq!(before.elapsed(), Duration::from_secs(seconds));
            assert!(matches!(
                rx.recv().await,
                Some(Event::Data(Data::Summary { result: Err(_), .. }))
            ));
        }
        reconnect.received();
        assert_eq!(reconnect.delay, Duration::from_secs(1));
        // An Unwatch aborts the task; receiver shutdown also ends a pending wait.
        let task = tokio::spawn(async move {
            reconnect
                .after_error(tonic::Status::unavailable("offline").into(), &tx, |e| {
                    Data::Summary {
                        id: 1,
                        result: Err(e),
                    }
                })
                .await
        });
        rx.recv().await.unwrap();
        drop(rx);
        assert!(!task.await.unwrap());
        opener.config.server = None;
        let mut local = Reconnect::new(&opener);
        let (tx, _rx) = unbounded_channel();
        assert!(
            !local
                .after_error(tonic::Status::unavailable("offline").into(), &tx, |e| {
                    Data::Summary {
                        id: 1,
                        result: Err(e),
                    }
                })
                .await
        );
    }

    /// A server whose blocker poll fails as a refusal or a missing capability
    /// would: a failed precondition either way.
    mod blocker_capability {
        use super::*;
        use std::convert::Infallible;
        use std::task::{Context, Poll};
        use tonic::{
            Response, Status,
            body::Body,
            codegen::{BoxFuture, Service, http},
        };

        #[derive(Clone)]
        struct Server {
            offers: bool,
            capability: &'static str,
        }

        impl tonic::server::UnaryService<proto::Empty> for Server {
            type Response = proto::ServerInfoResponse;
            type Future = BoxFuture<Response<proto::ServerInfoResponse>, Status>;
            fn call(&mut self, _: tonic::Request<proto::Empty>) -> Self::Future {
                // A current server's answer, with or without blockers.v1.
                let mut info = crate::protocol::Policy::default().info(false);
                assert!(
                    info.capabilities
                        .iter()
                        .any(|c| c == crate::backend::BLOCKERS)
                );
                if !self.offers {
                    info.capabilities.retain(|c| c != self.capability);
                }
                Box::pin(async move { Ok(Response::new(info)) })
            }
        }
        impl tonic::server::NamedService for Server {
            const NAME: &'static str = "bbs.v2.Bbs";
        }
        impl Service<http::Request<Body>> for Server {
            type Response = http::Response<Body>;
            type Error = Infallible;
            type Future = BoxFuture<Self::Response, Self::Error>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, req: http::Request<Body>) -> Self::Future {
                let server = self.clone();
                Box::pin(async move {
                    Ok(match req.uri().path() {
                        "/bbs.v2.Bbs/ServerInfo" => {
                            tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                                proto::ServerInfoResponse,
                                proto::Empty,
                            >::default())
                            .unary(server, req)
                            .await
                        }
                        "/bbs.v2.Bbs/ListBlockers" => {
                            Status::failed_precondition("refused").into_http()
                        }
                        _ => Status::unimplemented("unexpected RPC").into_http(),
                    })
                })
            }
        }

        #[tokio::test]
        async fn a_failed_poll_is_an_old_server_only_when_it_lacks_the_capability() {
            let capability = crate::backend::BLOCKERS_MINE;
            for offers in [true, false] {
                let (_dir, _conn, mut opener, _) = board();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                opener.config.server = Some(listener.local_addr().unwrap().to_string());
                let serving = tokio::spawn(
                    tonic::transport::Server::builder()
                        .add_service(Server { offers, capability })
                        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                            listener,
                        )),
                );
                let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
                    .await
                    .unwrap();
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
                tx.send(Request::Blockers { id: 61 }).unwrap();
                drop(tx);
                serve(backend, opener, rx, events).await;
                let Some(Event::Data(Data::Blockers { id: 61, result })) = replies.recv().await
                else {
                    panic!("a poll reply")
                };
                if offers {
                    assert!(
                        matches!(result, Err(SideError::Refused(_))),
                        "{capability}: {result:?}"
                    );
                } else {
                    assert!(
                        matches!(result, Err(SideError::Unsupported(_))),
                        "{capability}: {result:?}"
                    );
                }
                serving.abort();
            }
        }
    }

    mod reconnect_wire {
        use super::*;
        use std::{
            convert::Infallible,
            pin::Pin,
            sync::{
                Arc, Mutex,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll},
        };
        use tokio_stream::{Stream, StreamExt};
        use tonic::{
            Response, Status,
            body::Body,
            codegen::{BoxFuture, Service, http},
        };

        type Replies<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
        type Reply<T> = BoxFuture<Response<Replies<T>>, Status>;

        #[derive(Default)]
        struct State {
            feeds: AtomicUsize,
            summaries: AtomicUsize,
            cursors: Mutex<Vec<Option<i64>>>,
            refusal: Option<tonic::Code>,
            history: Option<Arc<History>>,
        }
        #[derive(Clone, Default)]
        struct Server(Arc<State>);

        #[derive(Default)]
        struct History {
            prefix: Mutex<Option<UnboundedSender<Result<proto::ActivitySnapshot, Status>>>>,
            live: Mutex<Option<UnboundedSender<Result<proto::ActivitySnapshot, Status>>>>,
            started: tokio::sync::Notify,
            prefix_closed: Arc<tokio::sync::Notify>,
            live_closed: Arc<tokio::sync::Notify>,
        }

        struct HeldActivity {
            rx: UnboundedReceiver<Result<proto::ActivitySnapshot, Status>>,
            closed: Arc<tokio::sync::Notify>,
        }

        impl Stream for HeldActivity {
            type Item = Result<proto::ActivitySnapshot, Status>;

            fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                self.get_mut().rx.poll_recv(cx)
            }
        }

        impl Drop for HeldActivity {
            fn drop(&mut self) {
                self.closed.notify_one();
            }
        }

        fn activity_row(id: i64) -> proto::Activity {
            proto::Activity {
                id,
                record: r#"{"event":"turn_end"}"#.into(),
                ..Default::default()
            }
        }

        fn reply<T: Send + 'static>(value: T, interrupt: bool) -> Reply<T> {
            Box::pin(async move {
                let replies: Replies<T> = if interrupt {
                    // Clean EOF must reconnect too, not just an explicit status.
                    Box::pin(tokio_stream::iter([Ok(value)]))
                } else {
                    Box::pin(tokio_stream::iter([Ok(value)]).chain(tokio_stream::pending()))
                };
                Ok(Response::new(replies))
            })
        }
        impl tonic::server::ServerStreamingService<proto::FeedRequest> for Server {
            type Response = proto::FeedSnapshot;
            type ResponseStream = Replies<Self::Response>;
            type Future = Reply<Self::Response>;
            fn call(&mut self, _: tonic::Request<proto::FeedRequest>) -> Self::Future {
                let n = self.0.feeds.fetch_add(1, Ordering::SeqCst);
                if let Some(code) = self.0.refusal {
                    return Box::pin(async move { Err(Status::new(code, "refused")) });
                }
                reply(
                    proto::FeedSnapshot {
                        messages: vec![proto::Message {
                            id: n as i64 + 1,
                            body: format!("mail {n}"),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    n == 0,
                )
            }
        }
        impl tonic::server::ServerStreamingService<proto::AgentSummaryRequest> for Server {
            type Response = proto::AgentSummarySnapshot;
            type ResponseStream = Replies<Self::Response>;
            type Future = Reply<Self::Response>;
            fn call(&mut self, _: tonic::Request<proto::AgentSummaryRequest>) -> Self::Future {
                let n = self.0.summaries.fetch_add(1, Ordering::SeqCst);
                reply(
                    proto::AgentSummarySnapshot {
                        agents: vec![proto::AgentSummary {
                            agent: "Mateo".into(),
                            unread_messages: n as u32,
                            ..Default::default()
                        }],
                        notes_revision: None,
                    },
                    n == 0,
                )
            }
        }
        impl tonic::server::ServerStreamingService<proto::ActivityRequest> for Server {
            type Response = proto::ActivitySnapshot;
            type ResponseStream = Replies<Self::Response>;
            type Future = Reply<Self::Response>;
            fn call(&mut self, request: tonic::Request<proto::ActivityRequest>) -> Self::Future {
                let request = request.into_inner();
                let cursor = request.after_id;
                let mut cursors = self.0.cursors.lock().unwrap();
                let n = cursors.iter().filter(|c| c.is_some()).count();
                cursors.push(cursor);
                if let Some(history) = &self.0.history {
                    if cursor.is_none() {
                        return reply(
                            proto::ActivitySnapshot {
                                events: if request.backlog == 1 {
                                    vec![activity_row(1598)]
                                } else {
                                    (0..200).map(|i| activity_row(1000 + i * 3)).collect()
                                },
                            },
                            false,
                        );
                    }
                    let (tx, rx) = unbounded_channel();
                    let closed = if cursor == Some(0) {
                        *history.prefix.lock().unwrap() = Some(tx);
                        history.started.notify_one();
                        history.prefix_closed.clone()
                    } else {
                        tx.send(Ok(proto::ActivitySnapshot {
                            events: vec![activity_row(cursor.unwrap() + 1)],
                        }))
                        .unwrap();
                        *history.live.lock().unwrap() = Some(tx);
                        history.live_closed.clone()
                    };
                    return Box::pin(async move {
                        let rows: Replies<proto::ActivitySnapshot> =
                            Box::pin(HeldActivity { rx, closed });
                        Ok(Response::new(rows))
                    });
                }
                let id = if n == 0 { 10 } else { 11 };
                reply(
                    proto::ActivitySnapshot {
                        events: vec![proto::Activity {
                            id,
                            record: r#"{"event":"turn_end"}"#.into(),
                            ..Default::default()
                        }],
                    },
                    cursor.is_some() && n == 0,
                )
            }
        }
        impl tonic::server::UnaryService<proto::ListAgentsRequest> for Server {
            type Response = proto::ListAgentsResponse;
            type Future = BoxFuture<Response<proto::ListAgentsResponse>, Status>;
            fn call(&mut self, _: tonic::Request<proto::ListAgentsRequest>) -> Self::Future {
                Box::pin(async { Ok(Response::new(proto::ListAgentsResponse::default())) })
            }
        }
        impl tonic::server::NamedService for Server {
            const NAME: &'static str = "bbs.v2.Bbs";
        }
        impl Service<http::Request<Body>> for Server {
            type Response = http::Response<Body>;
            type Error = Infallible;
            type Future = BoxFuture<Self::Response, Self::Error>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, req: http::Request<Body>) -> Self::Future {
                let server = self.clone();
                Box::pin(async move {
                    let response = match req.uri().path() {
                        // A side read that never answers, as on a half-open
                        // connection.
                        "/bbs.v2.Bbs/SideReport" => std::future::pending().await,
                        "/bbs.v2.Bbs/ListAgents" => {
                            tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                                proto::ListAgentsResponse,
                                proto::ListAgentsRequest,
                            >::default())
                            .unary(server, req)
                            .await
                        }
                        "/bbs.v2.Bbs/WatchFeed" => {
                            tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                                proto::FeedSnapshot,
                                proto::FeedRequest,
                            >::default())
                            .server_streaming(server, req)
                            .await
                        }
                        "/bbs.v2.Bbs/WatchActivity" => {
                            tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                                proto::ActivitySnapshot,
                                proto::ActivityRequest,
                            >::default())
                            .server_streaming(server, req)
                            .await
                        }
                        "/bbs.v2.Bbs/WatchAgentSummary" => {
                            tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                                proto::AgentSummarySnapshot,
                                proto::AgentSummaryRequest,
                            >::default())
                            .server_streaming(server, req)
                            .await
                        }
                        _ => Status::unimplemented("unexpected RPC").into_http(),
                    };
                    Ok(response)
                })
            }
        }

        #[tokio::test]
        async fn live_activity_survives_stalled_or_failed_history_and_cancels_with_it() {
            for outcome in ["timeout", "unavailable", "cancel"] {
                let (_dir, _conn, mut opener, mut req) = board();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                opener.config.server = Some(listener.local_addr().unwrap().to_string());
                req.after_id = None;
                let history = Arc::new(History::default());
                let state = Arc::new(State {
                    history: Some(history.clone()),
                    ..Default::default()
                });
                let serving = tokio::spawn(
                    tonic::transport::Server::builder()
                        .add_service(Server(state.clone()))
                        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                            listener,
                        )),
                );
                let (tx, mut rx) = unbounded_channel();
                let worker = tokio::spawn(watch_activity(opener, 9, req, tx));
                // A live event arrives with the older read still held open.
                let mut window = false;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match rx.recv().await.unwrap() {
                            Event::Data(Data::Activity {
                                result: Ok(rows), ..
                            }) => {
                                if rows.len() == 200 {
                                    assert_eq!(rows.first().unwrap().id, 1000);
                                    assert_eq!(rows.last().unwrap().id, 1597);
                                    window = true;
                                } else if rows.last().is_some_and(|r| r.id == 1598) {
                                    break;
                                }
                            }
                            Event::Data(Data::ActivityBaseline {
                                through_id: 1597, ..
                            }) => {}
                            _ => panic!("unexpected event before history completes"),
                        }
                    }
                    history.started.notified().await;
                })
                .await
                .unwrap();
                assert!(window);
                assert_eq!(
                    state
                        .cursors
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|c| **c == Some(0))
                        .count(),
                    1
                );

                if outcome == "cancel" {
                    worker.abort();
                    assert!(worker.await.unwrap_err().is_cancelled());
                    tokio::time::timeout(Duration::from_secs(5), async {
                        history.prefix_closed.notified().await;
                        history.live_closed.notified().await;
                    })
                    .await
                    .unwrap();
                    assert!(rx.try_recv().is_err(), "cancel sends no history result");
                } else {
                    if outcome == "timeout" {
                        tokio::time::pause();
                        tokio::time::advance(HISTORY_LOAD_TIMEOUT).await;
                    } else {
                        history
                            .prefix
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .send(Err(Status::unavailable("interrupted")))
                            .unwrap();
                    }
                    let Event::Data(Data::ActivityBackfill {
                        id: 9,
                        result: Err(error),
                    }) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                        .await
                        .unwrap()
                        .unwrap()
                    else {
                        panic!("history fails once, without a partial result")
                    };
                    if outcome == "timeout" {
                        assert!(error.contains("30 seconds"));
                        tokio::time::resume();
                    } else {
                        assert!(error.contains("interrupted"));
                    }
                    assert!(error.contains("r retries"));
                    tokio::time::timeout(Duration::from_secs(5), history.prefix_closed.notified())
                        .await
                        .unwrap();
                    // A live interruption still resumes its own cursor; it
                    // neither retries the failed prefix nor duplicates it.
                    history.live.lock().unwrap().take();
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            match rx.recv().await.unwrap() {
                                Event::Data(Data::ActivityBackfill { .. }) => {
                                    panic!("history retried")
                                }
                                Event::Data(Data::Activity {
                                    result: Ok(rows), ..
                                }) if rows.last().is_some_and(|r| r.id == 1599) => break,
                                _ => {}
                            }
                        }
                    })
                    .await
                    .unwrap();
                    assert_eq!(
                        state
                            .cursors
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|c| **c == Some(0))
                            .count(),
                        1
                    );
                    worker.abort();
                    assert!(worker.await.unwrap_err().is_cancelled());
                }
                serving.abort();
                let _ = serving.await;
            }
        }

        #[tokio::test]
        async fn remote_subscriptions_resume_without_duplicate_activity_or_app_restart() {
            let (_dir, _conn, mut opener, req) = board();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            opener.config.server = Some(listener.local_addr().unwrap().to_string());
            let server = Server::default();
            let state = server.0.clone();
            let serving = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(server)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
            );
            let (tx, mut rx) = unbounded_channel();
            let workers = [
                tokio::spawn(watch(
                    opener.clone(),
                    1,
                    proto::FeedRequest::default(),
                    tx.clone(),
                )),
                tokio::spawn(watch_summary(opener.clone(), 2, tx.clone())),
                tokio::spawn(watch_activity(opener, 3, req, tx)),
            ];
            let mut mail = vec![];
            let mut summaries = 0;
            let mut activity = vec![];
            let mut baselines = vec![];
            let mut revisions = vec![];
            let mut errors = 0;
            tokio::time::timeout(Duration::from_secs(8), async {
                while mail.len() < 2 || summaries < 2 || activity.len() < 2 {
                    match rx.recv().await.unwrap() {
                        Event::Data(Data::Feed {
                            result: Ok(rows), ..
                        }) => mail.extend(rows.into_iter().map(|m| m.id)),
                        Event::Data(Data::Summary { result: Ok(_), .. }) => summaries += 1,
                        Event::Data(Data::Activity {
                            result: Ok(rows), ..
                        }) => activity.extend(rows.into_iter().map(|m| m.id)),
                        Event::Data(Data::ActivityBaseline { through_id, .. }) => {
                            baselines.push(through_id)
                        }
                        Event::Data(Data::NotesRevision { id: 2, revision }) => {
                            revisions.push(revision)
                        }
                        Event::Data(
                            Data::Feed { result: Err(_), .. }
                            | Data::Summary { result: Err(_), .. }
                            | Data::Activity { result: Err(_), .. },
                        ) => errors += 1,
                        _ => panic!("unexpected event"),
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(mail, [1, 2]);
            assert_eq!(activity, [10, 11]);
            assert_eq!(baselines, [10, 11]);
            // Each summary is followed by its notes revision; this server has none.
            assert!(!revisions.is_empty() && revisions.iter().all(Option::is_none));
            assert_eq!(errors, 3);
            assert_eq!(
                *state.cursors.lock().unwrap(),
                [None, Some(0), None, Some(10)]
            );
            for worker in workers {
                worker.abort();
            }
            serving.abort();
        }

        #[tokio::test]
        async fn a_stalled_side_read_times_out_and_queued_work_proceeds() {
            let (_dir, _conn, mut opener, _) = board();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            opener.config.server = Some(listener.local_addr().unwrap().to_string());
            let serving = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(Server::default())
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
            );
            let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
                .await
                .unwrap();
            let (tx, rx) = unbounded_channel();
            let (events, mut replies) = unbounded_channel();
            let worker = tokio::spawn(serve(backend, opener, rx, events));
            tx.send(Request::SideReport {
                id: 1,
                agent: "Mateo".into(),
            })
            .unwrap();
            tx.send(Request::Agents).unwrap();
            let started = std::time::Instant::now();
            let (side, agents) = tokio::time::timeout(Duration::from_secs(10), async {
                (replies.recv().await.unwrap(), replies.recv().await.unwrap())
            })
            .await
            .expect("the worker never parks behind the side read");
            assert!(
                matches!(
                    side,
                    Event::Data(Data::SideReport {
                        id: 1,
                        result: Err(SideError::Transient(_))
                    })
                ),
                "a stalled read is transient"
            );
            assert!(matches!(agents, Event::Data(Data::Agents(Ok(_)))));
            assert!(started.elapsed() >= SIDE_CALL_TIMEOUT);
            worker.abort();
            serving.abort();
        }

        #[tokio::test]
        async fn remote_subscription_recovers_after_connection_refused() {
            let (_dir, _conn, mut opener, _) = board();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            opener.config.server = Some(address.to_string());
            drop(listener);
            let (tx, mut rx) = unbounded_channel();
            let worker = tokio::spawn(watch_summary(opener, 2, tx));
            let event = tokio::time::timeout(Duration::from_secs(4), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(event, Event::Data(Data::Summary { result: Err(ref e), .. }) if e.contains("reconnecting"))
            );
            let listener = tokio::net::TcpListener::bind(address).await.unwrap();
            let server = Server::default();
            server.0.summaries.store(1, Ordering::SeqCst);
            let serving = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(server)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
            );
            let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                event,
                Event::Data(Data::Summary { result: Ok(_), .. })
            ));
            worker.abort();
            serving.abort();
        }

        #[tokio::test]
        async fn remote_auth_refusal_stops_instead_of_retrying() {
            let (_dir, _conn, mut opener, _) = board();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            opener.config.server = Some(listener.local_addr().unwrap().to_string());
            let server = Server(Arc::new(State {
                refusal: Some(tonic::Code::Unauthenticated),
                ..Default::default()
            }));
            let state = server.0.clone();
            let serving = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(server)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
            );
            let (tx, mut rx) = unbounded_channel();
            tokio::time::timeout(
                Duration::from_secs(3),
                watch(opener, 1, proto::FeedRequest::default(), tx),
            )
            .await
            .unwrap();
            assert_eq!(state.feeds.load(Ordering::SeqCst), 1);
            assert!(matches!(
                rx.recv().await,
                Some(Event::Data(Data::Feed { result: Err(_), .. }))
            ));
            assert!(rx.recv().await.is_none());
            serving.abort();
        }
    }

    #[test]
    fn terminal_pastes_become_one_paste_event_not_keys() {
        use ratatui::crossterm::event::{KeyCode, KeyEventState, KeyModifiers};
        assert!(matches!(
            input(event::Event::Paste("a\r\nb".into())),
            Some(Event::Paste(text)) if text == "a\r\nb"
        ));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(input(event::Event::Key(enter)), Some(Event::Key(k)) if k == enter));
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
            ..enter
        };
        assert!(input(event::Event::Key(release)).is_none());
        assert!(matches!(
            input(event::Event::Resize(80, 24)),
            Some(Event::Resize)
        ));
        assert!(input(event::Event::FocusGained).is_none());
    }

    fn board() -> (
        tempfile::TempDir,
        rusqlite::Connection,
        Opener,
        proto::ActivityRequest,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("bbs.db")).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO humans (id, name, created_at, active_at) VALUES (1, 'ada', 't', 't');
             INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
             VALUES (1, 'Mateo', 'a', '/', 'h', 't', 't')",
        )
        .unwrap();
        let events = vec![("text".into(), r#"{"event":"text","text":"history"}"#.into()); 500];
        crate::db::post_activity(&conn, 1, "t", &events).unwrap();
        let opener = Opener {
            config: Config::default(),
            dir: dir.path().into(),
            caller: Caller {
                human: Some("ada".into()),
                ..Default::default()
            },
        };
        let req = proto::ActivityRequest {
            backlog: 200,
            after_id: Some(0),
            agent: Some("Mateo".into()),
        };
        (dir, conn, opener, req)
    }

    #[test]
    fn effort_send_failures_preserve_rejection_codes_without_text_inference() {
        use tonic::Code;
        for code in [
            Code::InvalidArgument,
            Code::NotFound,
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::FailedPrecondition,
            Code::Unimplemented,
        ] {
            let error = anyhow::Error::new(tonic::Status::new(code, "same text"));
            assert!(
                matches!(
                    effort_send_error(false, error),
                    EffortSendError::Rejected(_)
                ),
                "{code}"
            );
        }
        for code in [
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::Unknown,
            Code::Internal,
            Code::Cancelled,
            Code::DataLoss,
        ] {
            let error = anyhow::Error::new(tonic::Status::new(
                code,
                "permission denied: capability unsupported",
            ));
            assert!(
                matches!(effort_send_error(false, error), EffortSendError::Unknown(_)),
                "{code}"
            );
        }
        assert!(matches!(
            effort_send_error(false, anyhow::anyhow!("validation failure")),
            EffortSendError::Unknown(_)
        ));
        // A local service call is transactional: a returned error rolls back.
        assert!(matches!(
            effort_send_error(true, anyhow::anyhow!("local refusal")),
            EffortSendError::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn refused_effort_control_is_typed_and_does_not_insert_a_message() {
        let (_dir, conn, opener, _) = board();
        let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
        // No owner row grants this human control over Mateo.
        tx.send(Request::SetEffort {
            id: 45,
            agent: "Mateo".into(),
            value: "high".into(),
        })
        .unwrap();
        drop(tx);
        serve(backend, opener, rx, events).await;
        assert!(matches!(
            replies.recv().await,
            Some(Event::Data(Data::EffortSent {
                id: 45,
                result: Err(EffortSendError::Rejected(_)),
                ..
            }))
        ));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn side_asks_and_cancels_report_ids_and_typed_refusals() {
        let (_dir, conn, opener, _) = board();
        conn.execute_batch(
            "INSERT INTO agent_owners (agent_id, human_id) VALUES (1, 1);
             INSERT INTO side_state (agent_id, generation, reported_at, revision, closed_seq, supported)
             VALUES (1, 'run-1', '2026-10-20T02:30:00Z', 1, 0, 1);",
        )
        .unwrap();
        let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
        let ask = |id| Request::AskSide {
            follows: None,
            id,
            agent: "Mateo".into(),
            question: "which branch?".into(),
        };
        tx.send(ask(50)).unwrap();
        // One open question at a time: the second ask is refused.
        tx.send(ask(51)).unwrap();
        drop(tx);
        serve(backend, opener.clone(), rx, events).await;
        let Some(Event::Data(Data::SideAsked {
            id: 50,
            result: Ok(side),
        })) = replies.recv().await
        else {
            panic!("the new side question's id")
        };
        assert!(matches!(
            replies.recv().await,
            Some(Event::Data(Data::SideAsked {
                id: 51,
                result: Err(SideError::Refused(_)),
            }))
        ));
        let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Request::CancelSide {
            id: 52,
            side,
            end: false,
        })
        .unwrap();
        tx.send(Request::CancelSide {
            end: false,
            id: 53,
            side: side + 1,
        })
        .unwrap();
        drop(tx);
        serve(backend, opener, rx, events).await;
        assert!(matches!(
            replies.recv().await,
            Some(Event::Data(Data::SideCancelled { id: 52, side: s, result: Ok(()) })) if s == side
        ));
        assert!(matches!(
            replies.recv().await,
            Some(Event::Data(Data::SideCancelled {
                id: 53,
                result: Err(SideError::Refused(_)),
                ..
            }))
        ));
        let status: String = conn
            .query_row(
                "SELECT status FROM side_requests WHERE id = ?1",
                [side],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
        assert_eq!(
            conn.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "never mail"
        );
    }

    #[tokio::test]
    async fn effort_send_returns_control_message_id_without_a_draft_reply() {
        let (_dir, conn, opener, _) = board();
        conn.execute(
            "INSERT INTO agent_owners (agent_id, human_id) VALUES (1, 1)",
            [],
        )
        .unwrap();
        let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Request::SetEffort {
            id: 44,
            agent: "Mateo".into(),
            value: "high".into(),
        })
        .unwrap();
        drop(tx);
        serve(backend, opener, rx, events).await;
        let Some(Event::Data(Data::EffortSent {
            id: 44,
            agent,
            result: Ok(Some(id)),
        })) = replies.recv().await
        else {
            panic!("correlated effort reply")
        };
        assert_eq!(agent, "Mateo");
        let (body, control): (String, String) = conn
            .query_row(
                "SELECT body, control FROM messages WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((body.as_str(), control.as_str()), ("high", "set-effort"));
        assert!(
            replies.recv().await.is_none(),
            "not a Data::Sent composer completion"
        );
    }

    #[tokio::test]
    async fn read_requests_report_real_backend_completion_to_the_app() {
        let (_dir, _conn, opener, _) = board();
        let backend = Backend::open(&opener.config, &opener.dir, opener.caller.clone())
            .await
            .unwrap();
        let (requests, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut replies) = tokio::sync::mpsc::unbounded_channel();
        requests.send(Request::MarkRead("Mateo".into())).unwrap();
        requests.send(Request::MarkRead("missing".into())).unwrap();
        drop(requests);
        serve(backend, opener, rx, events).await;
        let Some(Event::Data(Data::MarkedRead { agent, result })) = replies.recv().await else {
            panic!("read acknowledgement")
        };
        assert_eq!(agent, "Mateo");
        assert!(result.is_ok());
        let Some(Event::Data(Data::MarkedRead { agent, result })) = replies.recv().await else {
            panic!("read error")
        };
        assert_eq!(agent, "missing");
        assert!(result.unwrap_err().contains("no such agent or human"));
    }

    #[tokio::test]
    async fn admin_success_notices_keep_typed_fields_until_the_ui_renders() {
        use super::super::admin::GroupAdmin;
        use crate::ui::theme::{
            style,
            testing::{SCHEMES, with_scheme},
        };
        let (_dir, conn, opener, _) = board();
        conn.execute_batch(
            "INSERT INTO agents (id,name,token,pwd,hostname,created_at,active_at)
             VALUES (2,'Zed','b','/','h','t','t')",
        )
        .unwrap();
        let mut backend = Backend::open(&opener.config, &opener.dir, opener.caller)
            .await
            .unwrap();
        admin(
            &mut backend,
            Admin::Create(proto::GroupRegisterRequest {
                group: "crew".into(),
                description: None,
            }),
        )
        .await
        .unwrap();
        admin(
            &mut backend,
            Admin::Add(proto::GroupAddRequest {
                group: "crew".into(),
                agents: vec!["Mateo".into()],
            }),
        )
        .await
        .unwrap();
        let added = admin(
            &mut backend,
            Admin::Add(proto::GroupAddRequest {
                group: "crew".into(),
                agents: vec!["Mateo".into(), "Zed".into()],
            }),
        )
        .await
        .unwrap();
        let writer = admin(
            &mut backend,
            Admin::Writer(proto::SetGroupWriterRequest {
                group: "crew".into(),
                agent: "Zed".into(),
                grant: true,
            }),
        )
        .await
        .unwrap();
        let deleted = admin(
            &mut backend,
            Admin::Delete(proto::GroupRequest {
                group: "crew".into(),
            }),
        )
        .await
        .unwrap();
        for (notice, expected) in [
            (added, "Added Zed to @crew. Already in @crew: Mateo."),
            (writer, "Zed can now write to @crew."),
            (deleted, "Deleted @crew."),
        ] {
            let mut ui = GroupAdmin::default();
            ui.on_data(
                7,
                &Data::Admin {
                    id: 7,
                    result: Ok(notice),
                },
            );
            for scheme in SCHEMES {
                with_scheme(scheme, || {
                    let line = ui.notice().unwrap();
                    assert_eq!(line.to_string(), expected);
                    for span in &line.spans {
                        let role = match span.content.as_ref() {
                            "@crew" => Role::Group,
                            "Zed" | "Mateo" => Role::Agent,
                            _ => Role::Text,
                        };
                        assert_eq!(span.style, style(role), "{span:?}");
                    }
                });
            }
        }
        let mut ui = GroupAdmin::default();
        ui.on_data(
            7,
            &Data::Admin {
                id: 7,
                result: Err("Mateo cannot change @crew".into()),
            },
        );
        let line = ui.notice().unwrap();
        assert_eq!(line.to_string(), "Mateo cannot change @crew");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.style, style(Role::Error));
    }

    #[tokio::test(start_paused = true)]
    async fn older_activity_keeps_a_bounded_recent_prefix_and_handles_an_empty_board() {
        let (_dir, conn, opener, req) = board();
        conn.execute_batch(
            r#"WITH RECURSIVE ids(id) AS (
                VALUES(501) UNION ALL SELECT id + 1 FROM ids WHERE id < 6500
            ) INSERT INTO activity (id, agent_id, created_at, event, record)
              SELECT id, 1, 't', 'text', '{"event":"text"}' FROM ids;
              UPDATE activity SET id = -id;
              UPDATE activity SET id = -id * 3;"#,
        )
        .unwrap();
        let rows = older_activity(opener.clone(), req.agent.clone(), 6201 * 3)
            .await
            .unwrap();
        assert_eq!(rows.len(), super::super::timeline::MAX_RECORDS);
        assert_eq!(rows.first().unwrap().id, 1201 * 3);
        assert_eq!(rows.last().unwrap().id, 6200 * 3);
        assert!(rows.windows(2).all(|pair| pair[0].id < pair[1].id));
        conn.execute("DELETE FROM activity", []).unwrap();
        assert!(
            older_activity(opener.clone(), req.agent, 6201 * 3)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            older_activity(opener, Some("retired".into()), 6201 * 3)
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn activity_opens_on_the_newest_window_then_older_history_once_then_live() {
        async fn next(rx: &mut UnboundedReceiver<Event>) -> Event {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
        }
        // (records kept, older history expected): more than a page before
        // the window, exactly a page, and none.
        for (kept, older) in [(500, 300), (400, 200), (200, 0), (150, 0), (0, 0)] {
            let (_dir, conn, opener, mut req) = board();
            conn.execute("DELETE FROM activity WHERE id > ?1", [kept])
                .unwrap();
            req.after_id = None;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let worker = tokio::spawn(watch_activity(opener, 9, req, tx));
            let Event::Data(Data::ActivityBaseline {
                id: 9,
                through_id,
                record,
            }) = next(&mut rx).await
            else {
                panic!("expected the window's watermark");
            };
            assert_eq!(through_id, kept);
            assert_eq!(record.is_some(), kept > 0);
            let Event::Data(Data::Activity {
                id: 9,
                result: Ok(window),
            }) = next(&mut rx).await
            else {
                panic!("expected the newest window");
            };
            let ids: Vec<i64> = window.iter().map(|row| row.id).collect();
            assert_eq!(ids, (kept - kept.min(200) + 1..=kept).collect::<Vec<_>>());
            // Live records follow the window with nothing missed or repeated;
            // older history arrives alongside, once and whole. The live
            // cursor's first batch may be empty.
            crate::db::post_activity(
                &conn,
                1,
                "u",
                &[("turn_end".into(), r#"{"event":"turn_end"}"#.into())],
            )
            .unwrap();
            let (mut live, mut history) = (Vec::new(), None);
            while live.is_empty() || (kept >= 200 && history.is_none()) {
                match next(&mut rx).await {
                    Event::Data(Data::Activity {
                        id: 9,
                        result: Ok(rows),
                    }) => live.extend(rows.into_iter().map(|row| row.id)),
                    Event::Data(Data::ActivityBackfill {
                        id: 9,
                        result: Ok(rows),
                    }) => {
                        assert!(history.is_none(), "older history arrives once");
                        history = Some(rows.into_iter().map(|row| row.id).collect::<Vec<_>>());
                    }
                    _ => panic!("unexpected event"),
                }
            }
            assert_eq!(live, [kept + 1]);
            if kept >= 200 {
                assert_eq!(history.unwrap(), (1..=older).collect::<Vec<_>>(), "{kept}");
            } else {
                assert!(
                    history.is_none(),
                    "a short window has nothing older to fetch"
                );
            }
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
            while let Ok(event) = rx.try_recv() {
                assert!(
                    matches!(event, Event::Data(Data::Activity { result: Ok(rows), .. }) if rows.is_empty()),
                    "nothing more, nothing twice"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn activity_worker_marks_all_replay_batches_before_live_events() {
        let (_dir, conn, opener, req) = board();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(watch_activity(opener, 9, req, tx));
        async fn next(rx: &mut UnboundedReceiver<Event>) -> Event {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
        }
        assert!(matches!(
            next(&mut rx).await,
            Event::Data(Data::ActivityBaseline {
                id: 9,
                through_id: 500,
                record: Some(_),
            })
        ));
        for (len, last) in [(200, 200), (200, 400), (100, 500)] {
            let Event::Data(Data::Activity {
                id: 9,
                result: Ok(events),
            }) = next(&mut rx).await
            else {
                panic!("expected replay batch");
            };
            assert_eq!(events.len(), len);
            assert_eq!(events.last().unwrap().id, last);
        }
        crate::db::post_activity(
            &conn,
            1,
            "u",
            &[("turn_end".into(), r#"{"event":"turn_end"}"#.into())],
        )
        .unwrap();
        let Event::Data(Data::Activity {
            id: 9,
            result: Ok(events),
        }) = next(&mut rx).await
        else {
            panic!("expected live batch");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 501);
        worker.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn activity_arriving_between_baseline_and_history_is_fresh() {
        use crate::connect::progress::{Progress, ProgressState};
        let (_dir, conn, opener, req) = board();
        let mut backend = Backend::open(&opener.config, &opener.dir, opener.caller)
            .await
            .unwrap();
        let baseline = activity_baseline(&mut backend, &req).await.unwrap();
        assert_eq!(baseline.as_ref().map(|row| row.id), Some(500));
        let now = std::time::Instant::now();
        crate::db::post_activity(
            &conn,
            1,
            "u",
            &[("text".into(), r#"{"event":"text"}"#.into())],
        )
        .unwrap();
        let mut progress = ProgressState::default();
        progress.baseline(
            baseline.as_ref().map_or(0, |row| row.id),
            baseline.as_ref().map(|row| row.record.as_str()),
        );
        let mut replay = backend.watch_activity(req).await.unwrap();
        for batch in 0..3 {
            let events = replay.next().await.unwrap();
            for event in &events {
                progress.observe(event.id, &event.record, now);
            }
            assert_eq!(
                progress.at(now),
                if batch == 2 {
                    assert_eq!(events.last().unwrap().id, 501);
                    Progress::Progressing(0)
                } else {
                    Progress::Unknown
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn baseline_failure_is_reported_without_replaying_as_fresh() {
        let (_dir, _conn, opener, mut req) = board();
        req.agent = Some("missing".into());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        watch_activity(opener, 9, req, tx).await;
        let Some(Event::Data(Data::Activity {
            id: 9,
            result: Err(error),
        })) = rx.recv().await
        else {
            panic!("expected the ordinary activity error path");
        };
        assert!(error.contains("no agent named"), "{error}");
        assert!(
            rx.recv().await.is_none(),
            "no baseline or replay emitted after failure"
        );
    }
}
