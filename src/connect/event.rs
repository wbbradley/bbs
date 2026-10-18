//! Everything the app reacts to, merged into one channel: key presses and
//! resizes, a clock tick, and replies from the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::backend::Backend;
use crate::config::Config;
use crate::proto;
use crate::service::Caller;

pub enum Event {
    Key(KeyEvent),
    Resize,
    Tick,
    Data(Data),
}

/// Work for the server, done off the drawing path.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Agents,
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

/// A server reply; errors are messages to show.
pub enum Data {
    Agents(Result<proto::ListAgentsResponse, String>),
    Groups(Result<proto::GroupListResponse, String>),
    /// A snapshot of the feed stream `id`. A stream ends after an error.
    Feed {
        id: u64,
        result: Result<Vec<proto::Message>, String>,
    },
    /// A batch of the activity stream `id`. A stream ends after an error.
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
    /// Summaries from the stream `id`. A stream ends after an error.
    Summary {
        id: u64,
        result: Result<Vec<proto::AgentSummary>, String>,
    },
    /// The outcome of the send `id`, even if its screen has closed.
    Sent {
        id: u64,
        result: Result<(), String>,
    },
    /// What a group change did, for the screen `id`.
    Admin {
        id: u64,
        result: Result<String, String>,
    },
}

/// What a new server connection needs: each feed stream holds one.
#[derive(Clone)]
pub struct Opener {
    pub config: Config,
    pub dir: PathBuf,
    pub caller: Caller,
}

/// Read terminal input on a thread of its own: crossterm's reads block.
pub fn spawn_input(tx: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            let event = match event::read() {
                Ok(event::Event::Key(key)) if key.kind == KeyEventKind::Press => Event::Key(key),
                Ok(event::Event::Resize(..)) => Event::Resize,
                Ok(_) => continue,
                Err(_) => return,
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
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
            Request::Groups => Data::Groups(backend.group_list().await.map_err(error)),
            Request::Send { id, req } => Data::Sent {
                id,
                result: backend.send(req).await.map(drop).map_err(error),
            },
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
            Request::MarkRead(from) => {
                // A failure leaves the mail unread, to be marked on a later view.
                if let Err(err) = backend.mark_read(proto::MarkReadRequest { from }).await {
                    tracing::warn!("marking mail read: {err:#}");
                }
                continue;
            }
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
async fn admin(backend: &mut Backend, op: Admin) -> anyhow::Result<String> {
    Ok(match op {
        Admin::Create(req) => {
            let group = backend.group_register(req).await?.group;
            format!("Created @{group}.")
        }
        Admin::Describe(req) => {
            let cleared = req.description.is_none();
            let group = backend.group_describe(req).await?.group;
            match cleared {
                true => format!("Cleared the description of @{group}."),
                false => format!("Described @{group}."),
            }
        }
        Admin::Delete(req) => {
            let group = backend.group_delete(req).await?.group;
            format!("Deleted @{group}.")
        }
        Admin::Add(req) => {
            let r = backend.group_add(req).await?;
            let mut parts = Vec::new();
            if !r.added.is_empty() {
                parts.push(format!("Added {} to @{}.", r.added.join(", "), r.group));
            }
            if !r.already.is_empty() {
                parts.push(format!(
                    "Already in @{}: {}.",
                    r.group,
                    r.already.join(", ")
                ));
            }
            parts.join(" ")
        }
        Admin::Remove(req) => {
            let r = backend.group_remove(req).await?;
            format!("Removed {} from @{}.", r.agent, r.group)
        }
        Admin::Writer(req) => {
            let grant = req.grant;
            let r = backend.set_group_writer(req).await?;
            match (grant, r.changed) {
                (true, true) => format!("{} can now write to @{}.", r.agent, r.group),
                (true, false) => format!("{} could already write to @{}.", r.agent, r.group),
                (false, true) => format!("{} can no longer write to @{}.", r.agent, r.group),
                (false, false) => format!("{} could not write to @{}.", r.agent, r.group),
            }
        }
    })
}

/// Stream feed `id` until it fails or the app goes away.
async fn watch(opener: Opener, id: u64, req: proto::FeedRequest, tx: UnboundedSender<Event>) {
    let result: anyhow::Result<()> = async {
        let mut backend = Backend::open(&opener.config, &opener.dir, opener.caller).await?;
        let mut feed = backend.watch_feed(req, false).await?;
        loop {
            let result = Ok(feed.next().await?);
            if tx.send(Event::Data(Data::Feed { id, result })).is_err() {
                return Ok(());
            }
        }
    }
    .await;
    if let Err(e) = result {
        let result = Err(format!("{e:#}"));
        let _ = tx.send(Event::Data(Data::Feed { id, result }));
    }
}

/// Stream summaries `id` until it fails or the app goes away.
async fn watch_summary(opener: Opener, id: u64, tx: UnboundedSender<Event>) {
    let result: anyhow::Result<()> = async {
        let mut backend = Backend::open(&opener.config, &opener.dir, opener.caller).await?;
        let mut summaries = backend.watch_agent_summary().await?;
        loop {
            let result = Ok(summaries.next().await?);
            if tx.send(Event::Data(Data::Summary { id, result })).is_err() {
                return Ok(());
            }
        }
    }
    .await;
    if let Err(e) = result {
        let result = Err(format!("{e:#}"));
        let _ = tx.send(Event::Data(Data::Summary { id, result }));
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

/// Stream activity `id` until it fails or the app goes away.
async fn watch_activity(
    opener: Opener,
    id: u64,
    req: proto::ActivityRequest,
    tx: UnboundedSender<Event>,
) {
    let result: anyhow::Result<()> = async {
        let mut backend = Backend::open(&opener.config, &opener.dir, opener.caller).await?;
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
        let mut activity = backend.watch_activity(req).await?;
        loop {
            let result = Ok(activity.next().await?);
            if tx.send(Event::Data(Data::Activity { id, result })).is_err() {
                return Ok(());
            }
        }
    }
    .await;
    if let Err(e) = result {
        let result = Err(format!("{e:#}"));
        let _ = tx.send(Event::Data(Data::Activity { id, result }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let now = chrono::Utc::now();
        // The gap event is fresh by its stored time, not by when replay
        // happened to deliver it.
        crate::db::post_activity(
            &conn,
            1,
            "u",
            &[(
                "text".into(),
                format!(r#"{{"event":"text","ts":"{}"}}"#, now.to_rfc3339()),
            )],
        )
        .unwrap();
        let mut progress = ProgressState::default();
        progress.baseline(
            baseline.as_ref().map_or(0, |row| row.id),
            baseline.as_ref().map(|row| row.record.as_str()),
            now,
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
