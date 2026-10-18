//! Source-preserving display rows shared by composer painting and navigation.

use ratatui::text::{Line, Span, Text};
use ratatui_textarea::DataCursor;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::ui::theme::{self, Role};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Point {
    pub row: usize,
    pub col: usize,
}

struct Cell {
    source: DataCursor,
    end: DataCursor,
    col: usize,
    text: String,
}

pub(super) struct Layout {
    rows: Vec<Vec<Cell>>,
    /// Every source character, including hidden separators and grapheme interiors.
    positions: Vec<Vec<Point>>,
    /// Valid visual destinations: grapheme starts and logical line ends.
    stops: Vec<(DataCursor, Point)>,
    width: usize,
    height: usize,
}

impl Layout {
    pub fn new(lines: &[String], width: u16, cursor: DataCursor, tab_len: u8) -> Self {
        let width = usize::from(width.max(1));
        let mut layout = Self {
            rows: Vec::new(),
            positions: Vec::new(),
            stops: Vec::new(),
            width,
            height: 0,
        };
        for (row, line) in lines.iter().enumerate() {
            layout.rows.push(Vec::new());
            let mut point = Point {
                row: layout.rows.len() - 1,
                col: 0,
            };
            let mut positions = vec![point; line.chars().count() + 1];
            let mut source_col = 0;
            let mut soft_start = false;
            let mut indentation = true;
            let mut word_start = true;
            let parts: Vec<_> = line.graphemes(true).collect();
            for (i, part) in parts.iter().enumerate() {
                let separator = part.chars().all(char::is_whitespace);
                let source = DataCursor(row, source_col);
                let end = source_col + part.chars().count();
                if word_start && !separator {
                    let word_width: usize = parts[i..]
                        .iter()
                        .take_while(|s| !s.chars().all(char::is_whitespace))
                        .map(|s| display(s, 0, width, tab_len).width())
                        .sum();
                    if word_width <= width && point.col > 0 && point.col + word_width > width {
                        layout.next_row(&mut point);
                        soft_start = true;
                    }
                }
                word_start = separator;
                let mut text = display(part, point.col, width, tab_len);
                if point.col > 0 && point.col + text.width() > width {
                    layout.next_row(&mut point);
                    soft_start = true;
                    text = display(part, 0, width, tab_len);
                }
                for p in &mut positions[source_col..end] {
                    *p = point;
                }
                if !(separator && soft_start && !indentation) {
                    layout.stops.push((source, point));
                    let cells = text.width();
                    layout.rows[point.row].push(Cell {
                        source,
                        end: DataCursor(row, end),
                        col: point.col,
                        text,
                    });
                    point.col += cells;
                    soft_start = false;
                }
                if !separator {
                    indentation = false;
                }
                source_col = end;
            }
            layout.height = layout.rows.len();
            if point.col >= width {
                if row + 1 == lines.len() {
                    // Keep a virtual end cell reachable by navigation. It only
                    // takes visible height when the caret actually occupies it.
                    layout.next_row(&mut point);
                } else {
                    // The hard newline already supplies the next display row.
                    point = Point {
                        row: layout.rows.len(),
                        col: 0,
                    };
                }
            }
            positions[source_col] = point;
            layout.stops.push((DataCursor(row, source_col), point));
            layout.positions.push(positions);
        }
        layout.height = layout.height.max(layout.point(cursor).row + 1);
        layout
    }

    fn next_row(&mut self, point: &mut Point) {
        self.rows.push(Vec::new());
        *point = Point {
            row: self.rows.len() - 1,
            col: 0,
        };
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn last_row(&self) -> usize {
        self.rows.len() - 1
    }

    pub fn point(&self, source: DataCursor) -> Point {
        self.positions[source.0][source.1]
    }

    pub fn source(&self, point: Point) -> DataCursor {
        self.stops
            .iter()
            .filter(|(_, p)| p.row == point.row && p.col < self.width)
            .take_while(|(_, p)| p.col <= point.col)
            .last()
            .or_else(|| self.stops.iter().find(|(_, p)| p.row == point.row))
            .expect("each display row has a source destination")
            .0
    }

    pub fn text(
        &self,
        cursor: DataCursor,
        focused: bool,
        selection: Option<((usize, usize), (usize, usize))>,
        top: usize,
        height: usize,
    ) -> Text<'static> {
        let caret = self.point(cursor);
        let surface = theme::style(Role::HumanSurface);
        let mut lines = Vec::new();
        for (row, cells) in self
            .rows
            .iter()
            .enumerate()
            .skip(top)
            .take(height.min(self.height.saturating_sub(top)))
        {
            let mut painted_cursor = false;
            let mut spans = Vec::new();
            for cell in cells {
                let mut style = surface;
                if selection.is_some_and(|(start, end)| {
                    cell.source < DataCursor(end.0, end.1)
                        && cell.end > DataCursor(start.0, start.1)
                }) {
                    style = style.patch(theme::style(Role::Selection));
                }
                if focused && row == caret.row && cell.col == caret.col {
                    style = style.patch(theme::style(Role::Cursor));
                    painted_cursor = true;
                }
                spans.push(Span::styled(cell.text.clone(), style));
            }
            // Selecting a hard newline paints its end cell, as the editor does.
            let selected_newline = self
                .positions
                .iter()
                .enumerate()
                .take(self.positions.len().saturating_sub(1))
                .any(|(source_row, positions)| {
                    let p = positions.last().unwrap();
                    let end = (source_row, positions.len() - 1);
                    p.row == row
                        && self.source(Point { row, col: 0 }).0 == source_row
                        && p.col < self.width
                        && selection.is_some_and(|(start, finish)| start <= end && end < finish)
                });
            if selected_newline && !(focused && row == caret.row && !painted_cursor) {
                spans.push(Span::styled(
                    " ",
                    surface.patch(theme::style(Role::Selection)),
                ));
            }
            if focused && row == caret.row && !painted_cursor {
                spans.push(Span::styled(" ", surface.patch(theme::style(Role::Cursor))));
            }
            lines.push(Line::from(spans));
        }
        Text::from(lines)
    }
}

