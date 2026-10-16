//! The app: a stack of screens under a shared title bar and status bar.

use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc::UnboundedSender;

use super::event::{Data, Event, Request};
use super::keys::{Action, Keymap};
use super::screens::help::Help;
use super::screens::menu::Menu;
use super::screens::splash::Splash;
use super::theme;

/// A fresh id, telling apart the screens that ask the server for things.
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
}

pub enum Transition {
    Stay,
    Push(Box<dyn Screen>),
    Pop,
    Quit,
    /// Ask the server for something on the screen's behalf.
    Request(Request),
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
    /// Before a reload re-sends `requests`: drop what they will send again.
    fn reload(&mut self) {}
    /// What to stop when the screen closes.
    fn on_close(&self) -> Vec<Request> {
        Vec::new()
    }
    fn handle(&mut self, action: Action, ctx: &Ctx) -> Transition;
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx);
}

pub struct App {
    stack: Vec<Box<dyn Screen>>,
    ctx: Ctx,
    keymap: Keymap,
    requests: UnboundedSender<Request>,
}

impl App {
    pub fn new(ctx: Ctx, requests: UnboundedSender<Request>) -> App {
        let mut app = App {
            stack: Vec::new(),
            ctx,
            keymap: Keymap::default(),
            requests,
        };
        app.push(Box::new(Menu::default()));
        app.push(Box::new(Splash));
        app
    }

