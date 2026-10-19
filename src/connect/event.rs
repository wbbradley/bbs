//! Everything the app reacts to, merged into one channel: key presses and
//! resizes, a clock tick, and replies from the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
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
    /// Stream agent activity, replacing any stream with the same id.
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

/// Own the terminal reader until it has stopped. It must be dropped before
/// restoring the terminal, so it cannot consume the shell's next input.
pub struct Input {
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Poll on the reader's own thread so shutdown can join it even with no input.
pub fn spawn_input(tx: UnboundedSender<Event>) -> Input {
    let stopped = Arc::new(AtomicBool::new(false));
    let stopping = stopped.clone();
    let thread = std::thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            match event::poll(Duration::from_millis(50)) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_) => return,
            }
            if stopping.load(Ordering::Acquire) {
                return;
            }
            let event = match event::read() {
                Ok(event) => match input(event) {
                    Some(event) => event,
                    None => continue,
                },
                Err(_) => return,
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    Input {
        stopped,
        thread: Some(thread),
    }
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
                let result = Ok(summaries.next().await?);
                if tx.send(Event::Data(Data::Summary { id, result })).is_err() {
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

/// Resume activity after the last delivered id, with a fresh history watermark.
async fn watch_activity(
    opener: Opener,
    id: u64,
    mut req: proto::ActivityRequest,
    tx: UnboundedSender<Event>,
) {
    let mut reconnect = Reconnect::new(&opener);
    loop {
        let result: anyhow::Result<()> = async {
            let mut backend =
                Backend::open(&opener.config, &opener.dir, opener.caller.clone()).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
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
        }
        #[derive(Clone, Default)]
        struct Server(Arc<State>);

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
                let cursor = request.into_inner().after_id;
                let mut cursors = self.0.cursors.lock().unwrap();
                let n = cursors.iter().filter(|c| c.is_some()).count();
                cursors.push(cursor);
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
