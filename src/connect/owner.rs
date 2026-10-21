//! What an agent's runner asks its owner, and what it reports about its
//! context: the question card above the composer, the `/answer` form, and the
//! `/context` panel, for the owner's open conversation only.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::app::next_id;
use super::event::{Data, Request, SideError};
use super::theme;
use crate::proto;

/// One choice a question offers.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    pub description: String,
    pub value: String,
}

/// One question in a set the runner asks.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub text: String,
    pub choices: Vec<Choice>,
    pub allow_free_text: bool,
    pub required: bool,
}

/// The questions a runner is waiting on, under one request id, with the
/// form's own message when the harness gives one.
#[derive(Clone, Debug, PartialEq)]
pub struct QuestionSet {
    pub request_id: i64,
    /// The runner waits on the answer; otherwise the questions are only
    /// available to answer.
    pub blocking: bool,
    pub message: Option<String>,
    pub questions: Vec<Question>,
}

/// What a context measurement covers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Measure {
    /// The context the session holds now.
    Current,
    /// The context the last model request used.
    LastRequest,
}

/// A runner's latest context report.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextUsage {
    pub measure: Measure,
    pub used: Option<u64>,
    pub limit: Option<u64>,
    pub reported_at: String,
}

/// One answer to one question.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Choice(String),
    Text(String),
}

/// The owner's reply to a question set.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// An answer per answered question, by question id; optional questions
    /// left blank are absent.
    Submit(Vec<(String, Response)>),
    Decline,
    Cancel,
}

/// The owner's view of one agent's side state while its conversation is
/// open: polled once a second, with the `/answer` form and `/context` panel.
pub struct Session {
    /// The id for this conversation's side requests and their replies.
    id: u64,
    in_flight: bool,
    report: Option<proto::SideReportResponse>,
    /// The server lacks agent-interactions.v1, or refused this owner: stop
    /// asking.
    pub unsupported: bool,
    pub form: Option<Form>,
    pub context_open: bool,
    /// The request id of a reply on its way: one at a time.
    sending: Option<i64>,
    /// A request answered and acknowledged: a poll begun before that still
    /// shows it waiting, and must not bring it back.
    answered: Option<i64>,
    pub notice: Option<String>,
    /// The `/btw` panel, from this session's ask or the first report's side
    /// question.
    pub side: Option<Side>,
    /// A first report has arrived: later side questions appear only when
    /// asked here.
    seen_report: bool,
    /// A draft the server took as a side question: the composer clears it if
    /// nothing was typed since.
    pub asked_draft: Option<String>,
}

/// The longest side question the server takes.
pub const SIDE_QUESTION_LIMIT: usize = 4 * 1024;

/// Earlier exchanges a side conversation keeps for display, in this session:
/// the newest of at most this many, and this many bytes.
const HISTORY_EXCHANGES: usize = 20;
const HISTORY_BYTES: usize = 256 * 1024;

/// One earlier question and answer of a side conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Exchange {
    pub question: String,
    pub answer: String,
}

/// The owner's `/btw` question and where it stands.
#[derive(Debug, PartialEq)]
pub struct Side {
    /// The draft as submitted.
    draft: String,
    pub question: String,
    /// The latest side id known before asking: an ask whose outcome is
    /// unknown adopts only a newer one with the same question.
    baseline: i64,
    pub state: SideState,
    /// A cancel on its way.
    cancelling: bool,
    pub notice: Option<String>,
    /// Lines scrolled up from the answer's end.
    pub scroll: Cell<usize>,
    /// The question this one follows up, in the same side conversation.
    follows: Option<i64>,
    /// The exchange this follow-up continues, kept until the server takes it.
    previous: Option<Exchange>,
    /// Earlier exchanges of this side conversation seen in this session,
    /// oldest first; bounded, never sent anywhere.
    pub history: Vec<Exchange>,
    /// History was lost (a reopened conversation shows only its latest
    /// exchange), though the agent's side copy keeps it.
    pub history_lost: bool,
    /// The side conversation's id (its first question's), once known.
    conversation: Option<i64>,
    /// The tracked question's answer as last reported.
    answer: String,
}

impl Side {
    fn new(draft: &str, question: &str, baseline: i64, state: SideState) -> Side {
        Side {
            draft: draft.to_string(),
            question: question.to_string(),
            baseline,
            state,
            cancelling: false,
            notice: None,
            scroll: Cell::new(0),
            follows: None,
            previous: None,
            history: Vec::new(),
            history_lost: false,
            conversation: None,
            answer: String::new(),
        }
    }

    /// The side panel for `row`, a conversation's latest question seen in a
    /// report: earlier exchanges, if any, are the agent's alone.
    fn adopt(row: &proto::SideRequest) -> Side {
        let mut side = Side::new("", &row.question, row.id, SideState::Tracking(row.id));
        side.conversation = row.conversation;
        side.history_lost = row.follows.is_some();
        side
    }