    fn push(&mut self, screen: Box<dyn Screen>) {
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
        if let Some(screen) = self.stack.pop() {
            self.send(screen.on_close());
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
                for screen in &mut self.stack {
                    screen.on_data(&data);
                }
                self.viewed();
                false
            }
            Event::Resize | Event::Tick => false,
        }
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
                screen.reload();
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
            Transition::Push(screen) => self.push(screen),
            Transition::Pop if self.stack.len() > 1 => self.pop(),
            Transition::Request(request) => self.send(vec![request]),
            Transition::Pop | Transition::Quit => return true,
        }
        false
    }

    pub fn draw(&mut self, frame: &mut Frame) {
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
        frame.render_widget(Paragraph::new(bar), status);
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

    fn app() -> (App, UnboundedReceiver<Request>) {
        let (tx, rx) = unbounded_channel();
        let ctx = Ctx {
            me: "ada".into(),
            server: "bbs:7777".into(),
            started: Instant::now(),
            composer_max_height_percent: 20,
        };
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
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn agent(name: &str, owner: &str, groups: &[&str]) -> proto::Agent {
        proto::Agent {
            name: name.into(),
            owner: Some(owner.into()),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            active_at: "2026-10-16T18:00:00.000000Z".into(),
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
        assert!(out.contains("baud"), "{out}");
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
        assert!(out.contains("Mateo@ada"), "{out}");
        assert!(!out.contains("Zed@bob"), "{out}");
        // `a` here shows everyone's; the menu's hot key does not apply.
        key(&mut app, 'a');
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("All agents (2)"), "{out}");
        assert!(out.contains("Zed@bob"), "{out}");
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
        assert!(out.contains("Mateo, owned by ada"), "{out}");
        assert!(
            out.contains("first needle") && out.contains("second"),
            "{out}"
        );
        assert!(!out.contains("elsewhere"), "{out}");
        assert!(
            out.contains("2 entries · A: all · E: all kinds · following"),
            "{out}"
        );
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
        key(&mut app, 'r');
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
        let rows = order(&out, &["Ana@", "Bo@", "Cy@", "Di@", "Ed@"]);
        assert_eq!(rows, ["Ed@", "Cy@", "Di@", "Bo@", "Ana@"], "{out}");
        assert!(out.contains("✉1 •4") && out.contains("•99"), "{out}");
        enter(&mut app);
        assert_eq!(watched(&mut rx).1.with.as_deref(), Some("Bo"));
        key(&mut app, 'q');
        // Without summaries (an older server), the server's order.
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Err("required capability agent-summary.v1 is unavailable".into()),
        }));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &["Ana@", "Bo@", "Cy@", "Di@", "Ed@"]),
            ["Ana@", "Bo@", "Cy@", "Di@", "Ed@"]
        );
        assert!(!out.contains('✉'), "{out}");
    }

    #[test]
    fn agent_cells_fit_their_columns() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        let mut busy = agent("Mateo", "ada", &["crew", "release", "infra"]);
        busy.status = Some("reviewing the very long pull request about feed filters".into());
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![busy],
        }))));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("crew, release +1"), "{out}");
        assert!(out.contains("reviewing…"), "{out}");
        // ACTIVE is local time, without the stored form's Z.
        let local = crate::ui::message::local_time("2026-10-16T18:00:00.000000Z");
        assert!(out.contains(&local), "{out}");
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
        assert!(screen(&mut app, 80, 24).contains("i: write to Mateo"));
        key(&mut app, 'i');
        // Keys are text now: q does not go back.
        for c in "quick".chars() {
            assert!(!key(&mut app, c));
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        for c in "note".chars() {
            key(&mut app, c);
        }
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("To Mateo · Enter sends"), "{out}");
        assert!(out.contains("quick") && out.contains("note"), "{out}");
        enter(&mut app);
        let Request::Send { id: sent, req } = rx.try_recv().unwrap() else {
            panic!("expected a send");
        };
        assert_eq!(
            (sent, req.to.as_str(), req.body.as_str()),
            (id, "Mateo", "quick\nnote")
        );
        assert!(screen(&mut app, 80, 24).contains("Sending…"));
        app.on_event(Event::Data(Data::Sent { id, result: Ok(()) }));
        let out = screen(&mut app, 80, 24);
        assert!(!out.contains("quick"), "{out}");
        // Esc leaves the composer, and q then goes back.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen(&mut app, 80, 24).contains("i: write to Mateo"));
        key(&mut app, 'q');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(id));
    }

    #[test]
    fn a_failed_send_keeps_the_text_and_says_why() {
        let (mut app, mut rx) = app();
        let id = mateo(&mut app, &mut rx);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, 'x');
        enter(&mut app);
        rx.try_recv().unwrap();
        app.on_event(Event::Data(Data::Sent {
            id,
            result: Err("no agent named Mateo".into()),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Not sent: no agent named Mateo"), "{out}");
        assert!(out.contains("│x"), "{out}");
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
                .filter(|l| l.starts_with("║│"))
                .count()
        };
        assert_eq!(rows(&mut app), 2);
        key(&mut app, 'i');
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
        assert!(out.contains("3 entries · A: all · E: all kinds"), "{out}");
        assert!(out.contains("i: write to Mateo"), "{out}");

        // A cycles messages only, activity only, then everything again.
        key(&mut app, 'A');
        let out = screen(&mut app, 100, 24);
        assert!(out.contains("1 entries · A: messages ·"), "{out}");
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
        assert!(screen(&mut app, 100, 24).contains("3 entries · A: all"));

        // A reload starts both streams over without the activity it had.
        while rx.try_recv().is_ok() {}
        key(&mut app, 'r');
        assert_eq!(watched(&mut rx).0, feed);
        assert!(matches!(rx.try_recv().unwrap(), Request::WatchActivity { id, .. } if id == act));
        let out = screen(&mut app, 100, 24);
        assert!(
            !out.contains("reply sent") && out.contains("a request"),
            "{out}"
        );
        // Closing the thread stops both.
        while rx.try_recv().is_ok() {}
        key(&mut app, 'q');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(feed));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(act));
    }

    #[test]
    fn activity_alone_says_when_nothing_is_shared_or_the_server_is_old() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
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
        key(&mut app, 'r');
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
    fn viewing_activity_marks_it_seen_as_far_as_shown() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
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
        // With messages alone shown, new activity is not seen.
        key(&mut app, 'A');
        app.on_event(Event::Data(batch(8)));
        assert!(rx.try_recv().is_err());
        key(&mut app, 'A');
        assert_eq!(rx.try_recv().unwrap(), seen(8));
    }
}
