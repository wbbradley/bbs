//! A live thread of messages: an agent's conversation, a group's broadcasts,
//! or the whole feed as `bbs view` shows it. Agent and group threads have a
//! composer below for writing to them, and group threads the admin dialogs.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::connect::activity::ActivityPane;
use crate::connect::admin::GroupAdmin;
use crate::connect::app::{Ctx, Screen, Transition, next_id};
use crate::connect::composer::Composer;
use crate::connect::event::{Data, Request};
use crate::connect::keys::{Action, GroupOp};
use crate::connect::theme;
use crate::proto::{self, FeedRequest, SendRequest};
use crate::ui::thread::ThreadView;

/// Messages each feed snapshot holds, as for `bbs view`.
const BACKLOG: u32 = 200;

pub struct Thread {
    title: String,
    id: u64,
    req: FeedRequest,
    view: ThreadView,
    /// The stream's last error; it has ended until a reload.
    error: Option<String>,
    /// The search being typed after `/`.
    prompt: Option<String>,
    composer: Option<Composer>,
    /// Keys go to the composer.
    composing: bool,
    /// A group thread's group, as last listed, and its admin dialogs.
    group: Option<(proto::Group, GroupAdmin)>,
    /// An agent thread's activity, while shown.
    activity: Option<ActivityPane>,
    /// An agent thread's agent, whose mail to the human viewing marks read.
    agent: Option<String>,
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
            error: None,
            prompt: None,
            composer: to.map(Composer::new),
            composing: false,
            group: None,
            activity: None,
            agent: None,
        }
    }

    /// What `agent` sent and what was sent to it directly.
    pub fn agent(agent: &str, owner: Option<&str>) -> Thread {
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
        thread
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
        requests.extend(self.activity.as_ref().map(ActivityPane::watch));
        requests
    }

    fn reload(&mut self) {
        if let Some(pane) = &mut self.activity {
            pane.reset();
        }
    }

    fn on_close(&self) -> Vec<Request> {
        let mut requests = vec![Request::Unwatch(self.id)];
        requests.extend(self.activity.as_ref().map(ActivityPane::unwatch));
        requests
    }

    fn on_data(&mut self, data: &Data) {
        if let Some(pane) = &mut self.activity {
            pane.on_data(data);
        }
        match data {
            Data::Feed { id, result } if *id == self.id => match result {
                Ok(messages) => {
                    self.error = None;
                    self.view.ingest(messages.clone());
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

    fn on_view(&mut self, ctx: &Ctx) -> Vec<Request> {
        let Some(agent) = &self.agent else {
            return Vec::new();
        };
        let unread = self.view.rows.iter().any(|m| {
            m.read_at.is_none()
                && m.from_name.eq_ignore_ascii_case(agent)
                && m.to_name.as_deref() == Some(ctx.me.as_str())
        });
        if unread {
            vec![Request::MarkRead(agent.clone())]
        } else {
            Vec::new()
        }
    }

    fn raw_key(&mut self, key: KeyEvent) -> Option<Transition> {
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
            Action::ActivityKinds => {
                if let Some(pane) = &mut self.activity {
                    pane.cycle();
                }
            }
            Action::ActivityPane => match (self.activity.take(), &self.req.with) {
                (Some(pane), _) => return Transition::Request(pane.unwatch()),
                (None, Some(agent)) => {
                    let pane = ActivityPane::new(agent);
                    let watch = pane.watch();
                    self.activity = Some(pane);
                    return Transition::Request(watch);
                }
                (None, None) => {}
            },
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
        let mut inner = window.inner(area);
        frame.render_widget(window, area);
        if let Some(pane) = &self.activity {
            let [thread, side] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(inner);
            pane.render(frame, side);
            inner = thread;
        }
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
        self.view.render(frame, body);
    }
}
