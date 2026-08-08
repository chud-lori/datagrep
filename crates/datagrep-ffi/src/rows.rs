//! The hot path: one materialised window, and nothing else.
//!
//! > "**Invariant: `datagrep` never holds a result set larger than
//! > `total_result_budget`, regardless of the query.**" (§3.2)
//!
//! and the claim this ABI has to make true on every scroll:
//!
//! > "The projected view materializes to a small Arrow batch **for the visible
//! > window only**." (§3.2)
//!
//! [`datagrep_query_rows`] is a single `CoreApi::get_rows(qid, off..off+len)` and a
//! single pass over exactly that rectangle. It never widens the range, never
//! reads ahead, and never waits: a window the feeder has not reached yet comes
//! back with `datagrep_rows_pending() == true` and zero rows, which is the signal to
//! draw skeletons. Asking for it is itself what resumes the feeder (§3.6:
//! *"scrolling is the pull signal"*), so the next call gets real rows.
//!
//! ## Memory shape of a window
//!
//! ```text
//! DatagrepRows
//!  ├─ slices : Vec<WindowSlice>   Arc clones of the store's own buffers. NOT copies.
//!  ├─ text   : String             ONE arena, holding only cells that needed formatting
//!  └─ cells  : Vec<CellMeta>      rows×cols of {ptr|offset, len, kind} — 24 bytes each
//! ```
//!
//! `datagrep_rows_cell` returns a pointer that is either into `text` or **straight
//! into an Arrow/`Arc<str>` buffer** the `slices` keep alive. The Swift side
//! never allocates, never copies, and never re-formats on redraw — design §5.1's
//! banned anti-pattern is "`format!` in the cell-render path", and there is no
//! cell-render path left on the Swift side to put one in.

use std::ffi::c_char;
use std::sync::Arc;

use datagrep_api::Value;
use datagrep_core::store::{DocSegment, RowWindow, WindowSlice, WindowStatus};

use crate::cells::{render_arrow, render_value, Rendered, KIND_ABSENT};
use crate::ffi_util::{guard, guard_quiet, to_c_string};
use crate::query::{query_ref, DatagrepQuery};
use crate::runtime::runtime;

/// Where one cell's UTF-8 bytes live.
#[derive(Clone, Copy)]
struct CellMeta {
    /// Byte offset into [`DatagrepRows::text`] — used only when `ptr` is null.
    off: u32,
    len: u32,
    kind: u8,
    /// Non-null: borrowed straight from a store buffer, zero copy (§5.1).
    ptr: *const u8,
}

impl CellMeta {
    fn empty(kind: u8) -> Self {
        Self {
            off: 0,
            len: 0,
            kind,
            ptr: std::ptr::null(),
        }
    }
}

/// Which slice, and which row within its underlying container, a window row
/// came from — so the detail pane can recover the original [`Value`] without
/// this window ever having stored one.
#[derive(Clone, Copy)]
struct RowSource {
    slice: u32,
    /// Row index inside the slice's `RecordBatch`/`DocSegment`.
    offset: u32,
}

/// One materialised window.
pub struct DatagrepRows {
    /// Arc clones of the store's buffers. Their only job is to outlive every
    /// borrowed pointer in `cells`. Never iterated on the hot path.
    slices: Vec<WindowSlice>,
    rows: u64,
    cols: u32,
    pending: bool,
    /// The window's single text arena.
    text: String,
    /// `rows * cols`, row-major.
    cells: Vec<CellMeta>,
    sources: Vec<RowSource>,
    /// Column names — the document lane projects by name, so it needs them.
    columns: Vec<String>,
}

impl std::fmt::Debug for DatagrepRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatagrepRows")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("pending", &self.pending)
            .field("arena_bytes", &self.text.len())
            .finish()
    }
}

/// Borrow a `DatagrepRows*` argument.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed.
unsafe fn rows_ref<'a>(r: *mut DatagrepRows) -> Option<&'a DatagrepRows> {
    if r.is_null() {
        None
    } else {
        Some(&*r)
    }
}

