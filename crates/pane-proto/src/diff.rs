//! Retained-mode terminal state-diff engine.
//!
//! The host keeps a headless VT grid per terminal and, on every coalesced
//! tick, converts it into a compact row representation: each row is a list of
//! run-length [`Span`]s of identically styled cells, with trailing blank cells
//! trimmed. [`diff_rows`] then compares against the previous tick and emits
//! only rows whose text **or** attributes changed. The client keeps a mirror
//! [`Screen`] and applies those [`DirtyRow`]s in place.

use std::ops::{BitOr, BitOrAssign};

use serde::{Deserialize, Serialize};

/// A terminal color, mirroring the classic VT model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's default foreground / background.
    #[default]
    Default,
    /// One of the 256 indexed palette colors.
    Idx(u8),
    /// 24-bit true color.
    Rgb(u8, u8, u8),
}

/// Bit set of SGR text attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StyleFlags(pub u8);

impl StyleFlags {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const UNDERLINE: Self = Self(1 << 2);
    pub const INVERSE: Self = Self(1 << 3);
    pub const DIM: Self = Self(1 << 4);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for StyleFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for StyleFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Visual attributes shared by every cell of a [`Span`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub flags: StyleFlags,
}

impl CellStyle {
    /// Whether a blank cell in this style is indistinguishable from an empty cell.
    pub fn blank_is_invisible(&self) -> bool {
        self.bg == Color::Default
            && !self.flags.contains(StyleFlags::INVERSE)
            && !self.flags.contains(StyleFlags::UNDERLINE)
    }
}

/// A run of consecutive, identically styled cells.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    /// Number of grid cells this span covers (wide CJK glyphs count as 2).
    pub width: u16,
    /// The text drawn in those cells.
    pub text: String,
    pub style: CellStyle,
}

/// A full row of the grid, as a list of spans starting at column 0.
pub type Row = Vec<Span>;

/// A row that changed since the previous tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyRow {
    pub row: u16,
    pub spans: Vec<Span>,
}

/// Incrementally builds a [`Row`] from individual cells, merging runs of equal
/// style and trimming invisible trailing blanks.
#[derive(Debug, Default)]
pub struct RowBuilder {
    spans: Row,
}

impl RowBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one cell. `text` may be empty (rendered as a space) and `width`
    /// is the number of columns the cell occupies.
    pub fn push_cell(&mut self, text: &str, width: u16, style: CellStyle) {
        let text = if text.is_empty() { " " } else { text };
        match self.spans.last_mut() {
            Some(last) if last.style == style => {
                last.text.push_str(text);
                last.width += width;
            }
            _ => self.spans.push(Span { width, text: text.to_owned(), style }),
        }
    }

    /// Finish the row, dropping trailing blank cells that would render as nothing.
    pub fn finish(mut self) -> Row {
        while let Some(last) = self.spans.last_mut() {
            if !last.style.blank_is_invisible() {
                break;
            }
            let trimmed_len = last.text.trim_end_matches(' ').len();
            let removed = (last.text.len() - trimmed_len) as u16;
            if trimmed_len == 0 {
                self.spans.pop();
            } else {
                last.text.truncate(trimmed_len);
                last.width -= removed;
                break;
            }
        }
        self.spans
    }
}

/// Compare two grid snapshots and return only the rows that differ.
///
/// Rows present in `next` but not in `prev` (e.g. after a resize) are always
/// dirty. Rows beyond `next.len()` are implicitly dropped by the receiver,
/// which truncates its mirror to the advertised screen height.
pub fn diff_rows(prev: &[Row], next: &[Row]) -> Vec<DirtyRow> {
    next.iter()
        .enumerate()
        .filter(|(i, row)| prev.get(*i) != Some(*row))
        .map(|(i, row)| DirtyRow { row: i as u16, spans: row.clone() })
        .collect()
}

