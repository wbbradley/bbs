//! The app: a stack of screens under a shared title bar and status bar.

use std::collections::HashMap;
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc::UnboundedSender;

use super::composer::Composer;
use super::drafts::{Draft, DraftStore};
use super::event::{Data, Event, Request};
use super::keys::{Action, Keymap};
use super::preferences::Preferences;
use super::screens::help::Help;
use super::screens::menu::Menu;
use super::screens::splash::Splash;
use super::theme;

/// A fresh id for a screen, stream or send request.
pub fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What every screen may need to know.
pub struct Ctx {
    /// The connected human.
    pub me: String,
    pub server: String,
    pub started: Instant,
    /// `[connect] composer_max_height_percent`.
    pub composer_max_height_percent: u8,
    pub preferences: Preferences,
}

pub enum Transition {
    Stay,
    Push(Box<dyn Screen>),
    Pop,
    Quit,
    /// Ask the server for something on the screen's behalf.
    Request(Request),
    /// Remember the agent conversation view locally for this and future sessions.
    Compact(bool),
    /// Apply and remember a local color scheme immediately.
    Theme(crate::ui::theme::Scheme),
}

pub trait Screen {
    /// Shown in the title bar.
    fn title(&self) -> String;
    /// A key press before the keymap sees it: `Some` when the screen took it,
    /// as the splash and a search prompt do.
    fn raw_key(&mut self, _key: KeyEvent) -> Option<Transition> {
        None
    }
    /// Keys this screen claims before the shared keymap.
    fn hotkeys(&self) -> &[char] {
        &[]
    }
    /// What to fetch when the screen opens or reloads.
    fn requests(&self) -> Vec<Request> {
        Vec::new()
    }
    fn on_data(&mut self, _data: &Data) {}
    /// What to ask for because the human is looking at this screen: called
    /// on the top screen after new data and after each key, which may have
    /// uncovered it or changed what it shows.
    fn on_view(&mut self, _ctx: &Ctx) -> Vec<Request> {
        Vec::new()
    }
    /// Reset before re-fetching; return any obsolete subscriptions to cancel.
    fn reload(&mut self) -> Vec<Request> {
        Vec::new()
    }
    /// What to stop when the screen closes.
    fn on_close(&self) -> Vec<Request> {
        Vec::new()
    }
    /// Move a discussion's composer between its screen and the session's drafts.
    fn save_draft(&mut self, _drafts: &mut HashMap<String, Composer>) {}
    fn restore_draft(&mut self, _drafts: &mut HashMap<String, Composer>, _store: &mut DraftStore) {}
    fn draft(&self) -> Option<(&str, Draft)> {
        None
    }
    /// Optional screen feedback for the shared bottom bar, after layout.
    fn bottom_status(
        &self,
        _width: u16,
        _ctx: &Ctx,
        _draft_error: Option<&str>,
    ) -> Option<Line<'static>> {
        None
    }
    /// Test access to an Agents screen in the stack.
    #[cfg(test)]
    fn as_agents_mut(&mut self) -> Option<&mut super::screens::agents::Agents> {
        None
    }
    fn handle(&mut self, action: Action, ctx: &Ctx) -> Transition;
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx);
}

pub struct App {
    stack: Vec<Box<dyn Screen>>,
    ctx: Ctx,
    keymap: Keymap,
    requests: UnboundedSender<Request>,
    drafts: HashMap<String, Composer>,
    pub draft_store: DraftStore,
}

impl App {
    pub fn new(ctx: Ctx, requests: UnboundedSender<Request>) -> App {
        let mut app = App {
            stack: Vec::new(),
            ctx,
            keymap: Keymap::default(),
            requests,
            drafts: HashMap::new(),
            draft_store: DraftStore::default(),
        };
        app.push(Box::new(Menu::default()));
        app.push(Box::new(Splash));
        app
    }

    fn push(&mut self, mut screen: Box<dyn Screen>) {
        screen.restore_draft(&mut self.drafts, &mut self.draft_store);
        self.fetch(screen.as_ref());
        self.stack.push(screen);
    }

    fn fetch(&self, screen: &dyn Screen) {
        self.send(screen.requests());
    }

    fn send(&self, requests: Vec<Request>) {
        for request in requests {
            // The server task outlives the app; a send cannot fail first.
            let _ = self.requests.send(request);
        }
    }

    fn pop(&mut self) {
        if let Some(mut screen) = self.stack.pop() {
            self.send(screen.on_close());
            screen.save_draft(&mut self.drafts);
            self.flush_drafts();
        }
    }

    fn viewed(&mut self) {
        let screen = self.stack.last_mut().expect("the menu is never popped");
        let requests = screen.on_view(&self.ctx);
        self.send(requests);
    }

    fn top(&mut self) -> &mut Box<dyn Screen> {
        self.stack.last_mut().expect("the menu is never popped")
    }