// ---- build -------------------------------------------------------------

/// Materialises ONLY `[offset, offset+len)`.
///
/// # Safety
/// `q` must come from `datagrep_query_run`; `err_out` must be NULL or writable.
/// The returned pointer must be freed with `datagrep_rows_free` **before** its
/// `DatagrepQuery` is freed.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_rows(
    q: *mut DatagrepQuery,
    offset: u64,
    len: u64,
    err_out: *mut *mut c_char,
) -> *mut DatagrepRows {
    guard(err_out, std::ptr::null_mut(), "datagrep_query_rows", || {
        let q = query_ref(q)?;
        let skeleton_cols = q.column_count();

        // Not accepted by the server yet (or already failed to start): there
        // is nothing to materialise, and blocking here would be exactly the
        // freeze this design exists to prevent. Skeletons.
        let Some(qid) = q.qid() else {
            return Ok(Box::into_raw(Box::new(DatagrepRows::skeleton(
                skeleton_cols,
            ))));
        };

        let rt = runtime()?;
        let window = rt
            .block_on(
                q.core()
                    .api
                    .get_rows(qid, offset..offset.saturating_add(len)),
            )
            .map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(DatagrepRows::build(
            window,
            skeleton_cols,
        ))))
    })
}

impl DatagrepRows {
    /// An empty, pending window — what a query that has not reached the
    /// server yet can honestly offer.
    fn skeleton(cols: u32) -> Self {
        Self {
            slices: Vec::new(),
            rows: 0,
            cols,
            pending: true,
            text: String::new(),
            cells: Vec::new(),
            sources: Vec::new(),
            columns: Vec::new(),
        }
    }

    /// Format one window, once.
    fn build(window: RowWindow, fallback_cols: u32) -> Self {
        // `Pending` means none of it arrived; `Partial` means the tail did
        // not. Both mean "the feeder was resumed, ask again" — and both mean
        // the caller should draw skeletons for what `datagrep_rows_count` does not
        // cover, so both set the flag (design §3.2 window resolver).
        let pending = matches!(window.status, WindowStatus::Pending | WindowStatus::Partial);

        let columns = project_columns(&window.slices);
        let cols = if columns.is_empty() {
            fallback_cols
        } else {
            columns.len() as u32
        };

        let rows: usize = window.slices.iter().map(WindowSlice::len).sum();
        let mut out = Self {
            slices: window.slices,
            rows: rows as u64,
            cols,
            pending,
            // A first guess at the arena: most cells are short. It grows if
            // wrong; it is one allocation per window either way, not per cell.
            text: String::with_capacity(rows * cols.max(1) as usize * 8),
            cells: Vec::with_capacity(rows * cols as usize),
            sources: Vec::with_capacity(rows),
            columns,
        };
        out.fill();
        out
    }

