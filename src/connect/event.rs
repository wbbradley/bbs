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
    /// Stop a stream.
    Unwatch(u64),
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
            Request::Watch { id, req } => {
                let task = tokio::spawn(watch(opener.clone(), id, req, tx.clone()));
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
