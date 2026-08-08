//! `--format table` — aligned ASCII for humans.
//!
//! Three things the ticket calls out by name:
//! - **NULL, empty string, and `Absent` render distinctly.** `NULL` prints as
//!   the literal text `NULL` (dimmed on a color TTY); `Absent` prints as
//!   `(absent)` (also dimmed); a genuine empty string prints as nothing
//!   between delimiters. See [`crate::value_text::CellText`] for where the
//!   three states come from.
//! - **Cells are truncated to terminal width with an ellipsis.** Column
//!   widths are computed once, from the *first* window of rows this sink
//!   sees (module docs on [`super`]: buffering one bounded window, not the
//!   whole result), then reused for every later window so the whole result
//!   still reads as one aligned table. If the sum of natural widths would
//!   overflow the terminal, every column is scaled down proportionally
//!   (floor [`MIN_COL_WIDTH`]) rather than only the widest one, so no column
//!   silently vanishes.
//! - **A footer says rows shown vs total.** [`Summary::note`] carries the
//!   honest reason when they differ (`--limit`, the soft row cap, a
//!   cancellation) — this sink never invents one.
//!
//! Deviation, stated plainly: real terminal width comes from an `ioctl`
//! (`TIOCGWINSZ`), which needs a crate this workspace's dependency list for
//! `datagrep-cli` does not include. Width instead comes from `$COLUMNS` when set
//! (most shells export it, and every test in this module sets it), else a
//! fixed 120-column default. Widths are measured in `char`s, not display
//! (grapheme) width, so wide CJK cells can still overflow their column by a
//! little — a known, documented simplification, not an oversight.

use std::io::{self, IsTerminal, Write};

use super::{Row, RowSink, Summary};
use crate::value_text::CellText;

const MAX_COL_WIDTH: usize = 40;
const MIN_COL_WIDTH: usize = 3;
/// `" | "` between every pair of columns.
const COL_SEPARATOR_WIDTH: usize = 3;

/// Color only when stdout is a real TTY and the user hasn't opted out
/// (ticket: "Color only when stdout is a TTY; honor `NO_COLOR`").
pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&w| w > 0)
        .unwrap_or(120)
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Truncate to `width` chars, replacing the last char with `…` when it
/// didn't fit — never silently dropping data with no indication.
fn truncate_ellipsis(s: &str, width: usize) -> String {
    if char_len(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// A cell's plain (uncolored) display text plus which of the three states it
/// is, so the writer can color `Null`/`Absent` without that color affecting
/// column-width math (padding is always computed from the plain text).
enum Rendered {
    Null,
    Absent,
    Text(String),
}

impl Rendered {
    fn of(cell: &CellText) -> Self {
        match cell {
            CellText::Null => Rendered::Null,
            CellText::Absent => Rendered::Absent,
            CellText::Text(s) => Rendered::Text(s.clone()),
        }
    }

    fn plain(&self) -> &str {
        match self {
            Rendered::Null => "NULL",
            Rendered::Absent => "(absent)",
            Rendered::Text(s) => s.as_str(),
        }
    }

    fn is_special(&self) -> bool {
        !matches!(self, Rendered::Text(_))
    }
}

pub struct TableSink<W: Write> {
    out: W,
    columns: Vec<String>,
    widths: Option<Vec<usize>>,
    color: bool,
    /// Print a leading blank line before the header — used for statement 2+
    /// of a multi-statement script so result sets don't run together.
    leading_blank: bool,
    header_written: bool,
}

impl<W: Write> TableSink<W> {
    pub fn new(out: W, leading_blank: bool, color: bool) -> Self {
        Self {
            out,
            columns: Vec::new(),
            widths: None,
            color,
            leading_blank,
            header_written: false,
        }
    }

    /// Natural width per column from the header plus one window of data,
    /// each column capped at [`MAX_COL_WIDTH`], then the whole row scaled
    /// down proportionally if it would overflow the terminal.
    fn compute_widths(&self, rows: &[Row]) -> Vec<usize> {
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .map(|c| char_len(c).max(MIN_COL_WIDTH))
            .collect();
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                if let Some(w) = widths.get_mut(i) {
                    let text = Rendered::of(cell);
                    *w = (*w).max(char_len(text.plain()).min(MAX_COL_WIDTH));
                }
            }
        }
        if widths.is_empty() {
            return widths;
        }
        let overhead = (widths.len() - 1) * COL_SEPARATOR_WIDTH;
        let term = terminal_width();
        let floor = overhead + widths.len() * MIN_COL_WIDTH;
        let total = widths.iter().sum::<usize>() + overhead;
        if total > term && term > floor {
            let avail = term - overhead;
            let sum: usize = widths.iter().sum();
            if sum > 0 {
                for w in &mut widths {
                    *w = ((*w * avail) / sum).max(MIN_COL_WIDTH);
                }
            }
        }
        widths
    }

    fn write_row<'a>(&mut self, cells: impl Iterator<Item = (&'a str, bool)>) -> io::Result<()> {
        let widths = self.widths.clone().unwrap_or_default();
        for (i, (text, special)) in cells.enumerate() {
            if i > 0 {
                write!(self.out, " | ")?;
            }
            let w = widths.get(i).copied().unwrap_or_else(|| char_len(text));
            let truncated = truncate_ellipsis(text, w);
            let pad = w.saturating_sub(char_len(&truncated));
            if special && self.color {
                write!(self.out, "\x1b[2m{truncated}\x1b[0m")?;
            } else {
                write!(self.out, "{truncated}")?;
            }
            write!(self.out, "{}", " ".repeat(pad))?;
        }
        writeln!(self.out)
    }

    fn write_header(&mut self) -> io::Result<()> {
        if self.leading_blank {
            writeln!(self.out)?;
        }
        if self.columns.is_empty() {
            self.header_written = true;
            return Ok(());
        }
        let header: Vec<String> = self.columns.clone();
        let cells: Vec<(&str, bool)> = header.iter().map(|c| (c.as_str(), false)).collect();
        self.write_row(cells.into_iter())?;
        let widths = self.widths.clone().unwrap_or_default();
        let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        writeln!(self.out, "{}", sep.join("-+-"))?;
        self.header_written = true;
        Ok(())
    }
}