    /// The single formatting pass. Everything the Swift side ever reads is
    /// produced here, once.
    fn fill(&mut self) {
        let cols = self.cols as usize;
        // Take the arena out so the borrow checker lets us read `self.slices`
        // and write the arena at the same time; put it back at the end.
        let mut arena = std::mem::take(&mut self.text);
        let mut cells = std::mem::take(&mut self.cells);
        let mut sources = std::mem::take(&mut self.sources);

        for (slice_index, slice) in self.slices.iter().enumerate() {
            match slice {
                WindowSlice::Table {
                    batch, offset, len, ..
                } => {
                    for r in *offset..*offset + *len {
                        sources.push(RowSource {
                            slice: slice_index as u32,
                            offset: r as u32,
                        });
                        for c in 0..cols {
                            let meta = match batch.columns().get(c) {
                                Some(array) => {
                                    let start = arena.len();
                                    let rendered = render_arrow(array.as_ref(), r, &mut arena);
                                    finish(&arena, start, rendered)
                                }
                                // Fewer Arrow columns than the projection (a
                                // window straddling a schema delta): absent,
                                // not a lie about being NULL.
                                None => CellMeta::empty(KIND_ABSENT),
                            };
                            cells.push(meta);
                        }
                    }
                }
                WindowSlice::Docs {
                    docs, offset, len, ..
                } => match docs.as_ref() {
                    DocSegment::Values(values) => {
                        for (r, row) in values.iter().enumerate().skip(*offset).take(*len) {
                            sources.push(RowSource {
                                slice: slice_index as u32,
                                offset: r as u32,
                            });
                            for c in 0..cols {
                                let name = self.columns.get(c).map(String::as_str);
                                let cell = doc_field(row, name, cols);
                                let meta = match cell {
                                    // The whole point: a field that is not in
                                    // this document is ABSENT, never NULL.
                                    None => CellMeta::empty(KIND_ABSENT),
                                    Some(v) => {
                                        let start = arena.len();
                                        let r = render_value(v, &mut arena);
                                        finish(&arena, start, r)
                                    }
                                };
                                cells.push(meta);
                            }
                        }
                    }
                    DocSegment::Pairs(pairs) => {
                        for (r, pair) in pairs.iter().enumerate().skip(*offset).take(*len) {
                            sources.push(RowSource {
                                slice: slice_index as u32,
                                offset: r as u32,
                            });
                            for c in 0..cols {
                                let value = match c {
                                    0 => Some(&pair.0),
                                    1 => Some(&pair.1),
                                    _ => None,
                                };
                                let meta = match value {
                                    None => CellMeta::empty(KIND_ABSENT),
                                    Some(v) => {
                                        let start = arena.len();
                                        let r = render_value(v, &mut arena);
                                        finish(&arena, start, r)
                                    }
                                };
                                cells.push(meta);
                            }
                        }
                    }
                },
            }
        }

        self.text = arena;
        self.cells = cells;
        self.sources = sources;
    }

    fn cell_meta(&self, row: u64, col: u32) -> Option<CellMeta> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let idx = row as usize * self.cols as usize + col as usize;
        self.cells.get(idx).copied()
    }

    /// The original [`Value`] at `(row, col)`, recovered from the borrowed
    /// slices — this window never stored one.
    fn value_at(&self, row: u64, col: u32) -> Option<Value> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let src = *self.sources.get(row as usize)?;
        match self.slices.get(src.slice as usize)? {
            WindowSlice::Table { batch, .. } => {
                let array = batch.columns().get(col as usize)?;
                Some(crate::cells::arrow_cell_to_value(
                    array.as_ref(),
                    src.offset as usize,
                ))
            }
            WindowSlice::Docs { docs, .. } => match docs.as_ref() {
                DocSegment::Values(values) => {
                    let row = values.get(src.offset as usize)?;
                    let name = self.columns.get(col as usize).map(String::as_str);
                    Some(
                        doc_field(row, name, self.cols as usize)
                            .cloned()
                            .unwrap_or(Value::Absent),
                    )
                }
                DocSegment::Pairs(pairs) => {
                    let pair = pairs.get(src.offset as usize)?;
                    match col {
                        0 => Some(pair.0.clone()),
                        1 => Some(pair.1.clone()),
                        _ => None,
                    }
                }
            },
        }
    }
}

/// Resolve a [`Rendered`] against the arena it may have written into.
fn finish(arena: &str, start: usize, rendered: Rendered<'_>) -> CellMeta {
    match rendered {
        Rendered::Empty(kind) => CellMeta::empty(kind),
        Rendered::Borrowed(s, kind) => CellMeta {
            off: 0,
            len: s.len() as u32,
            kind,
            ptr: s.as_ptr(),
        },
        Rendered::Arena(kind) => CellMeta {
            off: start as u32,
            len: (arena.len() - start) as u32,
            kind,
            ptr: std::ptr::null(),
        },
    }
}