    /// Keep `exchange` in the bounded history.
    fn remember(&mut self, exchange: Exchange) {
        self.history.push(exchange);
        let size = |h: &[Exchange]| {
            h.iter()
                .map(|e| e.question.len() + e.answer.len())
                .sum::<usize>()
        };
        while self.history.len() > HISTORY_EXCHANGES
            || (self.history.len() > 1 && size(&self.history) > HISTORY_BYTES)
        {
            self.history.remove(0);
            self.history_lost = true;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SideState {
    /// The ask is on its way.
    Asking,
    /// The server took it under this id; reports carry the rest.
    Tracking(i64),
    /// The ask's outcome is unknown, for this reason.
    Unconfirmed(String),
    /// Not asked, for this reason.
    NotAsked(String),
    /// A follow-up the server refused: the side conversation cannot go on.
    Ended,
}

/// What a side question shows: its server status and answer so far.
#[derive(Clone, Debug, PartialEq)]
pub enum SideView {
    Asking,
    Unconfirmed(String),
    NotAsked(String),
    /// Taken, before a report shows it.
    Asked,
    Open(String),
    Completed(String),
    Failed(String),
    Cancelled(String),
    Interrupted(String),
    /// A newer side question took its place in the reports.
    Replaced,
    /// The side conversation cannot go on; the answer so far, if any.
    Ended(String),
}

impl SideView {
    /// Whether the question may still be answered.
    pub fn running(&self) -> bool {
        matches!(self, SideView::Asking | SideView::Asked | SideView::Open(_))
    }
}

impl Session {
    pub fn new() -> Session {
        Session {
            id: next_id(),
            in_flight: false,
            report: None,
            unsupported: false,
            form: None,
            context_open: false,
            sending: None,
            answered: None,
            notice: None,
            side: None,
            seen_report: false,
            asked_draft: None,
        }
    }

    /// Ask `question` aside, from `draft`, starting a side conversation, or
    /// with `follow`, continuing the open one; the reason it cannot be asked
    /// otherwise. Never sent as mail either way.
    pub fn ask_side(
        &mut self,
        agent: &str,
        draft: &str,
        question: &str,
        follow: bool,
    ) -> Result<Request, String> {
        if follow {
            self.side_input()?;
        }
        if self.side_view().is_some_and(|v| v.running()) {
            return Err("A side question is already running (Esc cancels it).".into());
        }
        if self.unsupported {
            return Err("Side questions are not available on this server.".into());
        }
        let report = self
            .report
            .as_ref()
            .ok_or("Waiting for the agent's report; try again in a moment.")?;
        if report.side_supported != Some(true) {
            return Err(report
                .side_reason
                .clone()
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| format!("{agent}'s runner does not answer side questions.")));
        }
        if question.is_empty() {
            return Err("Usage: /btw <question>".into());
        }
        if question.len() > SIDE_QUESTION_LIMIT {
            return Err(format!(
                "A side question is at most 4 KiB; this one is {} bytes.",
                question.len()
            ));
        }
        if report
            .side
            .as_ref()
            .is_some_and(|s| s.status() == proto::SideStatus::SideOpen)
        {
            return Err("A side question is already open for this agent.".into());
        }
        let baseline = report.side.as_ref().map_or(0, |s| s.id);
        let mut side = Side::new(draft, question, baseline, SideState::Asking);
        if follow {
            let current = self.side.take().expect("an open side conversation");
            side.follows = Some(baseline);
            side.conversation = current.conversation;
            side.history = current.history;
            side.history_lost = current.history_lost;
            side.previous = Some(Exchange {
                question: current.question,
                answer: report
                    .side
                    .as_ref()
                    .and_then(|s| s.answer.clone())
                    .unwrap_or_default(),
            });
        }
        let follows = side.follows;
        self.side = Some(side);
        Ok(Request::AskSide {
            id: self.id,
            agent: agent.to_string(),
            question: question.to_string(),
            follows,
        })
    }

    /// Whether the runner continues side conversations.
    fn followups(&self) -> bool {
        self.report.as_ref().and_then(|r| r.side_followups) == Some(true)
    }

    /// Whether the panel's side conversation can take a follow-up now.
    pub fn continuable(&self) -> bool {
        let (Some(side), Some(latest)) = (
            self.side.as_ref(),
            self.report.as_ref().and_then(|r| r.side.as_ref()),
        ) else {
            return false;
        };
        side.state == SideState::Tracking(latest.id)
            && !latest.ended
            && matches!(
                latest.status(),
                proto::SideStatus::SideCompleted
                    | proto::SideStatus::SideFailed
                    | proto::SideStatus::SideCancelled
            )
            && self.followups()
    }

    /// btw mode: the side conversation takes the view and the input, while a
    /// question runs or a follow-up can be asked.
    pub fn btw_mode(&self) -> bool {
        // A runner without follow-ups keeps the one-answer panel.
        self.followups() && (self.side_view().is_some_and(|v| v.running()) || self.continuable())
    }

    /// Whether typing can go to the side conversation now, and why not. While
    /// the panel is up, typing never goes to the agent as mail.
    pub fn side_input(&self) -> Result<(), String> {
        if self.side_view().is_some_and(|v| v.running()) {
            return Err("Wait for the answer, or Esc to cancel it.".into());
        }
        if self.continuable() {
            return Ok(());
        }
        Err(if self.followups() {
            "Side conversation ended; your next /btw starts fresh.".into()
        } else {
            "Follow-ups need an update; Esc closes the side panel.".into()
        })
    }

    /// The panel's question and where it stands, if a panel is up.
    pub fn side_view(&self) -> Option<SideView> {
        let side = self.side.as_ref()?;
        Some(match &side.state {
            SideState::Asking => SideView::Asking,
            SideState::Unconfirmed(why) => SideView::Unconfirmed(why.clone()),
            SideState::NotAsked(why) => SideView::NotAsked(why.clone()),
            SideState::Ended => SideView::Ended(String::new()),
            SideState::Tracking(id) => {
                match self.report.as_ref().and_then(|r| r.side.as_ref()) {
                    Some(s) if s.id == *id => {
                        let answer = s.answer.clone().unwrap_or_default();
                        // A follow-up could not go on, or the conversation can
                        // no longer be continued (ended, idle, restarted).
                        if s.status() == proto::SideStatus::SideEnded
                            || (s.ended && s.status() != proto::SideStatus::SideOpen)
                        {
                            return Some(SideView::Ended(answer));
                        }
                        match s.status() {
                            proto::SideStatus::SideOpen => SideView::Open(answer),
                            proto::SideStatus::SideCompleted => SideView::Completed(answer),
                            proto::SideStatus::SideFailed => SideView::Failed(answer),
                            proto::SideStatus::SideCancelled => SideView::Cancelled(answer),
                            proto::SideStatus::SideInterrupted | proto::SideStatus::SideEnded => {
                                SideView::Interrupted(answer)
                            }
                            proto::SideStatus::Unspecified => SideView::Asked,
                        }
                    }
                    Some(s) if s.id > *id => SideView::Replaced,
                    // A report from before the ask: not this question.
                    _ => SideView::Asked,
                }
            }
        })
    }

    /// Esc on the panel: cancel a question still running and stay, or close
    /// the panel, ending a side conversation that could go on. `None` when
    /// there is nothing to send.
    pub fn side_escape(&mut self) -> Option<Request> {
        let view = self.side_view()?;
        let side = self.side.as_mut()?;
        match (&side.state, view) {
            // The id is not known yet: nothing to cancel by.
            (SideState::Asking, _) => {
                side.notice = Some("Waiting for the server to take the question.".into());
                None
            }
            (SideState::Tracking(id), view) if view.running() => {
                if side.cancelling {
                    return None;
                }
                side.cancelling = true;
                side.notice = Some("Cancelling…".into());
                Some(Request::CancelSide {
                    id: self.id,
                    side: *id,
                    end: false,
                })
            }
            (SideState::Tracking(id), _) => {
                let id = *id;
                let end = self.continuable();
                self.side = None;
                // Closed at once; a lost end falls to the runner's idle limit.
                end.then_some(Request::CancelSide {
                    id: self.id,
                    side: id,
                    end: true,
                })
            }
            _ => {
                self.side = None;
                None
            }
        }
    }

    /// Fit the panel to a report: adopt an unconfirmed ask's question, or on
    /// a session's first report, the side question it shows.
    fn track_side(&mut self) {
        let first = !std::mem::replace(&mut self.seen_report, true);
        let Some(latest) = self.report.as_ref().and_then(|r| r.side.as_ref()) else {
            return;
        };
        // A cancel shows done once its row is: a lost reply must not leave
        // "Cancelling…" up.
        if let Some(side) = self.side.as_mut()
            && side.cancelling
            && side.state == SideState::Tracking(latest.id)
            && latest.status() != proto::SideStatus::SideOpen
        {
            side.cancelling = false;
            side.notice = None;
        }
        // The asked conversation: a fresh question starts its own.
        let own = |side: &Side| match side.conversation {
            Some(c) => latest.conversation == Some(c),
            None => latest.conversation.is_none_or(|c| c == latest.id),
        };
        match &mut self.side {
            Some(side) if side.state == SideState::Tracking(latest.id) => {
                side.answer = latest.answer.clone().unwrap_or_default();
                if side.conversation.is_none() {
                    side.conversation = latest.conversation;
                }
            }
            Some(side)
                if matches!(side.state, SideState::Unconfirmed(_))
                    && latest.id > side.baseline
                    && latest.question == side.question
                    && latest.follows == side.follows
                    && own(side) =>
            {
                side.state = SideState::Tracking(latest.id);
                side.notice = None;
                side.conversation = latest.conversation;
                if let Some(previous) = side.previous.take() {
                    side.remember(previous);
                }
                self.asked_draft = Some(side.draft.clone());
            }
            // The same conversation went on from another view: keep this
            // exchange, and say so if some were missed.
            Some(side)
                if matches!(side.state, SideState::Tracking(id) if latest.id > id)
                    && side.conversation.is_some()
                    && latest.conversation == side.conversation =>
            {
                let SideState::Tracking(tracked) = side.state else {
                    unreachable!()
                };
                let exchange = Exchange {
                    question: std::mem::replace(&mut side.question, latest.question.clone()),
                    answer: std::mem::take(&mut side.answer),
                };
                side.remember(exchange);
                if latest.follows != Some(tracked) {
                    side.history_lost = true;
                }
                side.state = SideState::Tracking(latest.id);
                side.baseline = latest.id;
                side.answer = latest.answer.clone().unwrap_or_default();
            }
            // Another view started a different conversation: show that one,
            // never splicing in this one's history.
            Some(side)
                if matches!(side.state, SideState::Tracking(id) if latest.id > id)
                    && side.conversation.is_some()
                    && latest.conversation.is_some()
                    && latest.conversation != side.conversation =>
            {
                *side = Side::adopt(latest);
            }
            None if first => self.side = Some(Side::adopt(latest)),
            _ => {}
        }
    }

    /// The poll to send now, if one is due: one at a time, never once the
    /// server has shown it lacks the capability.
    pub fn poll(&mut self, agent: &str) -> Option<Request> {
        if self.unsupported || self.in_flight {
            return None;
        }
        self.in_flight = true;
        Some(Request::SideReport {
            id: self.id,
            agent: agent.to_string(),
        })
    }

    /// The questions waiting on the owner, if any.
    pub fn questions(&self) -> Option<QuestionSet> {
        let input = self.report.as_ref()?.pending_input.as_ref()?;
        (input.status() == proto::InputStatus::Pending && self.answered != Some(input.request_id))
            .then(|| question_set(input))
    }

    pub fn usage(&self) -> Option<ContextUsage> {
        let report = self.report.as_ref()?;
        let usage = report.usage.as_ref()?;
        let measure = match usage.source() {
            proto::context_usage::Source::CurrentContext => Measure::Current,
            proto::context_usage::Source::LastModelRequest => Measure::LastRequest,
            proto::context_usage::Source::Unspecified => return None,
        };
        Some(ContextUsage {
            measure,
            used: usage.used_tokens,
            limit: usage.limit_tokens,
            reported_at: report.reported_at.clone(),
        })
    }

    /// Open the form on the questions waiting, if there are any.
    pub fn open_form(&mut self) -> Result<(), &'static str> {
        let set = self.questions().ok_or("No questions are waiting.")?;
        self.notice = None;
        self.form = Some(Form::new(set));
        Ok(())
    }

    /// The request that sends `reply` to the form's question set.
    pub fn reply(&mut self, reply: Reply) -> Option<Request> {
        if self.sending.is_some() {
            // One reply at a time; a second Enter sends nothing more.
            return None;
        }
        let request_id = self.form.as_ref()?.request_id();
        let (action, answers) = match reply {
            Reply::Submit(answers) => (proto::AnswerAction::Submit, answers),
            Reply::Decline => (proto::AnswerAction::Decline, Vec::new()),
            Reply::Cancel => (proto::AnswerAction::Cancel, Vec::new()),
        };
        self.sending = Some(request_id);
        Some(Request::AnswerOwnerInput {
            id: self.id,
            req: proto::AnswerOwnerInputRequest {
                request_id,
                action: action.into(),
                answers: answers
                    .into_iter()
                    .map(|(question_id, response)| proto::InputAnswer {
                        question_id,
                        value: match response {
                            Response::Choice(value) | Response::Text(value) => value,
                        },
                    })
                    .collect(),
            },
        })
    }

    /// Take a reply for this session; whether anything shown changed.
    pub fn on_data(&mut self, data: &Data) -> bool {
        match data {
            Data::SideReport { id, result } if *id == self.id => {
                self.in_flight = false;
                match result {
                    Ok(report) => {
                        // The whole report, not its revision alone: a new agent
                        // under a reused name counts revisions afresh.
                        if self.report.as_ref() == Some(report.as_ref()) {
                            return false;
                        }
                        // The fence lifts once a report no longer shows the
                        // answered request waiting.
                        let waiting = report
                            .pending_input
                            .as_ref()
                            .filter(|i| i.status() == proto::InputStatus::Pending)
                            .map(|i| i.request_id);
                        if waiting != self.answered {
                            self.answered = None;
                        }
                        self.report = Some(report.as_ref().clone());
                        self.supersede();
                        self.track_side();
                    }
                    // Stop asking, and stop showing what was cached: the agent
                    // may have retired, or this owner lost it.
                    Err(SideError::Unsupported(_)) => {
                        self.unsupported = true;
                        self.form = None;
                        self.side = None;
                        self.report = None;
                        self.notice =
                            Some("Agent questions are not available on this server.".into());
                    }
                    Err(SideError::Refused(why)) => {
                        self.unsupported = true;
                        self.form = None;
                        self.side = None;
                        self.report = None;
                        self.notice = Some(why.clone());
                    }
                    // A transient failure is retried on the next tick.
                    Err(SideError::Transient(_)) => return false,
                }
                true
            }
            Data::OwnerAnswered {
                id,
                request_id,
                result,
            } if *id == self.id && self.sending == Some(*request_id) => {
                self.sending = None;
                match result {
                    Ok(()) => {
                        self.answered = Some(*request_id);
                        self.form = None;
                        self.notice = None;
                    }
                    // The form and what was typed stay, showing why in its
                    // footer; nothing is retried.
                    Err(error) => {
                        let why = match error {
                            SideError::Unsupported(why) | SideError::Refused(why) => {
                                format!("Not sent: {why}")
                            }
                            // The server may have taken it: the next poll settles it.
                            SideError::Transient(why) => {
                                format!("Could not confirm response: {why}")
                            }
                        };
                        match &mut self.form {
                            Some(form) => form.notice = Some(why),
                            None => self.notice = Some(why),
                        }
                    }
                }
                true
            }
            Data::SideAsked { id, result } if *id == self.id => {
                let Some(side) = self.side.as_mut() else {
                    return false;
                };
                if side.state != SideState::Asking {
                    return false;
                }
                side.state = match result {
                    Ok(side_id) => {
                        self.asked_draft = Some(side.draft.clone());
                        if let Some(previous) = side.previous.take() {
                            side.remember(previous);
                        }
                        // A fresh question starts its own conversation.
                        if side.follows.is_none() {
                            side.conversation = Some(*side_id);
                        }
                        side.answer.clear();
                        SideState::Tracking(*side_id)
                    }
                    // The server may have taken it: a newer report with the
                    // same question settles it; nothing is retried.
                    Err(SideError::Transient(why)) => SideState::Unconfirmed(why.clone()),
                    // Any refused follow-up ends the conversation, whatever
                    // the reason; the draft stays, fenced.
                    Err(_) if side.follows.is_some() => {
                        if let Some(previous) = side.previous.take() {
                            side.remember(previous);
                        }
                        SideState::Ended
                    }
                    Err(SideError::Unsupported(why) | SideError::Refused(why)) => {
                        SideState::NotAsked(why.clone())
                    }
                };
                self.track_side_unconfirmed();
                true
            }
            Data::SideCancelled { id, side, result } if *id == self.id => {
                let Some(panel) = self.side.as_mut() else {
                    return false;
                };
                if panel.state != SideState::Tracking(*side) {
                    return false;
                }
                panel.cancelling = false;
                // A cancel taken shows once a report does.
                panel.notice = result.as_ref().err().map(|e| match e {
                    SideError::Unsupported(why)
                    | SideError::Refused(why)
                    | SideError::Transient(why) => format!("Could not cancel: {why}"),
                });
                true
            }
            _ => false,
        }
    }

    /// An ask whose outcome is unknown may already show in the report.
    fn track_side_unconfirmed(&mut self) {
        if self
            .side
            .as_ref()
            .is_some_and(|s| matches!(s.state, SideState::Unconfirmed(_)))
        {
            self.track_side();
        }
    }

    /// A form whose questions were answered elsewhere, withdrawn or replaced
    /// closes: a reply to them could no longer apply.
    fn supersede(&mut self) {
        let Some(form) = &self.form else { return };
        let current = self.questions().map(|q| q.request_id);
        if current != Some(form.request_id()) {
            self.form = None;
            self.sending = None;
            self.notice = Some(match current {
                Some(_) => "Those questions were replaced; /answer shows the new ones.".into(),
                None => "Those questions are no longer waiting.".into(),
            });
        }
    }
}

fn question_set(input: &proto::OwnerInput) -> QuestionSet {
    QuestionSet {
        request_id: input.request_id,
        blocking: input.is_blocking,
        message: input.message.clone().filter(|m| !m.trim().is_empty()),
        questions: input
            .questions
            .iter()
            .map(|q| Question {
                id: q.id.clone(),
                header: q.header.clone(),
                text: q.text.clone(),
                choices: q
                    .options
                    .iter()
                    .map(|o| Choice {
                        label: o.label.clone().unwrap_or_else(|| o.value.clone()),
                        description: o.description.clone().unwrap_or_default(),
                        value: o.value.clone(),
                    })
                    .collect(),
                // A question without options is free text only.
                allow_free_text: q.allow_free_text || q.options.is_empty(),
                required: q.required,
            })
            .collect(),
    }
}

/// What a draft means for `/btw`.
#[derive(Debug, PartialEq)]
pub enum SideDraft<'a> {
    /// `/btw <question>`: a side question, never sent as mail.
    Ask(&'a str),
    /// `//btw …`: mail that starts with a literal `/btw`.
    Literal(String),
    /// Anything else: ordinary mail.
    Mail,
}

/// Read a draft for `/btw`: its first word, after leading space, decides.
pub fn side_draft(draft: &str) -> SideDraft<'_> {
    let text = draft.trim_start();
    let word = |prefix: &str| {
        text.strip_prefix(prefix)
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    };
    if word("//btw").is_some() {
        return SideDraft::Literal(text[1..].to_string());
    }
    match word("/btw") {
        Some(question) => SideDraft::Ask(question.trim()),
        None => SideDraft::Mail,
    }
}

