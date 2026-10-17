//! A live thread of messages: an agent's conversation with the activity it
//! shares, a group's broadcasts, or the whole feed as `bbs view` shows it.
//! Agent and group threads have a composer below for writing to them, and
//! group threads the admin dialogs.

use std::collections::{BTreeSet, HashSet};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::connect::admin::GroupAdmin;
use crate::connect::app::{Ctx, Screen, Transition, next_id};
use crate::connect::composer::Composer;
use crate::connect::event::{Data, Request};
use crate::connect::keys::{Action, GroupOp};
use crate::connect::theme;
use crate::connect::timeline::{Entry, Show, Stream};
use crate::proto::{self, FeedRequest, Message, SendRequest};
use crate::ui::thread::{MAX_ROWS, ThreadView};

/// Messages each feed snapshot holds, as for `bbs view`.
const BACKLOG: u32 = 200;

pub struct Thread {
    title: String,
    id: u64,
    req: FeedRequest,
    view: ThreadView<Entry>,
    /// Every message received, so another `show` can rebuild the view.
    messages: Vec<Message>,
    /// Raw feed boundaries that may have skipped messages, before compact filtering.
    message_gaps: BTreeSet<i64>,
    show: Show,
    /// Agent conversations start verbose; compact mode shows only direct mail
    /// from that agent to this human, retaining the verbose filter choices.
    compact: bool,
    human: Option<String>,
    /// The stream's last error; it has ended until a reload.
    error: Option<String>,
    /// The search being typed after `/`.
    prompt: Option<String>,
    composer: Option<Composer>,
    /// Keys go to the composer.
    composing: bool,
    /// A group thread's group, as last listed, and its admin dialogs.
    group: Option<(proto::Group, GroupAdmin)>,
    /// An agent thread's agent, whose mail to the human viewing marks read.
    agent: Option<String>,
    /// An agent thread's activity, which viewing marks seen.
    stream: Option<Stream>,
    /// The newest activity marked seen.
    seen: i64,
}

impl Thread {
    fn new(title: String, req: FeedRequest, to: Option<&str>) -> Thread {
        Thread {
            title,
            id: next_id(),
            req: FeedRequest {
                backlog: BACKLOG,
                ..req
            },
            view: ThreadView::new(Vec::new(), BACKLOG as usize),
            messages: Vec::new(),
            message_gaps: BTreeSet::new(),
            show: Show::All,
            compact: false,
            human: None,
            error: None,
            prompt: None,
            composer: to.map(Composer::new),
            composing: false,
            group: None,
            agent: None,
            stream: None,
            seen: 0,
        }
    }

    /// What `agent` sent and what was sent to it directly.
    pub fn agent(agent: &str, owner: Option<&str>, human: &str) -> Thread {
        let title = match owner {
            Some(owner) => format!("{agent}, owned by {owner}"),
            None => agent.to_string(),
        };
        let req = FeedRequest {
            with: Some(agent.to_string()),
            ..Default::default()
        };
        let mut thread = Thread::new(title, req, Some(agent));
        thread.agent = Some(agent.to_string());
        thread.human = Some(human.to_string());
        thread.stream = Some(Stream::new(agent));
        thread
    }

    /// Lay the timeline out afresh from everything held, for a new `show` or
    /// activity kinds, keeping the search.
    fn rebuild(&mut self) {
        let mut entries = Vec::new();
        if self.compact || self.show.messages() {
            entries.extend(self.message_entries(&self.messages));
        }
        if !self.compact
            && self.show.stream()
            && let Some(stream) = &mut self.stream
        {
            entries.extend(stream.entries());
        }
        let query = self.view.query.clone();
        self.view = ThreadView::new(entries, BACKLOG as usize);
        if let Some(query) = query {
            self.view.set_query(&query);
        }
    }

    fn message_entries(&self, messages: &[Message]) -> Vec<Entry> {
        if !self.compact {
            return messages.iter().cloned().map(Entry::Message).collect();
        }
        let visible_gaps: HashSet<_> = self
            .message_gaps
            .iter()
            .filter_map(|gap| {
                self.messages
                    .iter()
                    .find(|m| m.id >= *gap && self.direct_to_human(m))
                    .map(|m| m.id)
            })
            .collect();
        messages
            .iter()
            .filter(|m| self.direct_to_human(m))
            .cloned()
            .map(|m| Entry::CompactMessage {
                gap: visible_gaps.contains(&m.id),
                message: m,
            })
            .collect()
    }

