//! The private note a human keeps about an agent: a full-screen plain-text
//! editor that saves only when asked, with `Ctrl-s` or on leaving with `Esc`.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::composer::Composer;
use super::drafts::Draft;
use super::theme::{self, Role};

/// The most a note may hold, in UTF-8 bytes.
pub const MAX_NOTE_BYTES: usize = 32 * 1024;

/// Whether the note can be edited yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The stored notes are still arriving; a blank here could overwrite one.
    Loading,
    /// The server has no notes.
    Unsupported,
    Editable,
    /// The agent retired: the note is kept but no longer written.
    Retired,
}

/// What the app should do after the editor took a key or a save result.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    /// Store `body` as the note, answered by `saved(id, ..)`.
    Save {
        id: u64,
        body: String,
    },
    Close,
}

pub struct NoteEditor {
    agent_id: i64,
    agent: String,
    text: Composer,
    access: Access,
    /// The note as last stored.
    saved: String,
    /// A save's id and submitted text; one at a time.
    saving: Option<(u64, String)>,
    /// `Esc` asked to close once the text is stored.
    closing: bool,
    /// Why the last save failed, or why an edit was refused.
    error: Option<String>,
}

impl NoteEditor {
    /// An editor for the agent `agent_id`, named `agent`, that waits for the
    /// stored note before it can be edited.
    pub fn new(agent_id: i64, agent: &str) -> NoteEditor {
        NoteEditor {
            agent_id,
            agent: agent.to_string(),
            text: Composer::new(agent),
            access: Access::Loading,
            saved: String::new(),
            saving: None,
            closing: false,
            error: None,
        }
    }

    pub fn agent_id(&self) -> i64 {
        self.agent_id
    }

    pub fn access(&self) -> Access {
        self.access
    }

    pub fn draft(&self) -> Draft {
        self.text.draft()
    }

    /// Whether the text differs from the stored note.
    pub fn dirty(&self) -> bool {
        self.text.text() != self.saved
    }

    pub fn saving(&self) -> bool {
        self.saving.is_some()
    }

    /// The stored note has arrived (`None`: there is none yet), with the
    /// cursor on `line`.
    pub fn loaded(&mut self, body: Option<&str>, line: usize) {
        if self.access != Access::Loading {
            return;
        }
        let body = body.unwrap_or_default();
        self.saved = body.to_string();
        self.text.restore(Draft {
            text: body.to_string(),
            cursor: (line, 0),
        });
        self.access = Access::Editable;
    }

    /// The server keeps no notes; nothing can be edited.
    pub fn unsupported(&mut self) {
        if self.access == Access::Loading {
            self.access = Access::Unsupported;
        }
    }

    /// A newer stored note, written here or elsewhere. Unsaved edits win; a
    /// clean editor shows the new text.
    pub fn stored(&mut self, body: &str, retired: bool) {
        if self.access != Access::Editable {
            return;
        }
        if retired {
            self.access = Access::Retired;
            self.closing = false;
            return;
        }
        if !self.dirty() && self.saving.is_none() {
            let cursor = self.text.draft().cursor;
            self.text.restore(Draft {
                text: body.to_string(),
                cursor,
            });
        }
        self.saved = body.to_string();
    }