/// The card above the composer while a question set is open.
pub fn card(agent: &str, set: &QuestionSet, width: u16) -> Vec<Line<'static>> {
    let n = set.questions.len();
    let mut lines = vec![theme::clip_line(
        Line::from(vec![
            Span::styled(agent.to_string(), theme::style(theme::Role::Agent)),
            Span::styled(
                format!(
                    "{} {n} question{}{} · /answer to respond",
                    if set.blocking { " asks" } else { " has" },
                    if n == 1 { "" } else { "s" },
                    if set.blocking { "" } else { " available" },
                ),
                theme::heading(),
            ),
        ]),
        usize::from(width),
    )];
    if let Some(message) = &set.message {
        lines.push(theme::clip_line(
            Line::styled(
                format!("  {}", one_line(message)),
                theme::style(theme::Role::Text),
            ),
            usize::from(width),
        ));
    }
    for q in &set.questions {
        let text = one_line(&format!("{} {}", q.header, q.text));
        lines.push(theme::clip_line(
            Line::styled(format!("  {text}"), theme::style(theme::Role::Text)),
            usize::from(width),
        ));
    }
    lines
}

/// The `/context` panel's lines.
pub fn context_lines(agent: &str, usage: Option<&ContextUsage>) -> Vec<Line<'static>> {
    let title = Line::from(vec![
        Span::styled("Context · ", theme::heading()),
        Span::styled(agent.to_string(), theme::style(theme::Role::Agent)),
    ]);
    let body = match usage.and_then(|u| u.used.map(|used| (u, used))) {
        None => Line::styled("Context size is not reported.", theme::dim()),
        Some((u, used)) => {
            let label = match u.measure {
                Measure::Current => "Current context",
                Measure::LastRequest => "Last request",
            };
            let amount = match u.limit.filter(|&limit| limit > 0) {
                // Wide arithmetic: any u64 report, and over 100% as reported.
                Some(limit) => format!(
                    "{} of {} tokens ({}%)",
                    tokens(used),
                    tokens(limit),
                    u128::from(used) * 100 / u128::from(limit)
                ),
                None => format!("{} tokens", tokens(used)),
            };
            Line::from(vec![
                Span::styled(
                    format!("{label}: {amount}"),
                    theme::style(theme::Role::Text),
                ),
                Span::styled(
                    format!(" · reported {}", crate::ui::clock(&u.reported_at, true)),
                    theme::dim(),
                ),
            ])
        }
    };
    vec![title, body, Line::styled("Esc closes", theme::dim())]
}

