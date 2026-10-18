//! A live thread of messages: an agent's conversation with the activity it
//! shares, a group's broadcasts, or the whole feed as `bbs view` shows it.
//! Agent and group threads have a composer below for writing to them, and
//! group threads the admin dialogs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::connect::admin::GroupAdmin;
use crate::connect::app::{Ctx, Screen, Transition, next_id};
use crate::connect::composer::Composer;
use crate::connect::drafts::{Draft, DraftStore};
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
    /// Compact mode shows only direct mail
    /// between that agent and this human, retaining the verbose filter choices.
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
    /// A typed target identifies a draft independently of its feed stream.
    draft_key: Option<String>,
    /// An agent thread's activity, which viewing marks seen.
    stream: Option<Stream>,
    /// The newest activity marked seen.
    seen: i64,
    /// The clock for activity freshness and animation; the app's, or a test's.
    now: std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
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
            draft_key: None,
            stream: None,
            seen: 0,
            now: std::sync::Arc::new(Instant::now),
        }
    }

    /// What `agent` sent and what was sent to it directly.
    pub fn agent(
        agent: &str,
        human: &str,
        compact: bool,
        now: std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Thread {
        let title = format!("{agent} conversation");
        let req = FeedRequest {
            with: Some(agent.to_string()),
            ..Default::default()
        };
        let mut thread = Thread::new(title, req, Some(agent));
        thread.agent = Some(agent.to_string());
        thread.draft_key = Some(format!("agent:{}", agent.to_ascii_lowercase()));
        thread.composing = true;
        thread.human = Some(human.to_string());
        thread.compact = compact;
        thread.stream = Some(Stream::new(agent));
        thread.now = now;
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
        let visible_gaps: HashSet<_> = self
            .message_gaps
            .iter()
            .filter_map(|gap| {
                self.messages
                    .iter()
                    .find(|m| {
                        m.id >= *gap && (self.direct_to_human(m) || self.direct_from_human(m))
                    })
                    .map(|m| m.id)
            })
            .collect();
        messages
            .iter()
            .filter(|m| !self.compact || self.direct_to_human(m) || self.direct_from_human(m))
            .cloned()
            .map(|m| Entry::Message {
                compact: self.compact,
                outgoing: self.direct_from_human(&m),
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

    fn direct_from_human(&self, message: &Message) -> bool {
        message.from_kind() == proto::Kind::Human
            && self
                .human
                .as_ref()
                .is_some_and(|human| message.from_name.eq_ignore_ascii_case(human))
            && message.to_group.is_none()
            && self.agent.as_ref().is_some_and(|agent| {
                message
                    .to_name
                    .as_ref()
                    .is_some_and(|to| to.eq_ignore_ascii_case(agent))
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
        thread.draft_key = Some(format!("group:{}", group.to_ascii_lowercase()));
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
        let mut text = format!("{} messages · ", self.view.rows.len());
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
        Line::styled(text, theme::dim())
    }

    fn conversation_status(
        &self,
        width: u16,
        ctx: &Ctx,
        draft_error: Option<&str>,
    ) -> Line<'static> {
        let width = usize::from(width);
        let searching = self.prompt.is_some() || self.view.search_status().is_some();
        let mut mode = if self.view.following() {
            "following".to_string()
        } else {
            "paused".to_string()
        };
        if !self.view.following() {
            if self.view.new_count > 0 {
                mode.push_str(&format!(" · {} new", self.view.new_count));
            }
            if !searching {
                mode.push_str(" · G follows");
            }
        }
        let issue = draft_error
            .map(|e| ("draft error", e.to_string()))
            .or_else(|| {
                ctx.preferences
                    .error
                    .as_ref()
                    .map(|e| ("pref error", e.clone()))
            })
            .or_else(|| {
                self.composer
                    .as_ref()
                    .and_then(|c| c.error())
                    .map(|e| ("send error", format!("Not sent: {e}")))
            })
            .or_else(|| self.error.as_ref().map(|e| ("feed error", e.clone())))
            .or_else(|| {
                self.stream
                    .as_ref()
                    .and_then(|s| s.error.as_ref())
                    .map(|e| ("activity error", format!("activity: {e}")))
            });
        let error_style = issue.as_ref().map(|_| theme::error()).unwrap_or_default();
        if let Some(prompt) = &self.prompt {
            if let Some((badge, _)) = &issue {
                mode.push_str(&format!(" · {badge}"));
            }
            let suffix = "█ · Enter search · Esc cancel";
            let available = width.saturating_sub(mode.width() + 3 + 1 + suffix.width());
            return Line::styled(
                format!("{mode} · /{}{suffix}", fit_status(prompt, available, true)),
                error_style,
            );
        }
        if let Some((_, message)) = issue {
            let available = width.saturating_sub(mode.width() + 3);
            return Line::styled(
                format!("{mode} · {}", fit_status(&message, available, false)),
                error_style,
            );
        }
        let hints = |short: bool| {
            let mut parts = Vec::new();
            if !self.compact {
                parts.push(if short {
                    format!("A:{}", self.show.label())
                } else {
                    format!("A: {}", self.show.label())
                });
                if self.show.stream()
                    && let Some(stream) = &self.stream
                {
                    let label = stream.kinds.label();
                    parts.push(if short {
                        format!("E:{}", label.strip_suffix(" kinds").unwrap_or(label))
                    } else {
                        format!("E: {label}")
                    });
                }
            }
            parts.push(if short {
                format!(
                    "Ctrl-T:{}",
                    if self.compact { "verbose" } else { "compact" }
                )
            } else {
                format!(
                    "Ctrl-T: {}",
                    if self.compact { "verbose" } else { "compact" }
                )
            });
            parts.join(if short { " " } else { " · " })
        };
        let mut filters = hints(false);
        let mut text = mode;
        if self.composer.as_ref().is_some_and(Composer::sending) {
            text.push_str(" · Sending…");
        }
        if let Some((query, hits)) = self.view.search_status() {
            let suffix = format!(": {hits} matches · n/N");
            if text.width() + filters.width() + query.width() + suffix.width() + 7 > width {
                filters = hints(true);
            }
            let available =
                width.saturating_sub(text.width() + filters.width() + suffix.width() + 7);
            text.push_str(&format!(
                " · /{}{suffix}",
                fit_status(query, available, true)
            ));
        }
        text.push_str(&format!(" · {filters}"));
        let secs = ctx.started.elapsed().as_secs();
        for detail in [
            &ctx.server,
            &ctx.me,
            &format!(
                "online {:02}:{:02}:{:02}",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ),
        ] {
            if text.width() + detail.width() + 3 <= width {
                text.push_str(&format!(" · {detail}"));
            }
        }
        Line::from(fit_status(&text, width, false))
    }
}

/// Clip feedback by display columns without splitting emoji or combining marks.
fn fit_status(text: &str, width: usize, tail: bool) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut take = |part: &&str| {
        used += part.width();
        used < width
    };
    if tail {
        let mut parts: Vec<_> = text.graphemes(true).rev().take_while(&mut take).collect();
        parts.reverse();
        format!("…{}", parts.concat())
    } else {
        let prefix: String = text.graphemes(true).take_while(&mut take).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod status_tests {
    use super::fit_status;

    #[test]
    fn feedback_clipping_keeps_whole_graphemes_from_either_end() {
        assert_eq!(fit_status("👍🏽🇺🇸👩‍💻", 3, false), "👍🏽…");
        assert_eq!(fit_status("👍🏽🇺🇸👩‍💻", 3, true), "…👩‍💻");
        assert_eq!(fit_status("éé", 2, false), "éé");
        assert_eq!(fit_status("ééé", 2, true), "…é");
        assert_eq!(fit_status("👩‍💻", 1, true), "…");
        assert_eq!(fit_status("x", 0, false), "");
    }
}

impl Thread {
    /// Agent conversations open in insert mode; only PgUp switches to
    /// navigation. Groups keep Esc as the way out of their composer.
    fn compose(&mut self, key: KeyEvent) -> Option<Transition> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Quit through the keymap, as everywhere.
            return None;
        }
        if key.code == KeyCode::Esc {
            if self.agent.is_some() {
                return Some(Transition::Pop);
            }
            self.composing = false;
            return Some(Transition::Stay);
        }
        if self.agent.is_some() && key.code == KeyCode::PageUp {
            self.composing = false;
            let page = self.view.page();
            self.view.scroll_up(page);
            return Some(Transition::Stay);
        }
        let send_id = if key.code == KeyCode::Enter {
            next_id()
        } else {
            self.id
        };
        let composer = self.composer.as_mut()?;
        Some(match composer.key(key, send_id) {
            Some(body) => Transition::Request(Request::Send {
                id: send_id,
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

    fn bottom_status(
        &self,
        width: u16,
        ctx: &Ctx,
        draft_error: Option<&str>,
    ) -> Option<Line<'static>> {
        if self.agent.is_none() {
            if draft_error.is_some() {
                return None;
            }
            let composer = self.composer.as_ref()?;
            return if let Some(error) = composer.error() {
                Some(Line::styled(
                    fit_status(&format!("Not sent: {error}"), usize::from(width), false),
                    theme::error(),
                ))
            } else if composer.sending() {
                Some(Line::from("Sending…"))
            } else {
                None
            };
        }
        let indicator = self
            .stream
            .as_ref()
            .filter(|_| self.compact)
            .map(|s| s.progress((self.now)()).indicator());
        let text_width = width.saturating_sub(if indicator.is_some() { 2 } else { 0 });
        let mut line = self.conversation_status(text_width, ctx, draft_error);
        if let Some(mut indicator) = indicator
            && width > 0
        {
            indicator.style = theme::style(theme::Role::Screen).patch(indicator.style);
            let text = fit_status(&line.to_string(), usize::from(text_width), false);
            let padding = usize::from(width - 1).saturating_sub(text.width());
            line.spans = vec![Span::raw(text), Span::raw(" ".repeat(padding)), indicator];
        }
        Some(line)
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

    fn reload(&mut self) -> Vec<Request> {
        // The stream starts over from its oldest record.
        if let Some(stream) = &mut self.stream {
            let cancel = stream.unwatch();
            stream.reset();
            self.seen = 0;
            self.rebuild();
            return vec![cancel];
        }
        Vec::new()
    }

    fn on_close(&self) -> Vec<Request> {
        let mut requests = vec![Request::Unwatch(self.id)];
        requests.extend(self.stream.as_ref().map(Stream::unwatch));
        requests
    }

    fn next_animation_frame(&self) -> Option<Instant> {
        let stream = self.stream.as_ref()?;
        if self.compact {
            stream.next_frame_at((self.now)())
        } else {
            None
        }
    }

    fn save_draft(&mut self, drafts: &mut HashMap<String, Composer>) {
        if let Some(key) = &self.draft_key
            && let Some(composer) = self.composer.take()
        {
            drafts.insert(key.clone(), composer);
        }
    }

    fn restore_draft(&mut self, drafts: &mut HashMap<String, Composer>, store: &mut DraftStore) {
        if let Some(key) = &self.draft_key {
            if let Some(composer) = drafts.remove(key) {
                self.composer = Some(composer);
            } else if let Some(draft) = store.load(key)
                && let Some(composer) = &mut self.composer
            {
                composer.restore(draft);
            }
        }
    }

    fn draft(&self) -> Option<(&str, Draft)> {
        Some((self.draft_key.as_deref()?, self.composer.as_ref()?.draft()))
    }

    fn on_data(&mut self, data: &Data) {
        if let Some(stream) = &mut self.stream {
            let entries = stream.on_data(data, (self.now)());
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
            Data::Sent { id, result } => {
                if let Some(composer) = &mut self.composer
                    && composer.sent(*id, result.clone())
                {
                    // A saved draft can finish a send from an earlier screen.
                    if result.is_ok() {
                        self.view.follow();
                    }
                }
            }
            _ => {}
        }
    }

    /// An open conversation marks received mail read and activity seen,
    /// independently of the display filters.
    fn on_view(&mut self, _ctx: &Ctx) -> Vec<Request> {
        let Some(agent) = &self.agent else {
            return Vec::new();
        };
        let mut requests = Vec::new();
        let unread = self
            .messages
            .iter()
            .any(|m| m.read_at.is_none() && self.direct_to_human(m));
        if unread {
            requests.push(Request::MarkRead(agent.clone()));
        }
        if let Some(newest) = self.stream.as_ref().and_then(Stream::newest)
            && newest > self.seen
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
            return Some(Transition::Compact(self.compact));
        }
        if self.composing {
            return self.compose(key);
        }
        if self.agent.is_some() && self.prompt.is_none() && key.code == KeyCode::Esc {
            self.composing = true;
            return Some(Transition::Stay);
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
        let inner = if self.agent.is_some() {
            area
        } else {
            theme::screen(frame, area, &self.title)
        };
        let max_lines = frame.area().height * u16::from(ctx.composer_max_height_percent) / 100;
        let composer_height = self
            .composer
            .as_ref()
            .map_or(0, |c| c.height(inner.width, max_lines));
        let [status, body, composer] = Layout::vertical([
            Constraint::Length(u16::from(self.agent.is_none())),
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
                Line::styled(
                    if self.agent.is_some() {
                        "PgUp navigates · r reloads"
                    } else {
                        "r reloads"
                    },
                    theme::dim(),
                ),
            ];
            frame.render_widget(Paragraph::new(text), body);
            return;
        }
        // Lay out first, so the status line reflects this frame.
        self.view.layout(body.width, body.height as usize);
        if self.agent.is_none() {
            frame.render_widget(Paragraph::new(self.status()), status);
        }
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