/// Field `name` of a document row, or `None` when it is **not present**.
///
/// A non-document value in a `Shape::Documents` stream (a bare scalar, an
/// array) is the whole row and occupies the single projected column.
fn doc_field<'a>(row: &'a Value, name: Option<&str>, cols: usize) -> Option<&'a Value> {
    match (row, name) {
        (Value::Document(doc), Some(name)) => doc.get(name),
        // Single synthetic "value" column over a non-document row.
        (other, _) if cols <= 1 => Some(other),
        _ => None,
    }
}

/// The window's column projection.
///
/// - **Table**: the Arrow schema, verbatim.
/// - **Documents**: the ordered union of top-level field names seen *in this
///   window*. Design §3.1's `ViewProjection`, at its simplest honest form. It
///   is window-local on purpose — a document store has no global column list,
///   and inventing one would mean scanning rows nobody asked for, which is the
///   banned anti-pattern this whole design is built against.
/// - **Pairs**: `key`, `value` (Redis `SCAN`/`HGETALL`).
fn project_columns(slices: &[WindowSlice]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for slice in slices {
        match slice {
            WindowSlice::Table { batch, .. } => {
                return batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect();
            }
            WindowSlice::Docs { docs, .. } => match docs.as_ref() {
                DocSegment::Pairs(_) => return vec!["key".to_string(), "value".to_string()],
                DocSegment::Values(values) => {
                    for value in values.iter() {
                        if let Value::Document(doc) = value {
                            for (key, _) in doc.iter() {
                                if !names.iter().any(|n| n == key.as_ref()) {
                                    names.push(key.to_string());
                                }
                            }
                        }
                    }
                }
            },
        }
    }
    if names.is_empty() && !slices.is_empty() {
        // Documents that are not documents (a stream of bare scalars).
        names.push("value".to_string());
    }
    names
}

/// Column names of a whole `DocSegment` — used by
/// [`crate::query::datagrep_query_status_json`] to report a document result's
/// columns before any window has been asked for.
pub(crate) fn doc_columns(segment: &Arc<DocSegment>) -> Vec<String> {
    project_columns(&[WindowSlice::Docs {
        first_row: 0,
        docs: segment.clone(),
        offset: 0,
        len: segment.len(),
    }])
}

// ---- accessors ---------------------------------------------------------

/// Rows actually available in this window.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_count(r: *mut DatagrepRows) -> u64 {
    guard_quiet(0, || rows_ref(r).map(|r| r.rows).unwrap_or(0))
}

/// Columns in this window.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_columns(r: *mut DatagrepRows) -> u32 {
    guard_quiet(0, || rows_ref(r).map(|r| r.cols).unwrap_or(0))
}

/// `true` => not fetched yet, draw skeletons.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_pending(r: *mut DatagrepRows) -> bool {
    guard_quiet(false, || rows_ref(r).map(|r| r.pending).unwrap_or(false))
}

/// Cell text, borrowed — valid until `datagrep_rows_free`. NOT null-terminated.
///
/// Returns NULL (and writes 0 to `len_out`) for a cell outside the window.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed; `len_out` must be
/// NULL or point at a writable `size_t`. The returned pointer must not be used
/// after `datagrep_rows_free`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_cell(
    r: *mut DatagrepRows,
    row: u64,
    col: u32,
    len_out: *mut usize,
) -> *const c_char {
    guard_quiet(std::ptr::null(), || {
        if !len_out.is_null() {
            *len_out = 0;
        }
        let Some(rows) = rows_ref(r) else {
            return std::ptr::null();
        };
        let Some(meta) = rows.cell_meta(row, col) else {
            return std::ptr::null();
        };
        if !len_out.is_null() {
            *len_out = meta.len as usize;
        }
        if meta.len == 0 {
            // A real, empty string — distinct from NULL and from ABSENT,
            // which the caller tells apart with `datagrep_rows_cell_kind`. Return
            // a valid non-NULL pointer so an empty cell is never mistaken for
            // an error.
            return rows.text.as_ptr() as *const c_char;
        }
        if meta.ptr.is_null() {
            rows.text.as_ptr().add(meta.off as usize) as *const c_char
        } else {
            meta.ptr as *const c_char
        }
    })
}