fn display(part: &str, col: usize, width: usize, tab_len: u8) -> String {
    if part == "\t" {
        let tab = usize::from(tab_len.max(1));
        return " ".repeat((tab - col % tab).min(width));
    }
    if part.chars().any(char::is_control) || part.width() == 0 {
        return " ".into();
    }
    if part.width() > width {
        "…".into()
    } else {
        part.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(source: &str, width: u16, cursor: DataCursor) -> Layout {
        Layout::new(
            &source.split('\n').map(str::to_string).collect::<Vec<_>>(),
            width,
            cursor,
            4,
        )
    }

    #[test]
    fn words_hide_soft_separators_and_preserve_hard_indentation() {
        let cases: &[(&str, u16, &[&str])] = &[
            ("alpha beta", 8, &["alpha ", "beta"]),
            ("alpha beta", 5, &["alpha", "beta"]),
            ("ab    cd", 3, &["ab ", "cd"]),
            ("a\tbb", 4, &["a   ", "bb"]),
            ("\talpha beta", 12, &["    alpha ", "beta"]),
            ("a\n  b\n\nc", 3, &["a", "  b", "", "c"]),
            ("          a", 3, &["   ", "   ", "   ", " a"]),
            ("abcdefgh", 3, &["abc", "def", "gh"]),
            ("你 好", 3, &["你 ", "好"]),
            ("e\u{301}x y", 3, &["e\u{301}x ", "y"]),
            ("👩‍💻ab z", 4, &["👩‍💻ab", "z"]),
            ("ab cd", 1, &["a", "b", "c", "d"]),
            ("你", 1, &["…"]),
            ("a b", 0, &["a", "b"]),
        ];
        for &(source, width, expected) in cases {
            let cursor = DataCursor(0, 0);
            let layout = build(source, width, cursor);
            let rendered: Vec<_> = layout
                .text(cursor, false, None, 0, usize::MAX)
                .lines
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(rendered, expected, "{source:?} at {width}");
        }
    }

    #[test]
    fn every_source_caret_has_one_visible_cell_including_hidden_separators() {
        for source in [
            "abcd  ef",
            "abcd  ",
            "abcd  \nnext",
            "e\u{301}x",
            "👩‍💻x",
            "👍🏽🇺🇸",
            "\tX",
            "     ",
            "",
        ] {
            for width in [0, 1, 2, 4, 8] {
                for (row, line) in source.split('\n').enumerate() {
                    for col in 0..=line.chars().count() {
                        let cursor = DataCursor(row, col);
                        let layout = build(source, width, cursor);
                        let point = layout.point(cursor);
                        assert!(
                            point.row < layout.height() && point.col < usize::from(width.max(1))
                        );
                        let text = layout.text(cursor, true, None, 0, usize::MAX);
                        let style =
                            theme::style(Role::HumanSurface).patch(theme::style(Role::Cursor));
                        assert_eq!(
                            text.lines
                                .iter()
                                .flat_map(|l| &l.spans)
                                .filter(|s| s.style == style)
                                .count(),
                            1,
                            "{source:?} {width} {cursor:?}"
                        );
                        assert!(
                            text.lines
                                .iter()
                                .all(|line| line.width() <= usize::from(width.max(1)))
                        );
                        for y in 0..layout.height() {
                            let target = layout.source(Point {
                                row: y,
                                col: point.col,
                            });
                            assert_eq!(
                                layout.point(target).row,
                                y,
                                "visual destination remains on target row"
                            );
                        }
                    }
                }
            }
        }
    }
}