/// The `/btw` panel's rows: a header, the question, the answer and a
/// footer, at most `rows`.
pub fn side_height(agent: &str, side: &Side, view: &SideView, width: u16, rows: u16) -> u16 {
    let answer = Paragraph::new(side_text(agent, side, view))
        .wrap(Wrap { trim: false })
        .line_count(width);
    u16::try_from(answer.saturating_add(3))
        .unwrap_or(u16::MAX)
        .min(rows)
}

/// Draw the `/btw` panel: the answer shows its end, less what was scrolled
/// up.
/// With `follow_up`, a follow-up can be asked now.
pub fn render_side(
    frame: &mut Frame,
    area: Rect,
    agent: &str,
    side: &Side,
    view: &SideView,
    follow_up: bool,
    hidden: usize,
) {
    let width = usize::from(area.width);
    let (label, role) = match view {
        SideView::Asking => ("asking…", theme::Role::Muted),
        SideView::Asked => ("asked", theme::Role::Muted),
        SideView::Open(_) => ("answering…", theme::Role::Accent),
        SideView::Completed(_) => ("answered", theme::Role::Success),
        SideView::Failed(_) => ("failed", theme::Role::Error),
        SideView::Cancelled(_) => ("cancelled", theme::Role::Warning),
        SideView::Interrupted(_) => ("interrupted", theme::Role::Warning),
        SideView::Unconfirmed(_) => ("not confirmed", theme::Role::Warning),
        SideView::NotAsked(_) => ("not asked", theme::Role::Error),
        SideView::Replaced => ("replaced", theme::Role::Muted),
        SideView::Ended(_) => ("ended", theme::Role::Warning),
    };
    let header = theme::clip_line(
        Line::from(vec![
            Span::styled("btw · ", theme::heading()),
            Span::styled(agent.to_string(), theme::style(theme::Role::Agent)),
            Span::styled(" · ", theme::heading()),
            Span::styled(label, theme::style(role)),
            Span::styled(
                match hidden {
                    0 => String::new(),
                    1 => format!(" · 1 new message in {agent}"),
                    n => format!(" · {n} new messages in {agent}"),
                },
                theme::dim(),
            ),
        ]),
        width,
    );
    let question = theme::clip_line(
        Line::styled(
            format!("  {}", one_line(&side.question)),
            theme::style(theme::Role::Text),
        ),
        width,
    );
    let footer = theme::clip_line(
        match &side.notice {
            Some(notice) => Line::styled(notice.clone(), theme::dim()),
            None => Line::styled(
                if view.running() {
                    "Esc cancels · Alt-↑/↓ scrolls"
                } else if follow_up {
                    "Enter asks a follow-up · Esc ends · Alt-↑/↓ scrolls"
                } else {
                    "Esc closes · Alt-↑/↓ scrolls"
                },
                theme::dim(),
            ),
        },
        width,
    );
    let [top, middle, answer_area, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header), top);
    frame.render_widget(Paragraph::new(question), middle);
    frame.render_widget(Paragraph::new(footer), bottom);
    let answer = Paragraph::new(side_text(agent, side, view)).wrap(Wrap { trim: false });
    let total = answer.line_count(area.width);
    let shown = usize::from(answer_area.height);
    let scroll = side.scroll.get().min(total.saturating_sub(shown));
    side.scroll.set(scroll);
    let top_row = total.saturating_sub(shown).saturating_sub(scroll);
    frame.render_widget(
        answer.scroll((u16::try_from(top_row).unwrap_or(u16::MAX), 0)),
        answer_area,
    );
}