    fn direct_to_human(&self, message: &Message) -> bool {
        message.from_kind() == proto::Kind::Agent
            && self
                .agent
                .as_ref()
                .is_some_and(|agent| message.from_name.eq_ignore_ascii_case(agent))
            && message.to_group.is_none()
            && message.to_owner.is_none()
            && self.human.as_ref().is_some_and(|human| {
                message
                    .to_name
                    .as_ref()
                    .is_some_and(|to| to.eq_ignore_ascii_case(human))
            })
    }

    /// Keep a snapshot's messages, refreshing those held, oldest first.
    fn keep(&mut self, snapshot: &[Message]) {
        if let Some(first) = snapshot.first()
            && snapshot.len() >= BACKLOG as usize
            && self.messages.last().is_none_or(|last| first.id > last.id)
        {
            self.message_gaps.insert(first.id);
        }
        for m in snapshot {
            match self.messages.binary_search_by_key(&m.id, |r| r.id) {
                Ok(i) => self.messages[i] = m.clone(),
                Err(i) => self.messages.insert(i, m.clone()),
            }
        }
        let over = self.messages.len().saturating_sub(MAX_ROWS);
        self.messages.drain(..over);
        if let Some(oldest) = self.messages.first() {
            self.message_gaps.retain(|id| *id >= oldest.id);
        }
    }

    /// Broadcasts to `group`.
    pub fn group(group: &str) -> Thread {
        let req = FeedRequest {
            group: Some(group.to_string()),
            ..Default::default()
        };
        let to = format!("@{group}");
        let mut thread = Thread::new(to.clone(), req, Some(&to));
        let listed = proto::Group {
            name: group.to_string(),
            ..Default::default()
        };
        thread.group = Some((listed, GroupAdmin::default()));
        thread
    }

    /// Everything on the board.
    pub fn feed() -> Thread {
        Thread::new("Feed".into(), FeedRequest::default(), None)
    }

    fn status(&self) -> Line<'static> {
        let notice = self.group.as_ref().and_then(|(_, admin)| admin.notice());
        let mut line = self.thread_status();
        if let Some(notice) = notice {
            line.spans.push(Span::styled(" · ", theme::dim()));
            line.spans.extend(notice.spans);
        }
        line
    }

    fn thread_status(&self) -> Line<'static> {
        if let Some(prompt) = &self.prompt {
            return Line::from(vec![
                Span::styled(format!("/{prompt}"), theme::base()),
                Span::styled("█", theme::dim()),
                Span::styled("  Enter search · Esc cancel", theme::dim()),
            ]);
        }
        let mut text = if self.compact {
            format!("{} messages to you · ", self.view.rows.len())
        } else {
            match &self.stream {
                Some(stream) => {
                    let mut text = format!(
                        "{} entries · A: {}",
                        self.view.rows.len(),
                        self.show.label()
                    );
                    if self.show.stream() {
                        text.push_str(&format!(" · E: {}", stream.kinds.label()));
                    }
                    text + " · "
                }
                None => format!("{} messages · ", self.view.rows.len()),
            }
        };
        if self.view.following() {
            text.push_str("following");
        } else if self.view.new_count > 0 {
            text.push_str(&format!("paused · {} new · G follows", self.view.new_count));
        } else {
            text.push_str("paused · G follows");
        }
        if let Some((query, hits)) = self.view.search_status() {
            text.push_str(&format!(" · /{query}: {hits} matches · n/N"));
        }
        if !self.compact
            && let Some(error) = self.stream.as_ref().and_then(|s| s.error.as_ref())
            && self.show != Show::Stream
        {
            text.push_str(&format!(" · activity: {error}"));
        }
        if self.agent.is_some() {
            text.push_str(if self.compact {
                " · Ctrl-T: verbose"
            } else {
                " · Ctrl-T: compact"
            });
        }
        Line::styled(text, theme::dim())
    }
}

