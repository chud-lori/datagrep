//! Output formats (ticket: `table | json | ndjson | csv | tsv`).
//!
//! Every [`RowSink`] impl but [`table::TableSink`] writes each row the
//! instant it arrives — no buffering at all. `TableSink` buffers at most one
//! *window* (the caller's own streaming chunk, already bounded — see
//! `commands::query`) to compute column widths, then reuses those widths for
//! every later window so the whole result still reads as one aligned table.
//! That bound, not "the whole result", is what the streaming proof asserts.

pub mod csv;
pub mod json;
pub mod table;

use std::io;

use crate::value_text::CellText;

/// What every writer needs to know once a result set ends, to render an
/// honest trailer/summary (ticket: "say how many rows were shown vs total").
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub rows_shown: u64,
    /// `None` when the result finished normally (nothing was withheld).
    pub note: Option<String>,
    /// Affected-row count from an `Ack`-shaped statement (INSERT/UPDATE/DDL).
    /// When set, the human footer reads "N rows affected" instead of
    /// "(0 rows shown)".
    pub affected: Option<u64>,
}

/// One row, as columns paired with their already-classified [`CellText`].
/// Streaming sinks consume this and forget it immediately.
pub type Row = Vec<CellText>;

/// A format's streaming writer. `commands::query`/`commands::export` drive
/// this one window (or one row, for the non-table formats) at a time; no
/// implementation is allowed to accumulate the full result (design §3.2,
/// ticket "NEVER accumulate the whole result in a Vec before printing").
///
/// `Send` because `export` hands a sink adapter across
/// `datagrep_core::CoreApi::run_export`'s (`Send`) sink seam.
pub trait RowSink: Send {
    /// Called once per statement, before any rows. `columns` is empty for a
    /// non-tabular result (an `Ack`-shaped statement, e.g. DDL).
    fn start(&mut self, columns: &[String]) -> io::Result<()>;

    /// One row. May be called any number of times, in any-sized batches, but
    /// callers should keep the batches themselves bounded (a "window") for
    /// the streaming contract to mean anything end to end.
    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()>;

    /// Called once per statement, after the last row.
    fn finish(&mut self, summary: &Summary) -> io::Result<()>;
}
