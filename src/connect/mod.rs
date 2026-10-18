//! `bbs connect`: a BBS-style terminal app for humans. See CONNECT.md.

mod admin;
mod app;
mod art;
mod composer;
mod dialog;
mod drafts;
mod event;
mod keys;
mod navigation;
mod preferences;
mod progress;
mod screens;
mod theme;
mod timeline;

use std::io::IsTerminal;
use std::time::Instant;

use anyhow::{Result, bail};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use self::event::Event;

use crate::backend::Backend;
use crate::proto::Kind;

pub use event::Opener;

/// What the run loop waits on: an event to handle, or a redraw when the next
/// animation frame is due. With no deadline it waits for events only.
enum Wait {
    Event(Event),
    Redraw,
}

async fn wait(rx: &mut UnboundedReceiver<Event>, next_frame: Option<Instant>) -> Option<Wait> {
    let frame = async {
        match next_frame {
            Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        event = rx.recv() => event.map(Wait::Event),
        () = frame => Some(Wait::Redraw),
    }
}

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
    let draft_store = drafts::DraftStore::new(
        &crate::app::state_dir()?.join("connect-drafts"),
        opener.config.server.as_deref(),
        &opener.dir,
        &me.name,
    )?;
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
        now: std::sync::Arc::new(Instant::now),
        inbox: Default::default(),
        navigation_notice: None,
    };
    let (events, mut rx) = unbounded_channel();
    let (requests, requests_rx) = unbounded_channel();
    tokio::spawn(event::serve(backend, opener, requests_rx, events.clone()));
    let mut app = app::App::new(ctx, requests);
    app.draft_store = draft_store;
    let mut terminal = ratatui::init();
    event::spawn_input(events.clone());
    event::spawn_ticks(events);
    let result = async {
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            match wait(&mut rx, app.next_animation_frame()).await {
                None => return Ok(()),
                // A frame is due: only redraw, never housekeeping.
                Some(Wait::Redraw) => {}
                Some(Wait::Event(event)) => {
                    if app.on_event(event) {
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;
    let drafts_result = app.finish();
    ratatui::restore();
    result.and(drafts_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn wait_takes_events_and_frames_never_housekeeping() {
        let (tx, mut rx) = unbounded_channel();
        // Deadlines ride the same paused tokio clock as the sleep.
        let now = tokio::time::Instant::now().into_std();
        // An event wins over a far-future frame.
        tx.send(Event::Tick).unwrap();
        assert!(matches!(
            wait(&mut rx, Some(now + Duration::from_secs(3600))).await,
            Some(Wait::Event(Event::Tick))
        ));
        // A due frame redraws without consuming or producing events.
        assert!(matches!(wait(&mut rx, Some(now)).await, Some(Wait::Redraw)));
        assert!(rx.try_recv().is_err());
        // No frame armed: only events can wake the loop.
        let pending = tokio::time::timeout(Duration::from_secs(10), wait(&mut rx, None)).await;
        assert!(pending.is_err(), "no deadline, no wake");
        tx.send(Event::Resize).unwrap();
        assert!(matches!(
            wait(&mut rx, None).await,
            Some(Wait::Event(Event::Resize))
        ));
        // The channel closing ends the loop.
        drop(tx);
        assert!(wait(&mut rx, None).await.is_none());
    }
}