impl Thread {
    /// A key while composing: `Esc` leaves the composer, and `Enter` sends.
    fn compose(&mut self, key: KeyEvent) -> Option<Transition> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Quit through the keymap, as everywhere.
            return None;
        }
        if key.code == KeyCode::Esc {
            self.composing = false;
            return Some(Transition::Stay);
        }
        let composer = self.composer.as_mut()?;
        Some(match composer.key(key) {
            Some(body) => Transition::Request(Request::Send {
                id: self.id,
                req: SendRequest {
                    to: composer.to().to_string(),
                    body,
                    control: None,
                },
            }),
            None => Transition::Stay,
        })
    }
}

/// A stream error as the window shows it.
fn describe(error: &str) -> String {
    if error.contains("feed-with.v1") {
        "This screen needs a newer server (feed-with.v1). Ask its operator to upgrade.".into()
    } else {
        error.to_string()
    }
}

impl Screen for Thread {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn requests(&self) -> Vec<Request> {
        let mut requests = vec![Request::Watch {
            id: self.id,
            req: self.req.clone(),
        }];
        // The group's description, for the describe dialog.
        if self.group.is_some() {
            requests.push(Request::Groups);
        }
        requests.extend(self.stream.as_ref().map(Stream::watch));
        requests
    }

    fn reload(&mut self) {
        // The stream starts over from its oldest record.
        if let Some(stream) = &mut self.stream {
            stream.reset();
            self.seen = 0;
            self.rebuild();
        }
    }

    fn on_close(&self) -> Vec<Request> {
        let mut requests = vec![Request::Unwatch(self.id)];
        requests.extend(self.stream.as_ref().map(Stream::unwatch));
        requests
    }

    fn on_data(&mut self, data: &Data) {
        if let Some(stream) = &mut self.stream {
            let entries = stream.on_data(data);
            if !self.compact && self.show.stream() && !entries.is_empty() {
                self.view.ingest(entries);
            }
        }
        match data {
            Data::Feed { id, result } if *id == self.id => match result {
                Ok(messages) => {
                    self.error = None;
                    self.keep(messages);
                    if self.compact || self.show.messages() {
                        let entries = self.message_entries(messages);
                        self.view.ingest(entries);
                    }
                }
                Err(e) => self.error = Some(describe(e)),
            },
            Data::Groups(Ok(list)) => {
                if let Some((group, _)) = &mut self.group
                    && let Some(listed) = list
                        .groups
                        .iter()
                        .find(|g| g.name.eq_ignore_ascii_case(&group.name))
                {
                    *group = listed.clone();
                }
            }
            Data::Admin { .. } => {
                if let Some((_, admin)) = &mut self.group {
                    admin.on_data(self.id, data);
                }
            }
            Data::Sent { id, result } if *id == self.id => {
                if let Some(composer) = &mut self.composer {
                    composer.sent(result.clone());
                }
                // Show the message just sent.
                if result.is_ok() {
                    self.view.follow();
                }
            }
            _ => {}
        }
    }

    /// Mark read the agent's mail to the viewer, and seen its activity, as far
    /// as the timeline shows them.
    fn on_view(&mut self, ctx: &Ctx) -> Vec<Request> {
        let Some(agent) = &self.agent else {
            return Vec::new();
        };
        let mut requests = Vec::new();
        let unread = self.messages.iter().any(|m| {
            m.read_at.is_none()
                && m.from_name.eq_ignore_ascii_case(agent)
                && m.to_name.as_deref() == Some(ctx.me.as_str())
        });
        if unread && (self.compact || self.show.messages()) {
            requests.push(Request::MarkRead(agent.clone()));
        }
        if let Some(newest) = self.stream.as_ref().and_then(Stream::newest)
            && newest > self.seen
            && !self.compact
            && self.show.stream()
        {
            self.seen = newest;
            requests.push(Request::MarkSeen {
                agent: agent.clone(),
                through_id: newest,
            });
        }
        requests
    }

    fn raw_key(&mut self, key: KeyEvent) -> Option<Transition> {
        if self.agent.is_some()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('t')
        {
            self.compact = !self.compact;
            self.rebuild();
            return Some(Transition::Stay);
        }
        if self.composing {
            return self.compose(key);
        }
        if let Some((_, admin)) = &mut self.group
            && let Some(transition) = admin.key(self.id, key)
        {
            return Some(transition);
        }
        let prompt = self.prompt.as_mut()?;
        match key.code {
            KeyCode::Enter => {
                let query = std::mem::take(prompt);
                self.prompt = None;
                self.view.search(&query);
            }
            KeyCode::Esc => self.prompt = None,
            KeyCode::Backspace => {
                prompt.pop();
            }
            KeyCode::Char(c) => prompt.push(c),
            _ => {}
        }
        Some(Transition::Stay)
    }