/// What the panel says below the question.
fn side_body<'a>(agent: &str, view: &'a SideView) -> ratatui::text::Text<'a> {
    use ratatui::text::Text;
    let note = |text: String, style| Text::from(Line::styled(text, style));
    let answer = |text: &'a str, empty: String| {
        if text.trim().is_empty() {
            note(empty, theme::dim())
        } else {
            crate::ui::markdown::render(text)
        }
    };
    match view {
        SideView::Asking => note("Asking…".into(), theme::dim()),
        SideView::Asked => note(format!("Waiting for {agent}'s runner…"), theme::dim()),
        SideView::Open(text) => answer(text, format!("Waiting for {agent}'s runner…")),
        SideView::Completed(text) => answer(text, "No answer text.".into()),
        SideView::Cancelled(text) => answer(text, "Cancelled before an answer.".into()),
        SideView::Interrupted(text) => answer(text, "Interrupted before an answer.".into()),
        SideView::Failed(why) => note(
            if why.trim().is_empty() {
                "The runner could not answer.".into()
            } else {
                format!("Failed: {why}")
            },
            theme::error(),
        ),
        SideView::Unconfirmed(why) => note(
            format!(
                "Could not confirm the question was asked: {why}. \
                 Nothing was retried; the next report shows it if it was."
            ),
            theme::style(theme::Role::Warning),
        ),
        SideView::NotAsked(why) => note(format!("Not asked: {why}"), theme::error()),
        SideView::Replaced => note(
            "A newer side question took this one's place.".into(),
            theme::dim(),
        ),
        SideView::Ended(text) => {
            let mut body = if text.trim().is_empty() {
                Text::default()
            } else {
                let mut body = crate::ui::markdown::render(text);
                body.lines.push(Line::default());
                body
            };
            body.lines.push(Line::styled(
                "Side conversation ended; your next /btw starts fresh.",
                theme::style(theme::Role::Warning),
            ));
            body
        }
    }
}

/// The side conversation's earlier exchanges kept in this session, then what
/// the panel says below the question.
fn side_text<'a>(agent: &str, side: &'a Side, view: &'a SideView) -> ratatui::text::Text<'a> {
    use ratatui::text::Text;
    let mut text = Text::default();
    if side.history_lost {
        text.lines.push(Line::styled(
            format!("Earlier side questions aren't shown here; {agent} still has them in this side conversation."),
            theme::dim(),
        ));
        text.lines.push(Line::default());
    }
    for exchange in &side.history {
        text.lines.push(Line::styled(
            format!("› {}", one_line(&exchange.question)),
            theme::heading(),
        ));
        text.lines
            .extend(crate::ui::markdown::render(&exchange.answer).lines);
        text.lines.push(Line::default());
    }
    text.lines.extend(side_body(agent, view).lines);
    text
}