    /// Apply one event; `true` quits.
    pub fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => {
                let quit = if let Some(transition) = self.top().raw_key(key) {
                    self.keymap = Keymap::default();
                    self.apply(transition)
                } else {
                    let hotkeys = self.top().hotkeys().to_vec();
                    match self.keymap.resolve(key, &hotkeys) {
                        Some(action) => self.on_action(action),
                        None => false,
                    }
                };
                if !quit {
                    self.viewed();
                }
                quit
            }
            Event::Data(data) => {
                if let Data::Sent { id, result } = &data {
                    for draft in self.drafts.values_mut() {
                        draft.sent(*id, result.clone());
                    }
                }
                for screen in &mut self.stack {
                    screen.on_data(&data);
                }
                self.viewed();
                if matches!(data, Data::Sent { .. }) {
                    self.flush_drafts();
                }
                false
            }
            Event::Tick => {
                self.flush_drafts();
                false
            }
            Event::Resize => false,
        }
    }

    fn flush_drafts(&mut self) {
        for (key, composer) in &self.drafts {
            self.draft_store.save(key, composer.draft());
        }
        for screen in &self.stack {
            if let Some((key, draft)) = screen.draft() {
                self.draft_store.save(key, draft);
            }
        }
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        self.flush_drafts();
        if let Some(error) = self.draft_store.error() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn on_action(&mut self, action: Action) -> bool {
        match action {
            Action::Quit => return true,
            Action::Help => {
                self.push(Box::new(Help::default()));
                return false;
            }
            Action::Reload => {
                let mut screen = self.stack.pop().expect("the menu is never popped");
                self.send(screen.reload());
                self.fetch(screen.as_ref());
                self.stack.push(screen);
                return false;
            }
            _ => {}
        }
        let ctx = &self.ctx;
        let transition = self
            .stack
            .last_mut()
            .expect("the menu is never popped")
            .handle(action, ctx);
        self.apply(transition)
    }

    /// Apply a screen's transition; `true` quits.
    fn apply(&mut self, transition: Transition) -> bool {
        match transition {
            Transition::Stay => {}
            Transition::Theme(scheme) => {
                crate::ui::theme::select(scheme);
                self.ctx.preferences.set_theme(scheme);
            }
            Transition::Compact(compact) => self.ctx.preferences.set_compact(compact),
            Transition::Push(screen) => self.push(screen),
            Transition::Pop if self.stack.len() > 1 => self.pop(),
            Transition::Request(request) => self.send(vec![request]),
            Transition::Pop | Transition::Quit => return true,
        }
        false
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        frame.render_widget(
            ratatui::widgets::Block::new().style(theme::base()),
            frame.area(),
        );
        if theme::too_small(frame.area()) {
            theme::render_too_small(frame);
            return;
        }
        let [title, body, status] = theme::frame_areas(frame.area());
        let screen = self.stack.last_mut().expect("the menu is never popped");
        frame.render_widget(Paragraph::new(theme::title_bar(&screen.title())), title);
        screen.render(frame, body, &self.ctx);
        let online = self.ctx.started.elapsed();
        let bar = theme::status_bar(&self.ctx.server, &self.ctx.me, online);
        if let Some(line) = screen.bottom_status(status.width, &self.ctx, self.draft_store.error())
        {
            let style = theme::style(theme::Role::StatusBar).patch(line.style);
            frame.render_widget(Paragraph::new(line).style(style), status);
        } else if let Some(error) = self.draft_store.error() {
            frame.render_widget(
                Paragraph::new(error)
                    .style(theme::style(theme::Role::StatusBar).patch(theme::error())),
                status,
            );
        } else {
            frame.render_widget(
                Paragraph::new(bar).style(theme::style(theme::Role::StatusBar)),
                status,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::event::Admin;
    use crate::proto;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    #[test]
    fn all_screens_use_only_the_selected_scheme() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                the_splash_comes_first_and_any_key_passes_it();
                a_small_terminal_gets_a_notice();
                agents_load_on_open_and_show_owners();
                errors_show_in_the_window();
                groups_show_members_and_writers();
                help_lists_the_keys();
                groups_and_the_feed_open_threads();
                creating_a_group_takes_a_name_and_a_description();
                each_dialog_makes_its_change_to_the_selected_group();
                a_failed_send_keeps_the_text_and_says_why();
                an_agents_timeline_merges_messages_and_activity_in_time_order();
            });
        }
    }

    #[test]
    fn config_applies_live_saves_and_returns_without_requests() {
        use crate::ui::theme::{self as scheme, Scheme, testing::with_scheme};
        with_scheme(Scheme::Gruvbox, || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("connect.json");
            let (mut app, mut rx) = app();
            app.ctx.preferences = Preferences::load(path.clone()).unwrap();
            assert!(screen(&mut app, 80, 24).contains("[C]onfig"));
            key(&mut app, 'c');
            assert_eq!(app.stack.len(), 2, "Config opens directly over the menu");
            assert!(screen(&mut app, 80, 24).contains("Current: Gruvbox dark medium"));
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            for target in [Scheme::Terminal, Scheme::NoColor, Scheme::Gruvbox] {
                let code = if target == Scheme::Gruvbox {
                    KeyCode::Home
                } else {
                    KeyCode::Down
                };
                press(&mut app, code, KeyModifiers::NONE);
                let before = scheme::current();
                terminal.draw(|f| app.draw(f)).unwrap();
                assert_eq!(scheme::current(), before, "navigation alone does not save");
                enter(&mut app);
                assert_eq!(scheme::current(), target);
                terminal.draw(|f| app.draw(f)).unwrap();
                scheme::testing::assert_buffer(terminal.backend().buffer());
                assert!(screen(&mut app, 80, 24).contains(&format!("Current: {}", target.label())));
                assert!(app.ctx.preferences.error.is_none());
                with_scheme(Scheme::Terminal, || {
                    Preferences::load(path.clone())
                        .unwrap()
                        .restore_theme(false);
                    assert_eq!(scheme::current(), target);
                });
            }
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            assert_eq!(app.stack.len(), 1);
            assert!(screen(&mut app, 80, 24).contains("Main menu"));
            assert!(rx.try_recv().is_err(), "Config makes no backend requests");
            // Also open through the normal menu selection rather than its hotkey.
            for _ in 0..3 {
                key(&mut app, 'j');
            }
            enter(&mut app);
            assert!(screen(&mut app, 80, 24).contains("Color scheme"));
        });
    }

    #[test]
    fn config_shows_save_failure_without_reverting_the_live_theme() {
        use crate::ui::theme::{self as scheme, Scheme, testing::with_scheme};
        with_scheme(Scheme::Gruvbox, || {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("state");
            let (mut app, _rx) = app();
            app.ctx.preferences = Preferences::load(parent.join("connect.json")).unwrap();
            std::fs::write(&parent, "blocked").unwrap();
            key(&mut app, 'c');
            key(&mut app, 'j');
            enter(&mut app);
            let out = screen(&mut app, 80, 24);
            assert!(out.contains("Cannot save color scheme"), "{out}");
            assert!(out.contains("Current: Terminal palette"), "{out}");
            assert_eq!(scheme::current(), Scheme::Terminal);
        });
    }

    #[test]
    fn reopening_after_config_repaints_activity_and_retains_draft_and_cursor() {
        use crate::ui::theme::{self as scheme, Scheme, testing::with_scheme};
        with_scheme(Scheme::Gruvbox, || {
            let (mut app, mut rx) = app();
            let (feed, act) = mateo_streams(&mut app, &mut rx);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![proto::Message {
                    read_at: Some("2026-10-16T18:01:00Z".into()),
                    ..message(1, "Mateo", "ada", "**answer**")
                }]),
            }));
            let record = activity(1, "01", r#"{"event":"warning","message":"activity-body"}"#);
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(vec![record.clone()]),
            }));
            assert!(matches!(rx.try_recv(), Ok(Request::MarkSeen { .. })));
            typed(&mut app, "draft");
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
            assert!(screen(&mut app, 100, 30).contains("activity-body"));
            close_conversation(&mut app, &mut rx);
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            assert!(matches!(rx.try_recv(), Ok(Request::Unwatch(_))));
            assert_eq!(app.stack.len(), 1);
            assert!(rx.try_recv().is_err());
            key(&mut app, 'c');
            key(&mut app, 'j');
            enter(&mut app);
            assert_eq!(scheme::current(), Scheme::Terminal);
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            let (feed, act) = mateo_streams(&mut app, &mut rx);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![proto::Message {
                    read_at: Some("2026-10-16T18:01:00Z".into()),
                    ..message(1, "Mateo", "ada", "**answer**")
                }]),
            }));
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(vec![record]),
            }));
            let out = screen(&mut app, 100, 30);
            assert!(
                out.contains("activity-body") && out.contains("draft"),
                "{out}"
            );
            assert!(matches!(rx.try_recv(), Ok(Request::MarkSeen { .. })));
            key(&mut app, 'X');
            enter(&mut app);
            let Request::Send { req, .. } = rx.try_recv().unwrap() else {
                panic!("expected send");
            };
            assert_eq!(req.body, "drafXt", "saved cursor survives the theme change");
        });
    }

    fn app() -> (App, UnboundedReceiver<Request>) {
        let (tx, rx) = unbounded_channel();
        let mut ctx = Ctx {
            me: "ada".into(),
            server: "bbs:7777".into(),
            started: Instant::now(),
            composer_max_height_percent: 20,
            preferences: Preferences::default(),
        };
        // Most interaction tests exercise an explicitly chosen verbose view.
        ctx.preferences.set_compact(false);
        let mut app = App::new(ctx, tx);
        // Past the splash.
        key(&mut app, ' ');
        (app, rx)
    }

    fn key(app: &mut App, c: char) -> bool {
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        crate::ui::theme::testing::assert_buffer(buffer);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn bottom(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut covered = 0;
        let mut line = String::new();
        for x in 0..80 {
            if covered > 0 {
                covered -= 1;
                continue;
            }
            let symbol = buffer[(x, 23)].symbol();
            line.push_str(symbol);
            covered = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
        }
        line.trim_end().to_string()
    }

    #[test]
    fn agent_title_is_single_and_mode_uses_the_full_width_bottom_bar() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let (mut app, mut rx) = app();
                let (feed, _) = mateo_streams(&mut app, &mut rx);
                app.on_event(Event::Data(Data::Feed {
                    id: feed,
                    result: Ok(vec![message(1, "Mateo", "ada", "body-start")]),
                }));
                let out = screen(&mut app, 80, 24);
                assert_eq!(out.matches("Mateo, reports to ada").count(), 1);
                assert!(out.lines().nth(3).unwrap().contains("body-start"), "{out}");
                assert!(
                    out.lines()
                        .last()
                        .unwrap()
                        .starts_with("following · A: all")
                );
                assert!(
                    !out.contains("1 entries")
                        && !out.contains("1 messages")
                        && !out.contains("baud")
                );
                let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let buffer = terminal.backend().buffer();
                let expected = theme::base().patch(theme::style(theme::Role::StatusBar));
                for x in 0..80 {
                    assert_eq!(buffer[(x, 23)].fg, expected.fg.unwrap());
                    assert_eq!(buffer[(x, 23)].bg, expected.bg.unwrap());
                }
            });
        }
    }

    #[test]
    fn bottom_bar_keeps_pause_new_arrivals_filters_and_unicode_search_at_minimum_width() {
        let (mut app, mut rx) = app();
        let (feed, _) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((1..=50)
                .map(|id| message(id, "Mateo", "ada", "body"))
                .collect()),
        }));
        screen(&mut app, 80, 24);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(bottom(&mut app).starts_with("paused"));
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![message(51, "Mateo", "ada", "new arrival")]),
        }));
        assert!(bottom(&mut app).contains("paused · 1 new · G follows"));
        key(&mut app, 'G');
        assert!(bottom(&mut app).starts_with("following"));
        key(&mut app, 'E');
        assert!(bottom(&mut app).contains("E: no text"));
        key(&mut app, '/');
        typed(&mut app, &"👍🏽🇺🇸👩‍💻é".repeat(20));
        let prompt = bottom(&mut app);
        assert!(prompt.contains("…") && prompt.contains("👩‍💻é█"), "{prompt}");
        assert!(prompt.contains("Enter search · Esc cancel"), "{prompt}");
        enter(&mut app);
        let committed = bottom(&mut app);
        assert!(committed.contains("0 matches · n/N"), "{committed}");
        assert!(
            committed.contains("A:all E:no text Ctrl-T:compact"),
            "{committed}"
        );
        assert!(unicode_width::UnicodeWidthStr::width(committed.as_str()) <= 80);
    }

    #[test]
    fn bottom_bar_prioritizes_errors_without_hiding_the_active_search_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("drafts");
        let (mut app, mut rx) = persistent_app(&root);
        let (feed, act) = mateo_streams(&mut app, &mut rx);
        typed(&mut app, "draft");
        app.ctx.preferences.error = Some("preference failure".into());
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Err("feed failure".into()),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Err("activity failure".into()),
        }));
        assert!(bottom(&mut app).contains("preference failure"));
        std::fs::write(&root, "blocked").unwrap();
        app.on_event(Event::Tick);
        assert!(bottom(&mut app).contains("Draft save failed; retrying"));
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, '/');
        typed(&mut app, &"👩‍💻".repeat(40));
        let prompt = bottom(&mut app);
        assert!(
            prompt.contains("draft error") && prompt.contains("👩‍💻█"),
            "{prompt}"
        );
        assert!(prompt.contains("Enter search · Esc cancel"), "{prompt}");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(bottom(&mut app).contains("Draft save failed; retrying"));
        std::fs::remove_file(root).unwrap();
        app.on_event(Event::Tick);
        assert!(bottom(&mut app).contains("preference failure"));
        app.ctx.preferences.error = None;
        assert!(bottom(&mut app).contains("feed failure"));
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(Vec::new()),
        }));
        assert!(bottom(&mut app).contains("activity: activity failure"));
    }

    /// Whether a rendered line is exactly `text`, ignoring trailing padding.
    fn has_line(out: &str, text: &str) -> bool {
        out.lines().any(|l| l.trim_end() == text)
    }

    fn agent(name: &str, owner: &str, groups: &[&str]) -> proto::Agent {
        proto::Agent {
            name: name.into(),
            owner: Some(owner.into()),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            active_at: "2026-10-16T18:00:00.000000Z".into(),
            last_sent_at: Some("2026-10-16T18:00:00.000000Z".into()),
            status: Some(format!("{name} is busy")),
            ..Default::default()
        }
    }

    #[test]
    fn the_splash_comes_first_and_any_key_passes_it() {
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            me: "ada".into(),
            server: "bbs:7777".into(),
            started: Instant::now(),
            composer_max_height_percent: 20,
            preferences: Preferences::default(),
        };
        let mut app = App::new(ctx, tx);
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("██████"), "{out}");
        assert!(out.contains("CONNECT 14400/ARQ · bbs:7777"), "{out}");
        assert!(out.contains("press any key"), "{out}");
        // `g` would start `gg` elsewhere; here it only passes the splash.
        assert!(!key(&mut app, 'g'));
        assert!(screen(&mut app, 80, 24).contains("[A]gents"));
        assert!(!key(&mut app, 'g'));
        assert!(screen(&mut app, 80, 24).contains("BBS · Groups"));
    }

    #[test]
    fn ctrl_c_quits_from_the_splash() {
        let (tx, _rx) = unbounded_channel();
        let ctx = Ctx {
            me: "ada".into(),
            server: "local".into(),
            started: Instant::now(),
            composer_max_height_percent: 20,
            preferences: Preferences::default(),
        };
        let mut app = App::new(ctx, tx);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.on_event(Event::Key(ctrl_c)));
    }

    #[test]
    fn the_menu_has_hot_keys_and_the_bars() {
        let (mut app, _rx) = app();
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("BBS · Main menu"), "{out}");
        assert!(out.contains("[A]gents"), "{out}");
        assert!(out.contains("[G]roups"), "{out}");
        assert!(out.contains("bbs:7777"), "{out}");
        assert!(out.contains("ada"), "{out}");
        assert!(!out.contains("baud"), "{out}");
    }

    #[test]
    fn a_small_terminal_gets_a_notice() {
        let (mut app, _rx) = app();
        let out = screen(&mut app, 79, 24);
        assert!(out.contains("terminal too small"), "{out}");
        assert!(out.contains("79×24; needs 80×24"), "{out}");
        assert!(!out.contains("[A]gents"), "{out}");
    }

    #[test]
    fn screens_have_no_outer_border_and_a_full_width_status_bar() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &["crew"])],
        }))));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        // The screen heading is there, but the double border is gone.
        assert!(text.contains("Your agents (1)"), "{text}");
        for cell in buffer.content.iter() {
            assert!(!"╔╗╚╝═║".contains(cell.symbol()), "{text}");
        }
        // The status bar's style fills the whole bottom row.
        let bar = theme::style(theme::Role::StatusBar);
        for x in 0..80 {
            let cell = &buffer[(x, 23)];
            assert!(
                cell.fg == bar.fg.unwrap() && cell.bg == bar.bg.unwrap(),
                "{text}"
            );
        }
    }

    #[test]
    fn agents_load_on_open_and_show_owners() {
        let (mut app, mut rx) = app();
        assert!(!key(&mut app, 'a'));
        assert_eq!(rx.try_recv().unwrap(), Request::Agents);
        assert!(matches!(rx.try_recv().unwrap(), Request::WatchSummary(_)));
        assert!(screen(&mut app, 80, 24).contains("Loading"));
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![
                agent("Mateo", "ada", &["crew"]),
                agent("Zed", "bob", &["crew"]),
            ],
        }))));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Your agents (1)"), "{out}");
        // Name and owner show in separate AGENT and OWNER columns.
        assert!(out.contains("OWNER"), "{out}");
        assert!(out.contains("Mateo"), "{out}");
        assert!(out.contains("ada"), "{out}");
        assert!(!out.contains("Mateo@ada"), "{out}");
        assert!(!out.contains("Zed"), "{out}");
        // `a` here shows everyone's; the menu's hot key does not apply.
        key(&mut app, 'a');
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("All agents (2)"), "{out}");
        assert!(out.contains("Zed"), "{out}");
        assert!(out.contains("bob"), "{out}");
        // Reload asks again; q goes back, and q on the menu quits.
        key(&mut app, 'r');
        assert_eq!(rx.try_recv().unwrap(), Request::Agents);
        assert!(matches!(rx.try_recv().unwrap(), Request::WatchSummary(_)));
        assert!(!key(&mut app, 'q'));
        assert!(screen(&mut app, 80, 24).contains("Main menu"));
        assert!(key(&mut app, 'q'));
    }

    #[test]
    fn errors_show_in_the_window() {
        let (mut app, _rx) = app();
        key(&mut app, 'g');
        app.on_event(Event::Data(Data::Groups(Err("server down".into()))));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("server down"), "{out}");
    }

    #[test]
    fn groups_show_members_and_writers() {
        let (mut app, mut rx) = app();
        key(&mut app, 'g');
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
            groups: vec![proto::Group {
                name: "crew".into(),
                members: vec!["Mateo".into(), "Zed".into()],
                description: Some("the build crew".into()),
                writers: Some(proto::GroupWriters {
                    names: vec!["Zed".into()],
                }),
            }],
        }))));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("@crew"), "{out}");
        assert!(out.contains("the build crew"), "{out}");
        assert!(out.contains("Mateo, Zed [w]"), "{out}");
    }

    #[test]
    fn help_lists_the_keys() {
        let (mut app, _rx) = app();
        key(&mut app, '?');
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("gg / Home"), "{out}");
        assert!(out.contains("Ctrl-d"), "{out}");
        key(&mut app, 'q');
        assert!(screen(&mut app, 80, 24).contains("Main menu"));
    }

    /// Open the Agents screen, taking its list and summary requests.
    fn open_agents(app: &mut App, rx: &mut UnboundedReceiver<Request>) {
        key(app, 'a');
        assert_eq!(rx.try_recv().unwrap(), Request::Agents);
        assert!(matches!(rx.try_recv().unwrap(), Request::WatchSummary(_)));
    }

    /// Fix the render time of every Agents screen on the stack.
    fn set_agents_now(app: &mut App, now: &str) {
        let now = chrono::DateTime::parse_from_rfc3339(now)
            .unwrap()
            .with_timezone(&chrono::Local);
        for screen in app.stack.iter_mut() {
            if let Some(agents) = screen.as_agents_mut() {
                agents.set_now(now);
            }
        }
    }

    fn enter(app: &mut App) {
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    fn message(id: i64, from: &str, to: &str, body: &str) -> proto::Message {
        proto::Message {
            id,
            created_at: "2026-10-16T18:00:00.000000Z".into(),
            from_kind: proto::Kind::Agent.into(),
            from_name: from.into(),
            to_name: Some(to.into()),
            body: body.into(),
            ..Default::default()
        }
    }

    /// The feed stream a screen just asked for.
    fn watched(rx: &mut UnboundedReceiver<Request>) -> (u64, proto::FeedRequest) {
        match rx.try_recv().unwrap() {
            Request::Watch { id, req } => (id, req),
            other => panic!("expected a watch, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_opens_its_conversation_and_closing_it_stops_the_stream() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &["crew"])],
        }))));
        enter(&mut app);
        let (id, req) = watched(&mut rx);
        assert_eq!(req.with.as_deref(), Some("Mateo"));
        let Request::WatchActivity { id: act, .. } = rx.try_recv().unwrap() else {
            panic!("expected an activity watch");
        };
        assert_eq!(req.backlog, 200);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(screen(&mut app, 80, 24).contains("No messages yet"));
        app.on_event(Event::Data(Data::Feed {
            id,
            result: Ok(vec![
                message(1, "Mateo", "Zed", "first needle"),
                message(2, "Zed", "Mateo", "second"),
            ]),
        }));
        // Another stream's snapshot is not this thread's.
        app.on_event(Event::Data(Data::Feed {
            id: id + 100,
            result: Ok(vec![message(9, "Zed", "Lu", "elsewhere")]),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Mateo, reports to ada"), "{out}");
        assert!(
            out.contains("first needle") && out.contains("second"),
            "{out}"
        );
        assert!(!out.contains("elsewhere"), "{out}");
        assert!(out.contains("following · A: all · E: all kinds"), "{out}");
        // The search prompt takes every key, q included, until Enter.
        key(&mut app, '/');
        for c in "q needle".chars() {
            assert!(!key(&mut app, c));
        }
        assert!(screen(&mut app, 80, 24).contains("/q needle"));
        enter(&mut app);
        assert!(screen(&mut app, 80, 24).contains("/q needle: 0 matches"));
        key(&mut app, '/');
        for c in "needle".chars() {
            key(&mut app, c);
        }
        enter(&mut app);
        assert!(screen(&mut app, 80, 24).contains("/needle: 1 matches"));
        // q closes the thread and stops its streams.
        assert!(!key(&mut app, 'q'));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(id));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(act));
        assert!(screen(&mut app, 80, 24).contains("Your agents"));
    }

    #[test]
    fn viewing_an_agent_thread_marks_its_mail_to_me_read() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &["crew"])],
        }))));
        enter(&mut app);
        let (id, _) = watched(&mut rx);
        rx.try_recv().unwrap(); // its activity
        let feed = |read: bool| Data::Feed {
            id,
            result: Ok(vec![
                message(1, "Mateo", "Zed", "not to me"),
                proto::Message {
                    read_at: read.then(|| "2026-10-16T18:01:00.000000Z".into()),
                    ..message(2, "Mateo", "ada", "to me")
                },
            ]),
        };
        // Covered by help, the thread is not being viewed.
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, '?');
        app.on_event(Event::Data(feed(false)));
        assert!(rx.try_recv().is_err());
        // Uncovered, it is.
        key(&mut app, 'q');
        assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("Mateo".into()));
        // Once the feed shows it read, there is nothing to mark.
        app.on_event(Event::Data(feed(true)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_server_without_feed_with_asks_for_an_upgrade() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &["crew"])],
        }))));
        enter(&mut app);
        let (id, _) = watched(&mut rx);
        rx.try_recv().unwrap(); // its activity
        app.on_event(Event::Data(Data::Feed {
            id,
            result: Err("required capability feed-with.v1 is unavailable".into()),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("needs a newer server (feed-with.v1)"), "{out}");
        // Reload starts the stream again.
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, 'r');
        assert!(matches!(rx.try_recv().unwrap(), Request::Unwatch(_)));
        assert_eq!(watched(&mut rx).0, id);
    }

    #[test]
    fn groups_and_the_feed_open_threads() {
        let (mut app, mut rx) = app();
        key(&mut app, 'g');
        rx.try_recv().unwrap();
        app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
            groups: vec![proto::Group {
                name: "crew".into(),
                ..Default::default()
            }],
        }))));
        enter(&mut app);
        let (_, req) = watched(&mut rx);
        assert_eq!(req.group.as_deref(), Some("crew"));
        // And the group list, for the describe dialog.
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        assert!(screen(&mut app, 80, 24).contains("@crew"));
        key(&mut app, 'q');
        rx.try_recv().unwrap();
        key(&mut app, 'q');
        key(&mut app, 'f');
        let (_, req) = watched(&mut rx);
        assert_eq!(
            req,
            proto::FeedRequest {
                backlog: 200,
                ..Default::default()
            }
        );
        assert!(screen(&mut app, 80, 24).contains("BBS · Feed"));
    }

    #[test]
    fn agents_with_unread_mail_then_unseen_activity_come_first() {
        let (mut app, mut rx) = app();
        key(&mut app, 'a');
        assert_eq!(rx.try_recv().unwrap(), Request::Agents);
        let Request::WatchSummary(id) = rx.try_recv().unwrap() else {
            panic!("expected a summary watch");
        };
        let names = ["Ana", "Bo", "Cy", "Di", "Ed"];
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: names.iter().map(|n| agent(n, "ada", &["crew"])).collect(),
        }))));
        let summary =
            |agent: &str, mail: u32, mail_at: &str, seen: u32, seen_at: &str| proto::AgentSummary {
                agent: agent.into(),
                unread_messages: mail,
                oldest_unread_message: (mail > 0).then(|| format!("2026-10-16T18:00:{mail_at}Z")),
                unseen_stream: seen,
                oldest_unseen_stream: (seen > 0).then(|| format!("2026-10-16T18:00:{seen_at}Z")),
                last_sent_at: None,
            };
        // Selecting Bo, who then moves; the selection moves with it.
        key(&mut app, 'j');
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Ok(vec![
                summary("di", 0, "", 150, "01"),
                summary("Cy", 2, "05", 0, ""),
                summary("Ed", 1, "03", 4, "00"),
                summary("Bo", 0, "", 3, "02"),
            ]),
        }));
        let out = screen(&mut app, 100, 24);
        let rows = order(&out, &["Ana", "Bo", "Cy", "Di", "Ed"]);
        assert_eq!(rows, ["Ed", "Cy", "Di", "Bo", "Ana"], "{out}");
        assert!(
            out.contains("✉1") && out.contains("•4") && out.contains("•99"),
            "{out}"
        );
        enter(&mut app);
        assert_eq!(watched(&mut rx).1.with.as_deref(), Some("Bo"));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        // Without summaries (an older server), the server's order.
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Err("required capability agent-summary.v1 is unavailable".into()),
        }));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &["Ana", "Bo", "Cy", "Di", "Ed"]),
            ["Ana", "Bo", "Cy", "Di", "Ed"]
        );
        assert!(!out.contains('✉'), "{out}");
    }

    #[test]
    fn agents_show_separate_inbox_and_activity_counts_at_minimum_width() {
        let (mut app, mut rx) = app();
        key(&mut app, 'a');
        assert_eq!(rx.try_recv().unwrap(), Request::Agents);
        let Request::WatchSummary(id) = rx.try_recv().unwrap() else {
            panic!("expected summary watch");
        };
        // Ages are rendered against a fixed time; Old's age needs an 11-wide
        // ACTIVE column, the rest show "1d18h ago".
        let cases = [
            ("Missing", 0, 0),
            ("Zero", 0, 0),
            ("One", 1, 2),
            ("MailOnly", 4, 0),
            ("ActOnly", 0, 5),
            ("Limit", 99, 99),
            ("Capped", 123, 1000),
        ];
        let mut agents: Vec<_> = cases
            .iter()
            .map(|(name, _, _)| agent(name, "ada", &["crew"]))
            .collect();
        let mut old = agent("Old", "ada", &["crew"]);
        old.last_sent_at = Some("2026-07-06T13:00:00Z".into());
        agents.push(old);
        // No known sent event (or an old server without the field): a dash.
        let mut blank = agent("Blank", "ada", &["crew"]);
        blank.last_sent_at = None;
        agents.push(blank);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents,
        }))));
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Ok(cases
                .iter()
                .skip(1)
                .map(|(name, mail, activity)| proto::AgentSummary {
                    agent: (*name).into(),
                    unread_messages: *mail,
                    unseen_stream: *activity,
                    ..Default::default()
                })
                .collect()),
        }));
        set_agents_now(&mut app, "2026-10-18T12:00:00Z");
        for width in [80, 100] {
            let out = screen(&mut app, width, 24);
            let header: Vec<_> = out
                .lines()
                .find(|line| line.contains("INBOX"))
                .unwrap()
                .chars()
                .collect();
            let column = |label: &str| {
                header
                    .windows(label.len())
                    .position(|s| s.iter().collect::<String>() == label)
                    .unwrap()
            };
            let agent = column("AGENT");
            let owner = column("OWNER");
            let inbox = column("INBOX");
            let activity = column("ACTIVITY");
            let active = column("ACTIVE");
            // ACTIVE grew to fit the widest age, "103d23h ago".
            assert!(
                agent + 10 < owner
                    && owner + 9 < inbox
                    && inbox + 5 < activity
                    && activity + 8 < column("GROUPS")
                    && column("GROUPS") < active
                    && active + 11 < column("STATUS"),
                "{out}"
            );
            for (name, mail, stream) in cases {
                let row: Vec<_> = out
                    .lines()
                    .find(|line| line.contains(name))
                    .unwrap()
                    .chars()
                    .collect();
                let cell = |start, len| {
                    row[start..start + len]
                        .iter()
                        .collect::<String>()
                        .trim()
                        .to_string()
                };
                assert_eq!(cell(owner, 9), "ada", "{out}");
                assert_eq!(cell(active, 11), "1d18h ago", "{out}");
                assert_eq!(
                    cell(inbox, 5),
                    if mail == 0 {
                        String::new()
                    } else {
                        format!("✉{}", mail.min(99))
                    },
                    "{out}"
                );
                assert_eq!(
                    cell(activity, 8),
                    if stream == 0 {
                        String::new()
                    } else {
                        format!("•{}", stream.min(99))
                    },
                    "{out}"
                );
            }
            // A 100+ day age is not truncated.
            let row: Vec<_> = out
                .lines()
                .find(|line| line.contains("Old"))
                .unwrap()
                .chars()
                .collect();
            let cell: String = row[active..active + 11]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
            assert_eq!(cell, "103d23h ago", "{out}");
            // An unknown active time renders a dash.
            let row: Vec<_> = out
                .lines()
                .find(|line| line.contains("Blank"))
                .unwrap()
                .chars()
                .collect();
            let cell: String = row[active..active + 11]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
            assert_eq!(cell, "-", "{out}");
        }
    }

    #[test]
    fn agent_cells_fit_their_columns() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        let mut busy = agent("Mateo", "ada", &["crew", "release", "infra"]);
        busy.status = Some("reviewing the very long pull request about feed filters".into());
        busy.last_sent_at = Some("2026-10-18T11:46:00Z".into());
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![busy],
        }))));
        set_agents_now(&mut app, "2026-10-18T12:00:00Z");
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("crew, release +1"), "{out}");
        assert!(out.contains("reviewing the very…"), "{out}");
        // ACTIVE shows an age against the fixed render time.
        assert!(out.contains("14m ago"), "{out}");
    }

    #[test]
    fn agents_active_ages_update_live_from_the_summary_stream() {
        let (mut app, mut rx) = app();
        key(&mut app, 'a');
        assert_eq!(rx.try_recv().unwrap(), Request::Agents);
        let Request::WatchSummary(id) = rx.try_recv().unwrap() else {
            panic!("expected summary watch");
        };
        let mut mateo = agent("Mateo", "ada", &["crew"]);
        mateo.last_sent_at = None;
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![mateo],
        }))));
        set_agents_now(&mut app, "2026-10-18T12:00:00Z");
        // No known send yet: a dash, no age.
        assert!(!screen(&mut app, 80, 24).contains("ago"));
        let sum = |last_sent_at: Option<&str>| proto::AgentSummary {
            agent: "Mateo".into(),
            last_sent_at: last_sent_at.map(Into::into),
            ..Default::default()
        };
        // The agent's first send arrives over the summary stream...
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Ok(vec![sum(Some("2026-10-18T11:46:00Z"))]),
        }));
        assert!(screen(&mut app, 80, 24).contains("14m ago"));
        // ...and a newer one replaces it.
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Ok(vec![sum(Some("2026-10-18T11:56:00Z"))]),
        }));
        assert!(screen(&mut app, 80, 24).contains("4m ago"));
        // An old server has no summaries; the list's value (none here) rules.
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Err("required capability agent-summary.v1 is unavailable".into()),
        }));
        assert!(!screen(&mut app, 80, 24).contains("ago"));
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
        app.on_event(Event::Key(KeyEvent::new(code, modifiers)))
    }

    /// The agent thread for Mateo, with its feed stream's id.
    /// Open Mateo's thread; returns its feed stream and its activity stream.
    fn mateo_streams(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> (u64, u64) {
        open_agents(app, rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &["crew"])],
        }))));
        enter(app);
        let feed = watched(rx).0;
        match rx.try_recv().unwrap() {
            Request::WatchActivity { id, req } => {
                assert_eq!(req.agent.as_deref(), Some("Mateo"));
                (feed, id)
            }
            other => panic!("expected an activity watch, got {other:?}"),
        }
    }

    fn mateo(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> u64 {
        mateo_streams(app, rx).0
    }

    #[test]
    fn the_composer_sends_to_the_agent_and_empties_on_success() {
        let (mut app, mut rx) = app();
        let id = mateo(&mut app, &mut rx);
        assert!(screen(&mut app, 80, 24).contains("Mateo · Enter sends"));
        // Keys are text now: q does not go back.
        for c in "quick".chars() {
            assert!(!key(&mut app, c));
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        for c in "note".chars() {
            key(&mut app, c);
        }
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Mateo · Enter sends"), "{out}");
        assert!(out.contains("quick") && out.contains("note"), "{out}");
        enter(&mut app);
        let Request::Send { id: sent, req } = rx.try_recv().unwrap() else {
            panic!("expected a send");
        };
        assert_eq!(
            (req.to.as_str(), req.body.as_str()),
            ("Mateo", "quick\nnote")
        );
        assert!(screen(&mut app, 80, 24).contains("Sending…"));
        app.on_event(Event::Data(Data::Sent {
            id: sent,
            result: Ok(()),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(!out.contains("quick"), "{out}");
        // Esc closes the agent conversation directly.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen(&mut app, 80, 24).contains("Your agents"));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(id));
    }

    #[test]
    fn agent_insert_and_navigation_modes_use_explicit_switches() {
        let (mut app, mut rx) = app();
        let id = mateo(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id,
            result: Ok((1..50)
                .map(|id| message(id, "Zed", "Mateo", &format!("message {id}")))
                .collect()),
        }));
        assert!(screen(&mut app, 100, 24).contains("Mateo · Enter sends"));
        typed(&mut app, "first");
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut app, "second");
        // Up from either line stays in the editor, including at its top.
        for _ in 0..3 {
            press(&mut app, KeyCode::Up, KeyModifiers::NONE);
            assert!(screen(&mut app, 100, 24).contains("Mateo · Enter sends"));
        }
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let out = screen(&mut app, 100, 24);
        assert!(
            out.contains("Navigation · Esc/i write to Mateo") && out.contains("paused"),
            "{out}"
        );
        for key in [
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        ] {
            app.on_event(Event::Key(key));
            assert!(screen(&mut app, 100, 24).contains("Navigation · Esc/i write to Mateo"));
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let out = screen(&mut app, 100, 24);
        assert!(
            out.contains("Mateo · Enter sends") && out.contains("first") && out.contains("second"),
            "{out}"
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen(&mut app, 100, 24).contains("Your agents"));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(id));
    }

    #[test]
    fn ctrl_u_edits_and_undoes_in_agent_insert_mode_without_navigating() {
        let (mut app, mut rx) = app();
        mateo(&mut app, &mut rx);
        typed(&mut app, "first");
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut app, "second");
        for _ in 0..3 {
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        let out = screen(&mut app, 100, 24);
        assert!(
            out.contains("Mateo · Enter sends")
                && out.contains("PgUp scroll")
                && !out.contains("Navigation"),
            "{out}"
        );
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(screen(&mut app, 100, 24).contains("firstond"));
        press(&mut app, KeyCode::Char('7'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('7'), KeyModifiers::CONTROL);
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected send");
        };
        assert_eq!(req.body, "first\nsecond");
    }

    /// Reopen the selected agent after a conversation has closed.
    fn reopen(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> u64 {
        enter(app);
        let id = watched(rx).0;
        assert!(matches!(rx.try_recv(), Ok(Request::WatchActivity { .. })));
        id
    }

    fn close_conversation(app: &mut App, rx: &mut UnboundedReceiver<Request>) {
        press(app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Request::Unwatch(_))));
        assert!(matches!(rx.try_recv(), Ok(Request::Unwatch(_))));
    }

    fn persistent_app(root: &std::path::Path) -> (App, UnboundedReceiver<Request>) {
        let (mut app, rx) = app();
        app.draft_store = DraftStore::new(root, Some("bbs:7777"), root, "ada").unwrap();
        (app, rx)
    }

    #[test]
    fn draft_ticks_and_shutdown_restore_text_and_cursor_in_a_new_session() {
        for tick in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (mut first, mut rx) = persistent_app(dir.path());
            mateo(&mut first, &mut rx);
            typed(&mut first, "one");
            press(&mut first, KeyCode::Enter, KeyModifiers::ALT);
            typed(&mut first, "界two");
            press(&mut first, KeyCode::Left, KeyModifiers::NONE);
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                0,
                "no keystroke writes"
            );
            if tick {
                // Also capture drafts under help, not just the top screen.
                first.push(Box::new(Help::default()));
                first.on_event(Event::Tick);
            } else {
                first.finish().unwrap();
            }
            let (mut next, mut rx) = persistent_app(dir.path());
            assert!(
                screen(&mut next, 80, 24).contains("Main menu"),
                "drafts never open threads"
            );
            mateo(&mut next, &mut rx);
            typed(&mut next, "X");
            enter(&mut next);
            let Request::Send { req, .. } = rx.try_recv().unwrap() else {
                panic!("send")
            };
            assert_eq!(req.body, "one\n界twXo");
        }
    }

    #[test]
    fn group_drafts_restore_across_sessions_and_keep_undo_within_session() {
        let dir = tempfile::tempdir().unwrap();
        let open_group = |app: &mut App, rx: &mut UnboundedReceiver<Request>| {
            key(app, 'g');
            assert_eq!(rx.try_recv().unwrap(), Request::Groups);
            app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
                groups: vec![proto::Group {
                    name: "crew".into(),
                    ..Default::default()
                }],
            }))));
            enter(app);
            watched(rx);
            assert_eq!(rx.try_recv().unwrap(), Request::Groups);
            key(app, 'i');
        };
        let (mut first, mut rx) = persistent_app(dir.path());
        open_group(&mut first, &mut rx);
        typed(&mut first, "group draft");
        press(&mut first, KeyCode::Left, KeyModifiers::NONE);
        first.finish().unwrap();
        let (mut next, mut rx) = persistent_app(dir.path());
        open_group(&mut next, &mut rx);
        typed(&mut next, "X");
        // Esc leaves group composing; a second Esc closes its screen.
        press(&mut next, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut next, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Request::Unwatch(_))));
        enter(&mut next);
        watched(&mut rx);
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        key(&mut next, 'i');
        press(&mut next, KeyCode::Char('7'), KeyModifiers::CONTROL);
        enter(&mut next);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("send")
        };
        assert_eq!(req.to, "@crew");
        assert_eq!(
            req.body, "group draft",
            "session reopen preserved undo history"
        );
    }

    #[test]
    fn persisted_sends_clear_only_successful_unchanged_text_even_after_close() {
        for success in [false, true] {
            for edited in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let (mut first, mut rx) = persistent_app(dir.path());
                mateo(&mut first, &mut rx);
                typed(&mut first, "draft");
                first.on_event(Event::Tick);
                enter(&mut first);
                let Request::Send { id, .. } = rx.try_recv().unwrap() else {
                    panic!("send")
                };
                if edited {
                    typed(&mut first, "X");
                }
                close_conversation(&mut first, &mut rx);
                first.on_event(Event::Data(Data::Sent {
                    id,
                    result: if success {
                        Ok(())
                    } else {
                        Err("offline".into())
                    },
                }));
                let (mut next, mut rx) = persistent_app(dir.path());
                mateo(&mut next, &mut rx);
                assert!(!screen(&mut next, 80, 24).contains("Sending…"));
                enter(&mut next);
                if success && !edited {
                    assert!(rx.try_recv().is_err());
                } else {
                    let Request::Send { req, .. } = rx.try_recv().unwrap() else {
                        panic!("send")
                    };
                    assert_eq!(req.body, if edited { "draftX" } else { "draft" });
                }
            }
        }
    }

    #[test]
    fn unopened_or_removed_discussions_never_load_drafts_and_load_errors_stay_visible() {
        let dir = tempfile::tempdir().unwrap();
        let (mut first, mut rx) = persistent_app(dir.path());
        mateo(&mut first, &mut rx);
        typed(&mut first, "saved");
        first.finish().unwrap();
        let scope = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let file = std::fs::read_dir(scope)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&file, "broken").unwrap();
        let (mut absent, mut rx) = persistent_app(dir.path());
        open_agents(&mut absent, &mut rx);
        absent.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: Vec::new(),
        }))));
        enter(&mut absent);
        assert!(
            absent.draft_store.error().is_none(),
            "only opened listed targets load"
        );
        assert!(screen(&mut absent, 80, 24).contains("Your agents"));
        absent.finish().unwrap();
        let (mut existing, mut rx) = persistent_app(dir.path());
        mateo(&mut existing, &mut rx);
        typed(&mut existing, "session-only");
        existing.on_event(Event::Tick);
        let out = screen(&mut existing, 100, 24);
        assert!(
            out.contains("Draft load failed; repair file and restart")
                && out.contains("session-only"),
            "{out}"
        );
        assert!(existing.finish().is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "broken");
    }

    #[test]
    fn pending_send_restores_as_editable_draft_in_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let (mut first, mut rx) = persistent_app(dir.path());
        mateo(&mut first, &mut rx);
        typed(&mut first, "unconfirmed");
        enter(&mut first);
        assert!(matches!(rx.try_recv(), Ok(Request::Send { .. })));
        first.finish().unwrap();
        let (mut next, mut rx) = persistent_app(dir.path());
        mateo(&mut next, &mut rx);
        assert!(!screen(&mut next, 80, 24).contains("Sending…"));
        enter(&mut next);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("send")
        };
        assert_eq!(req.body, "unconfirmed");
    }

    #[test]
    fn draft_errors_keep_the_status_bar_surface_across_the_full_width() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("drafts");
                let (mut app, mut rx) = persistent_app(&root);
                mateo(&mut app, &mut rx);
                typed(&mut app, "draft");
                std::fs::write(&root, "blocked").unwrap();
                app.on_event(Event::Tick);
                let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let buffer = terminal.backend().buffer();
                let area = buffer.area;
                let expected = theme::base()
                    .patch(theme::style(theme::Role::StatusBar))
                    .patch(theme::error());
                for x in area.left()..area.right() {
                    let cell = &buffer[(x, area.bottom() - 1)];
                    assert_eq!(cell.fg, expected.fg.unwrap());
                    assert_eq!(cell.bg, expected.bg.unwrap());
                }
            });
        }
    }

    #[test]
    fn draft_save_errors_are_visible_and_retry_on_tick() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("drafts");
        let (mut app, mut rx) = persistent_app(&root);
        mateo(&mut app, &mut rx);
        typed(&mut app, "session draft");
        std::fs::write(&root, "blocked").unwrap();
        app.on_event(Event::Tick);
        let out = screen(&mut app, 100, 24);
        assert!(
            out.contains("Draft save failed; retrying (agent:mateo)")
                && out.contains("session draft"),
            "{out}"
        );
        assert!(
            app.finish().is_err(),
            "shutdown surfaces unsaved drafts too"
        );
        std::fs::remove_file(&root).unwrap();
        app.on_event(Event::Tick);
        assert!(app.draft_store.error().is_none());
        app.finish().unwrap();
    }

    #[test]
    fn each_agent_restores_its_own_draft_and_cursor() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &[]), agent("Zed", "ada", &[])],
        }))));
        reopen(&mut app, &mut rx);
        typed(&mut app, "one");
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut app, "two");
        for _ in 0..2 {
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        close_conversation(&mut app, &mut rx);

        key(&mut app, 'j');
        reopen(&mut app, &mut rx);
        assert!(!has_line(&screen(&mut app, 80, 24), "one"));
        typed(&mut app, "other");
        for _ in 0..3 {
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        close_conversation(&mut app, &mut rx);

        key(&mut app, 'k');
        reopen(&mut app, &mut rx);
        typed(&mut app, "X");
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected send");
        };
        assert_eq!((req.to.as_str(), req.body.as_str()), ("Mateo", "one\ntXwo"));
        close_conversation(&mut app, &mut rx);
        key(&mut app, 'j');
        reopen(&mut app, &mut rx);
        typed(&mut app, "Y");
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected send");
        };
        assert_eq!((req.to.as_str(), req.body.as_str()), ("Zed", "otYher"));
    }

    #[test]
    fn sends_finish_safely_while_conversations_are_closed_or_reopened() {
        for success in [true, false] {
            for edited in [true, false] {
                for reopened in [true, false] {
                    let (mut app, mut rx) = app();
                    mateo(&mut app, &mut rx);
                    typed(&mut app, "draft");
                    enter(&mut app);
                    let Request::Send { id, .. } = rx.try_recv().unwrap() else {
                        panic!("expected send");
                    };
                    if edited {
                        typed(&mut app, "X");
                    }
                    close_conversation(&mut app, &mut rx);
                    if reopened {
                        reopen(&mut app, &mut rx);
                        assert!(screen(&mut app, 80, 24).contains("Sending…"));
                        enter(&mut app);
                        assert!(rx.try_recv().is_err(), "cannot duplicate a pending send");
                    }
                    app.on_event(Event::Data(Data::Sent {
                        id,
                        result: if success {
                            Ok(())
                        } else {
                            Err("offline".into())
                        },
                    }));
                    if !reopened {
                        reopen(&mut app, &mut rx);
                    }
                    let out = screen(&mut app, 80, 24);
                    assert!(!out.contains("Sending…"), "{out}");
                    if !success {
                        assert!(out.contains("Not sent: offline"), "{out}");
                    }
                    enter(&mut app);
                    if edited || !success {
                        let Request::Send { id: retry, req } = rx.try_recv().unwrap() else {
                            panic!("expected retry/new send");
                        };
                        assert_ne!(retry, id);
                        assert_eq!(req.body, if edited { "draftX" } else { "draft" });
                        // An old reply cannot clear the newer send or unlock another Enter.
                        app.on_event(Event::Data(Data::Sent { id, result: Ok(()) }));
                        enter(&mut app);
                        assert!(rx.try_recv().is_err());
                    } else {
                        assert!(rx.try_recv().is_err(), "success clears the submitted draft");
                        assert!(!has_line(&out, "draft"), "{out}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_failed_send_keeps_the_text_and_says_why() {
        let (mut app, mut rx) = app();
        mateo(&mut app, &mut rx);
        key(&mut app, 'x');
        enter(&mut app);
        let Request::Send { id, .. } = rx.try_recv().unwrap() else {
            panic!("expected a send");
        };
        app.on_event(Event::Data(Data::Sent {
            id,
            result: Err("no agent named Mateo".into()),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Not sent: no agent named Mateo"), "{out}");
        assert!(has_line(&out, "x"), "{out}");
        // Ctrl-C still quits from the composer.
        assert!(press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn groups_compose_broadcasts_and_the_feed_has_no_composer() {
        let (mut app, mut rx) = app();
        key(&mut app, 'g');
        rx.try_recv().unwrap();
        app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
            groups: vec![proto::Group {
                name: "crew".into(),
                ..Default::default()
            }],
        }))));
        enter(&mut app);
        watched(&mut rx);
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        key(&mut app, 'i');
        key(&mut app, 'x');
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(!has_line(&screen(&mut app, 80, 24), "x"));
        key(&mut app, 'x');
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected a send");
        };
        assert_eq!(req.to, "@crew");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, 'q');
        rx.try_recv().unwrap();
        key(&mut app, 'q');
        key(&mut app, 'f');
        watched(&mut rx);
        assert!(!screen(&mut app, 80, 24).contains("write to"));
        // With no composer, i does nothing and q still goes back.
        key(&mut app, 'i');
        assert!(!key(&mut app, 'q'));
        assert!(screen(&mut app, 80, 24).contains("Main menu"));
    }

    #[test]
    fn the_composer_grows_to_its_share_of_the_window() {
        let (mut app, mut rx) = app();
        mateo(&mut app, &mut rx);
        let rows = |app: &mut App| {
            screen(app, 80, 30)
                .lines()
                .take(29)
                .skip_while(|l| !l.contains("Mateo · Enter sends"))
                .skip(1)
                .take_while(|l| !l.starts_with(" bbs:7777"))
                .count()
        };
        assert_eq!(rows(&mut app), 2);
        for _ in 0..10 {
            key(&mut app, 'x');
            press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        }
        // 20% of 30 rows.
        assert_eq!(rows(&mut app), 6);
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            assert!(!key(app, c));
        }
    }

    /// The Groups list holding @crew, described, with Mateo and Zed.
    fn crew_list(app: &mut App, rx: &mut UnboundedReceiver<Request>) {
        key(app, 'g');
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
            groups: vec![proto::Group {
                name: "crew".into(),
                members: vec!["Mateo".into(), "Zed".into()],
                description: Some("the build crew".into()),
                writers: None,
            }],
        }))));
    }

    fn admin(rx: &mut UnboundedReceiver<Request>) -> (u64, Admin) {
        match rx.try_recv().unwrap() {
            Request::Admin { id, op } => (id, op),
            other => panic!("expected an admin request, got {other:?}"),
        }
    }

    #[test]
    fn creating_a_group_takes_a_name_and_a_description() {
        let (mut app, mut rx) = app();
        crew_list(&mut app, &mut rx);
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("c create · e describe"), "{out}");
        key(&mut app, 'c');
        // q is text in a dialog, not back.
        typed(&mut app, "ops q");
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        typed(&mut app, "on call");
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Create a group"), "{out}");
        assert!(out.contains("name: ops"), "{out}");
        enter(&mut app);
        let (id, op) = admin(&mut rx);
        assert_eq!(
            op,
            Admin::Create(proto::GroupRegisterRequest {
                group: "ops".into(),
                description: Some("on call".into()),
            })
        );
        assert!(screen(&mut app, 80, 24).contains("Working…"));
        app.on_event(Event::Data(Data::Admin {
            id,
            result: Ok("Created @ops.".into()),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Created @ops."), "{out}");
        assert!(!out.contains("Create a group"), "{out}");
    }

    #[test]
    fn each_dialog_makes_its_change_to_the_selected_group() {
        let (mut app, mut rx) = app();
        crew_list(&mut app, &mut rx);
        // Describe starts from the current description; emptying it clears.
        key(&mut app, 'e');
        assert!(screen(&mut app, 80, 24).contains("description: the build crew"));
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        enter(&mut app);
        let (_, op) = admin(&mut rx);
        assert_eq!(
            op,
            Admin::Describe(proto::GroupDescribeRequest {
                group: "crew".into(),
                description: None,
            })
        );
        key(&mut app, '+');
        typed(&mut app, "Lu, Kai  Ines");
        enter(&mut app);
        let Admin::Add(add) = admin(&mut rx).1 else {
            panic!("expected add");
        };
        assert_eq!(add.agents, ["Lu", "Kai", "Ines"]);
        for (k, grant) in [('w', true), ('W', false)] {
            key(&mut app, k);
            typed(&mut app, "Zed");
            enter(&mut app);
            assert_eq!(
                admin(&mut rx).1,
                Admin::Writer(proto::SetGroupWriterRequest {
                    group: "crew".into(),
                    agent: "Zed".into(),
                    grant,
                })
            );
        }
        key(&mut app, '-');
        typed(&mut app, "Mateo");
        enter(&mut app);
        assert_eq!(
            admin(&mut rx).1,
            Admin::Remove(proto::GroupRemoveRequest {
                group: "crew".into(),
                agent: "Mateo".into(),
            })
        );
        // Esc cancels without a request.
        key(&mut app, '-');
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(rx.try_recv().is_err());
        assert!(screen(&mut app, 80, 24).contains("Groups (1)"));
    }

    #[test]
    fn deleting_asks_first_and_failures_show() {
        let (mut app, mut rx) = app();
        crew_list(&mut app, &mut rx);
        key(&mut app, 'D');
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Delete @crew?"), "{out}");
        key(&mut app, 'n');
        assert!(rx.try_recv().is_err());
        key(&mut app, 'D');
        key(&mut app, 'y');
        let (id, op) = admin(&mut rx);
        assert_eq!(
            op,
            Admin::Delete(proto::GroupRequest {
                group: "crew".into()
            })
        );
        app.on_event(Event::Data(Data::Admin {
            id,
            result: Err("only humans can delete groups".into()),
        }));
        assert!(screen(&mut app, 80, 24).contains("only humans can delete groups"));
    }

    #[test]
    fn a_group_thread_has_the_dialogs_for_its_own_group() {
        let (mut app, mut rx) = app();
        crew_list(&mut app, &mut rx);
        enter(&mut app);
        let (id, _) = watched(&mut rx);
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
            groups: vec![proto::Group {
                name: "crew".into(),
                description: Some("renamed crew".into()),
                ..Default::default()
            }],
        }))));
        // Creating is for the Groups list.
        key(&mut app, 'c');
        assert!(!screen(&mut app, 80, 24).contains("Create a group"));
        key(&mut app, 'e');
        assert!(screen(&mut app, 80, 24).contains("description: renamed crew"));
        typed(&mut app, "!");
        enter(&mut app);
        assert_eq!(
            rx.try_recv().unwrap(),
            Request::Admin {
                id,
                op: Admin::Describe(proto::GroupDescribeRequest {
                    group: "crew".into(),
                    description: Some("renamed crew!".into()),
                }),
            }
        );
        app.on_event(Event::Data(Data::Admin {
            id,
            result: Ok("Described @crew.".into()),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("following · Described @crew."), "{out}");
    }

    #[test]
    fn help_scrolls_to_its_last_key() {
        let (mut app, _rx) = app();
        key(&mut app, '?');
        assert!(!screen(&mut app, 80, 24).contains("a menu item's hot key"));
        key(&mut app, 'G');
        assert!(screen(&mut app, 80, 24).contains("a menu item's hot key"));
    }

    fn activity(id: i64, at: &str, record: &str) -> proto::Activity {
        proto::Activity {
            id,
            created_at: format!("2026-10-16T18:00:{at}.000000Z"),
            agent: "Mateo".into(),
            owner: Some("ada".into()),
            record: record.into(),
        }
    }

    fn at(id: i64, at: &str, from: &str, to: &str, body: &str) -> proto::Message {
        proto::Message {
            created_at: format!("2026-10-16T18:00:{at}.000000Z"),
            ..message(id, from, to, body)
        }
    }

    /// Where each of `needles` first appears in `out`, in order of appearance.
    fn order(out: &str, needles: &[&str]) -> Vec<String> {
        let mut found: Vec<(usize, &str)> = needles
            .iter()
            .map(|n| (out.find(n).unwrap_or_else(|| panic!("{n}: {out}")), *n))
            .collect();
        found.sort();
        found.into_iter().map(|(_, n)| n.to_string()).collect()
    }

    #[test]
    fn an_agents_timeline_merges_messages_and_activity_in_time_order() {
        let (mut app, mut rx) = app();
        let (feed, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        // Activity's history often arrives after newer messages.
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![at(1, "02", "Zed", "Mateo", "a request")]),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                activity(1, "01", r#"{"ts":"2026-10-16T18:00:01Z","event":"text","text":"working on the **build**"}"#),
                activity(2, "03", r#"{"ts":"2026-10-16T18:00:03Z","event":"text","text":"reply sent"}"#),
                activity(3, "04", r#"{"ts":"2026-10-16T18:00:04Z","event":"tool","id":"t1","title":"cargo test","status":"in_progress"}"#),
            ]),
        }));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &["working on the build", "a request", "reply sent"]),
            ["working on the build", "a request", "reply sent"]
        );
        assert!(out.contains("cargo test running"), "{out}");
        assert!(out.contains("following · A: all · E: all kinds"), "{out}");
        assert!(out.contains("Navigation · Esc/i write to Mateo"), "{out}");

        // A cycles messages only, activity only, then everything again.
        key(&mut app, 'A');
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("A: messages ·"), "{out}");
        assert!(
            out.contains("a request") && !out.contains("reply sent"),
            "{out}"
        );
        assert!(!out.contains("cargo test running"), "{out}");
        key(&mut app, 'A');
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("A: activity · E: all kinds"), "{out}");
        assert!(
            !out.contains("a request") && out.contains("reply sent"),
            "{out}"
        );
        key(&mut app, 'A');
        assert!(screen(&mut app, 100, 24).contains("A: all"));

        // A reload starts both streams over without the activity it had.
        while rx.try_recv().is_ok() {}
        key(&mut app, 'r');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(act));
        assert_eq!(watched(&mut rx).0, feed);
        let Request::WatchActivity { id: new_act, .. } = rx.try_recv().unwrap() else {
            panic!("expected replacement activity watch");
        };
        assert_ne!(new_act, act);
        let out = screen(&mut app, 100, 24);
        assert!(
            !out.contains("reply sent") && out.contains("a request"),
            "{out}"
        );
        // Closing the thread stops both.
        while rx.try_recv().is_ok() {}
        key(&mut app, 'q');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(feed));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(new_act));
    }

    #[test]
    fn compact_progress_uses_live_ids_and_never_advances_on_ticks_or_repaints() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        typed(&mut app, "ab");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        let hint_cell = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let area = buffer.area;
            let y = (area.top()..area.bottom())
                .find(|y| {
                    (area.left()..area.right())
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains("Mateo · Enter sends")
                })
                .unwrap();
            buffer[(area.right() - 1, y)]
                .symbol()
                .chars()
                .next()
                .unwrap()
        };
        assert_eq!(hint_cell(&mut app), ' ', "verbose has no indicator");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '?');
        app.on_event(Event::Data(Data::ActivityBaseline {
            id: act,
            through_id: 500,
        }));
        let batch = |id, kind| Data::Activity {
            id: act,
            result: Ok(vec![activity(
                id,
                "01",
                &format!(r#"{{"event":"{kind}","text":"hidden"}}"#),
            )]),
        };
        app.on_event(Event::Data(batch(500, "turn_end")));
        assert_eq!(hint_cell(&mut app), '?', "history does not imply done");
        app.on_event(Event::Data(batch(501, "text")));
        assert_eq!(hint_cell(&mut app), '⠋');
        app.on_event(Event::Tick);
        assert_eq!(hint_cell(&mut app), '⠋');
        app.on_event(Event::Data(batch(501, "text")));
        assert_eq!(hint_cell(&mut app), '⠋');
        app.on_event(Event::Data(batch(502, "tool")));
        assert_eq!(hint_cell(&mut app), '⠙');
        app.on_event(Event::Data(batch(503, "turn_end")));
        assert_eq!(hint_cell(&mut app), '✓');
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), ' ');
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '✓');
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Err("disconnected".into()),
        }));
        assert_eq!(hint_cell(&mut app), '?');
        while rx.try_recv().is_ok() {}
        key(&mut app, 'X');
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected send")
        };
        assert_eq!(
            req.body, "aXb",
            "activity/toggle keeps editing focus and cursor"
        );
    }

    #[test]
    fn compact_conversations_filter_direct_mail_and_live_arrivals() {
        let (mut app, mut rx) = app();
        let (feed, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let broadcast = proto::Message {
            to_name: None,
            to_group: Some("crew".into()),
            ..message(4, "Mateo", "", "broadcast-body")
        };
        let outbound = proto::Message {
            from_kind: proto::Kind::Human.into(),
            created_at: "2026-10-16T18:00:01Z".into(),
            to_owner: Some("ada".into()),
            ..message(5, "ADA", "MATEO", "outbound-body")
        };
        let wrong_kind = proto::Message {
            from_kind: proto::Kind::Human.into(),
            ..message(7, "Mateo", "ada", "human-sender-body")
        };
        let owned_recipient = proto::Message {
            to_owner: Some("someone".into()),
            ..message(8, "Mateo", "ada", "owned-recipient-body")
        };
        let direct = message(1, "Mateo", "ada", "**direct-one**");
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![
                direct,
                message(2, "Mateo", "Zed", "to-agent-body"),
                message(3, "Mateo", "Ben", "other-human-body"),
                broadcast,
                outbound,
                message(6, "Zed", "ada", "other-agent-body"),
                wrong_kind,
                owned_recipient,
                proto::Message {
                    created_at: "2026-10-16T18:00:02Z".into(),
                    ..message(9, "MATEO", "ADA", "direct-two")
                },
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..message(10, "Ben", "Mateo", "other-human-outbound")
                },
                message(11, "ada", "Mateo", "agent-named-human"),
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..message(12, "ada", "Zed", "outbound-other-agent")
                },
            ]),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![activity(
                1,
                "01",
                r#"{"event":"warning","message":"activity-body"}"#,
            )]),
        }));
        let verbose = screen(&mut app, 120, 40);
        assert!(
            verbose.contains("Ctrl-T: compact")
                && verbose.contains("broadcast-body")
                && verbose.contains("activity-body"),
            "{verbose}"
        );
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let compact = screen(&mut app, 120, 40);
        assert!(
            !compact.contains("3 messages") && compact.contains("Ctrl-T: verbose"),
            "{compact}"
        );
        for shown in ["direct-one", "outbound-body", "direct-two"] {
            assert!(compact.contains(shown), "{compact}");
        }
        for hidden in [
            "to-agent-body",
            "other-human-body",
            "broadcast-body",
            "other-human-outbound",
            "agent-named-human",
            "outbound-other-agent",
            "other-agent-body",
            "human-sender-body",
            "owned-recipient-body",
            "activity-body",
            "→",
            "unread",
            "**",
        ] {
            assert!(!compact.contains(hidden), "{hidden}: {compact}");
        }
        assert_eq!(
            order(&compact, &["direct-one", "outbound-body", "direct-two"]),
            ["direct-one", "outbound-body", "direct-two"]
        );
        assert!(!compact.contains("2026-10-16"), "{compact}");
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![
                proto::Message {
                    created_at: "2026-10-16T18:00:04Z".into(),
                    ..message(13, "Mateo", "ada", "live-direct")
                },
                message(14, "Mateo", "Ben", "live-hidden"),
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    created_at: "2026-10-16T18:00:03Z".into(),
                    ..message(15, "ada", "Mateo", "live-outbound")
                },
            ]),
        }));
        let compact = screen(&mut app, 120, 40);
        assert!(
            !compact.contains("5 messages")
                && compact.contains("live-direct")
                && !compact.contains("live-hidden"),
            "{compact}"
        );
        assert_eq!(
            order(&compact, &["direct-two", "live-outbound", "live-direct"]),
            ["direct-two", "live-outbound", "live-direct"]
        );
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let verbose = screen(&mut app, 120, 40);
        assert!(
            verbose.contains("live-hidden")
                && verbose.contains("activity-body")
                && verbose.contains("→"),
            "{verbose}"
        );
    }

    #[test]
    fn outgoing_mail_is_shaded_only_in_agent_conversations_and_stays_unread() {
        let (mut app, mut rx) = app();
        let (feed, _) = mateo_streams(&mut app, &mut rx);
        let outgoing = proto::Message {
            from_kind: proto::Kind::Human.into(),
            ..message(1, "ada", "Mateo", "my-outgoing-message")
        };
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![outgoing.clone()]),
        }));
        assert!(
            rx.try_recv().is_err(),
            "outgoing mail must not mark mail read or activity seen"
        );
        let assert_background = |app: &mut App, shaded: bool, compact: bool| {
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let buffer = terminal.backend().buffer();
            crate::ui::theme::testing::assert_buffer(buffer);
            let area = buffer.area;
            let y = (area.top()..area.bottom())
                .find(|y| {
                    (area.left()..area.right())
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains("my-outgoing-message")
                })
                .unwrap();
            assert_eq!(
                buffer[(area.right() - 1, y)].bg,
                if shaded {
                    theme::style(theme::Role::HumanSurface).bg.unwrap()
                } else {
                    theme::base().bg.unwrap()
                }
            );
            assert!(
                (area.left()..area.right())
                    .all(|x| buffer[(x, y)].bg == buffer[(area.right() - 1, y)].bg)
            );
            if shaded {
                for padding in [y - 2, y + 1] {
                    assert!((area.left()..area.right()).all(|x| {
                        buffer[(x, padding)].bg
                            == theme::style(if compact && padding < y {
                                theme::Role::Screen
                            } else {
                                theme::Role::HumanSurface
                            })
                            .bg
                            .unwrap()
                            && buffer[(x, padding)].symbol() == " "
                    }));
                }
                if compact {
                    assert!((area.left()..area.right()).all(|x| buffer[(x, y - 1)].bg
                        == theme::style(theme::Role::HumanSurface).bg.unwrap()
                        && buffer[(x, y - 1)].symbol() == " "));
                }
            }
        };
        assert_background(&mut app, true, false);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_background(&mut app, true, true);
        assert!(rx.try_recv().is_err());
        // Leave Insert mode, then close the conversation and open the board feed.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        while rx.try_recv().is_ok() {}
        key(&mut app, 'q');
        while rx.try_recv().is_ok() {}
        key(&mut app, 'f');
        let (feed, _) = watched(&mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![outgoing]),
        }));
        assert_background(&mut app, false, false);
    }

    #[test]
    fn conversation_preference_defaults_to_compact_and_is_shared_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connect.json");
        let (mut first_app, mut rx) = app();
        first_app.ctx.preferences = Preferences::load(path.clone()).unwrap();
        open_agents(&mut first_app, &mut rx);
        first_app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &[]), agent("Zed", "ada", &[])],
        }))));
        reopen(&mut first_app, &mut rx);
        assert!(screen(&mut first_app, 120, 24).contains("Ctrl-T: verbose"));
        typed(&mut first_app, "draft");
        press(&mut first_app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut first_app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!Preferences::load(path.clone()).unwrap().compact());
        close_conversation(&mut first_app, &mut rx);
        key(&mut first_app, 'j');
        reopen(&mut first_app, &mut rx);
        assert!(screen(&mut first_app, 120, 24).contains("Ctrl-T: compact"));
        press(&mut first_app, KeyCode::PageUp, KeyModifiers::NONE);
        press(&mut first_app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(screen(&mut first_app, 120, 24).contains("Navigation · Esc/i write to Zed"));
        assert!(Preferences::load(path.clone()).unwrap().compact());
        press(&mut first_app, KeyCode::Esc, KeyModifiers::NONE);
        close_conversation(&mut first_app, &mut rx);
        key(&mut first_app, 'k');
        reopen(&mut first_app, &mut rx);
        assert!(screen(&mut first_app, 120, 24).contains("Ctrl-T: verbose"));
        typed(&mut first_app, "X");
        enter(&mut first_app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected send");
        };
        assert_eq!(req.body, "drafXt");

        // A new app invocation inherits the saved global preference.
        for compact in [true, false] {
            let mut preferences = Preferences::load(path.clone()).unwrap();
            preferences.set_compact(compact);
            let (mut next_app, mut next_rx) = app();
            next_app.ctx.preferences = Preferences::load(path.clone()).unwrap();
            mateo(&mut next_app, &mut next_rx);
            let out = screen(&mut next_app, 120, 24);
            assert!(out.contains(if compact {
                "Ctrl-T: verbose"
            } else {
                "Ctrl-T: compact"
            }));
        }
    }

    #[test]
    fn preference_save_errors_are_visible_without_losing_the_view_or_draft() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("state");
        let (mut app, mut rx) = app();
        app.ctx.preferences = Preferences::load(parent.join("connect.json")).unwrap();
        std::fs::write(&parent, "blocked").unwrap();
        mateo(&mut app, &mut rx);
        typed(&mut app, "draft");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("Cannot save view preference")
                && out.contains("draft")
                && out.contains("Mateo · Enter sends"),
            "{out}"
        );
        assert!(!app.ctx.preferences.compact());
        std::fs::remove_file(&parent).unwrap();
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!screen(&mut app, 80, 24).contains("Cannot save view preference"));
    }

    #[test]
    fn compact_keeps_verbose_filters_while_marking_hidden_activity_seen() {
        let (mut app, mut rx) = app();
        let (feed, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, 'A');
        key(&mut app, 'A');
        key(&mut app, 'E');
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![message(1, "Mateo", "ada", "visible-direct")]),
        }));
        assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("Mateo".into()));
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![proto::Message {
                read_at: Some("2026-10-16T18:00:01Z".into()),
                ..message(1, "Mateo", "ada", "visible-direct")
            }]),
        }));
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                activity(7, "01", r#"{"event":"text","text":"filtered-text"}"#),
                activity(8, "02", r#"{"event":"warning","message":"hidden-warning"}"#),
            ]),
        }));
        assert_eq!(
            rx.try_recv().unwrap(),
            Request::MarkSeen {
                agent: "Mateo".into(),
                through_id: 8
            }
        );
        key(&mut app, 'A');
        key(&mut app, 'E');
        let out = screen(&mut app, 120, 24);
        assert!(
            out.contains("visible-direct")
                && !out.contains("hidden-warning")
                && !out.contains("filtered-text"),
            "{out}"
        );
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let out = screen(&mut app, 120, 24);
        assert!(
            out.contains("A: activity · E: no text") && out.contains("hidden-warning"),
            "{out}"
        );
        assert!(
            !out.contains("visible-direct") && !out.contains("filtered-text"),
            "{out}"
        );
        assert!(rx.try_recv().is_err(), "toggling does not re-mark activity");
    }

    #[test]
    fn compact_toggle_preserves_navigation_focus_and_composer_cursor() {
        let (mut app, mut rx) = app();
        mateo(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let out = screen(&mut app, 120, 24);
        assert!(
            out.contains("write to Mateo") && out.contains("Ctrl-T: verbose"),
            "{out}"
        );
        key(&mut app, 'i');
        typed(&mut app, "hello");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(screen(&mut app, 120, 24).contains("Mateo · Enter sends"));
        typed(&mut app, "X");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(screen(&mut app, 120, 24).contains("Mateo · Enter sends"));
        typed(&mut app, "Y");
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected send");
        };
        assert_eq!(req.body, "helXYlo");
    }

    #[test]
    fn compact_feed_keeps_raw_gap_warnings_through_filtering_and_toggles() {
        for all_hidden in [true, false] {
            let (mut app, mut rx) = app();
            let (feed, _) = mateo_streams(&mut app, &mut rx);
            press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![message(1, "Mateo", "ada", "before-gap")]),
            }));
            press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
            // A burst pushed a visible DM out of the raw 200-message window.
            let snapshot = (201..=400)
                .map(|id| {
                    message(
                        id,
                        "Mateo",
                        if !all_hidden && id == 400 {
                            "ada"
                        } else {
                            "Ben"
                        },
                        if id == 400 { "after-gap" } else { "hidden" },
                    )
                })
                .collect();
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(snapshot),
            }));
            if all_hidden {
                assert!(!screen(&mut app, 120, 30).contains("messages may be missing"));
                // A later visible arrival still carries the raw gap boundary.
                app.on_event(Event::Data(Data::Feed {
                    id: feed,
                    result: Ok(vec![proto::Message {
                        from_kind: proto::Kind::Human.into(),
                        ..message(401, "ada", "Mateo", "after-gap")
                    }]),
                }));
            }
            let out = screen(&mut app, 120, 30);
            assert_eq!(
                order(
                    &out,
                    &["before-gap", "messages may be missing", "after-gap"]
                ),
                ["before-gap", "messages may be missing", "after-gap"],
                "{out}"
            );
            press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
            press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
            let out = screen(&mut app, 120, 30);
            assert_eq!(out.matches("messages may be missing").count(), 1, "{out}");
        }
        let (mut app, mut rx) = app();
        let (feed, _) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((1..200)
                .map(|id| {
                    message(
                        id,
                        "Mateo",
                        if id == 1 { "ada" } else { "Ben" },
                        "already received",
                    )
                })
                .collect()),
        }));
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        // This full raw window overlaps previously held hidden messages.
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((2..=201)
                .map(|id| {
                    message(
                        id,
                        "Mateo",
                        if id == 201 { "ada" } else { "Ben" },
                        "new arrival",
                    )
                })
                .collect()),
        }));
        let out = screen(&mut app, 120, 30);
        assert!(
            out.contains("new arrival") && !out.contains("messages may be missing"),
            "{out}"
        );
    }

    #[test]
    fn compact_toggle_is_only_for_agent_conversations() {
        let (mut app, mut rx) = app();
        crew_list(&mut app, &mut rx);
        enter(&mut app);
        let (group, _) = watched(&mut rx);
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        let broadcast = proto::Message {
            to_name: None,
            to_group: Some("crew".into()),
            ..message(1, "Mateo", "", "broadcast remains")
        };
        app.on_event(Event::Data(Data::Feed {
            id: group,
            result: Ok(vec![broadcast.clone()]),
        }));
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let out = screen(&mut app, 100, 24);
        assert!(
            out.contains("broadcast remains")
                && out.contains("→ @crew")
                && !out.contains("Ctrl-T:"),
            "{out}"
        );
        key(&mut app, 'q');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(group));
        key(&mut app, 'q');
        key(&mut app, 'f');
        let (feed, _) = watched(&mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![broadcast]),
        }));
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let out = screen(&mut app, 100, 24);
        assert!(
            out.contains("broadcast remains")
                && out.contains("→ @crew")
                && !out.contains("Ctrl-T:"),
            "{out}"
        );
    }

    #[test]
    fn activity_alone_says_when_nothing_is_shared_or_the_server_is_old() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, 'A');
        key(&mut app, 'A');
        assert!(screen(&mut app, 100, 24).contains("Loading…"));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(Vec::new()),
        }));
        let out = screen(&mut app, 140, 24);
        assert!(out.contains("Mateo shares no activity"), "{out}");
        assert!(out.contains("share_activity = false"), "{out}");
        let old = Data::Activity {
            id: act,
            result: Err("required capability agent-activity.v1 is unavailable".into()),
        };
        app.on_event(Event::Data(old));
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("needs a newer server"), "{out}");
        // With messages shown, the error is a note on the status line.
        key(&mut app, 'A');
        let out = screen(&mut app, 120, 24);
        assert!(
            out.contains("activity: Activity needs a newer server"),
            "{out}"
        );
    }

    #[test]
    fn only_agent_threads_have_activity() {
        let (mut app, mut rx) = app();
        key(&mut app, 'f');
        watched(&mut rx);
        key(&mut app, 'A');
        assert!(rx.try_recv().is_err());
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("0 messages · following") && !out.contains("A: all"),
            "{out}"
        );
    }

    #[test]
    fn e_cycles_the_kinds_of_activity_shown() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                activity(1, "00", r#"{"ts":"2026-10-16T18:00:00Z","event":"thought","text":"pondering"}"#),
                activity(2, "01", r#"{"ts":"2026-10-16T18:00:01Z","event":"mail","count":1,"from":["Zed"],"queued":false}"#),
                activity(3, "02", r#"{"ts":"2026-10-16T18:00:02Z","event":"warning","message":"slow disk"}"#),
            ]),
        }));
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("E: all kinds"), "{out}");
        assert!(
            out.contains("pondering") && out.contains("mail from Zed"),
            "{out}"
        );
        key(&mut app, 'E');
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("E: no text"), "{out}");
        assert!(
            !out.contains("pondering") && out.contains("mail from Zed"),
            "{out}"
        );
        key(&mut app, 'E');
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("E: problems"), "{out}");
        assert!(
            !out.contains("mail from Zed") && out.contains("slow disk"),
            "{out}"
        );
        // New records follow the filter too, and the filter survives a reload.
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![activity(
                4,
                "03",
                r#"{"ts":"2026-10-16T18:00:03Z","event":"thought","text":"later thought"}"#,
            )]),
        }));
        assert!(!screen(&mut app, 100, 24).contains("later thought"));
        while rx.try_recv().is_ok() {}
        key(&mut app, 'r');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(act));
        watched(&mut rx);
        let Request::WatchActivity { id: act, .. } = rx.try_recv().unwrap() else {
            panic!("expected replacement activity watch");
        };
        assert!(screen(&mut app, 100, 24).contains("E: problems"));
        // Showing activity alone, nothing of the chosen kinds says so.
        key(&mut app, 'A');
        key(&mut app, 'A');
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![activity(
                5,
                "04",
                r#"{"ts":"2026-10-16T18:00:04Z","event":"text","text":"hi"}"#,
            )]),
        }));
        assert!(screen(&mut app, 100, 24).contains("Nothing of these kinds. E shows all kinds."));
        key(&mut app, 'E');
        assert!(screen(&mut app, 100, 24).contains("hi"));
    }

    #[test]
    fn active_conversations_mark_mail_and_activity_in_every_view_and_filter() {
        for (compact, show_steps, kind_steps) in
            [(true, 0, 0), (false, 1, 0), (false, 2, 1), (false, 0, 2)]
        {
            let (mut app, mut rx) = app();
            app.ctx.preferences.set_compact(compact);
            let (feed, act) = mateo_streams(&mut app, &mut rx);
            press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
            for _ in 0..show_steps {
                key(&mut app, 'A');
            }
            for _ in 0..kind_steps {
                key(&mut app, 'E');
            }
            assert!(rx.try_recv().is_err(), "nothing received, nothing to mark");
            for id in [1, 2] {
                app.on_event(Event::Data(Data::Feed {
                    id: feed,
                    result: Ok(vec![message(id, "Mateo", "ada", "received mail")]),
                }));
                assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("Mateo".into()));
                app.on_event(Event::Data(Data::Feed {
                    id: feed,
                    result: Ok(vec![proto::Message {
                        read_at: Some("2026-10-16T18:00:01Z".into()),
                        ..message(id, "Mateo", "ada", "received mail")
                    }]),
                }));
                app.on_event(Event::Data(Data::Activity {
                    id: act,
                    result: Ok(vec![activity(
                        id,
                        "00",
                        r#"{"event":"text","text":"filtered activity"}"#,
                    )]),
                }));
                assert_eq!(
                    rx.try_recv().unwrap(),
                    Request::MarkSeen {
                        agent: "Mateo".into(),
                        through_id: id
                    }
                );
                app.on_event(Event::Tick);
                assert!(
                    rx.try_recv().is_err(),
                    "already received activity is not marked twice"
                );
            }
            // Help covers the conversation; arrivals remain pending until it is uncovered.
            key(&mut app, '?');
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![message(3, "Mateo", "ada", "covered mail")]),
            }));
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(vec![activity(
                    3,
                    "00",
                    r#"{"event":"text","text":"covered activity"}"#,
                )]),
            }));
            assert!(rx.try_recv().is_err());
            key(&mut app, 'q');
            assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("Mateo".into()));
            assert_eq!(
                rx.try_recv().unwrap(),
                Request::MarkSeen {
                    agent: "Mateo".into(),
                    through_id: 3
                }
            );
            // Closing stops both watches, and late batches do not mark anything.
            key(&mut app, 'q');
            assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(feed));
            assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(act));
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![message(4, "Mateo", "ada", "closed mail")]),
            }));
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(vec![activity(
                    4,
                    "00",
                    r#"{"event":"text","text":"closed activity"}"#,
                )]),
            }));
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn viewing_activity_marks_received_records_seen_even_when_filtered() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let batch = |id: i64| Data::Activity {
            id: act,
            result: Ok(vec![activity(
                id,
                "00",
                r#"{"ts":"2026-10-16T18:00:00Z","event":"text","text":"hi"}"#,
            )]),
        };
        app.on_event(Event::Data(batch(7)));
        let seen = |through_id| Request::MarkSeen {
            agent: "Mateo".into(),
            through_id,
        };
        assert_eq!(rx.try_recv().unwrap(), seen(7));
        // Once marked, an unrelated update marks nothing.
        app.on_event(Event::Data(Data::Groups(Err("x".into()))));
        assert!(rx.try_recv().is_err());
        // With messages alone shown, received activity is still seen.
        key(&mut app, 'A');
        app.on_event(Event::Data(batch(8)));
        assert_eq!(rx.try_recv().unwrap(), seen(8));
        key(&mut app, 'A');
        assert!(rx.try_recv().is_err());
    }
}
