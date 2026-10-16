//! Everything the app reacts to, merged into one channel: key presses and
//! resizes, a clock tick, and replies from the server.

use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent, KeyEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::backend::Backend;
use crate::proto;

pub enum Event {
    Key(KeyEvent),
    Resize,
    Tick,
    Data(Data),
}

/// Work for the server, done off the drawing path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Agents,
    Groups,
}

/// A server reply; errors are messages to show.
pub enum Data {
    Agents(Result<proto::ListAgentsResponse, String>),
    Groups(Result<proto::GroupListResponse, String>),
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

/// Answer requests one at a time with the one server connection.
pub async fn serve(
    mut backend: Backend,
    mut requests: UnboundedReceiver<Request>,
    tx: UnboundedSender<Event>,
) {
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
        };
        if tx.send(Event::Data(data)).is_err() {
            return;
        }
    }
}