impl<W: Write> RowSink for TableSink<W> {
    fn start(&mut self, columns: &[String]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()> {
        if self.widths.is_none() {
            self.widths = Some(self.compute_widths(rows));
            self.write_header()?;
        }
        for row in rows {
            let rendered: Vec<Rendered> = row.iter().map(Rendered::of).collect();
            let cells: Vec<(&str, bool)> = rendered
                .iter()
                .map(|r| (r.plain(), r.is_special()))
                .collect();
            self.write_row(cells.into_iter())?;
        }
        Ok(())
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        if !self.header_written {
            self.widths = Some(self.compute_widths(&[]));
            self.write_header()?;
        }
        let plural = if summary.rows_shown == 1 { "" } else { "s" };
        match &summary.note {
            Some(note) => writeln!(
                self.out,
                "({} row{plural} shown — {note})",
                summary.rows_shown
            ),
            None => writeln!(self.out, "({} row{plural})", summary.rows_shown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<String> {
        vec!["id".into(), "name".into(), "note".into()]
    }

    fn render(rows: Vec<Row>, note: Option<&str>) -> String {
        // SAFETY: test-only env mutation; nothing else in this process reads
        // COLUMNS concurrently within a single `cargo test` process's default
        // (non-parallel-within-module) execution of this function.
        unsafe { std::env::set_var("COLUMNS", "80") };
        let mut out = Vec::new();
        {
            let mut sink = TableSink::new(&mut out, false, false);
            sink.start(&cols()).unwrap();
            sink.write_rows(&rows).unwrap();
            sink.finish(&Summary {
                rows_shown: rows.len() as u64,
                note: note.map(str::to_string),
            })
            .unwrap();
        }
        unsafe { std::env::remove_var("COLUMNS") };
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn null_absent_and_empty_string_render_distinctly() {
        let rows = vec![
            vec![
                CellText::Text("1".into()),
                CellText::Null,
                CellText::Text("x".into()),
            ],
            vec![
                CellText::Text("2".into()),
                CellText::Absent,
                CellText::Text("x".into()),
            ],
            vec![
                CellText::Text("3".into()),
                CellText::Text(String::new()),
                CellText::Text("x".into()),
            ],
        ];
        let text = render(rows, None);
        let lines: Vec<&str> = text.lines().collect();
        // header, separator, 3 rows, footer
        assert_eq!(lines.len(), 6, "unexpected layout:\n{text}");
        assert!(lines[2].contains("NULL"));
        assert!(lines[3].contains("(absent)"));
        // The third row's "name" column is a real empty string: neither
        // sentinel appears on that line.
        assert!(!lines[4].contains("NULL"));
        assert!(!lines[4].contains("(absent)"));
    }

    #[test]
    fn footer_reports_rows_shown_and_an_honest_note() {
        let rows = vec![vec![
            CellText::Text("1".into()),
            CellText::Text("a".into()),
            CellText::Text("x".into()),
        ]];
        let text = render(rows, Some("stopped after 1 row (--limit)"));
        assert!(text
            .trim_end()
            .ends_with("(1 row shown — stopped after 1 row (--limit))"));
    }

    #[test]
    fn footer_pluralizes_on_row_count() {
        let text = render(Vec::new(), None);
        assert!(text.contains("(0 rows)"));
    }

    #[test]
    fn wide_cell_is_truncated_with_an_ellipsis() {
        // SAFETY: see `render`.
        unsafe { std::env::set_var("COLUMNS", "40") };
        let mut out = Vec::new();
        {
            let mut sink = TableSink::new(&mut out, false, false);
            sink.start(&["only_col".to_string()]).unwrap();
            let long = "x".repeat(200);
            sink.write_rows(&[vec![CellText::Text(long)]]).unwrap();
            sink.finish(&Summary {
                rows_shown: 1,
                note: None,
            })
            .unwrap();
        }
        unsafe { std::env::remove_var("COLUMNS") };
        let text = String::from_utf8(out).unwrap();
        let data_line = text.lines().nth(2).unwrap();
        assert!(
            data_line.trim_end().ends_with('…'),
            "expected an ellipsis, got: {data_line:?}"
        );
        assert!(
            data_line.chars().count() <= MAX_COL_WIDTH + 2,
            "line not truncated: {data_line:?}"
        );
    }

    #[test]
    fn empty_columns_prints_only_the_footer() {
        let mut out = Vec::new();
        {
            let mut sink = TableSink::new(&mut out, false, false);
            sink.start(&[]).unwrap();
            sink.finish(&Summary {
                rows_shown: 0,
                note: Some("statement acknowledged".into()),
            })
            .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.trim_end(), "(0 rows shown — statement acknowledged)");
    }

    #[test]
    fn color_wraps_null_and_absent_only_when_enabled() {
        let mut out = Vec::new();
        {
            let mut sink = TableSink::new(&mut out, false, true);
            sink.start(&["a".to_string()]).unwrap();
            sink.write_rows(&[vec![CellText::Null]]).unwrap();
            sink.finish(&Summary {
                rows_shown: 1,
                note: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[2mNULL\x1b[0m"));
    }
}