/// A token count in a few columns: 950, 84k, 1.2M.
fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Text on one line: line breaks and tabs become spaces.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// A place the form's focus can rest.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Item {
    Choice(usize, usize),
    Text(usize),
    Submit,
    Decline,
    Cancel,
}

/// What a key did to the form.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Open,
    /// Closed without replying; the questions stay open.
    Close,
    Reply(Reply),
}

/// The `/answer` form: nothing chosen until the owner chooses it, and only
/// the actions reply.
pub struct Form {
    set: QuestionSet,
    chosen: Vec<Option<usize>>,
    text: Vec<String>,
    focus: usize,
    notice: Option<String>,
    /// The first body row shown, as last drawn; moving the focus scrolls it
    /// just enough, and PageUp/PageDown move it to read freely.
    scroll: Cell<usize>,
    /// Body rows shown, as last drawn: a page.
    page: Cell<usize>,
    /// Reading rather than following the focus: from the top when the form
    /// opens, or where PageUp/PageDown left it. The next other key ends it.
    reading: Cell<bool>,
}

/// The longest free-text answer the server takes, in bytes.
const FREE_TEXT_LIMIT: usize = 4096;

impl Form {
    pub fn new(set: QuestionSet) -> Form {
        let n = set.questions.len();
        Form {
            set,
            chosen: vec![None; n],
            text: vec![String::new(); n],
            focus: 0,
            notice: None,
            scroll: Cell::new(0),
            page: Cell::new(1),
            reading: Cell::new(true),
        }
    }

    /// The question set this form answers.
    pub fn request_id(&self) -> i64 {
        self.set.request_id
    }

