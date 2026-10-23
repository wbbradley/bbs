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

    /// Apply `change`, then put the editor back exactly as it was, history
    /// and selection included, if the note may not change or would grow
    /// past its limit.
    fn edit(&mut self, change: impl FnOnce(&mut Composer)) {
        let before = self.text.clone();
        change(&mut self.text);
        let after = self.text.text();
        if after == before.text() {
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
            self.text = before;
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

/// A stored note as the server lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredNote {
    pub agent_id: i64,
    pub agent: String,
    pub body: String,
    pub revision: u64,
    pub retired: bool,
}

/// One page of the human's notes changed after a revision.
#[derive(Debug, Default)]
pub struct Page {
    pub notes: Vec<StoredNote>,
    pub has_more: bool,
    pub next_after_revision: u64,
}

/// A page to ask for: the notes changed after revision `after`.
#[derive(Debug, PartialEq, Eq)]
pub struct Fetch {
    pub id: u64,
    pub after: u64,
}

/// The human's notes, kept in step with the server: paged in from the
/// start for each summary subscription, then again from the last page
/// whenever the summary says a newer note exists.
#[derive(Default)]
pub struct NoteStore {
    /// The summary subscription the pages belong to.
    generation: u64,
    /// Where the next page starts; only accepted pages move it.
    cursor: u64,
    /// The newest revision the summary has announced.
    announced: u64,
    /// The first full pass of this generation has finished.
    complete: bool,
    /// The last page said more follow.
    more: bool,
    fetching: Option<u64>,
    unsupported: bool,
    notes: std::collections::BTreeMap<i64, StoredNote>,
}

impl NoteStore {
    /// A new summary subscription: page everything in again.
    pub fn restart(&mut self, generation: u64, id: u64) -> Fetch {
        self.generation = generation;
        self.cursor = 0;
        self.announced = 0;
        self.complete = false;
        self.more = false;
        self.unsupported = false;
        self.fetching = Some(id);
        Fetch { id, after: 0 }
    }

    /// The summary of `generation` says notes up to `revision` exist.
    pub fn announced(&mut self, generation: u64, revision: u64, id: u64) -> Option<Fetch> {
        if generation != self.generation || self.unsupported {
            return None;
        }
        self.announced = self.announced.max(revision);
        self.next(id)
    }

    /// Ask for more while pages remain or something newer is known, one
    /// request at a time.
    fn next(&mut self, id: u64) -> Option<Fetch> {
        let caught_up = self.complete && !self.more && self.announced <= self.cursor;
        if self.fetching.is_some() || caught_up {
            return None;
        }
        self.fetching = Some(id);
        Some(Fetch {
            id,
            after: self.cursor,
        })
    }

    /// A page arrived, or failed; `id` names the next one it may need.
    pub fn page(&mut self, fetch: u64, result: Result<Page, String>, id: u64) -> Option<Fetch> {
        if self.fetching != Some(fetch) {
            return None;
        }
        self.fetching = None;
        let page = result.ok()?;
        for note in page.notes {
            self.stored(note);
        }
        self.cursor = self.cursor.max(page.next_after_revision);
        self.more = page.has_more;
        if !page.has_more {
            self.complete = true;
        }
        self.next(id)
    }

    /// The server keeps no notes.
    pub fn unsupported(&mut self, fetch: u64) {
        if self.fetching == Some(fetch) {
            self.fetching = None;
            self.unsupported = true;
        }
    }

    /// A note as stored, from a page or a save's answer. Never moves the
    /// cursor: other notes may have changed in between.
    pub fn stored(&mut self, note: StoredNote) {
        let newer = self
            .notes
            .get(&note.agent_id)
            .is_none_or(|known| note.revision >= known.revision);
        if newer {
            self.notes.insert(note.agent_id, note);
        }
    }

    /// Whether notes can be edited: every stored note is known.
    pub fn ready(&self) -> bool {
        self.complete && !self.unsupported
    }

    pub fn is_unsupported(&self) -> bool {
        self.unsupported
    }

    /// A live agent's note, if any.
    pub fn note(&self, agent_id: i64) -> Option<&StoredNote> {
        self.notes.get(&agent_id).filter(|note| !note.retired)
    }

    /// Notes to search: live and not empty.
    pub fn searchable(&self) -> impl Iterator<Item = &StoredNote> {
        self.notes
            .values()
            .filter(|note| !note.retired && !note.body.is_empty())
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
    fn undo_and_redo_cannot_change_a_read_only_note() {
        let mut editor = loaded("stored");
        press(&mut editor, KeyCode::End, KeyModifiers::NONE, 0);
        typed(&mut editor, " one");
        typed(&mut editor, " two");
        editor.stored("stored", true);
        let before = editor.draft();
        for (code, modifiers) in [
            (KeyCode::Char('_'), KeyModifiers::CONTROL),
            (KeyCode::Char('/'), KeyModifiers::CONTROL),
            (KeyCode::Char('7'), KeyModifiers::CONTROL),
            (KeyCode::Char('z'), KeyModifiers::CONTROL),
            (KeyCode::Char('r'), KeyModifiers::CONTROL),
            (KeyCode::Char('u'), KeyModifiers::CONTROL),
            (KeyCode::Backspace, KeyModifiers::NONE),
        ] {
            press(&mut editor, code, modifiers, 0);
            assert_eq!(editor.draft(), before, "{code:?}");
        }
        // Moving and selecting to copy still work.
        press(&mut editor, KeyCode::Home, KeyModifiers::NONE, 0);
        assert_eq!(editor.draft().cursor, (0, 0));
        assert_eq!(editor.draft().text, before.text);
    }

    #[test]
    fn a_refused_paste_keeps_the_undo_history() {
        let mut editor = loaded(&"x".repeat(MAX_NOTE_BYTES - 10));
        press(&mut editor, KeyCode::End, KeyModifiers::NONE, 0);
        typed(&mut editor, "ab");
        editor.paste(&"y".repeat(20));
        assert_eq!(editor.draft().text.len(), MAX_NOTE_BYTES - 8);
        press(&mut editor, KeyCode::Char('_'), KeyModifiers::CONTROL, 0);
        assert_eq!(
            editor.draft().text.len(),
            MAX_NOTE_BYTES - 9,
            "undo takes back the b"
        );
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

    fn note(agent_id: i64, body: &str, revision: u64) -> StoredNote {
        StoredNote {
            agent_id,
            agent: format!("Agent{agent_id}"),
            body: body.into(),
            revision,
            retired: false,
        }
    }

    fn page(notes: Vec<StoredNote>, has_more: bool, next: u64) -> Result<Page, String> {
        Ok(Page {
            notes,
            has_more,
            next_after_revision: next,
        })
    }

    #[test]
    fn the_store_pages_everything_in_before_it_is_ready() {
        let mut store = NoteStore::default();
        assert!(!store.ready());
        assert_eq!(store.restart(1, 10), Fetch { id: 10, after: 0 });
        assert_eq!(store.announced(1, 5, 11), None, "one fetch at a time");
        let next = store.page(10, page(vec![note(1, "a", 2)], true, 2), 12);
        assert_eq!(next, Some(Fetch { id: 12, after: 2 }));
        assert!(!store.ready(), "more pages to come");
        assert_eq!(
            store.page(12, page(vec![note(2, "b", 3)], false, 3), 13),
            Some(Fetch { id: 13, after: 3 }),
            "5 was announced"
        );
        assert!(store.ready());
        assert_eq!(
            store.page(13, page(vec![], false, 3), 14),
            Some(Fetch { id: 14, after: 3 }),
            "still short of 5"
        );
        assert_eq!(
            store.page(14, page(vec![note(1, "a2", 5)], false, 5), 15),
            None
        );
        assert_eq!(store.note(1).unwrap().body, "a2");
        // No notes at all is complete too.
        let mut empty = NoteStore::default();
        empty.restart(1, 1);
        assert_eq!(empty.page(1, page(vec![], false, 0), 2), None);
        assert!(empty.ready());
        assert_eq!(empty.announced(1, 0, 3), None);
    }

    #[test]
    fn only_pages_move_the_cursor_never_a_save_or_an_announcement() {
        let mut store = NoteStore::default();
        store.restart(1, 1);
        store.page(1, page(vec![note(1, "a", 100)], false, 100), 2);
        // A save answered at 500 must not skip whatever changed in between.
        store.stored(note(1, "mine", 500));
        assert_eq!(store.note(1).unwrap().body, "mine");
        assert_eq!(
            store.announced(1, 500, 3),
            Some(Fetch { id: 3, after: 100 })
        );
        let next = store.page(
            3,
            page(
                vec![note(2, "elsewhere", 250), note(1, "mine", 500)],
                false,
                500,
            ),
            4,
        );
        assert_eq!(next, None);
        assert_eq!(store.note(2).unwrap().body, "elsewhere");
        // An older row never replaces a newer one.
        store.stored(note(1, "stale", 300));
        assert_eq!(store.note(1).unwrap().body, "mine");
    }

    #[test]
    fn later_passes_follow_every_page_past_the_announced_revision() {
        let mut store = NoteStore::default();
        store.restart(1, 1);
        store.page(1, page(vec![], false, 0), 2);
        assert_eq!(store.announced(1, 2, 3), Some(Fetch { id: 3, after: 0 }));
        // Busy elsewhere meanwhile: the first page already passes 2.
        let rows = (1..=64).map(|r| note(r as i64, "x", r)).collect();
        assert_eq!(
            store.page(3, page(rows, true, 64), 4),
            Some(Fetch { id: 4, after: 64 })
        );
        assert_eq!(
            store.page(4, page(vec![note(65, "y", 65)], false, 65), 5),
            None
        );
        assert_eq!(store.note(65).unwrap().body, "y");
    }

    #[test]
    fn stale_generations_failures_and_old_servers_change_nothing() {
        let mut store = NoteStore::default();
        store.restart(1, 1);
        store.page(1, page(vec![], false, 0), 2);
        let fetch = store.restart(2, 3);
        assert!(
            !store.ready(),
            "read-only again until the new pass completes"
        );
        assert_eq!(store.announced(1, 9, 4), None, "an old subscription's news");
        assert_eq!(
            store.page(1, page(vec![note(1, "old", 1)], false, 1), 5),
            None
        );
        assert_eq!(store.note(1), None);
        // A failed page leaves the cursor; the next announcement asks again.
        assert_eq!(store.page(fetch.id, Err("offline".into()), 6), None);
        assert!(!store.ready());
        assert_eq!(store.announced(2, 1, 7), Some(Fetch { id: 7, after: 0 }));
        store.unsupported(7);
        assert!(store.is_unsupported() && !store.ready());
        assert_eq!(store.announced(2, 2, 8), None);
    }

    #[test]
    fn retired_and_empty_notes_are_kept_but_not_offered() {
        let mut store = NoteStore::default();
        store.restart(1, 1);
        store.page(
            1,
            page(
                vec![note(1, "live", 1), note(2, "", 2), note(3, "gone", 3)],
                false,
                3,
            ),
            2,
        );
        store.stored(StoredNote {
            retired: true,
            ..note(3, "gone", 4)
        });
        assert_eq!(store.note(3), None);
        assert_eq!(store.note(2).map(|n| n.body.as_str()), Some(""));
        let found: Vec<i64> = store.searchable().map(|n| n.agent_id).collect();
        assert_eq!(found, [1]);
    }
}
