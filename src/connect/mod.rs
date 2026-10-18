//! `bbs connect`: a BBS-style terminal app for humans. See CONNECT.md.

mod admin;
mod app;
mod art;
mod composer;
mod dialog;
mod event;
mod keys;
mod preferences;
mod progress;
mod screens;
mod theme;
mod timeline;

use std::io::IsTerminal;
use std::time::Instant;

use anyhow::{Result, bail};
use tokio::sync::mpsc::unbounded_channel;

use crate::backend::Backend;
use crate::proto::Kind;

pub use event::Opener;

pub async fn run(mut backend: Backend, opener: Opener, theme_explicit: bool) -> Result<()> {
    let me = backend.whoami().await?;
    if me.kind() != Kind::Human {
        bail!("bbs connect is for humans; agents use the other bbs commands");
    }
    if !std::io::stdout().is_terminal() {
        bail!("bbs connect needs a terminal");
    }
    let preferences =
        preferences::Preferences::load(crate::app::state_dir()?.join("connect.json"))?;
    preferences.restore_theme(theme_explicit);
    let ctx = app::Ctx {
        me: me.name,
        server: opener
            .config
            .server
            .clone()
            .unwrap_or_else(|| "local".into()),
        started: Instant::now(),
        composer_max_height_percent: opener.config.composer_max_height_percent(),
        preferences,
    };
    let (events, mut rx) = unbounded_channel();
    let (requests, requests_rx) = unbounded_channel();
    tokio::spawn(event::serve(backend, opener, requests_rx, events.clone()));
    let mut app = app::App::new(ctx, requests);
    let mut terminal = ratatui::init();
    event::spawn_input(events.clone());
    event::spawn_ticks(events);
    let result = async {
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            let Some(event) = rx.recv().await else {
                return Ok(());
            };
            if app.on_event(event) {
                return Ok(());
            }
        }
    }
    .await;
    ratatui::restore();
    result
}