    fn handle(&mut self, action: Action, _ctx: &Ctx) -> Transition {
        let page = self.view.page();
        match action {
            Action::Up => self.view.scroll_up(1),
            Action::Down => self.view.scroll_down_by(1),
            Action::PageUp => self.view.scroll_up(page),
            Action::PageDown => self.view.scroll_down_by(page),
            Action::HalfPageUp => self.view.scroll_up(page / 2),
            Action::HalfPageDown => self.view.scroll_down_by(page / 2),
            Action::Top => self.view.scroll_to(0),
            Action::Bottom => self.view.follow(),
            Action::Search => self.prompt = Some(String::new()),
            Action::Compose => self.composing = self.composer.is_some(),
            // Only an agent's thread has activity.
            Action::Show if self.stream.is_some() && !self.compact => {
                self.show = self.show.next();
                self.rebuild();
            }
            Action::ActivityKinds if !self.compact => {
                if let Some(stream) = &mut self.stream {
                    stream.kinds = stream.kinds.next();
                    self.rebuild();
                }
            }
            // Creating a group belongs to the Groups list.
            Action::Group(op) if op != GroupOp::Create => {
                if let Some((group, admin)) = &mut self.group {
                    admin.open(op, Some(group));
                }
            }
            Action::NextMatch => self.view.jump(true),
            Action::PrevMatch => self.view.jump(false),
            Action::Back => return Transition::Pop,
            _ => {}
        }
        Transition::Stay
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.draw(frame, area, ctx);
        if let Some((_, admin)) = &self.group {
            admin.render(frame, area);
        }
    }
}

impl Thread {
    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let window = theme::window(&self.title);
        let inner = window.inner(area);
        frame.render_widget(window, area);
        let max_lines = frame.area().height * u16::from(ctx.composer_max_height_percent) / 100;
        let composer_height = self
            .composer
            .as_ref()
            .map_or(0, |c| c.height(inner.width, max_lines));
        let [status, body, composer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(composer_height),
        ])
        .areas(inner);
        if let Some(c) = &mut self.composer {
            c.render(frame, composer, self.composing);
        }
        if let Some(error) = &self.error {
            let text = vec![
                Line::styled(error.clone(), theme::error()),
                Line::styled("r reloads", theme::dim()),
            ];
            frame.render_widget(Paragraph::new(text), body);
            return;
        }
        // Lay out first, so the status line reflects this frame.
        self.view.layout(body.width, body.height as usize);
        frame.render_widget(Paragraph::new(self.status()), status);
        if let Some(notice) = self.empty_activity() {
            frame.render_widget(Paragraph::new(notice).wrap(Wrap { trim: false }), body);
            return;
        }
        // Calls still running sit below the newest activity while following.
        let running = match &self.stream {
            Some(stream) if !self.compact && self.show.stream() && self.view.following() => {
                stream.running()
            }
            _ => Vec::new(),
        };
        let height = (running.len() as u16).min(body.height / 3);
        let [body, below] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(height)]).areas(body);
        frame.render_widget(Paragraph::new(running), below);
        self.view.render(frame, body);
    }

    /// Why an activity-only timeline is empty.
    fn empty_activity(&self) -> Option<Line<'static>> {
        if self.compact {
            return None;
        }
        let stream = self.stream.as_ref()?;
        if self.show != Show::Stream || !self.view.rows.is_empty() {
            return None;
        }
        let agent = self.agent.as_deref().unwrap_or_default();
        Some(if let Some(error) = &stream.error {
            Line::styled(format!("{error} r retries."), theme::error())
        } else if !stream.loaded {
            Line::styled("Loading…", theme::dim())
        } else if stream.is_empty() {
            Line::styled(
                format!(
                    "{agent} shares no activity yet. Headless agents share it unless their harness sets share_activity = false; see HEADLESS.md."
                ),
                theme::dim(),
            )
        } else {
            Line::styled(
                format!(
                    "Nothing of these kinds. E shows {}.",
                    stream.kinds.next().label()
                ),
                theme::dim(),
            )
        })
    }
}