/// 0 = value, 1 = SQL NULL, 2 = ABSENT, 3 = nested.
///
/// A cell outside the window reports `2` — "not present" is the literal truth
/// about a coordinate this window does not cover.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_cell_kind(r: *mut DatagrepRows, row: u64, col: u32) -> u8 {
    guard_quiet(KIND_ABSENT, || {
        rows_ref(r)
            .and_then(|rows| rows.cell_meta(row, col))
            .map(|m| m.kind)
            .unwrap_or(KIND_ABSENT)
    })
}

/// Full raw value of one cell as JSON, for the detail pane. Caller frees with
/// `datagrep_string_free`.
///
/// Returns NULL for a cell outside the window.
///
/// # Safety
/// `r` must come from `datagrep_query_rows` and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_cell_detail_json(
    r: *mut DatagrepRows,
    row: u64,
    col: u32,
) -> *mut c_char {
    guard_quiet(std::ptr::null_mut(), || {
        let Some(rows) = rows_ref(r) else {
            return std::ptr::null_mut();
        };
        let Some(value) = rows.value_at(row, col) else {
            return std::ptr::null_mut();
        };
        match serde_json::to_string(&crate::cells::value_to_json(&value)) {
            Ok(text) => to_c_string(text),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Release the window and every pointer borrowed from it.
///
/// # Safety
/// `r` must come from `datagrep_query_rows`, freed at most once. Every pointer
/// `datagrep_rows_cell` returned for it is dangling afterwards.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_free(r: *mut DatagrepRows) {
    guard_quiet((), || {
        if !r.is_null() {
            drop(Box::from_raw(r));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::{KIND_NESTED, KIND_VALUE};
    use datagrep_api::Document;

    fn doc(fields: Vec<(&str, Value)>) -> Value {
        Value::Document(Arc::new(Document::from_fields(
            fields.into_iter().map(|(k, v)| (Arc::from(k), v)).collect(),
        )))
    }

    fn docs_window(values: Vec<Value>) -> DatagrepRows {
        let len = values.len();
        let segment = Arc::new(DocSegment::Values(values));
        DatagrepRows::build(
            RowWindow {
                range: 0..len as u64,
                status: WindowStatus::Ready,
                slices: vec![WindowSlice::Docs {
                    first_row: 0,
                    docs: segment,
                    offset: 0,
                    len,
                }],
            },
            0,
        )
    }

    fn text(rows: &DatagrepRows, row: u64, col: u32) -> String {
        let meta = rows.cell_meta(row, col).expect("cell");
        if meta.len == 0 {
            return String::new();
        }
        let ptr = if meta.ptr.is_null() {
            unsafe { rows.text.as_ptr().add(meta.off as usize) }
        } else {
            meta.ptr
        };
        let slice = unsafe { std::slice::from_raw_parts(ptr, meta.len as usize) };
        std::str::from_utf8(slice).expect("utf8").to_string()
    }

    /// **The reason the document model exists**, end to end through the ABI's
    /// own data structures: one document has `note`, one has it as an explicit
    /// null, one does not have it at all. Three states, three kinds.
    #[test]
    fn absent_null_and_empty_survive_the_window() {
        let rows = docs_window(vec![
            doc(vec![
                ("id", Value::I64(1)),
                ("note", Value::Str(Arc::from("hi"))),
            ]),
            doc(vec![("id", Value::I64(2)), ("note", Value::Null)]),
            doc(vec![("id", Value::I64(3))]),
            doc(vec![
                ("id", Value::I64(4)),
                ("note", Value::Str(Arc::from(""))),
            ]),
        ]);

        assert_eq!(rows.cols, 2, "projection is id, note");
        assert_eq!(rows.columns, vec!["id", "note"]);
        assert_eq!(rows.rows, 4);

        assert_eq!(rows.cell_meta(0, 1).unwrap().kind, KIND_VALUE);
        assert_eq!(text(&rows, 0, 1), "hi");

        assert_eq!(rows.cell_meta(1, 1).unwrap().kind, 1, "explicit NULL");
        assert_eq!(text(&rows, 1, 1), "");

        assert_eq!(
            rows.cell_meta(2, 1).unwrap().kind,
            KIND_ABSENT,
            "the field is simply not in that document"
        );
        assert_eq!(text(&rows, 2, 1), "");

        assert_eq!(
            rows.cell_meta(3, 1).unwrap().kind,
            KIND_VALUE,
            "an empty string is a value"
        );
        assert_eq!(text(&rows, 3, 1), "");
    }

    #[test]
    fn nested_cells_summarise_and_the_detail_pane_gets_the_real_thing() {
        let rows = docs_window(vec![doc(vec![
            ("id", Value::I64(1)),
            (
                "meta",
                doc(vec![("a", Value::I64(1)), ("b", Value::Bool(true))]),
            ),
        ])]);
        assert_eq!(rows.cell_meta(0, 1).unwrap().kind, KIND_NESTED);
        assert_eq!(text(&rows, 0, 1), "{2 fields}");

        let value = rows.value_at(0, 1).expect("value");
        let json = crate::cells::value_to_json(&value);
        assert_eq!(json["a"], serde_json::json!(1));
        assert_eq!(json["b"], serde_json::json!(true));
    }

    #[test]
    fn a_document_string_cell_is_borrowed_not_copied() {
        let rows = docs_window(vec![doc(vec![("s", Value::Str(Arc::from("borrow-me")))])]);
        let meta = rows.cell_meta(0, 0).unwrap();
        assert!(
            !meta.ptr.is_null(),
            "an Arc<str> cell must be borrowed, not copied into the arena"
        );
        assert!(rows.text.is_empty(), "the arena must stay untouched");
        assert_eq!(text(&rows, 0, 0), "borrow-me");
    }

    #[test]
    fn out_of_range_coordinates_are_absent_not_a_crash() {
        let rows = docs_window(vec![doc(vec![("a", Value::I64(1))])]);
        assert!(rows.cell_meta(99, 0).is_none());
        assert!(rows.cell_meta(0, 99).is_none());
        assert!(rows.value_at(99, 0).is_none());
    }

    #[test]
    fn a_pending_window_is_empty_and_says_so() {
        let rows = DatagrepRows::build(
            RowWindow {
                range: 9000..9050,
                status: WindowStatus::Pending,
                slices: Vec::new(),
            },
            3,
        );
        assert!(rows.pending, "Pending must map to pending == true");
        assert_eq!(rows.rows, 0);
        assert_eq!(rows.cols, 3, "skeletons still need a width");
    }

    #[test]
    fn a_partial_window_delivers_what_exists_and_still_asks_for_more() {
        let rows = DatagrepRows::build(
            RowWindow {
                range: 0..50,
                status: WindowStatus::Partial,
                slices: vec![WindowSlice::Docs {
                    first_row: 0,
                    docs: Arc::new(DocSegment::Values(vec![doc(vec![("a", Value::I64(1))])])),
                    offset: 0,
                    len: 1,
                }],
            },
            0,
        );
        assert!(rows.pending);
        assert_eq!(rows.rows, 1, "the rows that do exist are delivered");
    }

    #[test]
    fn key_value_pairs_project_to_two_columns() {
        let segment = Arc::new(DocSegment::Pairs(vec![(
            Value::Str(Arc::from("k")),
            Value::Str(Arc::from("v")),
        )]));
        let rows = DatagrepRows::build(
            RowWindow {
                range: 0..1,
                status: WindowStatus::Ready,
                slices: vec![WindowSlice::Docs {
                    first_row: 0,
                    docs: segment,
                    offset: 0,
                    len: 1,
                }],
            },
            0,
        );
        assert_eq!(rows.columns, vec!["key", "value"]);
        assert_eq!(text(&rows, 0, 0), "k");
        assert_eq!(text(&rows, 0, 1), "v");
    }
}