    /// A key; `save_id` names the save it may start.
    pub fn key(&mut self, key: KeyEvent, save_id: u64) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return self.close(save_id),
            KeyCode::Char('s') if ctrl => return self.save(save_id),
            KeyCode::Enter if key.modifiers.is_empty() => {
                // A new line: Enter never sends a note.
                self.edit(|text| {
                    text.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), 0);
                });
            }
            _ => self.edit(|text| {
                text.key(key, 0);
            }),
        }
        Outcome::Stay
    }

    /// A bracketed paste, refused whole if it would not fit.
    pub fn paste(&mut self, text: &str) {
        self.edit(|composer| composer.paste(text));
    }

    /// Apply `change`, then take it back if the note may not change or
    /// would grow past its limit.
    fn edit(&mut self, change: impl FnOnce(&mut Composer)) {
        let before = self.text.text();
        change(&mut self.text);
        let after = self.text.text();
        if after == before {
            return;
        }
        let refused = match self.access {
            Access::Editable if after.len() > MAX_NOTE_BYTES => Some(format!(
                "a note holds at most {MAX_NOTE_BYTES} bytes; that would make {}",
                after.len()
            )),
            Access::Editable => None,
            Access::Loading => Some("the note is still loading".to_string()),
            Access::Unsupported => Some("this server keeps no notes".to_string()),
            Access::Retired => Some("the agent retired; its note is read-only".to_string()),
        };
        if let Some(why) = refused {
            self.text.undo();
            self.error = Some(why);
        } else {
            self.error = None;
        }
    }

    /// `Ctrl-s`: store a changed note, one save at a time.
    fn save(&mut self, save_id: u64) -> Outcome {
        if self.access != Access::Editable || self.saving.is_some() || !self.dirty() {
            return Outcome::Stay;
        }
        let body = self.text.text();
        self.saving = Some((save_id, body.clone()));
        self.error = None;
        Outcome::Save { id: save_id, body }
    }

    /// `Esc`: close, storing a changed note first. A failed save keeps the
    /// editor open; nothing is ever dropped unasked.
    fn close(&mut self, save_id: u64) -> Outcome {
        if self.access != Access::Editable || (!self.dirty() && self.saving.is_none()) {
            return Outcome::Close;
        }
        self.closing = true;
        if self.saving.is_some() {
            return Outcome::Stay;
        }
        self.save(save_id)
    }

    /// A save's result; `next_id` names a follow-up save of later edits
    /// when closing.
    pub fn saved(&mut self, id: u64, result: Result<String, String>, next_id: u64) -> Outcome {
        if self.saving.as_ref().map(|(pending, _)| *pending) != Some(id) {
            return Outcome::Stay;
        }
        self.saving = None;
        match result {
            Ok(body) => {
                self.saved = body;
                if !self.closing {
                    Outcome::Stay
                } else if self.dirty() {
                    // Typed while it saved: store the newest text, then close.
                    self.save(next_id)
                } else {
                    Outcome::Close
                }
            }
            Err(why) => {
                self.closing = false;
                self.error = Some(why);
                Outcome::Stay
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let [title, body, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(Block::new().style(theme::base()), area);
        frame.render_widget(
            Paragraph::new(theme::title_bar(Line::raw(format!(
                "Notes · {}",
                self.agent
            )))),
            title,
        );
        let editable = self.access == Access::Editable;
        self.text.render(frame, body, editable);
        let bytes = self.text.text().len();
        let (state, role) = match (&self.error, self.access) {
            (Some(why), _) => (why.clone(), Role::Error),
            (None, Access::Loading) => ("loading the note…".into(), Role::Muted),
            (None, Access::Unsupported) => ("notes need a newer server".into(), Role::Muted),
            (None, Access::Retired) => (
                "the agent retired; its note is read-only".into(),
                Role::Muted,
            ),
            (None, Access::Editable) if self.saving.is_some() => ("saving…".into(), Role::Muted),
            (None, Access::Editable) if self.dirty() => ("unsaved".into(), Role::Muted),
            (None, Access::Editable) => ("saved".into(), Role::Muted),
        };
        let keys = if editable {
            format!("{bytes}/{MAX_NOTE_BYTES} · Ctrl-S save · Esc save and close")
        } else {
            "Esc close".to_string()
        };
        let room = usize::from(status.width).saturating_sub(keys.width() + 1);
        let mut line = theme::clip_line(Line::from(Span::styled(state, theme::style(role))), room);
        let gap = usize::from(status.width).saturating_sub(line.width() + keys.width());
        line.spans.push(Span::raw(" ".repeat(gap)));
        line.spans.push(Span::styled(keys, theme::dim()));
        frame.render_widget(
            Paragraph::new(theme::clip_line(line, usize::from(status.width)))
                .style(theme::style(Role::StatusBar)),
            status,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut NoteEditor, code: KeyCode, modifiers: KeyModifiers, id: u64) -> Outcome {
        editor.key(KeyEvent::new(code, modifiers), id)
    }

    fn typed(editor: &mut NoteEditor, text: &str) {
        for c in text.chars() {
            assert_eq!(
                press(editor, KeyCode::Char(c), KeyModifiers::NONE, 0),
                Outcome::Stay
            );
        }
    }

    fn ctrl_s(editor: &mut NoteEditor, id: u64) -> Outcome {
        press(editor, KeyCode::Char('s'), KeyModifiers::CONTROL, id)
    }

    fn esc(editor: &mut NoteEditor, id: u64) -> Outcome {
        press(editor, KeyCode::Esc, KeyModifiers::NONE, id)
    }

    fn loaded(body: &str) -> NoteEditor {
        let mut editor = NoteEditor::new(7, "Mateo");
        editor.loaded(Some(body), 0);
        editor
    }

    fn save(id: u64, body: &str) -> Outcome {
        Outcome::Save {
            id,
            body: body.into(),
        }
    }

    #[test]
    fn nothing_is_editable_or_saved_until_the_stored_note_arrives() {
        let mut editor = NoteEditor::new(7, "Mateo");
        typed(&mut editor, "x");
        editor.paste("pasted");
        assert_eq!(editor.draft().text, "");
        assert_eq!(ctrl_s(&mut editor, 1), Outcome::Stay);
        assert_eq!(esc(&mut editor, 1), Outcome::Close, "nothing to keep");
        // Arriving later, the stored note is what shows, not a blank.
        let mut editor = NoteEditor::new(7, "Mateo");
        editor.loaded(Some("kept\nsecond"), 1);
        assert_eq!(editor.draft().text, "kept\nsecond");
        assert_eq!(editor.draft().cursor.0, 1);
        assert!(!editor.dirty());
        editor.loaded(Some("late"), 0);
        assert_eq!(editor.draft().text, "kept\nsecond", "loads once");
        // An old server: never editable.
        let mut editor = NoteEditor::new(7, "Mateo");
        editor.unsupported();
        editor.loaded(Some("x"), 0);
        typed(&mut editor, "y");
        assert_eq!(editor.access(), Access::Unsupported);
        assert_eq!(editor.draft().text, "");
    }

    #[test]
    fn enter_starts_a_line_and_ctrl_s_saves_only_changes() {
        let mut editor = loaded("");
        assert_eq!(ctrl_s(&mut editor, 1), Outcome::Stay, "unchanged");
        typed(&mut editor, "one");
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::NONE, 1),
            Outcome::Stay
        );
        typed(&mut editor, "two");
        assert_eq!(ctrl_s(&mut editor, 2), save(2, "one\ntwo"));
        assert_eq!(ctrl_s(&mut editor, 3), Outcome::Stay, "one save at a time");
        assert_eq!(editor.saved(2, Ok("one\ntwo".into()), 4), Outcome::Stay);
        assert!(!editor.dirty() && !editor.saving());
        assert_eq!(ctrl_s(&mut editor, 5), Outcome::Stay);
    }

    #[test]
    fn esc_closes_at_once_when_unchanged_and_after_a_save_otherwise() {
        assert_eq!(esc(&mut loaded("same"), 1), Outcome::Close);
        let mut editor = loaded("");
        typed(&mut editor, "new");
        assert_eq!(esc(&mut editor, 1), save(1, "new"));
        assert_eq!(editor.saved(1, Ok("new".into()), 2), Outcome::Close);
    }

    #[test]
    fn a_failed_save_keeps_the_editor_its_text_and_why_until_retried() {
        let mut editor = loaded("");
        typed(&mut editor, "precious");
        assert_eq!(esc(&mut editor, 1), save(1, "precious"));
        assert_eq!(editor.saved(1, Err("offline".into()), 2), Outcome::Stay);
        assert_eq!(editor.draft().text, "precious");
        assert!(editor.dirty());
        // Never a silent discard: Esc again retries, as Ctrl-S does.
        assert_eq!(esc(&mut editor, 3), save(3, "precious"));
        assert_eq!(
            editor.saved(3, Err("still offline".into()), 4),
            Outcome::Stay
        );
        assert_eq!(ctrl_s(&mut editor, 5), save(5, "precious"));
        assert_eq!(
            editor.saved(5, Ok("precious".into()), 6),
            Outcome::Stay,
            "Ctrl-S stays"
        );
    }

    #[test]
    fn edits_during_a_save_survive_its_answer_and_esc_waits_then_saves_them() {
        let mut editor = loaded("");
        typed(&mut editor, "first");
        assert_eq!(ctrl_s(&mut editor, 1), save(1, "first"));
        typed(&mut editor, " more");
        // A stale or unknown answer changes nothing.
        assert_eq!(editor.saved(9, Ok("other".into()), 10), Outcome::Stay);
        assert!(editor.saving());
        assert_eq!(esc(&mut editor, 2), Outcome::Stay, "waits for the save");
        assert_eq!(
            editor.saved(1, Ok("first".into()), 3),
            save(3, "first more")
        );
        assert_eq!(editor.draft().text, "first more");
        assert_eq!(editor.saved(3, Ok("first more".into()), 4), Outcome::Close);
        // Esc while an unchanged save is out closes once it lands.
        let mut editor = loaded("");
        typed(&mut editor, "x");
        assert_eq!(ctrl_s(&mut editor, 1), save(1, "x"));
        assert_eq!(esc(&mut editor, 2), Outcome::Stay);
        assert_eq!(editor.saved(1, Ok("x".into()), 3), Outcome::Close);
    }

    #[test]
    fn the_limit_is_bytes_and_refuses_whole_edits_and_pastes() {
        let mut editor = loaded(&"é".repeat(MAX_NOTE_BYTES / 2 - 1));
        assert_eq!(editor.draft().text.len(), MAX_NOTE_BYTES - 2);
        typed(&mut editor, "ab");
        assert_eq!(
            editor.draft().text.len(),
            MAX_NOTE_BYTES,
            "exactly the limit fits"
        );
        typed(&mut editor, "c");
        assert_eq!(
            editor.draft().text.len(),
            MAX_NOTE_BYTES,
            "one byte over is refused"
        );
        assert!(editor.error.as_deref().unwrap().contains("32768"));
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE, 0);
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE, 0);
        typed(&mut editor, "é");
        assert_eq!(
            editor.draft().text.len(),
            MAX_NOTE_BYTES,
            "a two-byte character"
        );
        let before = editor.draft().text;
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE, 0);
        editor.paste("line\nanother line");
        assert_eq!(
            editor.draft().text,
            before[..before.len() - 2],
            "the paste is refused whole"
        );
        editor.paste("é");
        assert_eq!(editor.draft().text, before);
        assert_eq!(editor.error, None);
    }

    #[test]
    fn a_retired_agents_note_is_read_only_and_keeps_unsaved_text() {
        let mut editor = loaded("stored");
        press(&mut editor, KeyCode::End, KeyModifiers::NONE, 0);
        typed(&mut editor, " unsaved");
        editor.stored("stored", true);
        assert_eq!(editor.access(), Access::Retired);
        typed(&mut editor, "!");
        assert_eq!(editor.draft().text, "stored unsaved");
        assert_eq!(ctrl_s(&mut editor, 1), Outcome::Stay);
        assert_eq!(esc(&mut editor, 1), Outcome::Close, "no refused write");
        assert_eq!(editor.draft().text, "stored unsaved");
    }

    #[test]
    fn notes_stored_elsewhere_replace_only_a_clean_editor() {
        let mut editor = loaded("old");
        editor.stored("from elsewhere", false);
        assert_eq!(editor.draft().text, "from elsewhere");
        assert!(!editor.dirty());
        press(&mut editor, KeyCode::End, KeyModifiers::NONE, 0);
        typed(&mut editor, "!");
        editor.stored("newer elsewhere", false);
        assert_eq!(editor.draft().text, "from elsewhere!", "unsaved edits win");
        assert!(editor.dirty());
        assert_eq!(ctrl_s(&mut editor, 1), save(1, "from elsewhere!"));
    }

    #[test]
    fn the_editor_fills_the_screen_with_its_state_and_keys() {
        use crate::ui::theme::testing::{SCHEMES, assert_buffer, with_scheme};
        use ratatui::{Terminal, backend::TestBackend};
        for scheme in SCHEMES {
            with_scheme(scheme, || {
                for (width, height) in [(60u16, 24u16), (80, 24)] {
                    let mut editor = NoteEditor::new(7, "Mateo");
                    let draw = |editor: &mut NoteEditor| {
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal.draw(|f| editor.render(f, f.area())).unwrap();
                        let buffer = terminal.backend().buffer().clone();
                        assert_buffer(&buffer);
                        (0..height)
                            .map(|y| {
                                (0..width)
                                    .map(|x| buffer[(x, y)].symbol())
                                    .collect::<String>()
                            })
                            .collect::<Vec<_>>()
                    };
                    let rows = draw(&mut editor);
                    assert!(rows[0].contains("Notes · Mateo"), "{rows:?}");
                    assert!(rows[usize::from(height) - 1].contains("loading the note…"));
                    assert!(rows[usize::from(height) - 1].contains("Esc close"));
                    editor.loaded(Some("remember the crew"), 0);
                    typed(&mut editor, "Do ");
                    let rows = draw(&mut editor);
                    assert!(
                        rows.iter().any(|r| r.contains("Do remember the crew")),
                        "{rows:?}"
                    );
                    let status = &rows[usize::from(height) - 1];
                    assert!(status.starts_with("unsaved"), "{status}");
                    assert!(
                        status.contains("20/32768 · Ctrl-S save · Esc save and close"),
                        "{status}"
                    );
                }
            });
        }
    }
}
