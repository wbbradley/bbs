//! `bbs connect`: a BBS-style terminal app for humans. See CONNECT.md.

mod admin;
mod app;
mod art;
mod blockers;
mod composer;
mod dialog;
mod drafts;
mod effort;
mod event;
mod history;
mod keys;
mod launcher;
mod navigation;
mod notes;
mod owner;
mod preferences;
mod progress;
mod screens;
mod theme;
mod timeline;

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use self::event::Event;

use crate::backend::Backend;
use crate::proto::Kind;

pub use event::Opener;

/// What the run loop waits on: an event to handle, or a redraw when the next
/// animation frame is due. Empty input polls never request a redraw.
enum Wait {
    Event(Event),
    Redraw,
}

async fn wait(
    rx: &mut UnboundedReceiver<Event>,
    next_frame: Option<Instant>,
    mut poll_input: impl FnMut() -> std::io::Result<Option<Event>>,
) -> Result<Option<Wait>> {
    let frame = async {
        match next_frame {
            Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(frame);
    let mut input = tokio::time::interval(Duration::from_millis(20));
    input.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = rx.recv() => return Ok(event.map(Wait::Event)),
            () = &mut frame => return Ok(Some(Wait::Redraw)),
            _ = input.tick() => {
                if let Some(event) = poll_input()? {
                    return Ok(Some(Wait::Event(event)));
                }
            }
        }
    }
}

/// Bracketed paste for the session: the terminal reports a paste as one
/// event, not as keystrokes in which each newline is `Enter`. Dropping it,
/// on any exit, turns the mode back off; ratatui's panic hook restores the
/// rest of the terminal first, and unwinding then drops this.
struct BracketedPaste;

impl BracketedPaste {
    fn enable() -> std::io::Result<BracketedPaste> {
        ratatui::crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(BracketedPaste)
    }
}

impl Drop for BracketedPaste {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    }
}

/// How `connect` starts: with a server, or from the history saved for the
/// human last signed in to this board.
pub enum Start {
    Online(Backend),
    Offline { human: String },
}

/// Where a board's and human's history lives.
fn history_dir(opener: &Opener, human: &str) -> Result<std::path::PathBuf> {
    let board = match &opener.config.server {
        Some(server) => format!("remote:{}", server.to_ascii_lowercase()),
        None => format!("local:{}", opener.dir.canonicalize()?.display()),
    };
    Ok(history::dir_for(
        &crate::app::state_dir()?.join("connect-history"),
        &board,
        human,
    ))
}

/// The human `connect` may start offline as: the one last signed in to this
/// board as `configured`, if their history was saved.
pub fn offline_start(opener: &Opener, configured: &str) -> Option<Start> {
    let dir = history_dir(opener, configured).ok()?;
    if !dir.exists() {
        return None;
    }
    let human = history::Store::open(&dir).ok()?.identity().ok()??;
    Some(Start::Offline { human })
}

pub async fn run(start: Start, opener: Opener, theme_explicit: bool) -> Result<()> {
    let (backend, me) = match start {
        Start::Online(mut backend) => {
            let me = backend.whoami().await?;
            if me.kind() != Kind::Human {
                bail!("bbs connect is for humans; agents use the other bbs commands");
            }
            // Remembered so a later start without the server can open it.
            if let Ok(dir) = history_dir(&opener, &me.name)
                && let Err(e) = history::Store::open(&dir).and_then(|s| s.set_identity(&me.name))
            {
                tracing::warn!("saving the history identity: {e}");
            }
            (Some(backend), me.name)
        }
        Start::Offline { human } => (None, human),
    };
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
        &me,
    )?;
    let history = history_dir(&opener, &me);
    let ctx = app::Ctx {
        me,
        server: opener
            .config
            .server
            .clone()
            .unwrap_or_else(|| "local".into()),
        composer_max_height_percent: opener.config.composer_max_height_percent(),
        compact_hides_thoughts: opener.config.compact_hides_thoughts(),
        preferences,
        now: std::sync::Arc::new(Instant::now),
        inbox: Default::default(),
        efforts: Default::default(),
        navigation_notice: None,
        blocker_marker: None,
    };
    let (events, mut rx) = unbounded_channel();
    let (requests, requests_rx) = unbounded_channel();
    tokio::spawn(event::serve(backend, opener, requests_rx, events.clone()));
    let mut app = app::App::new(ctx, requests);
    app.draft_store = draft_store;
    // Without it, connect works as before, from what it receives.
    app.stored = history
        .map_err(|e| tracing::warn!("finding the history: {e:#}"))
        .ok()
        .and_then(|dir| {
            app::Stored::open(&dir, events.clone())
                .map_err(|e| tracing::warn!("opening the history: {e}"))
                .ok()
        });
    let mut terminal = ratatui::init();
    let _paste = match BracketedPaste::enable() {
        Ok(paste) => paste,
        Err(e) => {
            ratatui::restore();
            return Err(anyhow::Error::new(e).context("could not enable bracketed paste"));
        }
    };
    event::spawn_ticks(events);
    let result = async {
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            match wait(&mut rx, app.next_animation_frame(), event::poll_input).await? {
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
            wait(&mut rx, Some(now + Duration::from_secs(3600)), || Ok(None))
                .await
                .unwrap(),
            Some(Wait::Event(Event::Tick))
        ));
        // A due frame redraws without consuming or producing events.
        assert!(matches!(
            wait(&mut rx, Some(now), || Ok(None)).await.unwrap(),
            Some(Wait::Redraw)
        ));
        assert!(rx.try_recv().is_err());
        // Empty terminal polls do not redraw or run housekeeping.
        let pending =
            tokio::time::timeout(Duration::from_secs(10), wait(&mut rx, None, || Ok(None))).await;
        assert!(pending.is_err(), "no deadline, no wake");
        assert!(matches!(
            wait(&mut rx, None, || Ok(Some(Event::Resize)))
                .await
                .unwrap(),
            Some(Wait::Event(Event::Resize))
        ));
        assert!(
            wait(&mut rx, None, || Err(std::io::Error::other("input failed")))
                .await
                .is_err()
        );
        tx.send(Event::Resize).unwrap();
        assert!(matches!(
            wait(&mut rx, None, || Ok(None)).await.unwrap(),
            Some(Wait::Event(Event::Resize))
        ));
        // The channel closing ends the loop.
        drop(tx);
        assert!(wait(&mut rx, None, || Ok(None)).await.unwrap().is_none());
    }
}