/// Client-side mirror of a remote terminal grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Row>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub scrollback: u32,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows, lines: vec![Row::new(); rows as usize], cursor_visible: true, ..Default::default() }
    }

    /// Apply a batch of dirty rows for a grid of the given size.
    pub fn apply(&mut self, screen_cols: u16, screen_rows: u16, dirty: Vec<DirtyRow>) {
        self.cols = screen_cols;
        self.rows = screen_rows;
        self.lines.resize(screen_rows as usize, Row::new());
        for d in dirty {
            if let Some(line) = self.lines.get_mut(d.row as usize) {
                *line = d.spans;
            }
        }
    }

    /// Plain text of a row (useful for copy / tests).
    pub fn row_text(&self, row: usize) -> String {
        self.lines.get(row).map(|l| l.iter().map(|s| s.text.as_str()).collect()).unwrap_or_default()
    }

    /// Plain text of the whole visible grid, trailing blank lines removed.
    pub fn text(&self) -> String {
        let mut out: Vec<String> = (0..self.lines.len()).map(|r| self.row_text(r)).collect();
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Row {
        let mut b = RowBuilder::new();
        for ch in text.chars() {
            b.push_cell(&ch.to_string(), 1, CellStyle::default());
        }
        b.finish()
    }

    fn grid(lines: &[&str]) -> Vec<Row> {
        lines.iter().map(|l| plain(l)).collect()
    }

    #[test]
    fn row_builder_merges_runs_and_trims_trailing_blanks() {
        let red = CellStyle { fg: Color::Idx(1), ..Default::default() };
        let mut b = RowBuilder::new();
        for ch in "ls ".chars() {
            b.push_cell(&ch.to_string(), 1, CellStyle::default());
        }
        for ch in "-la".chars() {
            b.push_cell(&ch.to_string(), 1, red);
        }
        for _ in 0..20 {
            b.push_cell("", 1, CellStyle::default());
        }
        let row = b.finish();
        assert_eq!(row.len(), 2);
        assert_eq!(row[0], Span { width: 3, text: "ls ".into(), style: CellStyle::default() });
        assert_eq!(row[1], Span { width: 3, text: "-la".into(), style: red });
    }

    #[test]
    fn row_builder_keeps_visible_trailing_background() {
        let bar = CellStyle { bg: Color::Idx(4), ..Default::default() };
        let mut b = RowBuilder::new();
        for _ in 0..10 {
            b.push_cell("", 1, bar);
        }
        let row = b.finish();
        assert_eq!(row, vec![Span { width: 10, text: " ".repeat(10), style: bar }]);
        assert!(plain("          ").is_empty(), "blank default row encodes to zero spans");
    }

    #[test]
    fn identical_snapshots_produce_no_diff() {
        let a = grid(&["$ echo hi", "hi", "$"]);
        assert!(diff_rows(&a, &a.clone()).is_empty());
    }

    #[test]
    fn only_changed_rows_are_dirty() {
        let a = grid(&["$ echo hi", "hi", "$"]);
        let b = grid(&["$ echo hi", "hi", "$ ls"]);
        let d = diff_rows(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].row, 2);
        assert_eq!(d[0].spans, plain("$ ls"));
    }

    #[test]
    fn attribute_only_change_is_dirty() {
        let a = grid(&["warning"]);
        let mut b = a.clone();
        b[0][0].style.flags |= StyleFlags::BOLD;
        let d = diff_rows(&a, &b);
        assert_eq!(d.len(), 1, "same text but different attributes must be resent");
    }

    #[test]
    fn grown_screen_marks_new_rows_dirty() {
        let a = grid(&["a", "b"]);
        let b = grid(&["a", "b", "", "d"]);
        let rows: Vec<u16> = diff_rows(&a, &b).iter().map(|d| d.row).collect();
        assert_eq!(rows, vec![2, 3]);
    }

    #[test]
    fn mirror_converges_after_applying_diffs() {
        let frames = [
            grid(&["", "", ""]),
            grid(&["$ cargo test", "", ""]),
            grid(&["$ cargo test", "running 12 tests", ""]),
            grid(&["running 12 tests", "test result: ok", "$"]),
            grid(&["test result: ok", "$"]), // shrink
        ];
        let mut mirror = Screen::new(80, 3);
        let mut prev: Vec<Row> = Vec::new();
        for f in &frames {
            let d = diff_rows(&prev, f);
            mirror.apply(80, f.len() as u16, d);
            assert_eq!(mirror.lines, *f);
            prev = f.clone();
        }
        assert_eq!(mirror.text(), "test result: ok\n$");
    }

    #[test]
    fn flags_contains() {
        let f = StyleFlags::BOLD | StyleFlags::UNDERLINE;
        assert!(f.contains(StyleFlags::BOLD));
        assert!(f.contains(StyleFlags::UNDERLINE));
        assert!(!f.contains(StyleFlags::INVERSE));
    }
}
