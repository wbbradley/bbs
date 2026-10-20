//! The app: a stack of screens under a shared title bar and status bar.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc::UnboundedSender;

use super::composer::Composer;
use super::drafts::{Draft, DraftStore};
use super::event::{Data, Event, Request};
use super::keys::{Action, Keymap};
use super::navigation::{Conversation, Feedback, History, Inbox};
use super::preferences::Preferences;
use super::screens::help::Help;
use super::screens::menu::Menu;
use super::screens::splash::Splash;
use super::screens::thread::Thread;
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
    /// `[connect] composer_max_height_percent`.
    pub composer_max_height_percent: u8,
    pub preferences: Preferences,
    /// The clock for activity freshness and animation; tests pin it.
    pub now: std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
    pub inbox: Inbox,
    pub efforts: super::effort::Efforts,
    pub navigation_notice: Option<Feedback>,
}

pub enum Transition {
    Stay,
    Push(Box<dyn Screen>),
    Pop,
    Quit,
    /// Ask the server for something on the screen's behalf.
    Request(Request),
    Effort {
        agent: String,
        value: String,
    },
    /// Apply and remember a local color scheme immediately.
    Theme(crate::ui::theme::Scheme),
}

pub trait Screen {
    /// Shown in the title bar.
    fn title(&self) -> String;
    fn title_line(&self) -> Line<'static> {
        Line::raw(self.title())
    }
    fn conversation(&self) -> Option<Conversation> {
        None
    }
    fn is_thread(&self) -> bool {
        false
    }
    fn navigation_blocked(&self) -> bool {
        false
    }
    fn unread_mail(&self) -> Option<(String, i64)> {
        None
    }
    /// A key press before the keymap sees it: `Some` when the screen took it,
    /// as the splash and a search prompt do.
    fn raw_key(&mut self, _key: KeyEvent) -> Option<Transition> {
        None
    }
    /// A bracketed paste: text for the focused input, never keys, so it
    /// cannot send or reach a shortcut. Screens without an input ignore it.
    fn paste(&mut self, _text: &str) {}
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
    /// What to ask for once a second while this screen is on top.
    fn on_tick(&mut self, _ctx: &Ctx) -> Vec<Request> {
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
    /// When the screen's animation frame next changes, if it animates.
    fn next_animation_frame(&self) -> Option<Instant> {
        None
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
    history: History,
    known_agents: Option<HashSet<String>>,
    known_groups: Option<HashSet<String>>,
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
            history: History::default(),
            known_agents: None,
            known_groups: None,
        };
        app.send(vec![Request::WatchSummary(app.ctx.inbox.id)]);
        app.push(Box::new(Menu::default()));
        app.push(Box::new(Splash));
        app
    }

    fn push(&mut self, screen: Box<dyn Screen>) {
        self.open(screen, true);
    }

    fn open(&mut self, mut screen: Box<dyn Screen>, record: bool) {
        if let Some(target) = screen.conversation() {
            if let Conversation::Agent(agent) = &target {
                self.ctx.inbox.retry_failed(agent);
            }
            if self.stack.last().is_some_and(|s| s.is_thread()) {
                self.pop();
            }
            if record {
                self.history.visit(target);
            }
        }
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
            let request = match request {
                Request::Agents => Request::AgentsAt {
                    effort_revision: self.ctx.efforts.revision(),
                },
                request => request,
            };
            let _ = self.requests.send(request);
        }
    }

    /// When the top screen's animation frame next changes, if it animates.
    pub fn next_animation_frame(&self) -> Option<Instant> {
        self.stack
            .last()
            .and_then(|screen| screen.next_animation_frame())
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
        let unread = screen.unread_mail();
        let mut requests = screen.on_view(&self.ctx);
        if let Some((agent, through)) = unread
            && self.ctx.inbox.begin_read(&agent, through)
        {
            requests.insert(0, Request::MarkRead(agent));
        }
        self.send(requests);
        let refresh = self.ctx.inbox.refresh_after_reads();
        self.send(refresh);
    }

    fn jump(&mut self, oldest: bool) {
        let target = if oldest {
            if let Some(error) = &self.ctx.inbox.error {
                self.ctx.navigation_notice =
                    Some(Feedback::Summary(format!("Inbox unavailable: {error}")));
                return;
            }
            if !self.ctx.inbox.loaded {
                self.ctx.navigation_notice =
                    Some(Feedback::Summary("Inbox summaries are loading.".into()));
                return;
            }
            match self.ctx.inbox.oldest() {
                Some(agent) => {
                    self.ctx.inbox.retry_failed(&agent);
                    Conversation::Agent(agent)
                }
                None => {
                    self.ctx.navigation_notice =
                        Some(Feedback::Summary("No unread agent messages.".into()));
                    return;
                }
            }
        } else {
            let agents = &self.known_agents;
            let groups = &self.known_groups;
            match self.history.back(|target| match target {
                Conversation::Agent(name) => agents
                    .as_ref()
                    .is_none_or(|a| a.contains(&name.to_ascii_lowercase())),
                Conversation::Group(name) => groups
                    .as_ref()
                    .is_none_or(|g| g.contains(&name.to_ascii_lowercase())),
            }) {
                Some(target) => target,
                None => {
                    self.ctx.navigation_notice =
                        Some(Feedback::Other("No conversation history.".into()));
                    return;
                }
            }
        };
        self.ctx.navigation_notice = None;
        if self
            .stack
            .last()
            .and_then(|s| s.conversation())
            .is_some_and(|c| c.same(&target))
        {
            return;
        }
        let screen = match &target {
            Conversation::Agent(agent) => Thread::agent(agent, &self.ctx.me, self.ctx.now.clone()),
            Conversation::Group(group) => Thread::group(group),
        };
        self.open(Box::new(screen), oldest);
    }

    fn top(&mut self) -> &mut Box<dyn Screen> {
        self.stack.last_mut().expect("the menu is never popped")
    }

    /// Apply one event; `true` quits.
    pub fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('g' | 'o'))
                {
                    self.keymap = Keymap::default();
                    if !self.top().navigation_blocked() {
                        self.jump(key.code == KeyCode::Char('g'));
                        self.viewed();
                    }
                    return false;
                }
                self.ctx.navigation_notice = None;
                if let Some(action) = super::keys::effort_action(key)
                    && matches!(self.top().conversation(), Some(Conversation::Agent(_)))
                    && !self.top().navigation_blocked()
                {
                    self.keymap = Keymap::default();
                    let quit = self.on_action(action);
                    if !quit {
                        self.viewed();
                    }
                    return quit;
                }
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
            Event::Paste(text) => {
                self.keymap = Keymap::default();
                self.ctx.navigation_notice = None;
                self.top().paste(&text);
                self.viewed();
                false
            }
            Event::Data(data) => {
                let (data, list_revision) = match data {
                    Data::AgentsAt {
                        effort_revision,
                        result,
                    } => (Data::Agents(result), effort_revision),
                    data => (data, self.ctx.efforts.revision()),
                };
                match &data {
                    Data::Summary { id, result } if *id == self.ctx.inbox.id => {
                        // Only ListAgents establishes absence. Sparse summary servers
                        // omit agents with zero counts; positive rows can also describe
                        // agents created after the last list.
                        if let Ok(rows) = result
                            && let Some(known) = &mut self.known_agents
                        {
                            known.extend(rows.iter().map(|s| s.agent.to_ascii_lowercase()));
                        }
                        if let Ok(rows) = result {
                            self.ctx.efforts.summarized(rows);
                        }
                        self.ctx.inbox.snapshot(*id, result);
                        if matches!(self.ctx.navigation_notice, Some(Feedback::Summary(_))) {
                            self.ctx.navigation_notice = None;
                        }
                    }
                    Data::EffortSent { id, agent, result } => {
                        self.ctx.efforts.sent(agent, *id, result);
                    }
                    Data::Agents(Ok(list)) => {
                        self.ctx.efforts.listed(&list.agents, list_revision);
                        self.known_agents = Some(
                            list.agents
                                .iter()
                                .map(|a| a.name.to_ascii_lowercase())
                                .collect(),
                        );
                    }
                    Data::Groups(Ok(list)) => {
                        self.known_groups = Some(
                            list.groups
                                .iter()
                                .map(|g| g.name.to_ascii_lowercase())
                                .collect(),
                        );
                    }
                    Data::MarkedRead { agent, result } => {
                        self.ctx.inbox.acknowledged(agent, result);
                        if let Err(error) = result {
                            self.ctx.navigation_notice = Some(Feedback::ReadError {
                                agent: agent.clone(),
                                text: format!("Could not mark mail read: {error}"),
                            });
                        } else if matches!(&self.ctx.navigation_notice,
                            Some(Feedback::ReadError { agent: failed, .. }) if failed.eq_ignore_ascii_case(agent))
                        {
                            self.ctx.navigation_notice = None;
                        }
                    }
                    _ => {}
                }
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
                let screen = self.stack.last_mut().expect("the menu is never popped");
                let requests = screen.on_tick(&self.ctx);
                self.send(requests);
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
        self.send(vec![Request::Unwatch(self.ctx.inbox.id)]);
        self.flush_drafts();
        if let Some(error) = self.draft_store.error() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn on_action(&mut self, action: Action) -> bool {
        match action {
            Action::NextUnread | Action::PreviousConversation => {
                self.jump(action == Action::NextUnread);
                return false;
            }
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
                let refresh = self.ctx.inbox.restart();
                self.send(refresh);
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
            Transition::Effort { agent, value } => {
                if let Some(request) =
                    self.ctx
                        .efforts
                        .request(&agent, &self.ctx.me, value, next_id())
                {
                    self.send(vec![request]);
                }
            }
            Transition::Theme(scheme) => {
                crate::ui::theme::select(scheme);
                self.ctx.preferences.set_theme(scheme);
            }
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
        let [title, mut body, status] = theme::frame_areas(frame.area());
        let screen = self.stack.last_mut().expect("the menu is never popped");
        // An agent conversation's title is at the left of its bottom bar, so
        // its timeline takes the title row.
        if matches!(screen.conversation(), Some(Conversation::Agent(_))) {
            body = title.union(body);
        } else {
            frame.render_widget(Paragraph::new(theme::title_bar(screen.title_line())), title);
        }
        screen.render(frame, body, &self.ctx);
        let bar = theme::status_bar(&self.ctx.me);
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
            let notice = self
                .ctx
                .inbox
                .error
                .as_ref()
                .map(|e| format!("Inbox unavailable: {e}"))
                .or_else(|| {
                    self.ctx
                        .navigation_notice
                        .as_ref()
                        .map(|n| n.text().to_string())
                });
            let bar = notice
                .map(|notice| theme::clip_line(Line::raw(notice), status.width as usize))
                .unwrap_or(bar);
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
    use std::time::Duration;
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
    fn shared_summary_lives_across_screens_and_reload_fences_old_streams() {
        let (mut app, mut rx) = app();
        let summary = app.ctx.inbox.id;
        open_agents(&mut app, &mut rx);
        key(&mut app, 'q');
        assert!(
            rx.try_recv().is_err(),
            "closing Agents keeps the one app subscription"
        );
        key(&mut app, 'f');
        let (feed, _) = watched(&mut rx);
        inbox_rows(&mut app, &[("Zed", 1, "2026-10-18T12:00:00Z")]);
        ctrl(&mut app, 'g');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(feed));
        watched(&mut rx);
        assert!(matches!(rx.try_recv(), Ok(Request::WatchActivity { .. })));
        assert_eq!(
            app.stack.len(),
            2,
            "Feed's stream is not hidden under the conversation"
        );
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, 'r');
        let requests: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            requests
                .iter()
                .filter(|r| matches!(r, Request::WatchSummary(_)))
                .count(),
            1
        );
        assert!(requests.contains(&Request::Unwatch(summary)));
        let generation = app.ctx.inbox.id;
        assert_ne!(summary, generation);
        app.on_event(Event::Data(Data::Summary {
            id: summary,
            result: Ok(vec![proto::AgentSummary {
                agent: "old".into(),
                unread_messages: 1,
                oldest_unread_message: Some("2026-10-18T10:00:00Z".into()),
                ..Default::default()
            }]),
        }));
        assert!(!app.ctx.inbox.loaded);
        app.on_event(Event::Data(Data::Summary {
            id: generation,
            result: Err("offline".into()),
        }));
        ctrl(&mut app, 'g');
        assert!(bottom(&mut app).contains("Inbox unavailable"));
        assert_eq!(opened(&app), Some(Conversation::Agent("Zed".into())));
        assert!(
            rx.try_recv().is_err(),
            "failure does not poll/reconnect itself"
        );
        key(&mut app, 'r');
        while rx.try_recv().is_ok() {}
        inbox_rows(&mut app, &[]);
        ctrl(&mut app, 'g');
        assert!(bottom(&mut app).contains("No unread agent messages"));
        assert!(rx.try_recv().is_err());
        app.finish().unwrap();
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(app.ctx.inbox.id));
    }

    #[test]
    fn app_read_acknowledgments_coalesce_without_losing_fresh_unread_mail() {
        let (mut app, mut rx) = app();
        let rows = [
            ("A", 1, "2026-10-18T12:00:00Z"),
            ("B", 1, "2026-10-18T12:00:01Z"),
        ];
        inbox_rows(&mut app, &rows);
        ctrl(&mut app, 'g');
        let (feed_a, _) = watched(&mut rx);
        rx.try_recv().unwrap();
        app.on_event(Event::Data(Data::Feed {
            id: feed_a,
            result: Ok(vec![message(11, "A", "ada", "one")]),
        }));
        assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("A".into()));
        ctrl(&mut app, 'g');
        assert_eq!(opened(&app), Some(Conversation::Agent("B".into())));
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();
        let (feed_b, _) = watched(&mut rx);
        rx.try_recv().unwrap();
        app.on_event(Event::Data(Data::Feed {
            id: feed_b,
            result: Ok(vec![message(12, "B", "ada", "two")]),
        }));
        assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("B".into()));
        let old = app.ctx.inbox.id;
        app.on_event(Event::Data(Data::MarkedRead {
            agent: "A".into(),
            result: Ok(()),
        }));
        assert!(
            rx.try_recv().is_err(),
            "wait for outstanding B acknowledgement"
        );
        inbox_rows(&mut app, &rows); // queued stale snapshot from the old generation
        ctrl(&mut app, 'g');
        assert_eq!(opened(&app), Some(Conversation::Agent("B".into())));
        assert!(rx.try_recv().is_err());
        app.on_event(Event::Data(Data::MarkedRead {
            agent: "B".into(),
            result: Ok(()),
        }));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(old));
        assert_eq!(
            rx.try_recv().unwrap(),
            Request::WatchSummary(app.ctx.inbox.id)
        );
        for _ in 0..3 {
            key(&mut app, 'x');
            app.on_event(Event::Tick);
        }
        assert!(
            rx.try_recv().is_err(),
            "stale local unread rows cause no repeat requests"
        );
        // A's next unread message can even have the same timestamp as before.
        inbox_rows(&mut app, &[rows[0], ("B", 0, "")]);
        ctrl(&mut app, 'g');
        assert_eq!(opened(&app), Some(Conversation::Agent("A".into())));
        while rx.try_recv().is_ok() {}
        // Queued batches for the closed A subscription must not mark anything.
        app.on_event(Event::Data(Data::Feed {
            id: feed_a,
            result: Ok(vec![message(13, "A", "ada", "late")]),
        }));
        assert!(rx.try_recv().is_err());
        let requests = app.top().requests();
        let Request::Watch { id: new_a, .. } = requests[0] else {
            panic!("feed")
        };
        app.on_event(Event::Data(Data::Feed {
            id: new_a,
            result: Ok(vec![message(13, "A", "ada", "new")]),
        }));
        assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("A".into()));
        app.on_event(Event::Data(Data::MarkedRead {
            agent: "A".into(),
            result: Err("offline".into()),
        }));
        assert!(
            rx.try_recv().is_err(),
            "failure does not hot-loop on the stale unread row"
        );
        assert!(bottom(&mut app).contains("Could not mark"));
        key(&mut app, 'z');
        assert!(rx.try_recv().is_err());
        ctrl(&mut app, 'g');
        assert_eq!(
            rx.try_recv().unwrap(),
            Request::MarkRead("A".into()),
            "explicit retry is allowed"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn group_discussions_participate_in_history_and_keep_their_own_draft_cursor() {
        let (mut app, mut rx) = app();
        crew_list(&mut app, &mut rx);
        enter(&mut app);
        let (group, _) = watched(&mut rx);
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        key(&mut app, 'i');
        typed(&mut app, "group draft");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        inbox_rows(&mut app, &[("Zed", 1, "2026-10-18T12:00:00Z")]);
        ctrl(&mut app, 'g');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(group));
        watched(&mut rx);
        rx.try_recv().unwrap();
        typed(&mut app, "agent draft");
        ctrl(&mut app, 'o');
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();
        let (_, req) = watched(&mut rx);
        assert_eq!(req.group.as_deref(), Some("crew"));
        assert_eq!(rx.try_recv().unwrap(), Request::Groups);
        assert_eq!(opened(&app), Some(Conversation::Group("crew".into())));
        key(&mut app, 'i');
        typed(&mut app, "X");
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("send")
        };
        assert_eq!(req.body, "group drafXt");
        assert_eq!(req.to, "@crew");
        assert_eq!(app.stack.len(), 3, "menu, Groups and exactly one thread");
    }

    fn inbox_rows(app: &mut App, rows: &[(&str, u32, &str)]) {
        app.on_event(Event::Data(Data::Summary {
            id: app.ctx.inbox.id,
            result: Ok(rows
                .iter()
                .map(|(agent, count, time)| proto::AgentSummary {
                    agent: (*agent).into(),
                    unread_messages: *count,
                    oldest_unread_message: (*count > 0).then(|| (*time).into()),
                    ..Default::default()
                })
                .collect()),
        }));
    }

    fn ctrl(app: &mut App, c: char) {
        press(app, KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    fn opened(app: &App) -> Option<Conversation> {
        app.stack.last().unwrap().conversation()
    }

    #[test]
    fn unread_jumps_are_global_and_current_target_is_not_reopened_before_feed_arrives() {
        let (mut app, mut rx) = app();
        ctrl(&mut app, 'g');
        assert!(bottom(&mut app).contains("loading"));
        inbox_rows(
            &mut app,
            &[
                ("Zed", 1, "2026-10-18T12:00:00Z"),
                ("Mateo", 1, "2026-10-18T12:01:00Z"),
            ],
        );
        ctrl(&mut app, 'g');
        assert_eq!(opened(&app), Some(Conversation::Agent("Zed".into())));
        let (feed, req) = watched(&mut rx);
        assert_eq!(
            req.with.as_deref(),
            Some("Zed"),
            "not limited to the connected human's agents"
        );
        assert!(matches!(rx.try_recv(), Ok(Request::WatchActivity { .. })));
        typed(&mut app, "abc");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        ctrl(&mut app, 'g');
        assert!(
            rx.try_recv().is_err(),
            "selection alone issues no mark or replacement streams"
        );
        assert_eq!(app.stack.len(), 2);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![message(10, "Zed", "ada", "mail")]),
        }));
        assert_eq!(rx.try_recv().unwrap(), Request::MarkRead("Zed".into()));
        ctrl(&mut app, 'g');
        assert_eq!(opened(&app), Some(Conversation::Agent("Mateo".into())));
        assert!(matches!(rx.try_recv(),Ok(Request::Unwatch(id)) if id==feed));
        assert!(matches!(rx.try_recv(), Ok(Request::Unwatch(_))));
        watched(&mut rx);
        assert!(matches!(rx.try_recv(), Ok(Request::WatchActivity { .. })));
        assert!(rx.try_recv().is_err());
        ctrl(&mut app, 'o');
        assert_eq!(opened(&app), Some(Conversation::Agent("Zed".into())));
        while rx.try_recv().is_ok() {}
        typed(&mut app, "X");
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("send")
        };
        assert_eq!(req.body, "abXc", "draft and cursor survive keyboard jumps");
        assert_eq!(app.stack.len(), 2, "no duplicate hidden conversations");
    }

    #[test]
    fn sparse_or_empty_summaries_do_not_remove_existing_agents_from_history() {
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            agents: vec![agent("A", "ada", &[]), agent("B", "ben", &[])],
            humans: vec![],
        }))));
        inbox_rows(&mut app, &[("A", 1, "2026-10-18T12:00:00Z")]);
        ctrl(&mut app, 'g');
        while rx.try_recv().is_ok() {}
        inbox_rows(&mut app, &[("B", 1, "2026-10-18T12:00:01Z")]);
        ctrl(&mut app, 'g');
        while rx.try_recv().is_ok() {}
        assert_eq!(opened(&app), Some(Conversation::Agent("B".into())));
        for snapshot in [vec![("A", 1, "2026-10-18T12:00:00Z")], vec![]] {
            inbox_rows(&mut app, &snapshot);
            for name in ["A", "B", "A", "B"] {
                ctrl(&mut app, 'o');
                while rx.try_recv().is_ok() {}
                assert_eq!(opened(&app), Some(Conversation::Agent(name.into())));
                assert_eq!(app.stack.len(), 3, "menu, Agents and one conversation");
            }
        }
        // A positive sparse row can describe a new agent absent from an older list.
        inbox_rows(&mut app, &[("New", 1, "2026-10-18T12:00:02Z")]);
        ctrl(&mut app, 'g');
        while rx.try_recv().is_ok() {}
        inbox_rows(&mut app, &[]);
        for name in ["B", "A", "New"] {
            ctrl(&mut app, 'o');
            while rx.try_recv().is_ok() {}
            assert_eq!(opened(&app), Some(Conversation::Agent(name.into())));
        }
    }

    #[test]
    fn history_ring_keeps_pending_sends_and_skips_deleted_targets() {
        for success in [true, false] {
            let (mut app, mut rx) = app();
            let (_, _) = mateo_streams(&mut app, &mut rx);
            typed(&mut app, "pending");
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
            enter(&mut app);
            let Request::Send { id: send, .. } = rx.try_recv().unwrap() else {
                panic!("send")
            };
            inbox_rows(
                &mut app,
                &[("Zed", 1, "2026-10-18T12:00:00Z"), ("Mateo", 0, "")],
            );
            ctrl(&mut app, 'g');
            assert_eq!(opened(&app), Some(Conversation::Agent("Zed".into())));
            while rx.try_recv().is_ok() {}
            app.on_event(Event::Data(Data::Sent {
                id: send,
                result: if success {
                    Ok(())
                } else {
                    Err("send failed".into())
                },
            }));
            ctrl(&mut app, 'o');
            while rx.try_recv().is_ok() {}
            assert_eq!(opened(&app), Some(Conversation::Agent("Mateo".into())));
            typed(&mut app, "X");
            enter(&mut app);
            let Request::Send { req, .. } = rx.try_recv().unwrap() else {
                panic!("send")
            };
            assert_eq!(req.body, if success { "X" } else { "pendinXg" });
            ctrl(&mut app, 'o');
            while rx.try_recv().is_ok() {}
            assert_eq!(
                opened(&app),
                Some(Conversation::Agent("Zed".into())),
                "history wraps without reordering"
            );
            // A successful ListAgents result, not summary omission, establishes removal.
            app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
                agents: vec![agent("Mateo", "ada", &[])],
                humans: vec![],
            }))));
            ctrl(&mut app, 'o');
            while rx.try_recv().is_ok() {}
            assert_eq!(opened(&app), Some(Conversation::Agent("Mateo".into())));
            ctrl(&mut app, 'o');
            assert!(
                rx.try_recv().is_err(),
                "one remaining history target is a no-op"
            );
        }
    }

    #[test]
    fn global_navigation_leaves_prompts_dialogs_help_and_config_untouched() {
        let (mut app, mut rx) = app();
        let (_, _) = mateo_streams(&mut app, &mut rx);
        inbox_rows(&mut app, &[("Zed", 1, "2026-10-18T12:00:00Z")]);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, '/');
        typed(&mut app, "needle");
        for c in ['g', 'o'] {
            ctrl(&mut app, c);
        }
        assert!(bottom(&mut app).contains("/needle█"));
        assert_eq!(opened(&app), Some(Conversation::Agent("Mateo".into())));
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, '?');
        ctrl(&mut app, 'g');
        assert_eq!(app.top().title(), "Help");
        key(&mut app, 'q');
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        while rx.try_recv().is_ok() {}
        key(&mut app, 'q');
        key(&mut app, 'c');
        for c in ['g', 'o'] {
            ctrl(&mut app, c);
        }
        assert_eq!(app.top().title(), "Config");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        crew_list(&mut app, &mut rx);
        key(&mut app, '+');
        typed(&mut app, "Zed");
        for c in ['g', 'o'] {
            ctrl(&mut app, c);
        }
        assert!(screen(&mut app, 80, 24).contains("agents: Zed"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn mailbox_and_spinner_share_the_bar_in_both_modes_and_hide_unknown_state() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                {
                    let (mut app, mut rx) = app();
                    let (_, now) = test_clock();
                    app.ctx.now = now;
                    let (_, act) = mateo_streams(&mut app, &mut rx);
                    inbox_rows(
                        &mut app,
                        &[
                            ("Mateo", 1, "2026-10-18T12:00:00Z"),
                            ("Zed", 1, "2026-10-18T12:00:01Z"),
                        ],
                    );
                    for (event, glyph) in
                        [(None, "·"), (Some("text"), "⠋"), (Some("turn_end"), "✓")]
                    {
                        if let Some(event) = event {
                            app.on_event(Event::Data(Data::ActivityBaseline {
                                id: act,
                                through_id: 0,
                                record: None,
                            }));
                            app.on_event(Event::Data(Data::Activity {
                                id: act,
                                result: Ok(vec![activity(
                                    if event == "text" { 1 } else { 2 },
                                    "00",
                                    &format!(r#"{{"event":"{event}"}}"#),
                                )]),
                            }));
                        }
                        // Reload progress for the next mode's unknown case.
                        if event.is_none() {
                            app.on_event(Event::Data(Data::ActivityBaseline {
                                id: act,
                                through_id: 0,
                                record: None,
                            }));
                        }
                        app.ctx.preferences.error = Some("cannot save preference".into());
                        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                        terminal.draw(|f| app.draw(f)).unwrap();
                        let b = terminal.backend().buffer();
                        assert_eq!(b[(77, 23)].symbol(), "✉");
                        assert_eq!(b[(78, 23)].symbol(), " ");
                        assert_eq!(b[(79, 23)].symbol(), glyph);
                        let bar = theme::base().patch(theme::style(theme::Role::StatusBar));
                        for x in [77, 78, 79] {
                            assert_eq!(b[(x, 23)].fg, bar.fg.unwrap());
                            assert_eq!(b[(x, 23)].bg, bar.bg.unwrap());
                            assert_eq!(b[(x, 23)].modifier, bar.add_modifier);
                        }
                        assert!(
                            bottom(&mut app).contains("pref error")
                                || bottom(&mut app).contains("cannot save")
                        );
                        app.ctx.preferences.error = None;
                    }
                    inbox_rows(&mut app, &[("Mateo", 1, "2026-10-18T12:00:00Z")]);
                    assert!(
                        !bottom(&mut app).contains('✉'),
                        "current-agent mail does not count as elsewhere"
                    );
                    app.on_event(Event::Data(Data::Summary {
                        id: app.ctx.inbox.id,
                        result: Err("offline".into()),
                    }));
                    let b = bottom(&mut app);
                    assert!(b.contains("Inbox unavailable") && !b.contains('✉'), "{b}");
                }
            });
        }
    }

    #[test]
    fn categories_survive_selection_titles_dialogs_status_and_live_scheme_changes() {
        use crate::ui::theme::{
            Scheme,
            testing::{SCHEMES, with_scheme},
        };
        use ratatui::buffer::Buffer;
        use ratatui::style::Modifier;
        let find = |buffer: &Buffer, y: u16, text: &str| -> u16 {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            unicode_width::UnicodeWidthStr::width(
                &row[..row
                    .find(text)
                    .unwrap_or_else(|| panic!("missing {text} in {row}"))],
            ) as u16
        };
        let check = |buffer: &Buffer, y: u16, text: &str, role, surface| {
            let x = find(buffer, y, text);
            let expected =
                theme::base().patch(theme::on_surface(theme::style(surface), theme::style(role)));
            for dx in 0..text.len() as u16 {
                let cell = &buffer[(x + dx, y)];
                assert_eq!(cell.fg, expected.fg.unwrap(), "{text}: {cell:?}");
                assert_eq!(cell.bg, expected.bg.unwrap(), "{text}: {cell:?}");
                assert!(cell.modifier.contains(expected.add_modifier));
            }
        };
        with_scheme(Scheme::Gruvbox, || {
            let (mut app, mut rx) = app();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            for scheme in SCHEMES {
                crate::ui::theme::select(scheme);
                terminal.draw(|f| app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                let y = (1..23)
                    .find(|&y| {
                        let row: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                        row.contains("Welcome, ada.")
                    })
                    .unwrap();
                check(buffer, y, "ada", theme::Role::Human, theme::Role::Screen);
                check(
                    buffer,
                    23,
                    "ada",
                    theme::Role::Human,
                    theme::Role::StatusBar,
                );
                let status: String = (0..80).map(|x| buffer[(x, 23)].symbol()).collect();
                assert!(
                    !status.contains("online") && !status.contains("bbs:7777"),
                    "{status}"
                );
            }
            open_agents(&mut app, &mut rx);
            app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
                agents: vec![agent("Mateo", "ada", &["crew"])],
                humans: vec![],
            }))));
            set_agents_now(&mut app, "2026-10-18T12:00:00Z");
            for scheme in SCHEMES {
                crate::ui::theme::select(scheme);
                terminal.draw(|f| app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                check(
                    buffer,
                    3,
                    "Mateo",
                    theme::Role::Agent,
                    theme::Role::Selection,
                );
                check(buffer, 3, "ada", theme::Role::Human, theme::Role::Selection);
                check(
                    buffer,
                    3,
                    "crew",
                    theme::Role::Group,
                    theme::Role::Selection,
                );
                check(
                    buffer,
                    3,
                    "ago",
                    theme::Role::Timestamp,
                    theme::Role::Selection,
                );
                if scheme == Scheme::NoColor {
                    assert!(buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
                }
            }
            enter(&mut app);
            watched(&mut rx);
            rx.try_recv().unwrap();
            for scheme in SCHEMES {
                crate::ui::theme::select(scheme);
                terminal.draw(|f| app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                check(
                    buffer,
                    23,
                    "Mateo",
                    theme::Role::Agent,
                    theme::Role::StatusBar,
                );
                check(
                    buffer,
                    23,
                    "ada",
                    theme::Role::Human,
                    theme::Role::StatusBar,
                );
            }
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            key(&mut app, 'q');
            key(&mut app, 'g');
            rx.try_recv().unwrap();
            app.on_event(Event::Data(Data::Groups(Ok(proto::GroupListResponse {
                groups: vec![proto::Group {
                    name: "crew".into(),
                    members: vec!["Mateo".into()],
                    ..Default::default()
                }],
            }))));
            for scheme in SCHEMES {
                crate::ui::theme::select(scheme);
                terminal.draw(|f| app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                check(
                    buffer,
                    3,
                    "@crew",
                    theme::Role::Group,
                    theme::Role::Selection,
                );
                check(
                    buffer,
                    3,
                    "Mateo",
                    theme::Role::Agent,
                    theme::Role::Selection,
                );
                // Dialog roles resolve at creation; no Config screen is accessible under a modal.
                key(&mut app, '+');
                typed(&mut app, "Zed");
                terminal.draw(|f| app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                let y = (0..24)
                    .find(|&y| {
                        let row: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                        row.contains("Add agents to @crew")
                    })
                    .unwrap();
                let x = find(buffer, y, "@crew");
                assert_eq!(
                    buffer[(x, y)].fg,
                    theme::style(theme::Role::Group)
                        .fg
                        .unwrap_or(buffer[(x, y)].fg)
                );
                let y = (0..24)
                    .find(|&y| {
                        let row: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                        row.contains("Zed")
                    })
                    .unwrap();
                let x = find(buffer, y, "Zed");
                let expected = theme::base().patch(theme::style(theme::Role::Agent));
                assert_eq!(buffer[(x, y)].fg, expected.fg.unwrap());
                press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            }
            enter(&mut app);
            for scheme in SCHEMES {
                crate::ui::theme::select(scheme);
                terminal.draw(|f| app.draw(f)).unwrap();
                check(
                    terminal.backend().buffer(),
                    0,
                    "@crew",
                    theme::Role::Group,
                    theme::Role::TitleBar,
                );
            }
        });
    }

    fn assert_status_foreground(cell: &ratatui::buffer::Cell, bar: ratatui::style::Style) {
        assert!(
            [theme::Role::Text, theme::Role::Human, theme::Role::Agent]
                .into_iter()
                .map(|role| theme::on_surface(bar, theme::style(role))
                    .fg
                    .unwrap_or(cell.fg))
                .chain(bar.fg)
                .any(|fg| cell.fg == fg),
            "unexpected status foreground: {cell:?}"
        );
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
                let bar = scheme::style(scheme::Role::Screen)
                    .patch(scheme::style(scheme::Role::StatusBar));
                for x in 0..80 {
                    let cell = &terminal.backend().buffer()[(x, 23)];
                    assert_status_foreground(cell, bar);
                    assert_eq!(cell.bg, bar.bg.unwrap());
                    assert!(cell.modifier.contains(bar.add_modifier));
                }
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
        let (tx, mut rx) = unbounded_channel();
        let ctx = Ctx {
            me: "ada".into(),
            server: "bbs:7777".into(),
            composer_max_height_percent: 20,
            preferences: Preferences::default(),
            now: std::sync::Arc::new(Instant::now),
            inbox: Default::default(),
            efforts: Default::default(),
            navigation_notice: None,
        };
        let mut app = App::new(ctx, tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            Request::WatchSummary(app.ctx.inbox.id)
        );
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
    fn agent_title_moves_to_the_bottom_bar_and_the_timeline_takes_its_row() {
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
                assert!(!out.contains("conversation") && !out.contains('░'), "{out}");
                // Mail between the viewer and the agent is headerless.
                assert!(out.lines().nth(1).unwrap().contains("body-start"), "{out}");
                let last = out.lines().last().unwrap();
                assert!(
                    last.starts_with("BBS · Mateo · effort unknown · following"),
                    "{last}"
                );
                assert!(
                    !last.contains("bbs:7777") && !last.contains("online"),
                    "{last}"
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
                let agent = theme::on_surface(expected, theme::style(theme::Role::Agent));
                for x in 0..80 {
                    // "BBS · " then the agent's name in its category colour.
                    if (6..11).contains(&x) {
                        assert_eq!(buffer[(x, 23)].fg, agent.fg.unwrap(), "x={x}");
                    } else {
                        assert_status_foreground(&buffer[(x, 23)], expected);
                    }
                    assert_eq!(buffer[(x, 23)].bg, expected.bg.unwrap());
                }
            });
        }
    }

    #[test]
    fn agent_conversations_trade_the_title_row_for_a_quiet_bottom_bar() {
        let (mut app, mut rx) = app();
        let (clock, now) = test_clock();
        app.ctx.now = now;
        // Other screens keep their title row; no screen shows the server or
        // a clock.
        let menu = screen(&mut app, 80, 24);
        assert!(
            menu.lines()
                .next()
                .unwrap()
                .contains("░▒▓█ BBS · Main menu █▓▒░")
        );
        assert_eq!(menu.lines().last().unwrap().trim(), "ada");
        let (feed, _) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((1..=40)
                .map(|id| message(id, "Mateo", "ada", &format!("line {id}")))
                .collect()),
        }));
        report_effort(&mut app, effort_report(Some("low")));
        let first_row = |out: &str| out.lines().next().unwrap().to_string();
        // Typing and navigation: the timeline reaches the top row and the
        // bottom bar starts with the title, with no filter labels at defaults.
        for insert in [true, false, true] {
            if editor_focused(&mut app) != insert {
                let code = if insert {
                    KeyCode::Esc
                } else {
                    KeyCode::PageUp
                };
                press(&mut app, code, KeyModifiers::NONE);
            }
            let out = screen(&mut app, 80, 24);
            assert_eq!(editor_focused(&mut app), insert, "{out}");
            assert!(
                !first_row(&out).contains("BBS") && !out.contains('░'),
                "{out}"
            );
            let last = out.lines().last().unwrap();
            assert!(last.starts_with("BBS · Mateo · effort low · "), "{last}");
            for gone in [
                "conversation",
                "reported",
                "Ctrl-T",
                "A:",
                "E:",
                "bbs:7777",
                "online",
            ] {
                assert!(!last.contains(gone), "{gone}: {last}");
            }
        }
        // A pending change reads as one quiet next-turn value, however long
        // its outcome takes.
        app.on_action(Action::EffortUp);
        while rx.try_recv().is_ok() {}
        assert!(bottom(&mut app).contains("effort low → medium next turn"));
        clock.advance(Duration::from_secs(600));
        let row = bottom(&mut app);
        assert!(
            row.contains("effort low → medium next turn") && !row.contains("unconf"),
            "{row}"
        );
        // One view: Ctrl-T changes nothing, and Help no longer lists it.
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let before = screen(&mut app, 80, 24);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(screen(&mut app, 80, 24), before);
        assert!(rx.try_recv().is_err());
        for _ in 0..3 {
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        }
        key(&mut app, '?');
        let help = screen(&mut app, 80, 60);
        assert!(help.contains("Help") && !help.contains("Ctrl-T"), "{help}");
    }

    #[test]
    fn a_long_unicode_agent_title_leaves_room_for_feedback_mailbox_and_spinner() {
        use unicode_width::UnicodeWidthStr;
        let (mut app, mut rx) = app();
        let (_, now) = test_clock();
        app.ctx.now = now;
        let name = "Ünïcødé👩‍💻Agent-".repeat(4);
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent(&name, "ada", &["crew"])],
        }))));
        enter(&mut app);
        watched(&mut rx);
        let Request::WatchActivity { id: act, .. } = rx.try_recv().unwrap() else {
            panic!("expected an activity watch");
        };
        // Another agent's unread mail lights the mailbox; fresh activity spins.
        app.on_event(Event::Data(Data::Summary {
            id: app.ctx.inbox.id,
            result: Ok(vec![proto::AgentSummary {
                agent: "Other".into(),
                unread_messages: 1,
                ..Default::default()
            }]),
        }));
        app.on_event(Event::Data(Data::ActivityBaseline {
            id: act,
            through_id: 0,
            record: None,
        }));
        let mut record = activity(1, "01", r#"{"event":"text"}"#);
        record.agent = name.clone();
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![record]),
        }));
        app.ctx.preferences.error = Some("local preference failure".into());
        let check = |app: &mut App, needles: &[&str]| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let buffer = terminal.backend().buffer();
            crate::ui::theme::testing::assert_buffer(buffer);
            let row = bottom(app);
            // The name keeps at most a third of the bar, clipped, so the
            // feedback still shows and the mailbox and spinner keep their cells.
            assert!(
                row.starts_with("BBS · Ünïcødé👩‍💻") && row.contains("… · "),
                "{row}"
            );
            assert!(row.width() <= 80, "{row}");
            assert!(row.ends_with("✉ ⠋"), "{row}");
            assert_eq!(buffer[(79, 23)].symbol(), "⠋");
            assert_eq!(buffer[(77, 23)].symbol(), "✉");
            for needle in needles {
                assert!(row.contains(needle), "{needle}: {row}");
            }
        };
        check(&mut app, &["local preference"]);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, '/');
        typed(&mut app, &"🔍é".repeat(30));
        // Typed text keeps its room ahead of the key hint.
        check(&mut app, &["pref error", "🔍é🔍é█"]);
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
        assert!(bottom(&mut app).contains(" · paused"));
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![message(51, "Mateo", "ada", "new arrival")]),
        }));
        assert!(bottom(&mut app).contains("paused · 1 new · G follows"));
        key(&mut app, 'G');
        assert!(bottom(&mut app).contains(" · following"));
        key(&mut app, 'E');
        let row = bottom(&mut app);
        assert!(row.contains("E: no text"), "{row}");
        key(&mut app, '/');
        typed(&mut app, &"👍🏽🇺🇸👩‍💻é".repeat(20));
        let prompt = bottom(&mut app);
        assert!(prompt.contains("…") && prompt.contains("👩‍💻é█"), "{prompt}");
        assert!(prompt.contains("Enter search · Esc cancel"), "{prompt}");
        enter(&mut app);
        let committed = bottom(&mut app);
        assert!(committed.contains("0 matches · n/N"), "{committed}");
        assert!(
            committed.contains("E:no text") && !committed.contains("A:"),
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

    fn effort_report(current: Option<&str>) -> proto::EffortReport {
        proto::EffortReport {
            state: Some(proto::EffortState {
                supported: true,
                reason: String::new(),
                current: current.map(str::to_string),
                choices: [
                    ("low", Some(1)),
                    ("default", None),
                    ("medium", Some(4)),
                    ("high", Some(9)),
                ]
                .into_iter()
                .map(|(value, rank)| proto::EffortChoice {
                    value: value.into(),
                    label: value.into(),
                    rank,
                })
                .collect(),
            }),
            pending: None,
            answered: None,
        }
    }

    fn report_effort(app: &mut App, report: proto::EffortReport) {
        app.on_event(Event::Data(Data::Summary {
            id: app.ctx.inbox.id,
            result: Ok(vec![proto::AgentSummary {
                agent: "Mateo".into(),
                effort: Some(report),
                effort_reported_at: Some(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            }]),
        }));
    }

    #[test]
    fn effort_picker_is_explicit_modal_and_preserves_draft_cursor_and_focus() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        report_effort(&mut app, effort_report(Some("default")));
        typed(&mut app, "draft,.é");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        let saved = app.top().draft().unwrap().1;
        assert!(editor_focused(&mut app));
        app.on_action(Action::EffortUp);
        assert!(app.top().navigation_blocked());
        assert!(screen(&mut app, 80, 24).contains("Enter apply"));
        // No implicit first selection, no global jumps, no draft typing.
        enter(&mut app);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        typed(&mut app, "bad");
        assert!(rx.try_recv().is_err());
        assert_eq!(app.top().draft().unwrap().1, saved);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        // Selected low disappears while the dialog is open: Enter must not use
        // its old index (now the provider default).
        let mut changed = effort_report(Some("default"));
        changed.state.as_mut().unwrap().choices.remove(0);
        report_effort(&mut app, changed);
        enter(&mut app);
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        enter(&mut app);
        let Request::SetEffort { id, agent, value } = rx.try_recv().unwrap() else {
            panic!("effort control")
        };
        assert_eq!((agent.as_str(), value.as_str()), ("Mateo", "default"));
        assert!(editor_focused(&mut app));
        app.on_event(Event::Data(Data::EffortSent {
            id,
            agent,
            result: Ok(Some(100)),
        }));
        assert_eq!(
            app.top().draft().unwrap().1,
            saved,
            "control response cannot finish a draft send"
        );
        assert!(rx.try_recv().is_err());
        app.on_action(Action::EffortDown);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.top().draft().unwrap().1, saved);
        assert!(editor_focused(&mut app));
    }

    #[test]
    fn effort_controls_use_pending_basis_survive_reopen_and_ignore_stale_streams() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        report_effort(&mut app, effort_report(Some("low")));
        typed(&mut app, "keep");
        app.on_action(Action::EffortUp);
        let Request::SetEffort { id: a, value, .. } = rx.try_recv().unwrap() else {
            panic!("first")
        };
        assert_eq!(value, "medium");
        app.on_action(Action::EffortUp);
        let Request::SetEffort { id: b, value, .. } = rx.try_recv().unwrap() else {
            panic!("second")
        };
        assert_eq!(value, "high");
        app.on_action(Action::EffortUp);
        assert!(rx.try_recv().is_err(), "boundary does not enqueue");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let _ = rx.try_recv().unwrap();
        let _ = rx.try_recv().unwrap();
        app.on_event(Event::Data(Data::EffortSent {
            id: a,
            agent: "Mateo".into(),
            result: Err(crate::connect::event::EffortSendError::Unknown(
                "older".into(),
            )),
        }));
        app.on_event(Event::Data(Data::EffortSent {
            id: b,
            agent: "Mateo".into(),
            result: Ok(Some(102)),
        }));
        app.open(
            Box::new(Thread::agent("Mateo", "ada", app.ctx.now.clone())),
            true,
        );
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();
        assert_eq!(app.top().draft().unwrap().1.text, "keep");
        let status = bottom(&mut app);
        assert!(
            status.contains("effort low → high next turn") && status.contains("following"),
            "{status}"
        );
        let mut r = effort_report(Some("low"));
        r.answered = Some(proto::EffortOutcome {
            request_id: 101,
            result: 1,
            detail: String::new(),
        });
        report_effort(&mut app, r.clone());
        assert!(bottom(&mut app).contains("next"));
        r.state.as_mut().unwrap().current = Some("high".into());
        r.answered.as_mut().unwrap().request_id = 102;
        app.on_event(Event::Data(Data::Summary {
            id: app.ctx.inbox.id + 999,
            result: Ok(vec![proto::AgentSummary {
                agent: "Mateo".into(),
                effort: Some(r.clone()),
                ..Default::default()
            }]),
        }));
        assert!(
            bottom(&mut app).contains("next"),
            "obsolete generation cannot resolve request"
        );
        report_effort(&mut app, r);
        assert!(bottom(&mut app).contains("effort high"));
        assert!(!bottom(&mut app).contains("next"));
        app.on_action(Action::EffortDown);
        let Request::SetEffort { value, .. } = rx.try_recv().unwrap() else {
            panic!("navigation action")
        };
        assert_eq!(value, "medium");
    }

    #[test]
    fn cleared_effort_stays_unknown_on_reopen_and_crossed_list_cannot_erase_new_report() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        report_effort(&mut app, effort_report(Some("low")));
        app.on_action(Action::EffortUp);
        let Request::SetEffort { id, .. } = rx.try_recv().unwrap() else {
            panic!("control")
        };
        app.on_event(Event::Data(Data::EffortSent {
            id,
            agent: "Mateo".into(),
            result: Ok(Some(100)),
        }));
        app.send(vec![Request::Agents]);
        let Request::AgentsAt {
            effort_revision: old,
        } = rx.try_recv().unwrap()
        else {
            panic!("fenced list")
        };
        report_effort(&mut app, effort_report(Some("high")));
        let cleared = || proto::ListAgentsResponse {
            humans: vec![],
            agents: vec![agent("Mateo", "ada", &[])],
        };
        app.on_event(Event::Data(Data::AgentsAt {
            effort_revision: old,
            result: Ok(cleared()),
        }));
        assert!(app.ctx.efforts.status("Mateo").contains("effort high"));
        app.send(vec![Request::Agents]);
        let Request::AgentsAt { effort_revision } = rx.try_recv().unwrap() else {
            panic!("fresh list")
        };
        app.on_event(Event::Data(Data::AgentsAt {
            effort_revision,
            result: Ok(cleared()),
        }));
        app.open(
            Box::new(Thread::agent("Mateo", "ada", app.ctx.now.clone())),
            true,
        );
        while rx.try_recv().is_ok() {}
        let status = app.ctx.efforts.status("Mateo");
        assert!(status == "effort unknown → medium next turn", "{status}");
        app.on_action(Action::EffortUp);
        assert!(
            !app.top().navigation_blocked(),
            "stale choices cannot open picker"
        );
        assert!(rx.try_recv().is_err(), "stale rank cannot emit request");
    }

    #[test]
    fn effort_tabs_step_in_both_agent_modes_without_indenting_or_moving_the_draft() {
        {
            let (mut app, mut rx) = app();
            mateo_streams(&mut app, &mut rx);
            report_effort(&mut app, effort_report(Some("low")));
            typed(&mut app, "é ab,.");
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
            let draft = app.top().draft().unwrap().1;
            for navigation in [false, true] {
                if navigation {
                    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
                }
                for (code, modifiers, expected) in [
                    (KeyCode::Tab, KeyModifiers::NONE, "medium"),
                    (KeyCode::Tab, KeyModifiers::NONE, "high"),
                    (KeyCode::BackTab, KeyModifiers::SHIFT, "medium"),
                    (KeyCode::Tab, KeyModifiers::SHIFT, "low"),
                ] {
                    press(&mut app, code, modifiers);
                    let Request::SetEffort { agent, value, .. } = rx.try_recv().unwrap() else {
                        panic!("effort tab")
                    };
                    assert_eq!(agent, "Mateo");
                    assert_eq!(value, expected);
                    assert_eq!(app.top().draft().unwrap().1, draft);
                    assert_eq!(editor_focused(&mut app), !navigation);
                    let status = bottom(&mut app);
                    assert!(
                        !status.contains("boundary") && status.contains("next"),
                        "{status}"
                    );
                    if expected == "high" {
                        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
                        assert!(rx.try_recv().is_err(), "upper boundary is a no-op");
                    }
                }
                press(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
                assert!(rx.try_recv().is_err(), "lower boundary is a no-op");
                assert_eq!(app.top().draft().unwrap().1, draft);
            }
            // i and Esc remain ways to enter typing; punctuation is ordinary text.
            key(&mut app, 'i');
            assert!(editor_focused(&mut app));
            press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            assert!(editor_focused(&mut app));
            assert_eq!(app.top().draft().unwrap().1, draft);
            key(&mut app, ',');
            key(&mut app, '.');
            assert_eq!(app.top().draft().unwrap().1.text, "é ab,,..");
        }
    }

    #[test]
    fn effort_tabs_leave_non_conversation_screens_unchanged() {
        let screens: Vec<Box<dyn Screen>> = vec![
            Box::new(Menu::default()),
            Box::new(crate::connect::screens::agents::Agents::default()),
            Box::new(crate::connect::screens::groups::Groups::new()),
            Box::new(Help::default()),
            Box::new(crate::connect::screens::config::Config::new()),
        ];
        for page in screens {
            let (mut app, mut rx) = app();
            app.push(page);
            report_effort(&mut app, effort_report(Some("low")));
            while rx.try_recv().is_ok() {}
            let title = app.top().title();
            let before = screen(&mut app, 80, 24)
                .lines()
                .take(23)
                .collect::<Vec<_>>()
                .join("\n");
            for (code, modifiers) in [
                (KeyCode::Tab, KeyModifiers::NONE),
                (KeyCode::BackTab, KeyModifiers::NONE),
                (KeyCode::Tab, KeyModifiers::SHIFT),
            ] {
                press(&mut app, code, modifiers);
                assert!(rx.try_recv().is_err(), "no request from {title}");
                assert_eq!(app.top().title(), title);
                let after = screen(&mut app, 80, 24)
                    .lines()
                    .take(23)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(after, before, "Tab cannot change {title}");
            }
        }
    }

    #[test]
    fn effort_tabs_leave_picker_search_and_other_screen_input_in_control() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        report_effort(&mut app, effort_report(Some("default")));
        typed(&mut app, "draft");
        let draft = app.top().draft().unwrap().1;
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.top().navigation_blocked());
        assert!(screen(&mut app, 80, 24).contains("Enter apply"));
        enter(&mut app);
        assert!(rx.try_recv().is_err(), "no implicit picker value");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
        assert!(rx.try_recv().is_err(), "picker Tab only moves selection");
        enter(&mut app);
        let Request::SetEffort { value, .. } = rx.try_recv().unwrap() else {
            panic!("explicit choice")
        };
        assert_eq!(value, "low");
        assert_eq!(app.top().draft().unwrap().1, draft);
        assert!(editor_focused(&mut app));
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, '/');
        typed(&mut app, "search");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
        assert!(bottom(&mut app).contains("/search█"));
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        for (code, modifiers) in [
            (KeyCode::Tab, KeyModifiers::ALT),
            (KeyCode::Tab, KeyModifiers::CONTROL),
            (KeyCode::BackTab, KeyModifiers::ALT),
        ] {
            press(&mut app, code, modifiers);
            assert!(
                rx.try_recv().is_err(),
                "unrelated modifiers cannot set effort"
            );
        }
        app.open(Box::new(Thread::group("crew")), true);
        while rx.try_recv().is_ok() {}
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(
            editor_focused(&mut app),
            "group Tab still enters its composer"
        );
        typed(&mut app, "group draft");
        assert_eq!(app.top().draft().unwrap().1.text, "group draft");
        assert!(rx.try_recv().is_err());
        app.open(Box::new(Thread::feed()), true);
        while rx.try_recv().is_ok() {}
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn effort_unavailable_and_group_actions_do_not_send_or_change_drafts() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        typed(&mut app, ",.");
        app.on_action(Action::EffortDown);
        assert!(bottom(&mut app).contains("no driver report"));
        assert!(rx.try_recv().is_err());
        let mut r = effort_report(None);
        r.state.as_mut().unwrap().supported = false;
        r.state.as_mut().unwrap().reason = "No effort configuration".into();
        report_effort(&mut app, r);
        app.on_action(Action::EffortUp);
        let row = bottom(&mut app);
        assert!(row.contains("No effort configuration"), "{row}");
        assert!(rx.try_recv().is_err());
        assert_eq!(app.top().draft().unwrap().1.text, ",.");
        app.open(Box::new(Thread::group("crew")), true);
        while rx.try_recv().is_ok() {}
        app.on_action(Action::EffortUp);
        assert!(rx.try_recv().is_err());
        assert!(!bottom(&mut app).contains("effort"));
    }

    #[test]
    fn effort_feedback_fits_with_search_errors_mailbox_and_spinner_in_all_schemes() {
        use crate::ui::theme::testing::{SCHEMES, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let (mut app, mut rx) = app();
                let (clock, now) = test_clock();
                app.ctx.now = now;
                let (_, act) = mateo_streams(&mut app, &mut rx);
                let mut r = effort_report(Some("low"));
                for c in &mut r.state.as_mut().unwrap().choices {
                    c.label = "👩‍💻long provider label ".repeat(5);
                }
                report_effort(&mut app, r);
                app.on_event(Event::Data(Data::Summary {
                    id: app.ctx.inbox.id,
                    result: Ok(vec![proto::AgentSummary {
                        agent: "Other".into(),
                        unread_messages: 1,
                        ..Default::default()
                    }]),
                }));
                app.on_event(Event::Data(Data::ActivityBaseline {
                    id: act,
                    through_id: 0,
                    record: None,
                }));
                app.on_event(Event::Data(Data::Activity {
                    id: act,
                    result: Ok(vec![activity(1, "01", r#"{"event":"text"}"#)]),
                }));
                while rx.try_recv().is_ok() {}
                for _ in 0..2 {
                    app.on_action(Action::EffortUp);
                    while rx.try_recv().is_ok() {}
                    let row = bottom(&mut app);
                    assert!(
                        row.starts_with("BBS · Mateo · effort")
                            && row.contains("following")
                            && row.contains("next"),
                        "{row}"
                    );
                    assert!(row.ends_with("✉ ⠋"), "{row}");
                    assert!(app.next_animation_frame().is_some());
                    press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
                }
                // A slow outcome raises no alarm: the next-turn value stays.
                clock.advance(Duration::from_secs(61));
                let delayed = bottom(&mut app);
                assert!(
                    !delayed.contains("unconf") && delayed.contains("next"),
                    "{delayed}"
                );
                app.ctx.preferences.error = Some("local preference failure".into());
                let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                terminal.draw(|f| app.draw(f)).unwrap();
                let b = terminal.backend().buffer();
                crate::ui::theme::testing::assert_buffer(b);
                let bar = theme::base().patch(theme::style(theme::Role::StatusBar));
                assert_eq!(b[(79, 23)].symbol(), "·");
                assert_status_foreground(&b[(79, 23)], bar);
                assert_eq!(b[(79, 23)].bg, bar.bg.unwrap());
                assert!(bottom(&mut app).contains("local preference failure"));
                press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
                key(&mut app, '/');
                typed(&mut app, &"👩‍💻".repeat(40));
                let search = bottom(&mut app);
                assert!(
                    search.contains("pref error")
                        && search.contains("👩‍💻█")
                        && search.contains("Enter search · Esc cancel"),
                    "{search}"
                );
            });
        }
    }

    fn editor_focused(app: &mut App) -> bool {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let cursor = theme::base()
            .patch(theme::style(theme::Role::HumanSurface))
            .patch(theme::style(theme::Role::Cursor));
        (17..23).any(|y| {
            (0..80).any(|x| {
                let cell = &buffer[(x, y)];
                cell.fg == cursor.fg.unwrap()
                    && cell.bg == cursor.bg.unwrap()
                    && cell.modifier.contains(cursor.add_modifier)
            })
        })
    }

    /// Whether a rendered line is exactly `text`, ignoring trailing padding.
    fn has_line(out: &str, text: &str) -> bool {
        out.lines().any(|l| l.trim_end() == text)
    }

    /// A pinned clock a test advances; the app's clock function reads it.
    #[derive(Clone)]
    struct TestClock {
        base: Instant,
        millis: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    fn test_clock() -> (TestClock, std::sync::Arc<dyn Fn() -> Instant + Send + Sync>) {
        let clock = TestClock {
            base: Instant::now(),
            millis: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let read = clock.clone();
        let now = std::sync::Arc::new(move || {
            read.base
                + Duration::from_millis(read.millis.load(std::sync::atomic::Ordering::Relaxed))
        });
        (
            clock,
            now as std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
        )
    }

    impl TestClock {
        fn now(&self) -> Instant {
            self.base
                + Duration::from_millis(self.millis.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn advance(&self, by: Duration) {
            self.millis
                .fetch_add(by.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
        }
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
            composer_max_height_percent: 20,
            preferences: Preferences::default(),
            now: std::sync::Arc::new(Instant::now),
            inbox: Default::default(),
            efforts: Default::default(),
            navigation_notice: None,
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
            composer_max_height_percent: 20,
            preferences: Preferences::default(),
            now: std::sync::Arc::new(Instant::now),
            inbox: Default::default(),
            efforts: Default::default(),
            navigation_notice: None,
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
        assert!(
            !out.contains("bbs:7777") && !out.contains("online"),
            "{out}"
        );
        assert!(out.lines().last().unwrap().trim() == "ada", "{out}");
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
            assert!(cell.bg == bar.bg.unwrap(), "{text}");
        }
    }

    #[test]
    fn agents_load_on_open_and_show_owners() {
        let (mut app, mut rx) = app();
        assert!(!key(&mut app, 'a'));
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
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
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
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
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
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
        assert!(screen(&mut app, 80, 24).contains("Loading…"));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![]),
        }));
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
        assert!(out.contains("BBS · Mateo · "), "{out}");
        assert!(
            out.contains("first needle") && out.contains("second"),
            "{out}"
        );
        assert!(!out.contains("elsewhere"), "{out}");
        // Filter labels stay hidden at their defaults.
        assert!(out.contains("following") && !out.contains("A:"), "{out}");
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
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
        let id = app.ctx.inbox.id;
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
                effort: None,
                effort_reported_at: None,
                last_human_conversation_at: None,
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
            out.contains("✉ 1") && out.contains("•4") && out.contains("•99"),
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
    fn agents_a_human_talked_with_longest_ago_come_first() {
        let (mut app, mut rx) = app();
        key(&mut app, 'a');
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
        let id = app.ctx.inbox.id;
        let at = |s: &str| Some(format!("2026-10-19T00:00:{s}.000000Z"));
        // The list says when Ana last talked with a human; the rest never did.
        let mut agents: Vec<_> = ["Ana", "Bo", "Cy", "Di", "Ed"]
            .iter()
            .map(|n| agent(n, "ada", &["crew"]))
            .collect();
        agents[0].last_human_conversation_at = at("01");
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents,
        }))));
        let names = ["Ana", "Bo", "Cy", "Di", "Ed"];
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &names),
            ["Ana", "Bo", "Cy", "Di", "Ed"],
            "{out}"
        );
        let summary = |agent: &str, mail: u32, talked: Option<String>| proto::AgentSummary {
            agent: agent.into(),
            unread_messages: mail,
            oldest_unread_message: (mail > 0).then(|| "2026-10-19T00:00:00.000000Z".into()),
            last_human_conversation_at: talked,
            ..Default::default()
        };
        // Select Ed, then the live summary reorders: Ana's list time is the
        // oldest; Cy and Di tie, so Di's unread mail puts it first; Bo's
        // unread mail cannot beat any conversation, and Ed, who never talked
        // with a human, keeps the server's order.
        for _ in 0..4 {
            key(&mut app, 'j');
        }
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Ok(vec![
                summary("Bo", 3, None),
                summary("Cy", 0, at("05")),
                summary("Di", 1, at("05")),
            ]),
        }));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &names),
            ["Ana", "Di", "Cy", "Bo", "Ed"],
            "{out}"
        );
        // A new conversation puts Ed after the older ones; the selection
        // follows it.
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Ok(vec![
                summary("Bo", 3, None),
                summary("Cy", 0, at("05")),
                summary("Di", 1, at("05")),
                summary("Ed", 0, at("09")),
            ]),
        }));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &names),
            ["Ana", "Di", "Cy", "Ed", "Bo"],
            "{out}"
        );
        enter(&mut app);
        assert_eq!(watched(&mut rx).1.with.as_deref(), Some("Ed"));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        // Without summaries, the list's times still order; an older server
        // with neither keeps the server's order.
        app.on_event(Event::Data(Data::Summary {
            id,
            result: Err("required capability agent-summary.v1 is unavailable".into()),
        }));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &names),
            ["Ana", "Bo", "Cy", "Di", "Ed"],
            "{out}"
        );
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: names
                .iter()
                .rev()
                .map(|n| agent(n, "ada", &["crew"]))
                .collect(),
        }))));
        let out = screen(&mut app, 100, 24);
        assert_eq!(
            order(&out, &names),
            ["Ed", "Di", "Cy", "Bo", "Ana"],
            "{out}"
        );
    }

    #[test]
    fn agents_show_separate_inbox_and_activity_counts_at_minimum_width() {
        let (mut app, mut rx) = app();
        key(&mut app, 'a');
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
        let id = app.ctx.inbox.id;
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
                        format!("✉ {}", mail.min(99))
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
        assert!(matches!(rx.try_recv().unwrap(), Request::AgentsAt { .. }));
        let id = app.ctx.inbox.id;
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

    #[test]
    fn animation_arms_only_while_progress_is_fresh() {
        let (mut app, mut rx) = app();
        // A pinned clock shared with the thread makes the deadline exact.
        let (clock, now_fn) = test_clock();
        app.ctx.now = now_fn;
        let (_, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        app.on_event(Event::Data(Data::ActivityBaseline {
            id: act,
            through_id: 0,
            record: None,
        }));
        assert_eq!(app.next_animation_frame(), None, "nothing fresh yet");
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![activity(1, "01", r#"{"event":"text"}"#)]),
        }));
        assert_eq!(
            app.next_animation_frame(),
            Some(clock.now() + Duration::from_millis(100))
        );
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![activity(2, "01", r#"{"event":"turn_end"}"#)]),
        }));
        assert_eq!(
            app.next_animation_frame(),
            None,
            "recorded completion does not animate"
        );
    }

    /// A side report for Mateo with `questions` waiting under `request_id`.
    fn side_report(
        revision: i64,
        request_id: i64,
        questions: Vec<proto::InputQuestion>,
    ) -> proto::SideReportResponse {
        proto::SideReportResponse {
            agent: "Mateo".into(),
            pending_input: (!questions.is_empty()).then(|| proto::OwnerInput {
                request_id,
                status: proto::InputStatus::Pending.into(),
                questions,
                answers: Vec::new(),
                updated_at: "2026-10-20T02:30:00Z".into(),
                message: None,
            }),
            usage: Some(proto::ContextUsage {
                used_tokens: Some(84_321),
                limit_tokens: Some(200_000),
                source: proto::context_usage::Source::CurrentContext.into(),
            }),
            revision,
            reported_at: "2026-10-20T02:30:00Z".into(),
        }
    }

    fn deploy_questions() -> Vec<proto::InputQuestion> {
        let option = |v: &str| proto::InputOption {
            value: v.into(),
            label: Some(v.to_uppercase()),
            description: None,
        };
        vec![
            proto::InputQuestion {
                id: "deploy".into(),
                header: "Deploy".into(),
                text: "Ship to staging?".into(),
                options: vec![option("yes"), option("no")],
                allow_free_text: false,
                required: true,
            },
            proto::InputQuestion {
                id: "note".into(),
                header: "Note".into(),
                text: "Anything else?".into(),
                options: Vec::new(),
                allow_free_text: true,
                required: false,
            },
        ]
    }

    /// The owner's poll this tick, which must be one.
    fn poll(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> u64 {
        app.on_event(Event::Tick);
        match without_sends(rx) {
            Some(Request::SideReport { id, agent }) => {
                assert_eq!(agent, "Mateo");
                id
            }
            other => panic!("expected a side poll, got {other:?}"),
        }
    }

    fn without_sends(rx: &mut UnboundedReceiver<Request>) -> Option<Request> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .find(|r| !matches!(r, Request::MarkRead(_) | Request::MarkSeen { .. }))
    }

    fn command(app: &mut App, text: &str) {
        typed(app, text);
        enter(app);
    }

    #[test]
    fn the_owner_answers_waiting_questions_through_an_explicit_form() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        // One poll at a time: none while the first is unanswered.
        app.on_event(Event::Tick);
        assert!(without_sends(&mut rx).is_none());
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(1, 7, deploy_questions()))),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("Mateo asks 2 questions · /answer to respond"),
            "{out}"
        );
        // /answer opens the form; nothing is sent, nothing is answered yet.
        command(&mut app, "/answer");
        assert!(rx.try_recv().is_err(), "a local command, never sent");
        assert!(app.top().navigation_blocked());
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("( ) YES") && out.contains("( ) NO") && out.contains("[Submit]"),
            "{out}"
        );
        assert!(out.contains("[Cancel request]"), "{out}");
        // Esc closes the form unchanged: the questions still wait, the
        // conversation stays open, nothing is sent.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(rx.try_recv().is_err());
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("Mateo asks 2 questions") && out.contains("BBS · Mateo"),
            "{out}"
        );
        // Reopen, choose "no" (↓ then Enter), leave the optional note, Submit.
        command(&mut app, "/answer");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..2 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let Request::AnswerOwnerInput { id: sent, req } = rx.try_recv().unwrap() else {
            panic!("expected an answer");
        };
        assert_eq!(sent, id);
        assert_eq!(req.request_id, 7);
        assert_eq!(req.action(), proto::AnswerAction::Submit);
        assert_eq!(
            req.answers,
            [proto::InputAnswer {
                question_id: "deploy".into(),
                value: "no".into()
            }]
        );
        app.on_event(Event::Data(Data::OwnerAnswered {
            id,
            request_id: 7,
            result: Ok(()),
        }));
        assert!(!app.top().navigation_blocked(), "the form closes once sent");
        // The last report still shows request 7 waiting, as does a poll that
        // began before the answer landed: neither brings it back.
        assert!(!screen(&mut app, 80, 24).contains("asks 2"));
        poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(2, 7, deploy_questions()))),
        }));
        assert!(!screen(&mut app, 80, 24).contains("asks 2"), "fenced");
        command(&mut app, "/answer");
        assert!(screen(&mut app, 80, 24).contains("No questions are waiting."));
        // A fresh request after that shows and answers as usual.
        poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(3, 8, deploy_questions()))),
        }));
        assert!(screen(&mut app, 80, 24).contains("asks 2"));
        // The explicit Cancel request action sends CANCEL, once: a second
        // Enter while it is on its way sends nothing more.
        command(&mut app, "/answer");
        for _ in 0..5 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let Request::AnswerOwnerInput { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected a cancel");
        };
        assert_eq!(req.action(), proto::AnswerAction::Cancel);
        assert_eq!(req.request_id, 8);
        assert!(req.answers.is_empty());
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(rx.try_recv().is_err(), "one reply in flight");
    }

    #[test]
    fn replaced_or_withdrawn_questions_close_the_form_and_a_stale_reply_is_ignored() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(1, 7, deploy_questions()))),
        }));
        command(&mut app, "/answer");
        assert!(app.top().navigation_blocked());
        // A new runner generation replaced request 7 with request 8.
        poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(2, 8, deploy_questions()))),
        }));
        assert!(
            !app.top().navigation_blocked(),
            "a reply to 7 could not apply"
        );
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("Those questions were replaced"), "{out}");
        // A late result for 7 changes nothing.
        app.on_event(Event::Data(Data::OwnerAnswered {
            id,
            request_id: 7,
            result: Err(crate::connect::event::SideError::Refused("stale".into())),
        }));
        assert!(!screen(&mut app, 80, 24).contains("Not sent"));
        // Withdrawn: no questions left.
        command(&mut app, "/answer");
        poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(3, 0, Vec::new()))),
        }));
        assert!(!app.top().navigation_blocked());
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("no longer waiting") && !out.contains("asks 2"),
            "{out}"
        );
        command(&mut app, "/answer");
        assert!(screen(&mut app, 80, 24).contains("No questions are waiting."));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn context_shows_on_demand_and_esc_closes_only_the_panel() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(1, 0, Vec::new()))),
        }));
        assert!(
            !screen(&mut app, 80, 24).contains("Context"),
            "nothing until asked"
        );
        command(&mut app, "/context");
        assert!(rx.try_recv().is_err(), "a local command, never sent");
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("Current context: 84k of 200k tokens (42%)"),
            "{out}"
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let out = screen(&mut app, 80, 24);
        assert!(
            !out.contains("Current context") && out.contains("BBS · Mateo"),
            "{out}"
        );
    }

    #[test]
    fn only_the_owner_polls_and_a_server_without_it_is_asked_once() {
        // Someone else's agent: no polls; the commands explain and send nothing.
        let (mut app, mut rx) = app();
        open_agents(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Zed", "other", &["crew"])],
        }))));
        key(&mut app, 'a');
        while rx.try_recv().is_ok() {}
        app.open(
            Box::new(Thread::agent("Zed", "ada", app.ctx.now.clone())),
            true,
        );
        while rx.try_recv().is_ok() {}
        app.on_event(Event::Tick);
        assert!(
            without_sends(&mut rx).is_none(),
            "no polls for someone else's agent"
        );
        command(&mut app, "/answer");
        assert!(rx.try_recv().is_err());
        assert!(
            bottom(&mut app).contains("Only Zed's owner"),
            "{}",
            bottom(&mut app)
        );
        // An owned agent on a server without side-questions.v1: one poll, then none.
        let (mut app, mut rx) = self::app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Err(crate::connect::event::SideError::Unsupported(
                "required capability agent-interactions.v1 is unavailable".into(),
            )),
        }));
        app.on_event(Event::Tick);
        assert!(without_sends(&mut rx).is_none());
        command(&mut app, "/context");
        assert!(rx.try_recv().is_err());
        assert!(
            bottom(&mut app).contains("does not support"),
            "{}",
            bottom(&mut app)
        );
    }

    #[test]
    fn long_questions_stay_whole_and_scroll_within_eighty_by_twenty_four() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        let mut questions = deploy_questions();
        questions[0].text = (1..=12)
            .map(|n| format!("detail line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        questions[0].options = (1..=8)
            .map(|n| proto::InputOption {
                value: format!("v{n}"),
                label: Some(format!("option {n}")),
                description: Some(
                    "a description long enough to wrap past the box edge at eighty columns".into(),
                ),
            })
            .collect();
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(1, 7, questions))),
        }));
        command(&mut app, "/answer");
        // Every choice is reachable and shown whole as the focus moves: the
        // end of each long description wraps into view rather than being cut.
        let mut seen = std::collections::BTreeSet::new();
        let mut tails = 0;
        for _ in 0..10 {
            let out = screen(&mut app, 80, 24);
            for n in 1..=8 {
                if out.contains(&format!("option {n} — a description")) {
                    seen.insert(n);
                }
            }
            tails += out.matches("eighty columns").count();
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(seen.len(), 8, "{seen:?}");
        assert!(tails > 0, "descriptions wrap whole, not clipped");
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("detail line 12") || out.contains("[Submit]"),
            "{out}"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_long_form_scrolls_while_its_actions_and_keys_stay_in_view() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        let mut report = side_report(
            1,
            7,
            vec![proto::InputQuestion {
                id: "pick".into(),
                header: "Pick".into(),
                text: "Which one?".into(),
                options: (1..=6)
                    .map(|n| proto::InputOption {
                        value: format!("v{n}"),
                        label: Some(format!("choice {n} 漢字")),
                        description: Some(format!("long description {n} that wraps past the edge of an eighty column box, ending tail{n}")),
                    })
                    .collect(),
                allow_free_text: true,
                required: false,
            }],
        );
        report.pending_input.as_mut().unwrap().message = Some(
            (1..=14)
                .map(|n| format!("context line {n} 👩‍💻"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(report)),
        }));
        command(&mut app, "/answer");
        // Every frame keeps the actions and keys visible; the form message
        // opens at the top.
        let out = screen(&mut app, 80, 24);
        assert!(
            out.contains("[Submit]") && out.contains("PgUp/PgDn scroll"),
            "{out}"
        );
        assert!(out.contains("context line 1 "), "{out}");
        // PgDn reads on without moving the focus; PgUp comes back.
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        let read = screen(&mut app, 80, 24);
        assert!(
            !read.contains("context line 1 ") && read.contains("[Submit]"),
            "{read}"
        );
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(screen(&mut app, 80, 24).contains("context line 1 "));
        // Moving through the choices brings each whole, description tail
        // included, into view, actions still shown.
        let mut tails = std::collections::BTreeSet::new();
        for _ in 0..6 {
            let out = screen(&mut app, 80, 24);
            assert!(out.contains("[Submit]"), "{out}");
            for n in 1..=6 {
                if out.contains(&format!("tail{n}")) {
                    tails.insert(n);
                }
            }
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(tails.len(), 6, "{tails:?}");
        // A long typed answer shows its end, where typing goes on.
        typed(&mut app, &format!("{}END", "word ".repeat(80)));
        let out = screen(&mut app, 80, 24);
        assert!(out.contains("END") && out.contains("[Submit]"), "{out}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn side_errors_are_typed_and_a_reopened_conversation_ignores_old_replies() {
        use crate::connect::event::SideError;
        // A transient poll failure retries on the next tick.
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Err(SideError::Transient("connection reset".into())),
        }));
        assert_eq!(poll(&mut app, &mut rx), id);
        // A transient failure answering keeps the form and what was typed, and
        // says it may have arrived; nothing is retried.
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Ok(Box::new(side_report(1, 7, deploy_questions()))),
        }));
        command(&mut app, "/answer");
        for _ in 0..2 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        typed(&mut app, "keep me");
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..3 {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            rx.try_recv().unwrap(),
            Request::AnswerOwnerInput { .. }
        ));
        app.on_event(Event::Data(Data::OwnerAnswered {
            id,
            request_id: 7,
            result: Err(SideError::Transient("timed out".into())),
        }));
        let out = screen(&mut app, 80, 24);
        assert!(
            app.top().navigation_blocked() && out.contains("keep me"),
            "{out}"
        );
        assert!(rx.try_recv().is_err(), "no automatic retry");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(screen(&mut app, 80, 24).contains("Not confirmed"));
        // A refusal stops polling and says why.
        let (mut app, mut rx) = self::app();
        mateo_streams(&mut app, &mut rx);
        let id = poll(&mut app, &mut rx);
        app.on_event(Event::Data(Data::SideReport {
            id,
            result: Err(SideError::Refused("you no longer own Mateo".into())),
        }));
        app.on_event(Event::Tick);
        assert!(without_sends(&mut rx).is_none());
        assert!(screen(&mut app, 80, 24).contains("you no longer own Mateo"));
        // Reopened, the conversation's new session ignores the old one's replies.
        let (mut app, mut rx) = self::app();
        mateo_streams(&mut app, &mut rx);
        let old = poll(&mut app, &mut rx);
        close_conversation(&mut app, &mut rx);
        reopen(&mut app, &mut rx);
        while rx.try_recv().is_ok() {}
        let new = poll(&mut app, &mut rx);
        assert_ne!(old, new);
        app.on_event(Event::Data(Data::SideReport {
            id: old,
            result: Ok(Box::new(side_report(1, 7, deploy_questions()))),
        }));
        assert!(
            !screen(&mut app, 80, 24).contains("asks 2"),
            "a stale reply"
        );
        app.on_event(Event::Data(Data::SideReport {
            id: new,
            result: Ok(Box::new(side_report(1, 7, deploy_questions()))),
        }));
        assert!(screen(&mut app, 80, 24).contains("asks 2"));
    }

    /// The requests waiting, but the owner's once-a-second side polls.
    fn without_polls(rx: &mut UnboundedReceiver<Request>) -> Vec<Request> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|r| !matches!(r, Request::SideReport { .. }))
            .collect()
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
        app.on_event(Event::Key(KeyEvent::new(code, modifiers)))
    }

    /// The agent thread for Mateo, with its feed stream's id.
    /// Open Mateo's thread; returns its feed stream and its activity stream.
    fn mateo_streams(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> (u64, u64) {
        let (feed, activity) = mateo_loading_streams(app, rx);
        // Most fixtures exercise a loaded conversation. Startup has its own
        // multi-batch tests through mateo_loading_streams.
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![]),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: activity,
            result: Ok(vec![]),
        }));
        (feed, activity)
    }

    fn mateo_loading_streams(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> (u64, u64) {
        open_agents(app, rx);
        app.on_event(Event::Data(Data::Agents(Ok(proto::ListAgentsResponse {
            humans: Vec::new(),
            agents: vec![agent("Mateo", "ada", &["crew"])],
        }))));
        enter(app);
        let (feed, req) = watched(rx);
        assert!(req.include_group_deliveries);
        match rx.try_recv().unwrap() {
            Request::WatchActivity { id, req } => {
                assert_eq!(req.agent.as_deref(), Some("Mateo"));
                (feed, id)
            }
            other => panic!("expected an activity watch, got {other:?}"),
        }
    }

    #[test]
    fn opening_conversation_waits_for_both_sources_and_the_history_boundary() {
        for feed_first in [false, true] {
            let (mut app, mut rx) = app();
            let (_, now) = test_clock();
            app.ctx.now = now;
            let (feed, act) = mateo_loading_streams(&mut app, &mut rx);
            typed(&mut app, "draft stays usable");
            let initial = screen(&mut app, 80, 24);
            assert!(initial.contains("Loading…") && initial.contains("draft stays usable"));
            app.on_event(Event::Data(Data::ActivityBaseline {
                id: act,
                through_id: 400,
                record: Some(r#"{"event":"turn_end"}"#.into()),
            }));
            let mail = || Data::Feed {
                id: feed,
                result: Ok(vec![message(7, "Mateo", "ada", "canonical reply")]),
            };
            if feed_first {
                app.on_event(Event::Data(mail()));
            }
            let mut first: Vec<_> = (1..=200)
                .map(|id| {
                    activity(
                        id,
                        "2026-10-19T12:00:00Z",
                        r#"{"event":"thought","text":"old history"}"#,
                    )
                })
                .collect();
            first[199] = activity(
                200,
                "2026-10-19T12:00:00Z",
                r#"{"event":"tool","id":"read","title":"historical read","status":"in_progress"}"#,
            );
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(first),
            }));
            let middle = screen(&mut app, 80, 24);
            assert!(middle.contains("Loading…"), "{middle}");
            assert!(
                !middle.contains("old history")
                    && !middle.contains("historical read")
                    && !middle.contains("canonical reply")
            );
            let mut last: Vec<_> = (201..=400).map(|id| activity(id, "2026-10-19T12:00:01Z", r#"{"event":"tool","id":"read","title":"historical read","status":"completed"}"#)).collect();
            last[199] = activity(400, "2026-10-19T12:00:02Z", r#"{"event":"turn_end"}"#);
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(last),
            }));
            if !feed_first {
                assert!(screen(&mut app, 80, 24).contains("Loading…"));
                app.on_event(Event::Data(mail()));
            }
            let loaded = screen(&mut app, 80, 24);
            assert!(
                !loaded.contains("Loading…") && !loaded.contains("running"),
                "{loaded}"
            );
            assert!(loaded.contains("draft stays usable"));
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(vec![activity(
                    401,
                    "2026-10-19T12:00:03Z",
                    r#"{"event":"thought","text":"live update"}"#,
                )]),
            }));
            let live = screen(&mut app, 80, 24);
            assert!(
                live.contains("live update") && !live.contains("Loading…"),
                "{live}"
            );
        }
    }

    #[test]
    fn opening_conversation_releases_empty_failed_or_stalled_history() {
        for outcome in ["empty", "error", "stall"] {
            let (mut app, mut rx) = app();
            let (clock, now) = test_clock();
            app.ctx.now = now;
            let (feed, act) = mateo_loading_streams(&mut app, &mut rx);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![message(1, "Mateo", "ada", "usable mail")]),
            }));
            match outcome {
                "empty" => {
                    app.on_event(Event::Data(Data::ActivityBaseline {
                        id: act,
                        through_id: 0,
                        record: None,
                    }));
                    app.on_event(Event::Data(Data::Activity {
                        id: act,
                        result: Ok(vec![]),
                    }));
                }
                "error" => {
                    app.on_event(Event::Data(Data::Activity {
                        id: act,
                        result: Err("activity unavailable".into()),
                    }));
                }
                _ => {
                    assert!(screen(&mut app, 80, 24).contains("Loading…"));
                    assert!(app.next_animation_frame().is_some());
                    clock.advance(Duration::from_secs(2));
                }
            }
            let out = screen(&mut app, 80, 24);
            assert!(
                out.contains("usable mail") && !out.contains("Loading…"),
                "{out}"
            );
            assert!(app.next_animation_frame().is_none());
        }
    }

    #[test]
    fn initial_display_hold_expires_even_when_the_feed_stalls_and_resets_on_reload() {
        let (mut app, mut rx) = app();
        let (clock, now) = test_clock();
        app.ctx.now = now;
        let (feed, act) = mateo_loading_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![activity(
                1,
                "01",
                r#"{"event":"thought","text":"available activity"}"#,
            )]),
        }));
        assert!(screen(&mut app, 80, 24).contains("Loading…"));
        clock.advance(Duration::from_secs(2));
        assert!(screen(&mut app, 80, 24).contains("available activity"));
        // Later events do not re-enter the hold.
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![]),
        }));
        assert!(!screen(&mut app, 80, 24).contains("Loading…"));
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        while rx.try_recv().is_ok() {}
        key(&mut app, 'r');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(act));
        assert_eq!(watched(&mut rx).0, feed);
        let Request::WatchActivity { id: fresh, .. } = rx.try_recv().unwrap() else {
            panic!("activity watch")
        };
        assert!(screen(&mut app, 80, 24).contains("Loading…"));
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![]),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![]),
        }));
        assert!(screen(&mut app, 80, 24).contains("Loading…"));
        app.on_event(Event::Data(Data::Activity {
            id: fresh,
            result: Ok(vec![]),
        }));
        assert!(!screen(&mut app, 80, 24).contains("Loading…"));
    }

    #[test]
    fn loading_deadline_cannot_spin_redraws_in_a_too_small_terminal() {
        let (mut app, mut rx) = app();
        let (clock, now) = test_clock();
        app.ctx.now = now;
        let (feed, _) = mateo_loading_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![message(1, "Mateo", "ada", "available mail")]),
        }));
        assert!(app.next_animation_frame().is_some());
        clock.advance(Duration::from_secs(2));
        assert!(screen(&mut app, 79, 24).contains("terminal too small"));
        assert!(app.next_animation_frame().is_none());
        let restored = screen(&mut app, 80, 24);
        assert!(restored.contains("available mail") && !restored.contains("Loading…"));
    }

    fn mateo(app: &mut App, rx: &mut UnboundedReceiver<Request>) -> u64 {
        mateo_streams(app, rx).0
    }

    #[test]
    fn the_composer_sends_to_the_agent_and_empties_on_success() {
        let (mut app, mut rx) = app();
        let id = mateo(&mut app, &mut rx);
        assert!(editor_focused(&mut app));
        // Keys are text now: q does not go back.
        for c in "quick".chars() {
            assert!(!key(&mut app, c));
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        for c in "note".chars() {
            key(&mut app, c);
        }
        let out = screen(&mut app, 80, 24);
        assert!(editor_focused(&mut app), "{out}");
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
        assert!(editor_focused(&mut app));
        typed(&mut app, "first");
        press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        typed(&mut app, "second");
        // Up from either line stays in the editor, including at its top.
        for _ in 0..3 {
            press(&mut app, KeyCode::Up, KeyModifiers::NONE);
            assert!(editor_focused(&mut app));
        }
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let out = screen(&mut app, 100, 24);
        assert!(!editor_focused(&mut app) && out.contains("paused"), "{out}");
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
            assert!(!editor_focused(&mut app));
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let out = screen(&mut app, 100, 24);
        assert!(
            editor_focused(&mut app) && out.contains("first") && out.contains("second"),
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
            editor_focused(&mut app) && !out.contains("PgUp scroll"),
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

    #[test]
    fn pastes_are_text_never_sends_or_shortcuts() {
        let (mut app, mut rx) = app();
        mateo_streams(&mut app, &mut rx);
        report_effort(&mut app, effort_report(Some("default")));
        typed(&mut app, "say ");
        app.on_event(Event::Paste("first\r\nsecond\r\n\t/q".into()));
        assert!(rx.try_recv().is_err(), "a pasted newline is not Enter");
        assert!(editor_focused(&mut app));
        assert_eq!(app.top().draft().unwrap().1.text, "say first\nsecond\n\t/q");
        enter(&mut app);
        let Request::Send { req, .. } = rx.try_recv().unwrap() else {
            panic!("expected one send");
        };
        assert_eq!(req.body, "say first\nsecond\n\t/q");
        assert!(rx.try_recv().is_err());
        // Navigation mode has no input: pasted keys do not search or quit.
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert!(!app.on_event(Event::Paste("/q\n".into())));
        assert!(!editor_focused(&mut app) && !bottom(&mut app).contains("/q"));
        key(&mut app, '/');
        app.on_event(Event::Paste("a\r\nb".into()));
        assert!(bottom(&mut app).contains("/a b█"));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        // The effort picker's choices take no text.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.top().navigation_blocked());
        app.on_event(Event::Paste("low\n".into()));
        assert!(app.top().navigation_blocked());
        assert!(rx.try_recv().is_err());
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
                // The owner's open conversation polls the agent's side state.
                assert!(matches!(rx.try_recv().unwrap(), Request::SideReport { .. }));
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
                // "BBS · Mateo · " keeps its colours; the error follows it.
                let bar = theme::base().patch(theme::style(theme::Role::StatusBar));
                for x in 0..14 {
                    assert_status_foreground(&buffer[(x, area.bottom() - 1)], bar);
                }
                for x in 14..area.right() {
                    let cell = &buffer[(x, area.bottom() - 1)];
                    assert_eq!(
                        cell.fg,
                        if x + 1 >= area.right() {
                            theme::base()
                                .patch(theme::style(theme::Role::StatusBar))
                                .fg
                                .unwrap()
                        } else {
                            expected.fg.unwrap()
                        }
                    );
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
        assert!(!has_line(&screen(&mut app, 80, 24), "› one"));
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
                        assert!(!has_line(&out, "› draft"), "{out}");
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
        assert!(has_line(&out, "› x"), "{out}");
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
        assert!(!has_line(&screen(&mut app, 80, 24), "› x"));
        key(&mut app, 'x');
        enter(&mut app);
        let Request::Send { id: sent, req } = rx.try_recv().unwrap() else {
            panic!("expected a send");
        };
        assert_eq!(req.to, "@crew");
        assert!(bottom(&mut app).contains("Sending…"));
        app.on_event(Event::Data(Data::Sent {
            id: sent,
            result: Err("offline".into()),
        }));
        assert!(bottom(&mut app).contains("Not sent: offline"));
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
            let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let surface = theme::base()
                .patch(theme::style(theme::Role::HumanSurface))
                .bg
                .unwrap();
            (0..29).filter(|y| buffer[(79, *y)].bg == surface).count()
        };
        assert_eq!(rows(&mut app), 3);
        for _ in 0..10 {
            key(&mut app, 'x');
            press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        }
        // 20% of 30 rows.
        assert_eq!(rows(&mut app), 7);
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
        assert!(
            out.contains("following") && !out.contains("A:") && !out.contains("E:"),
            "{out}"
        );
        assert!(!editor_focused(&mut app), "{out}");

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
        assert!(
            out.contains("A: activity ·") && !out.contains("E:"),
            "{out}"
        );
        assert!(
            !out.contains("a request") && out.contains("reply sent"),
            "{out}"
        );
        key(&mut app, 'A');
        assert!(!screen(&mut app, 100, 24).contains("A:"));

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
            !out.contains("reply sent") && out.contains("Loading…"),
            "{out}"
        );
        // Closing the thread stops both.
        while rx.try_recv().is_ok() {}
        key(&mut app, 'q');
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(feed));
        assert_eq!(rx.try_recv().unwrap(), Request::Unwatch(new_act));
    }

    #[test]
    fn compact_bottom_progress_reserves_its_cell_and_preserves_input_and_search() {
        use crate::connect::progress::Progress;
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                let (mut app, mut rx) = app();
                let (_, now_fn) = test_clock();
                app.ctx.now = now_fn;
                let (_, act) = mateo_streams(&mut app, &mut rx);
                press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
                typed(&mut app, "draft");
                press(&mut app, KeyCode::Left, KeyModifiers::NONE);
                press(&mut app, KeyCode::Left, KeyModifiers::NONE);
                app.on_event(Event::Data(Data::ActivityBaseline {
                    id: act,
                    through_id: 0,
                    record: None,
                }));
                for (id, event, state) in [
                    (0, None, Progress::Unknown),
                    (1, Some("text"), Progress::Progressing(0)),
                    (2, Some("turn_end"), Progress::Done),
                ] {
                    if let Some(event) = event {
                        app.on_event(Event::Data(Data::Activity {
                            id: act,
                            result: Ok(vec![activity(
                                id,
                                "01",
                                &format!(r#"{{"event":"{event}"}}"#),
                            )]),
                        }));
                    }
                    for width in [80, 100] {
                        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                        terminal.draw(|frame| app.draw(frame)).unwrap();
                        let buffer = terminal.backend().buffer();
                        assert_buffer(buffer);
                        let cell = &buffer[(width - 1, 23)];
                        let indicator = state.indicator();
                        let style = theme::base().patch(theme::style(theme::Role::StatusBar));
                        assert_eq!(cell.symbol(), indicator.content);
                        assert_eq!(cell.fg, style.fg.unwrap());
                        assert_eq!(cell.bg, style.bg.unwrap());
                        assert_eq!(cell.modifier, style.add_modifier);
                        assert_eq!(buffer[(width - 2, 23)].symbol(), " ");
                        assert_eq!(
                            buffer[(width - 2, 23)].bg,
                            theme::base()
                                .patch(theme::style(theme::Role::StatusBar))
                                .bg
                                .unwrap()
                        );
                        // Dark spacer, light top padding, one text row and fixed light bottom padding.
                        assert_eq!(
                            buffer[(width - 1, 19)].bg,
                            theme::base()
                                .patch(theme::style(theme::Role::Screen))
                                .bg
                                .unwrap()
                        );
                        assert_eq!(
                            buffer[(width - 1, 21)].bg,
                            theme::base()
                                .patch(theme::style(theme::Role::HumanSurface))
                                .bg
                                .unwrap()
                        );
                        assert_eq!(
                            buffer[(width - 1, 22)].bg,
                            theme::base()
                                .patch(theme::style(theme::Role::HumanSurface))
                                .bg
                                .unwrap()
                        );
                        for x in 0..width {
                            assert_status_foreground(&buffer[(x, 23)], style);
                            assert_eq!(buffer[(x, 23)].bg, style.bg.unwrap());
                            assert!(buffer[(x, 23)].modifier.contains(style.add_modifier));
                        }
                        app.ctx.preferences.error = Some("preference save failed".into());
                        terminal.draw(|frame| app.draw(frame)).unwrap();
                        let error_buffer = terminal.backend().buffer();
                        let glyph = &error_buffer[(width - 1, 23)];
                        assert_eq!(glyph.symbol(), indicator.content);
                        assert_eq!(glyph.fg, style.fg.unwrap());
                        assert_eq!(glyph.bg, style.bg.unwrap());
                        assert_eq!(glyph.modifier, style.add_modifier);
                        for x in 0..width {
                            assert_eq!(error_buffer[(x, 23)].bg, style.bg.unwrap());
                        }
                        app.ctx.preferences.error = None;
                        let buffer = terminal.backend().buffer();
                        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
                        assert!(!text.contains("Enter sends") && !text.contains("Navigation"));
                    }
                }
                press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
                key(&mut app, '/');
                typed(&mut app, &"👩‍💻".repeat(60));
                app.ctx.preferences.error = Some("save failure".into());
                let status = bottom(&mut app);
                assert!(
                    status.contains("pref error") && status.contains("Enter search · Esc cancel"),
                    "{status}"
                );
                assert!(status.ends_with(" ✓"), "{status}");
                while rx.try_recv().is_ok() {}
                press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
                press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
                typed(&mut app, "X");
                enter(&mut app);
                let Request::Send { req, .. } = rx.try_recv().unwrap() else {
                    panic!("expected send");
                };
                assert_eq!(req.body, "draXft");
            });
        }
    }

    #[test]
    fn compact_progress_uses_live_ids_and_never_advances_on_ticks_or_repaints() {
        let (mut app, mut rx) = app();
        // Pin the clock before the thread opens so frames are deterministic.
        let (clock, now_fn) = test_clock();
        app.ctx.now = now_fn;
        let (_, act) = mateo_streams(&mut app, &mut rx);
        typed(&mut app, "ab");
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        let hint_cell = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let area = buffer.area;
            buffer[(area.right() - 1, area.bottom() - 1)]
                .symbol()
                .chars()
                .next()
                .unwrap()
        };
        assert_eq!(hint_cell(&mut app), '·', "verbose also shows progress");
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '·');
        app.on_event(Event::Data(Data::ActivityBaseline {
            id: act,
            through_id: 500,
            record: None,
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
        assert_eq!(
            hint_cell(&mut app),
            '·',
            "replayed events below the watermark stay inert"
        );
        app.on_event(Event::Data(batch(501, "text")));
        assert_eq!(hint_cell(&mut app), '⠋');
        app.on_event(Event::Tick);
        assert_eq!(hint_cell(&mut app), '⠋');
        app.on_event(Event::Data(batch(501, "text")));
        assert_eq!(hint_cell(&mut app), '⠋');
        app.on_event(Event::Data(batch(502, "tool")));
        assert_eq!(
            hint_cell(&mut app),
            '⠋',
            "frames are time-derived, not per event"
        );
        // Time advances the frame: 250ms in, the third frame shows.
        clock.advance(Duration::from_millis(250));
        assert_eq!(hint_cell(&mut app), '⠹', "frames follow the clock");
        let deadline = app.next_animation_frame();
        assert_eq!(deadline, Some(clock.now() + Duration::from_millis(50)));
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '⠹', "verbose keeps the same phase");
        assert_eq!(
            app.next_animation_frame(),
            deadline,
            "verbose remains armed"
        );
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '⠹');
        assert_eq!(app.next_animation_frame(), deadline);
        app.on_event(Event::Data(batch(503, "turn_end")));
        assert_eq!(hint_cell(&mut app), '✓');
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '✓');
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(hint_cell(&mut app), '✓');
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Err("disconnected".into()),
        }));
        assert_eq!(
            hint_cell(&mut app),
            '✓',
            "recorded completion survives a lost connection"
        );
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
    fn one_view_shows_every_message_once_headerless_only_between_viewer_and_agent() {
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
        // The line above a body: a header names the sender, else none.
        let above = |out: &str, body: &str| -> String {
            let lines: Vec<&str> = out.lines().collect();
            let i = lines
                .iter()
                .position(|l| l.contains(body))
                .unwrap_or_else(|| panic!("{body}: {out}"));
            i.checked_sub(1)
                .map_or(String::new(), |j| lines[j].to_string())
        };
        let out = screen(&mut app, 120, 40);
        for body in ["direct-one", "outbound-body", "direct-two"] {
            assert_eq!(out.matches(body).count(), 1, "{body}: {out}");
            assert!(!above(&out, body).contains('→'), "{body}: {out}");
        }
        for (body, header) in [
            ("to-agent-body", "Mateo → Zed"),
            ("other-human-body", "Mateo → Ben"),
            ("broadcast-body", "Mateo → @crew"),
            ("other-human-outbound", "Ben → Mateo"),
            ("agent-named-human", "ada → Mateo"),
            ("outbound-other-agent", "ada → Zed"),
            ("other-agent-body", "Zed → ada"),
            ("human-sender-body", "Mateo → ada"),
            ("owned-recipient-body", "Mateo → ada"),
        ] {
            assert_eq!(out.matches(body).count(), 1, "{body}: {out}");
            assert!(above(&out, body).contains(header), "{body}: {out}");
        }
        assert!(out.contains("activity-body"), "{out}");
        assert!(!out.contains("**"), "{out}");
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![
                proto::Message {
                    created_at: "2026-10-16T18:00:04Z".into(),
                    ..message(13, "Mateo", "ada", "live-direct")
                },
                message(14, "Mateo", "Ben", "live-other"),
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    created_at: "2026-10-16T18:00:03Z".into(),
                    ..message(15, "ada", "Mateo", "live-outbound")
                },
            ]),
        }));
        let out = screen(&mut app, 120, 40);
        assert!(!above(&out, "live-direct").contains('→'), "{out}");
        assert!(above(&out, "live-other").contains("Mateo → Ben"), "{out}");
        assert_eq!(
            order(&out, &["direct-two", "live-outbound", "live-direct"]),
            ["direct-two", "live-outbound", "live-direct"]
        );
    }

    #[test]
    fn other_participants_have_a_distinct_surface_only_in_agent_conversations() {
        use crate::ui::theme::{Scheme, select, testing::with_scheme};
        with_scheme(Scheme::Gruvbox, || {
            let (mut app, mut rx) = app();
            let (feed, _) = mateo_streams(&mut app, &mut rx);
            let other = proto::Message {
                from_owner: Some("ada".into()),
                to_owner: Some("ada".into()),
                ..message(
                    1,
                    "Zed",
                    "Mateo",
                    &format!("other-body {} tail-end", "wrapped text ".repeat(14)),
                )
            };
            let messages = vec![
                other,
                message(2, "Mateo", "ADA", "incoming-body"),
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..message(3, "ADA", "Mateo", "outgoing-body")
                },
                proto::Message {
                    to_owner: Some("someone".into()),
                    ..message(4, "Mateo", "ada", "agent-named-viewer")
                },
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    to_group: Some("crew".into()),
                    to_name: None,
                    ..message(5, "ada", "", "viewer-group-post")
                },
                proto::Message {
                    to_group: Some("crew".into()),
                    to_name: None,
                    ..message(6, "Mateo", "", "agent-group-post")
                },
            ];
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(messages.clone()),
            }));
            let check = |app: &mut App, conversation: bool| {
                let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
                terminal.draw(|f| app.draw(f)).unwrap();
                let buffer = terminal.backend().buffer();
                crate::ui::theme::testing::assert_buffer(buffer);
                let row = |text: &str| {
                    (0..40)
                        .find(|&y| {
                            (0..80)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                                .contains(text)
                        })
                        .unwrap_or_else(|| panic!("missing {text}"))
                };
                let base = theme::base();
                let other = base.patch(theme::style(theme::Role::OtherConversation));
                if crate::ui::theme::current() != Scheme::NoColor {
                    assert_ne!(other.bg, base.bg);
                    assert_ne!(other.bg, theme::style(theme::Role::HumanSurface).bg);
                }
                for (text, role) in [
                    ("other-body", theme::Role::OtherConversation),
                    ("tail-end", theme::Role::OtherConversation),
                    ("incoming-body", theme::Role::Screen),
                    ("outgoing-body", theme::Role::HumanSurface),
                    ("agent-named-viewer", theme::Role::OtherConversation),
                    ("viewer-group-post", theme::Role::GroupConversation),
                    ("agent-group-post", theme::Role::GroupConversation),
                ] {
                    let y = row(text);
                    let expected = base.patch(theme::style(if conversation {
                        role
                    } else {
                        theme::Role::Screen
                    }));
                    assert!(
                        (0..80).all(|x| buffer[(x, y)].bg == expected.bg.unwrap()),
                        "{text}"
                    );
                }
                // The header has its own tint; wrapped body lines keep the
                // quieter surface and the separator keeps the screen background.
                let first = row("other-body");
                let last = row("tail-end");
                assert!(last > first);
                for y in first..=last {
                    assert!((0..80).all(|x| buffer[(x, y)].bg
                        == if conversation {
                            other.bg.unwrap()
                        } else {
                            base.bg.unwrap()
                        }));
                }
                let header = (0..80)
                    .map(|x| buffer[(x, first - 1)].symbol())
                    .collect::<String>();
                if conversation {
                    assert_eq!(header.trim(), "• Zed → Mateo");
                } else {
                    assert!(header.contains("Zed@ada → Mateo@ada"), "{header}");
                    assert!(header.contains("2026-10-16"), "{header}");
                }
                let header_style = if conversation {
                    base.patch(theme::style(theme::Role::OtherConversationHeader))
                } else {
                    base
                };
                // The stronger tint fills the attribution row, including the
                // unused columns; ordinary feed headers keep their background.
                assert!((0..80).all(|x| buffer[(x, first - 1)].bg == header_style.bg.unwrap()));
                if conversation && crate::ui::theme::current() != Scheme::NoColor {
                    assert_ne!(header_style.bg, other.bg);
                }
                assert!((0..80).all(|x| buffer[(x, first - 2)].bg == base.bg.unwrap()));
            };
            // Restyle the same loaded messages, without changing their data.
            for scheme in Scheme::ALL {
                select(scheme);
                check(&mut app, true);
            }
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            while rx.try_recv().is_ok() {}
            key(&mut app, 'q');
            while rx.try_recv().is_ok() {}
            key(&mut app, 'f');
            let (feed, _) = watched(&mut rx);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(messages),
            }));
            for scheme in Scheme::ALL {
                select(scheme);
                check(&mut app, false);
            }
        });
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
        let assert_background = |app: &mut App, shaded: bool, headerless: bool| {
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
                    theme::base()
                        .patch(theme::style(theme::Role::HumanSurface))
                        .bg
                        .unwrap()
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
                            == theme::style(if headerless && padding < y {
                                theme::Role::Screen
                            } else {
                                theme::Role::HumanSurface
                            })
                            .bg
                            .unwrap()
                            && buffer[(x, padding)].symbol() == " "
                    }));
                }
                if headerless {
                    assert!((area.left()..area.right()).all(|x| {
                        buffer[(x, y - 1)].bg
                            == theme::base()
                                .patch(theme::style(theme::Role::HumanSurface))
                                .bg
                                .unwrap()
                            && buffer[(x, y - 1)].symbol() == " "
                    }));
                }
            }
        };
        // Mail between the viewer and the agent is headerless, with the
        // separator and padding rows around it.
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
    fn group_cards_replace_log_envelopes_in_either_arrival_order_and_keep_theme_geometry() {
        use crate::ui::theme::{Scheme, testing::with_scheme};
        for scheme in Scheme::ALL {
            with_scheme(scheme, || {
                for activity_first in [true, false] {
                    let (mut app, mut rx) = app();
                    let (feed, act) = mateo_streams(&mut app, &mut rx);
                    let body = format!(
                        "**Joined** @nw-api. {} final-tail",
                        "wrapped words ".repeat(20)
                    );
                    let mail = proto::Message {
                        from_owner: Some("thmtz".into()),
                        to_name: None,
                        to_group: Some("nw-api".into()),
                        ..at(10, "01", "Zoe", "", &body)
                    };
                    let envelope = format!(
                        "New BBS mail (1 message):\n\n[2026-10-16T18:00:01Z] from Zoe@thmtz to @nw-api:\n{body}\n"
                    );
                    let feed_event = Event::Data(Data::Feed {
                        id: feed,
                        result: Ok(vec![mail]),
                    });
                    let activity_event = Event::Data(Data::Activity {
                        id: act,
                        result: Ok(vec![
                            activity(1, "02", r#"{"event":"mail","from":["Zoe@thmtz"],"mail_ids":[10]}"#),
                            activity(2, "03", &serde_json::json!({"event":"prompt", "text":envelope, "mail_ids":[10]}).to_string()),
                        ]),
                    });
                    if activity_first {
                        app.on_event(activity_event);
                        let old = screen(&mut app, 80, 40);
                        assert!(
                            old.contains("mail from Zoe@thmtz"),
                            "old-server fallback: {old}"
                        );
                        app.on_event(feed_event);
                    } else {
                        app.on_event(feed_event);
                        app.on_event(activity_event);
                    }
                    let out = screen(&mut app, 80, 40);
                    assert_eq!(out.matches("• Zoe → @nw-api").count(), 1, "{out}");
                    assert_eq!(out.matches("Joined").count(), 1, "{out}");
                    assert!(
                        !out.contains("mail from")
                            && !out.contains("thmtz")
                            && !out.contains("[2026-"),
                        "{out}"
                    );
                    for width in [80, 120] {
                        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                        terminal.draw(|f| app.draw(f)).unwrap();
                        let buf = terminal.backend().buffer();
                        crate::ui::theme::testing::assert_buffer(buf);
                        let row = |needle: &str| {
                            (0..40)
                                .find(|&y| {
                                    (0..width)
                                        .map(|x| buf[(x, y)].symbol())
                                        .collect::<String>()
                                        .contains(needle)
                                })
                                .unwrap()
                        };
                        let header = row("Zoe → @nw-api");
                        let end = row("final-tail");
                        assert_eq!(buf[(0, header)].symbol(), "•");
                        assert_eq!(buf[(2, header)].symbol(), "Z");
                        let bg = |role| theme::base().patch(theme::style(role)).bg.unwrap();
                        assert!((0..width).all(
                            |x| buf[(x, header)].bg == bg(theme::Role::GroupConversationHeader)
                        ));
                        assert!(
                            (0..width).all(|x| buf[(x, header - 1)].bg == bg(theme::Role::Screen))
                        );
                        for y in header + 1..=end {
                            assert!(
                                (0..width)
                                    .all(|x| buf[(x, y)].bg == bg(theme::Role::GroupConversation))
                            );
                            assert_eq!(buf[(0, y)].symbol(), " ");
                            assert_eq!(buf[(1, y)].symbol(), " ");
                        }
                        assert!(buf.content.iter().any(|c| c.symbol() == "J"
                            && c.modifier.contains(ratatui::style::Modifier::BOLD)));
                    }
                    // Activity-only filtering restores the raw echo, not an empty view.
                    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
                    key(&mut app, 'A');
                    key(&mut app, 'A');
                    assert!(screen(&mut app, 80, 40).contains("mail from Zoe@thmtz"));
                    key(&mut app, 'A');
                    assert_eq!(screen(&mut app, 80, 40).matches("Joined").count(), 1);
                }
            });
        }
    }

    #[test]
    fn delivered_text_and_owner_mail_echoes_show_once_in_either_arrival_order() {
        // ada asks (mail 10); the turn echoes it; Mateo's answer is delivered
        // as owner mail 11 and its text record names it.
        let mail = || {
            vec![
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..at(10, "01", "ada", "Mateo", "please build")
                },
                proto::Message {
                    auto_reply: Some(true),
                    ..at(11, "05", "Mateo", "ada", "built it")
                },
            ]
        };
        let records = || {
            vec![
                activity(
                    1,
                    "02",
                    r#"{"ts":"2026-10-16T18:00:02Z","event":"mail","from":["ada"],"mail_ids":[10]}"#,
                ),
                activity(
                    2,
                    "03",
                    r#"{"ts":"2026-10-16T18:00:03Z","event":"prompt","text":"You are Mateo.\n\nNew BBS mail (1 message):\n\n[2026-10-16T18:00:01Z] from ada (human) to Mateo:\nplease build\n","mail_ids":[10]}"#,
                ),
                activity(
                    3,
                    "04",
                    r#"{"ts":"2026-10-16T18:00:04Z","event":"tool","id":"t","title":"cargo build","status":"completed"}"#,
                ),
                activity(
                    4,
                    "05",
                    r#"{"ts":"2026-10-16T18:00:05Z","event":"text","text":"built it","mail_id":11}"#,
                ),
            ]
        };
        let once = |out: &str| {
            for body in ["please build", "built it"] {
                assert_eq!(out.matches(body).count(), 1, "{body}: {out}");
            }
            // The turn marker is left to the progress mark.
            assert!(!out.contains("━━") && out.contains("cargo build"), "{out}");
            assert!(!out.contains("mail from ada"), "{out}");
        };
        for mail_first in [true, false] {
            let (mut app, mut rx) = app();
            let (feed, act) = mateo_streams(&mut app, &mut rx);
            press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
            let feed = Event::Data(Data::Feed {
                id: feed,
                result: Ok(mail()),
            });
            let activity = Event::Data(Data::Activity {
                id: act,
                result: Ok(records()),
            });
            if mail_first {
                app.on_event(feed);
                app.on_event(activity);
            } else {
                // Before the mail loads, the activity stands in for it.
                app.on_event(activity);
                let out = screen(&mut app, 100, 40);
                assert!(
                    out.contains("please build")
                        && out.contains("built it")
                        && out.contains("mail from ada"),
                    "{out}"
                );
                // One feed event settles it, and a paused reader stays paused.
                press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
                assert!(bottom(&mut app).contains("paused"));
                app.on_event(feed);
                assert!(bottom(&mut app).contains("paused"));
            }
            once(&screen(&mut app, 100, 40));
        }

        // Older runners' records, a partly loaded echo, undelivered text and
        // another human's mail all stay visible.
        let (mut app, mut rx) = app();
        let (feed, act) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..at(10, "01", "ada", "Mateo", "please build")
                },
                proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..at(20, "06", "Ben", "Mateo", "from ben")
                },
            ]),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                activity(1, "02", r#"{"ts":"2026-10-16T18:00:02Z","event":"text","text":"legacy words"}"#),
                activity(2, "03", r#"{"ts":"2026-10-16T18:00:03Z","event":"prompt","text":"New BBS mail (2 messages):\n\npartial echo\n","mail_ids":[10,99]}"#),
                activity(3, "04", r#"{"ts":"2026-10-16T18:00:04Z","event":"steer","text":"New BBS mail (1 message):\n\nsteered echo\n","mail_ids":[10]}"#),
                activity(4, "05", r#"{"ts":"2026-10-16T18:00:05Z","event":"text","text":"undelivered words","mail_id":12}"#),
            ]),
        }));
        let out = screen(&mut app, 100, 40);
        for shown in [
            "legacy words",
            "partial echo",
            "undelivered words",
            "Ben → Mateo",
        ] {
            assert!(out.contains(shown), "{shown}: {out}");
        }
        assert!(
            !out.contains("steered echo") && !out.contains("steered into"),
            "{out}"
        );
    }

    #[test]
    fn delivered_text_hides_only_for_its_agents_auto_reply_and_draws_once() {
        let text = |id: i64, body: &str, mail_id: i64| {
            activity(
                id,
                "05",
                &format!(
                    r#"{{"ts":"2026-10-16T18:00:05Z","event":"text","text":"{body}","mail_id":{mail_id}}}"#
                ),
            )
        };
        let (mut app, mut rx) = app();
        let (feed, act) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![
                proto::Message {
                    auto_reply: Some(true),
                    ..at(11, "04", "Mateo", "ada", "the reply")
                },
                // Mail by hand, and another agent's automatic reply.
                at(12, "04", "Mateo", "ada", "by hand"),
                proto::Message {
                    auto_reply: Some(true),
                    ..at(13, "04", "Zed", "ada", "zed reply")
                },
            ]),
        }));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                // Newer than the filler below, so it outlives the reply in
                // the bounded view.
                activity(
                    1,
                    "07",
                    r#"{"ts":"2026-10-16T18:00:07Z","event":"text","text":"the reply","mail_id":11}"#,
                ),
                text(2, "manual words", 12),
                text(3, "foreign words", 13),
                // A retried post repeats an undelivered-looking copy; equal
                // prose under another id is a different reply.
                text(4, "retried words", 50),
                text(5, "retried words", 50),
                text(6, "retried words", 51),
            ]),
        }));
        let out = screen(&mut app, 100, 40);
        assert_eq!(out.matches("the reply").count(), 1, "{out}");
        for shown in ["manual words", "foreign words", "by hand", "zed reply"] {
            assert!(out.contains(shown), "{shown}: {out}");
        }
        assert_eq!(out.matches("retried words").count(), 2, "{out}");
        // With messages filtered out, the text stands in for its reply;
        // with activity filtered out, the reply shows.
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, 'A');
        assert!(bottom(&mut app).contains("A: messages"));
        assert_eq!(screen(&mut app, 100, 40).matches("the reply").count(), 1);
        key(&mut app, 'A');
        assert!(bottom(&mut app).contains("A: activity"));
        assert_eq!(screen(&mut app, 100, 40).matches("the reply").count(), 1);
        key(&mut app, 'A');
        assert_eq!(screen(&mut app, 100, 40).matches("the reply").count(), 1);

        // Evicting the reply from the bounded message history brings the
        // text back, once.
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((100..100 + crate::ui::thread::MAX_ROWS as i64)
                .map(|id| at(id, "06", "Mateo", "Ben", "filler"))
                .collect()),
        }));
        let matches = |app: &mut App, query: &str| {
            key(app, '/');
            typed(app, query);
            enter(app);
            bottom(app)
        };
        assert!(
            matches(&mut app, "by hand").contains(": 0 matches"),
            "evicted"
        );
        let row = matches(&mut app, "the reply");
        assert!(row.contains(": 1 matches"), "{row}");

        // Evicting the first copy from the bounded activity keeps one.
        let (mut app, mut rx) = self::app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        let mut records = vec![text(1, "kept once", 60)];
        records.extend(
            (2..=crate::connect::timeline::MAX_RECORDS as i64 + 1).map(|id| {
                activity(
                    id,
                    "05",
                    r#"{"ts":"2026-10-16T18:00:05Z","event":"turn_end","stop_reason":"end_turn"}"#,
                )
            }),
        );
        records.push(text(
            crate::connect::timeline::MAX_RECORDS as i64 + 2,
            "kept once",
            60,
        ));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(records),
        }));
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let row = matches(&mut app, "kept once");
        assert!(row.contains(": 1 matches"), "{row}");
    }

    #[test]
    fn a_retry_after_every_copy_is_evicted_still_shows_once() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let copy = |id: i64| {
            activity(
                id,
                "05",
                r#"{"ts":"2026-10-16T18:00:05Z","event":"text","text":"retry words","mail_id":70}"#,
            )
        };
        let count = |app: &mut App| {
            key(app, '/');
            typed(app, "retry words");
            enter(app);
            bottom(app)
        };
        // The original is visible.
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![copy(1)]),
        }));
        assert!(count(&mut app).contains(": 1 matches"));
        // Every record naming mail 70 is evicted from the bounded history.
        let n = crate::connect::timeline::MAX_RECORDS as i64;
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok((2..=n + 1)
                .map(|id| activity(id, "06", r#"{"event":"turn_end","stop_reason":"end_turn"}"#))
                .collect()),
        }));
        // A retried post naming mail 70 arrives: still one copy.
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![copy(n + 2)]),
        }));
        let row = count(&mut app);
        assert!(row.contains(": 1 matches"), "{row}");
    }

    #[test]
    fn turn_markers_give_way_to_the_progress_mark_and_tool_runs_get_one_blank() {
        let rec = |id: i64, s: &str, body: &str| {
            activity(
                id,
                s,
                &format!(r#"{{"ts":"2026-10-16T18:00:{s}Z",{body}}}"#),
            )
        };
        let records = || {
            vec![
                // ada's mail is loaded, so this turn's echo goes entirely.
                rec(
                    1,
                    "02",
                    r#""event":"prompt","text":"New BBS mail (1 message):\n\nplease build\n","mail_ids":[10]"#,
                ),
                rec(2, "03", r#""event":"text","text":"Looking.""#),
                rec(
                    3,
                    "04",
                    r#""event":"tool","id":"a","title":"Read a.rs","status":"pending""#,
                ),
                rec(4, "05", r#""event":"tool","id":"a","status":"completed""#),
                rec(
                    5,
                    "06",
                    r#""event":"tool","id":"b","title":"Read b.rs","status":"pending""#,
                ),
                rec(6, "07", r#""event":"tool","id":"b","status":"completed""#),
                rec(7, "08", r#""event":"text","text":"Done.""#),
                // Mail not in this thread keeps its body, without a marker.
                rec(
                    8,
                    "09",
                    r#""event":"prompt","text":"New BBS mail (1 message):\n\ngroup note\n","mail_ids":[99]"#,
                ),
                rec(
                    9,
                    "10",
                    r#""event":"tool","id":"c","title":"Read c.rs","status":"pending""#,
                ),
                rec(10, "11", r#""event":"tool","id":"c","status":"completed""#),
                rec(11, "12", r#""event":"turn_end","stop_reason":"end_turn""#),
            ]
        };
        // The lines from the one holding `first` through the one holding `last`.
        let span = |out: &str, first: &str, last: &str| -> Vec<String> {
            let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
            let a = lines.iter().position(|l| l.contains(first)).expect(first);
            let b = lines.iter().position(|l| l.contains(last)).expect(last);
            // Through the line after `last`, for the run's closing blank.
            lines[a..=b + 1].iter().map(|l| l.to_string()).collect()
        };
        let expected = [
            "  Looking.",
            "",
            "  ✓ Read a.rs completed (1.0s)",
            "  ✓ Read b.rs completed (1.0s)",
            "",
            "  Done.",
            "group note",
            "",
            "  ✓ Read c.rs completed (1.0s)",
            "",
        ];
        let start = |app: &mut App, rx: &mut UnboundedReceiver<Request>| {
            let (feed, act) = mateo_streams(app, rx);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![proto::Message {
                    from_kind: proto::Kind::Human.into(),
                    ..at(10, "01", "ada", "Mateo", "please build")
                }]),
            }));
            app.on_event(Event::Data(Data::ActivityBaseline {
                id: act,
                through_id: 0,
                record: None,
            }));
            (feed, act)
        };

        // Streamed one record at a time, then redrawn by a filter round trip:
        // the same lines, no turn marker, and the progress mark at Done.
        let (mut app, mut rx) = app();
        let (_, act) = start(&mut app, &mut rx);
        for record in records() {
            app.on_event(Event::Data(Data::Activity {
                id: act,
                result: Ok(vec![record]),
            }));
        }
        let streamed = screen(&mut app, 80, 30);
        assert_eq!(
            span(&streamed, "Looking.", "Read c.rs"),
            expected,
            "{streamed}"
        );
        assert!(
            !streamed.contains("━━") && !streamed.contains("turn end"),
            "{streamed}"
        );
        let row = bottom(&mut app);
        assert!(row.ends_with('✓'), "{row}");
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        for _ in 0..3 {
            key(&mut app, 'A');
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let rebuilt = screen(&mut app, 80, 30);
        assert_eq!(
            span(&rebuilt, "Looking.", "Read c.rs"),
            expected,
            "{rebuilt}"
        );

        // Mail landing inside a run splits it: each part gets one blank line
        // above and below, never two.
        let (mut app, mut rx) = self::app();
        let (feed, act) = start(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(records()),
        }));
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![at(20, "06", "Zed", "Mateo", "zed note")]),
        }));
        let out = screen(&mut app, 80, 40);
        let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
        let b = lines.iter().position(|l| l.contains("Read b.rs")).unwrap();
        assert!(lines[b - 1].is_empty() && !lines[b - 2].is_empty(), "{out}");
        let a = lines.iter().position(|l| l.contains("Read a.rs")).unwrap();
        assert!(lines[a - 1].is_empty() && !lines[a - 2].is_empty(), "{out}");
        assert!(lines[a + 1].is_empty() && !lines[a + 2].is_empty(), "{out}");

        // A run at the top still gets its blank line; a call still running
        // shows in the pane below (its blank lines follow opens_run's
        // rule), and E: problems hides it.
        let (mut app, mut rx) = self::app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                rec(
                    1,
                    "01",
                    r#""event":"tool","id":"a","title":"Read a.rs","status":"pending""#,
                ),
                rec(2, "02", r#""event":"tool","id":"a","status":"completed""#),
                rec(3, "03", r#""event":"text","text":"Next.""#),
                rec(
                    4,
                    "04",
                    r#""event":"tool","id":"d","title":"Read d.rs","status":"pending""#,
                ),
            ]),
        }));
        let out = screen(&mut app, 80, 30);
        let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
        assert!(
            lines[0].is_empty() && lines[1].contains("Read a.rs"),
            "{out}"
        );
        assert!(lines[2].is_empty() && lines[3].contains("Next."), "{out}");
        assert!(out.contains("Read d.rs running"), "{out}");
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, 'E');
        // Following again, so only the filter can hide the pane.
        key(&mut app, 'G');
        assert!(screen(&mut app, 80, 30).contains("Read d.rs running"));
        key(&mut app, 'E');
        let out = screen(&mut app, 80, 30);
        assert!(
            out.contains("following") && !out.contains("Read d.rs running"),
            "{out}"
        );
    }

    #[test]
    fn long_tool_calls_take_one_line_that_fits_and_widens_on_resize() {
        use unicode_width::UnicodeWidthStr;
        let script = "python3 - <<'EOF'\\nimport json\\nfor i in range(100):\\n    print(json.dumps({'i': i, 'square': i * i, 'cube': i * i * i, 'label': f'item-{i}'}))\\nEOF";
        let call = |id: i64, s: &str, call: &str, status: &str| {
            let title = if status == "in_progress" {
                format!(r#","title":"{script} #{call}""#)
            } else {
                String::new()
            };
            activity(
                id,
                s,
                &format!(
                    r#"{{"ts":"2026-10-16T18:00:{s}Z","event":"tool","id":"{call}","status":"{status}"{title}}}"#
                ),
            )
        };
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                activity(
                    1,
                    "01",
                    r#"{"ts":"2026-10-16T18:00:01Z","event":"text","text":"Running it."}"#,
                ),
                call(2, "02", "a", "in_progress"),
                call(3, "03", "a", "completed"),
                call(4, "04", "b", "in_progress"),
                call(5, "05", "b", "completed"),
                call(6, "06", "c", "in_progress"),
            ]),
        }));
        let rows =
            |out: &str| -> Vec<String> { out.lines().map(|l| l.trim_end().to_string()).collect() };
        for width in [80u16, 120] {
            let out = screen(&mut app, width, 30);
            let lines = rows(&out);
            let text = lines
                .iter()
                .position(|l| l.contains("Running it."))
                .unwrap();
            // One blank line, then each call on exactly one line, tight.
            assert_eq!(lines[text + 1], "", "{out}");
            for (offset, line) in lines[text + 2..text + 4].iter().enumerate() {
                assert!(
                    line.starts_with("  ✓ python3 - <<'EOF' import json"),
                    "{out}"
                );
                assert!(line.ends_with("… completed (1.0s)"), "{offset}: {out}");
                assert!(line.width() <= usize::from(width), "{out}");
            }
            assert!(
                !lines[text + 4].contains("import"),
                "no wrapped tail: {out}"
            );
            let running = lines
                .iter()
                .find(|l| l.contains(" running"))
                .expect("running");
            assert!(
                running.contains('…') && running.width() <= usize::from(width),
                "{out}"
            );
        }
        // The wider screen shows more of each call than the narrow one.
        let at = |out: &str| rows(out).iter().find(|l| l.contains("✓")).unwrap().width();
        let narrow = at(&screen(&mut app, 80, 30));
        let wide = at(&screen(&mut app, 120, 30));
        assert!(wide > narrow, "{narrow} {wide}");
        // Search still reaches the part of a call the line leaves out.
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        key(&mut app, '/');
        typed(&mut app, "json.dumps");
        enter(&mut app);
        let row = bottom(&mut app);
        assert!(row.contains(": 2 matches"), "{row}");
    }

    #[test]
    fn running_calls_continue_the_finished_run_with_one_blank_below() {
        let rec = |id: i64, body: String| {
            activity(
                id,
                "05",
                &format!(r#"{{"ts":"2026-10-16T18:00:05Z",{body}}}"#),
            )
        };
        let tool = |id: i64, call: &str, status: &str| {
            rec(
                id,
                format!(
                    r#""event":"tool","id":"{call}","title":"Read {call}.rs","status":"{status}""#
                ),
            )
        };
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        // Enough text to fill the timeline, so its last row meets the pane.
        let mut records: Vec<_> = (1..=40)
            .map(|id| rec(id, format!(r#""event":"text","text":"line {id}""#)))
            .collect();
        records.extend([tool(41, "a", "in_progress"), tool(42, "a", "completed")]);
        records.extend((0..12).map(|n| tool(43 + n, &format!("r{n}"), "in_progress")));
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(records),
        }));
        let out = screen(&mut app, 80, 30);
        let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
        let done = lines
            .iter()
            .position(|l| l.contains("✓ Read a.rs"))
            .expect("done");
        // One blank opens the run; the running calls follow with none between.
        assert!(
            lines[done - 1].is_empty() && lines[done - 2].contains("line 40"),
            "{out}"
        );
        assert!(lines[done + 1].contains("… Read r0.rs running"), "{out}");
        // More calls than the pane holds: it keeps its closing blank anyway.
        let last = lines.iter().rposition(|l| l.contains(" running")).unwrap();
        let prompt = lines.iter().rposition(|l| l.starts_with('›')).unwrap();
        // The pane's closing blank, then the composer's two padding rows.
        assert_eq!(prompt, last + 4, "{out}");
        assert!(
            lines[last + 1..prompt].iter().all(|l| l.is_empty()),
            "{out}"
        );
        assert!(
            !out.contains("Read r11.rs"),
            "the pane holds only a third: {out}"
        );
        // When the calls finish, the run closes after the last finished one.
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok((0..12)
                .map(|n| tool(55 + n, &format!("r{n}"), "completed"))
                .collect()),
        }));
        let out = screen(&mut app, 80, 30);
        let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
        let last = lines
            .iter()
            .rposition(|l| l.contains("✓ Read r11.rs"))
            .unwrap();
        assert!(
            lines[last + 1].is_empty() && !out.contains(" running"),
            "{out}"
        );
    }

    #[test]
    fn a_running_pane_too_small_for_a_call_and_its_blanks_is_not_shown() {
        let (mut app, mut rx) = app();
        let (_, act) = mateo_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Activity {
            id: act,
            result: Ok(vec![
                activity(1, "01", r#"{"ts":"2026-10-16T18:00:01Z","event":"text","text":"Working."}"#),
                activity(2, "02", r#"{"ts":"2026-10-16T18:00:02Z","event":"tool","id":"d","title":"Read d.rs","status":"in_progress"}"#),
            ]),
        }));
        assert!(screen(&mut app, 80, 24).contains("Read d.rs running"));
        // A composer allowed the whole height, grown by a long draft, leaves
        // the timeline too little for the pane's share to hold a call.
        app.ctx.composer_max_height_percent = 100;
        for n in 0..30 {
            typed(&mut app, &format!("draft {n}"));
            press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        }
        let out = screen(&mut app, 80, 24);
        assert!(!out.contains("running"), "{out}");
        assert!(out.contains("draft 29"), "{out}");
        // Every share: a pane is a call or more and all its blank lines, or
        // nothing at all.
        use crate::connect::screens::thread::running_pane;
        let calls = || vec![Line::raw("… a running"), Line::raw("… b running")];
        let shape = |pane: Vec<Line<'static>>| -> Vec<String> {
            pane.iter().map(|l| l.to_string()).collect()
        };
        for share in 0..=5u16 {
            for open in [false, true] {
                let pane = shape(running_pane(calls(), open, share));
                let fits = usize::from(share) >= usize::from(open) + 2;
                assert_eq!(!pane.is_empty(), fits, "{share} {open}: {pane:?}");
                if fits {
                    assert!(pane.len() <= usize::from(share), "{share}: {pane:?}");
                    assert_eq!(pane[0].is_empty(), open, "{pane:?}");
                    assert!(pane[usize::from(open)].contains("running"), "{pane:?}");
                    assert_eq!(pane.last().unwrap(), "", "closing blank: {pane:?}");
                }
            }
        }
        assert!(running_pane(Vec::new(), true, 5).is_empty());
    }

    #[test]
    fn raw_feed_gaps_show_one_warning_and_overlaps_none() {
        {
            let (mut app, mut rx) = app();
            let (feed, _) = mateo_streams(&mut app, &mut rx);
            press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(vec![message(1, "Mateo", "ada", "before-gap")]),
            }));
            // A burst pushed messages out of the raw 200-message window.
            let snapshot = (201..=400)
                .map(|id| {
                    let body = if id == 201 { "after-gap" } else { "burst" };
                    message(id, "Mateo", if id == 400 { "ada" } else { "Ben" }, body)
                })
                .collect();
            app.on_event(Event::Data(Data::Feed {
                id: feed,
                result: Ok(snapshot),
            }));
            // Tall enough for the whole history: the warning sits between
            // the two sides of the gap.
            let out = screen(&mut app, 120, 700);
            assert_eq!(
                order(
                    &out,
                    &["before-gap", "messages may be missing", "after-gap"]
                ),
                ["before-gap", "messages may be missing", "after-gap"],
                "{out}"
            );
            assert_eq!(out.matches("messages may be missing").count(), 1, "{out}");
        }
        let (mut app, mut rx) = app();
        let (feed, _) = mateo_streams(&mut app, &mut rx);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((1..200)
                .map(|id| message(id, "Mateo", "Ben", "already received"))
                .collect()),
        }));
        // This full raw window overlaps previously held messages.
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok((2..=201)
                .map(|id| message(id, "Mateo", "Ben", "new arrival"))
                .collect()),
        }));
        let out = screen(&mut app, 120, 30);
        assert!(
            out.contains("new arrival") && !out.contains("messages may be missing"),
            "{out}"
        );
    }

    #[test]
    fn activity_alone_says_when_nothing_is_shared_or_the_server_is_old() {
        let (mut app, mut rx) = app();
        let (feed, act) = mateo_loading_streams(&mut app, &mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![]),
        }));
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
        assert!(!out.contains("E:"), "{out}");
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
        let (feed, _) = watched(&mut rx);
        app.on_event(Event::Data(Data::Feed {
            id: feed,
            result: Ok(vec![]),
        }));
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
        for (show_steps, kind_steps) in [(0, 0), (1, 0), (2, 1), (0, 2)] {
            let (mut app, mut rx) = app();
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
                app.on_event(Event::Data(Data::MarkedRead {
                    agent: "Mateo".into(),
                    result: Ok(()),
                }));
                assert!(matches!(rx.try_recv(), Ok(Request::Unwatch(_))));
                assert_eq!(
                    rx.try_recv().unwrap(),
                    Request::WatchSummary(app.ctx.inbox.id)
                );
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
                    without_polls(&mut rx).is_empty(),
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