    fn items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        for (q, question) in self.set.questions.iter().enumerate() {
            items.extend((0..question.choices.len()).map(|c| Item::Choice(q, c)));
            if question.allow_free_text {
                items.push(Item::Text(q));
            }
        }
        items.extend([Item::Submit, Item::Decline, Item::Cancel]);
        items
    }

    fn answered(&self, q: usize) -> bool {
        self.chosen[q].is_some() || !self.text[q].trim().is_empty()
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let items = self.items();
        let item = items[self.focus.min(items.len() - 1)];
        let reading = self.reading.get();
        if !matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            self.reading.set(false);
        }
        // While reading, the focus may be out of view: the first Enter or
        // Space only brings it back, never chooses or replies unseen.
        if reading
            && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
            && !matches!(item, Item::Text(_))
        {
            return Outcome::Open;
        }
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::PageUp | KeyCode::PageDown => {
                let page = self.page.get().max(1);
                let scroll = self.scroll.get();
                self.scroll.set(if key.code == KeyCode::PageUp {
                    scroll.saturating_sub(page)
                } else {
                    scroll + page
                });
                self.reading.set(true);
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.focus = (self.focus + items.len() - 1) % items.len();
            }
            KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1) % items.len(),
            KeyCode::Enter | KeyCode::Char(' ') if !matches!(item, Item::Text(_)) => {
                return self.act(item, &items);
            }
            KeyCode::Enter => self.focus = (self.focus + 1) % items.len(),
            KeyCode::Backspace => {
                if let Item::Text(q) = item {
                    self.text[q].pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Item::Text(q) = item {
                    self.type_text(q, &c.to_string());
                }
            }
            _ => {}
        }
        Outcome::Open
    }

    /// Paste into the free-text field in focus, on one line.
    pub fn paste(&mut self, text: &str) {
        let items = self.items();
        if let Some(&Item::Text(q)) = items.get(self.focus) {
            self.type_text(q, &one_line(text));
        }
    }

    /// Add to question `q`'s free text, whole characters only, up to the
    /// server's limit; typed text replaces a chosen option.
    fn type_text(&mut self, q: usize, text: &str) {
        for c in text.chars() {
            if self.text[q].len() + c.len_utf8() > FREE_TEXT_LIMIT {
                self.notice = Some("Free text is limited to 4 KiB.".into());
                break;
            }
            self.text[q].push(c);
        }
        self.chosen[q] = None;
    }

    fn act(&mut self, item: Item, items: &[Item]) -> Outcome {
        match item {
            // Choosing the chosen option again clears it back to unanswered.
            Item::Choice(q, c) => {
                self.chosen[q] = (self.chosen[q] != Some(c)).then_some(c);
                self.text[q].clear();
                Outcome::Open
            }
            Item::Text(_) => Outcome::Open,
            Item::Submit => {
                let missing = (0..self.set.questions.len())
                    .find(|&q| self.set.questions[q].required && !self.answered(q));
                if let Some(q) = missing {
                    self.notice = Some("Answer the required questions first.".into());
                    self.focus = items
                        .iter()
                        .position(|i| matches!(i, Item::Choice(iq, _) | Item::Text(iq) if *iq == q))
                        .unwrap_or(self.focus);
                    return Outcome::Open;
                }
                let answers = self
                    .set
                    .questions
                    .iter()
                    .enumerate()
                    .filter_map(|(q, question)| {
                        let response = match self.chosen[q] {
                            Some(c) => Response::Choice(question.choices[c].value.clone()),
                            None if !self.text[q].trim().is_empty() => {
                                Response::Text(self.text[q].clone())
                            }
                            None => return None,
                        };
                        Some((question.id.clone(), response))
                    })
                    .collect();
                Outcome::Reply(Reply::Submit(answers))
            }
            Item::Decline => Outcome::Reply(Reply::Decline),
            Item::Cancel => Outcome::Reply(Reply::Cancel),
        }
    }

    /// The form in a box over `area`: the form's message and every question,
    /// choice and description in full, wrapped and scrolled; the actions and
    /// keys stay in view below them at any size.
    pub fn render(&self, frame: &mut Frame, area: Rect, agent: &str) {
        let width = area.width.saturating_sub(2).min(78);
        let inner = width.saturating_sub(2);
        let items = self.items();
        let focused = items.get(self.focus).copied();
        let mut body: Vec<Line<'static>> = Vec::new();
        if let Some(message) = &self.set.message {
            for line in message.lines() {
                body.push(Line::styled(
                    line.to_string(),
                    theme::style(theme::Role::Text),
                ));
            }
            body.push(Line::raw(""));
        }
        let mut focus_line = None;
        for (q, question) in self.set.questions.iter().enumerate() {
            let mark = if question.required {
                " (required)"
            } else {
                " (optional)"
            };
            body.push(Line::from(vec![
                Span::styled(one_line(&question.header), theme::heading()),
                Span::styled(mark, theme::dim()),
            ]));
            for text in question.text.lines() {
                body.push(Line::styled(
                    text.to_string(),
                    theme::style(theme::Role::Text),
                ));
            }
            for (c, choice) in question.choices.iter().enumerate() {
                let here = focused == Some(Item::Choice(q, c));
                if here {
                    focus_line = Some(body.len());
                }
                let chosen = self.chosen[q] == Some(c);
                let mut spans = vec![
                    Span::raw(if here { "› " } else { "  " }),
                    Span::raw(if chosen { "(•) " } else { "( ) " }),
                    Span::styled(one_line(&choice.label), theme::style(theme::Role::Text)),
                ];
                if !choice.description.is_empty() {
                    spans.push(Span::styled(
                        format!(" — {}", one_line(&choice.description)),
                        theme::dim(),
                    ));
                }
                let mut line = Line::from(spans);
                if here {
                    line = line.style(theme::style(theme::Role::Selection));
                }
                body.push(line);
            }
            if question.allow_free_text {
                let here = focused == Some(Item::Text(q));
                if here {
                    focus_line = Some(body.len());
                }
                let mut value = Span::styled(self.text[q].clone(), theme::style(theme::Role::Text));
                if here {
                    value = value.add_modifier(Modifier::UNDERLINED);
                }
                let mut spans = vec![
                    Span::raw(if here { "› " } else { "  " }),
                    Span::styled("Other: ", theme::dim()),
                    value,
                ];
                if here {
                    spans.push(Span::styled("█", theme::dim()));
                }
                body.push(Line::from(spans));
            }
            body.push(Line::raw(""));
        }
        let actions: Vec<Span> = [
            (Item::Submit, "Submit"),
            (Item::Decline, "Decline"),
            (Item::Cancel, "Cancel request"),
        ]
        .into_iter()
        .flat_map(|(item, label)| {
            let style = if focused == Some(item) {
                theme::style(theme::Role::Selection)
            } else {
                theme::style(theme::Role::Hotkey)
            };
            [Span::styled(format!("[{label}]"), style), Span::raw(" ")]
        })
        .collect();
        let mut footer = vec![Line::from(actions)];
        // One line at most, so a long error never hides the body or actions.
        if let Some(notice) = &self.notice {
            footer.push(theme::clip_line(
                Line::styled(one_line(notice), theme::error()),
                usize::from(inner),
            ));
        }
        footer.push(Line::styled(
            "↑/↓ move · Enter or Space choose · PgUp/PgDn scroll · Esc closes, questions stay open",
            theme::dim(),
        ));
        let rows = |line: &Line<'static>| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(inner)
        };
        let heights: Vec<usize> = body.iter().map(rows).collect();
        let total: usize = heights.iter().sum();
        let footer_rows: usize = footer.iter().map(rows).sum();
        let room = usize::from(area.height.saturating_sub(2)).max(1);
        let body_room = room.saturating_sub(footer_rows).max(1);
        // Scroll just enough to show the whole focused item, its typed tail
        // included, unless PageUp/PageDown is reading elsewhere.
        let mut scroll = self.scroll.get();
        if !self.reading.get()
            && let Some(line) = focus_line
        {
            let start: usize = heights[..line].iter().sum();
            let end = start + heights[line];
            if end - start > body_room && matches!(focused, Some(Item::Text(_))) {
                // Typing goes on at the end of a long answer: keep it, and
                // the caret, in view.
                scroll = end - body_room;
            } else {
                if end > scroll + body_room {
                    scroll = end - body_room;
                }
                // A long choice begins at its label; PgDn reads the rest.
                if start < scroll {
                    scroll = start;
                }
            }
        }
        let scroll = scroll.min(total.saturating_sub(body_room));
        self.scroll.set(scroll);
        self.page.set(body_room.saturating_sub(1).max(1));
        let shown_body = total.min(body_room);
        let height = ((shown_body + footer_rows) as u16 + 2).min(area.height);
        let [row] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .areas(area);
        let [rect] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::Center)
            .areas(row);
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(theme::style(theme::Role::Hotkey))
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled(agent.to_string(), theme::style(theme::Role::Agent)),
                Span::styled(" asks ", theme::heading()),
            ]))
            .style(theme::base());
        let inside = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        let [body_area, footer_area] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(footer_rows.min(usize::from(u16::MAX)) as u16),
        ])
        .areas(inside);
        frame.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .scroll((scroll.min(usize::from(u16::MAX)) as u16, 0)),
            body_area,
        );
        frame.render_widget(
            Paragraph::new(footer).wrap(Wrap { trim: false }),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> QuestionSet {
        let choice = |v: &str| Choice {
            label: v.to_uppercase(),
            description: format!("{v} it"),
            value: v.into(),
        };
        QuestionSet {
            request_id: 7,
            blocking: true,
            message: None,
            questions: vec![
                Question {
                    id: "deploy".into(),
                    header: "Deploy".into(),
                    text: "Ship to staging?".into(),
                    choices: vec![choice("yes"), choice("no")],
                    allow_free_text: false,
                    required: true,
                },
                Question {
                    id: "note".into(),
                    header: "Note".into(),
                    text: "Anything else?".into(),
                    choices: vec![],
                    allow_free_text: true,
                    required: false,
                },
            ],
        }
    }

    fn press(form: &mut Form, code: KeyCode) -> Outcome {
        form.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Move the focus to the action labelled `n` steps from the first item.
    fn to(form: &mut Form, steps: usize) {
        form.focus = 0;
        // A focus move ends the opening read, as any arrow key would.
        form.reading.set(false);
        for _ in 0..steps {
            press(form, KeyCode::Down);
        }
    }

    // Items: yes, no, note text, Submit, Decline, Cancel.
    const SUBMIT: usize = 3;
    const DECLINE: usize = 4;
    const CANCEL: usize = 5;

    #[test]
    fn nothing_is_chosen_until_chosen_and_submit_needs_every_required_answer() {
        let mut form = Form::new(set());
        // Submitting at once answers nothing: the required question is due.
        to(&mut form, SUBMIT);
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Open);
        assert_eq!(form.focus, 0, "focus moves to the unanswered question");
        assert!(form.notice.is_some());
        // Choosing "no" by Enter only chooses; the optional note may stay blank.
        to(&mut form, 1);
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Open);
        to(&mut form, SUBMIT);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Outcome::Reply(Reply::Submit(vec![(
                "deploy".into(),
                Response::Choice("no".into())
            )]))
        );
    }

    #[test]
    fn free_text_takes_typing_and_enter_there_never_submits() {
        let mut form = Form::new(set());
        to(&mut form, 0);
        press(&mut form, KeyCode::Char(' '));
        to(&mut form, 2);
        for c in "be quick".chars() {
            assert_eq!(press(&mut form, KeyCode::Char(c)), Outcome::Open);
        }
        form.paste("\nplease");
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Open);
        assert_eq!(form.focus, SUBMIT, "Enter in text moves on");
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Outcome::Reply(Reply::Submit(vec![
                ("deploy".into(), Response::Choice("yes".into())),
                ("note".into(), Response::Text("be quick please".into())),
            ]))
        );
    }

    #[test]
    fn a_choice_out_of_view_while_reading_is_brought_back_not_chosen() {
        let mut form = Form::new(set());
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Open);
        assert_eq!(form.chosen[0], None, "the first Enter only shows the focus");
        assert_eq!(press(&mut form, KeyCode::Enter), Outcome::Open);
        assert_eq!(form.chosen[0], Some(0));
    }

    #[test]
    fn esc_closes_unchanged_and_decline_and_cancel_are_replies() {
        let mut form = Form::new(set());
        assert_eq!(press(&mut form, KeyCode::Esc), Outcome::Close);
        to(&mut form, DECLINE);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Outcome::Reply(Reply::Decline)
        );
        to(&mut form, CANCEL);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Outcome::Reply(Reply::Cancel)
        );
        // Focus wraps both ways.
        to(&mut form, 0);
        press(&mut form, KeyCode::Up);
        assert_eq!(form.focus, CANCEL);
    }

    #[test]
    fn context_shows_what_it_measures_and_hides_the_unknown() {
        let text = |usage: Option<&ContextUsage>| -> String {
            context_lines("Mateo", usage)[1].to_string()
        };
        let usage = |measure, used, limit| ContextUsage {
            measure,
            used,
            limit,
            reported_at: "2026-10-20T02:30:00Z".into(),
        };
        let current = text(Some(&usage(Measure::Current, Some(84_321), Some(200_000))));
        assert!(
            current.starts_with("Current context: 84k of 200k tokens (42%)"),
            "{current}"
        );
        let last = text(Some(&usage(Measure::LastRequest, Some(1_250_000), None)));
        assert!(last.starts_with("Last request: 1.2M tokens ·"), "{last}");
        assert!(!last.contains('%'), "no limit, no percentage: {last}");
        let zero_limit = text(Some(&usage(Measure::Current, Some(950), Some(0))));
        assert!(
            zero_limit.starts_with("Current context: 950 tokens"),
            "{zero_limit}"
        );
        for unknown in [None, Some(&usage(Measure::Current, None, Some(200_000)))] {
            assert_eq!(text(unknown), "Context size is not reported.");
        }
    }

    #[test]
    fn percentages_use_wide_arithmetic_and_are_never_clamped() {
        let line = |used, limit| {
            context_lines(
                "Mateo",
                Some(&ContextUsage {
                    measure: Measure::Current,
                    used: Some(used),
                    limit: Some(limit),
                    reported_at: "2026-10-20T02:30:00Z".into(),
                }),
            )[1]
            .to_string()
        };
        assert!(line(u64::MAX, u64::MAX).contains("(100%)"));
        assert!(line(u64::MAX, 1).contains(&format!("({}%)", u128::from(u64::MAX) * 100)));
        assert!(
            line(300, 200).contains("(150%)"),
            "over the limit as reported"
        );
    }

    #[test]
    fn choosing_the_chosen_option_again_clears_an_optional_answer() {
        let mut form = Form::new(set());
        form.set.questions[0].required = false;
        to(&mut form, 0);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.chosen[0], Some(0));
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(form.chosen[0], None, "back to unanswered");
        to(&mut form, SUBMIT);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Outcome::Reply(Reply::Submit(vec![]))
        );
    }

    #[test]
    fn free_text_stops_at_four_kib_on_a_character_boundary() {
        let mut form = Form::new(set());
        to(&mut form, 2);
        form.paste(&"é".repeat(3000));
        assert!(form.text[1].len() <= FREE_TEXT_LIMIT);
        assert_eq!(form.text[1].len(), 4096, "2048 two-byte characters fit");
        assert!(form.notice.is_some());
        let mut form = Form::new(set());
        to(&mut form, 2);
        form.paste(&format!("{}👩‍💻", "a".repeat(4094)));
        assert_eq!(
            form.text[1],
            "a".repeat(4094),
            "a wide character never splits"
        );
        press(&mut form, KeyCode::Char('b'));
        press(&mut form, KeyCode::Char('c'));
        press(&mut form, KeyCode::Char('d'));
        assert_eq!(form.text[1].len(), 4096);
    }

    #[test]
    fn side_history_keeps_the_newest_exchanges_within_its_bounds() {
        let mut side = Side::new("", "q", 0, SideState::Asking);
        for n in 0..25 {
            side.remember(Exchange {
                question: format!("q{n}"),
                answer: "a".into(),
            });
        }
        assert_eq!(side.history.len(), 20);
        assert_eq!(side.history[0].question, "q5", "the oldest go first");
        assert!(
            side.history_lost,
            "the panel says earlier ones are not shown"
        );
        let mut big = Side::new("", "q", 0, SideState::Asking);
        for n in 0..3 {
            big.remember(Exchange {
                question: format!("q{n}"),
                answer: "x".repeat(100 * 1024),
            });
        }
        assert_eq!(big.history.len(), 2, "within 256 KiB");
        assert!(big.history_lost);
        // One exchange is always kept, however large.
        let mut one = Side::new("", "q", 0, SideState::Asking);
        one.remember(Exchange {
            question: "q".into(),
            answer: "x".repeat(300 * 1024),
        });
        assert_eq!(one.history.len(), 1);
    }

    #[test]
    fn btw_is_a_first_word_and_a_doubled_slash_sends_it_literally() {
        assert_eq!(side_draft("/btw what is X?"), SideDraft::Ask("what is X?"));
        assert_eq!(
            side_draft("  /btw\nline one\nline two "),
            SideDraft::Ask("line one\nline two")
        );
        assert_eq!(side_draft("/btw"), SideDraft::Ask(""));
        assert_eq!(
            side_draft("//btw is a command"),
            SideDraft::Literal("/btw is a command".into())
        );
        for mail in ["/btwx no", "btw /btw", "say /btw", "/b tw", "///btw", ""] {
            assert_eq!(side_draft(mail), SideDraft::Mail, "{mail:?}");
        }
    }

    #[test]
    fn the_card_names_the_agent_and_fits_each_question_on_a_line() {
        let mut long = set();
        long.questions[0].text = "a\nvery long question ".repeat(10);
        let lines = card("Mateo", &long, 40);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().starts_with("Mateo asks 2 questions"));
        assert!(
            lines
                .iter()
                .all(|l| l.width() <= 40 && !l.to_string().contains('\n'))
        );
    }
}
