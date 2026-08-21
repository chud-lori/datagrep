use std::ffi::c_char;
use std::sync::Arc;

use datagrep_api::Value;
use datagrep_core::store::{DocSegment, RowWindow, WindowSlice, WindowStatus};

use crate::cells::{render_arrow, render_value, Rendered, KIND_ABSENT};
use crate::ffi_util::{guard, guard_quiet, to_c_string};
use crate::query::{query_ref, DatagrepQuery};
use crate::runtime::runtime;

#[derive(Clone, Copy)]
struct CellMeta {
    off: u32,
    len: u32,
    kind: u8,
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

#[derive(Clone, Copy)]
struct RowSource {
    slice: u32,
    offset: u32,
}

pub struct DatagrepRows {
    slices: Vec<WindowSlice>,
    rows: u64,
    cols: u32,
    pending: bool,
    text: String,
    cells: Vec<CellMeta>,
    sources: Vec<RowSource>,
    columns: Vec<String>,
    root: Option<String>,
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

unsafe fn rows_ref<'a>(r: *mut DatagrepRows) -> Option<&'a DatagrepRows> {
    if r.is_null() {
        None
    } else {
        // SAFETY: non-NULL (checked) and live per the contract; a window is immutable once built, so concurrent shared borrows are sound.
        Some(unsafe { &*r })
    }
}

// ---- build -------------------------------------------------------------

/// # Safety
/// `q` is an unfreed query handle from `datagrep_query_run`. `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_rows(
    q: *mut DatagrepQuery,
    offset: u64,
    len: u64,
    err_out: *mut *mut c_char,
) -> *mut DatagrepRows {
    guard(err_out, std::ptr::null_mut(), "datagrep_query_rows", || {
        // SAFETY: q unfreed per the contract; the window holds Arc clones so it never depends on q, but the header still requires freeing the window first.
        let q = unsafe { query_ref(q) }?;
        let skeleton_cols = q.column_count();
        let root = q.projection_root();

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
            root,
        ))))
    })
}

impl DatagrepRows {
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
            root: None,
        }
    }

    fn build(window: RowWindow, fallback_cols: u32, root: Option<String>) -> Self {
        let pending = matches!(window.status, WindowStatus::Pending | WindowStatus::Partial);

        let root = effective_root(&window.slices, root);
        let columns = project_columns(&window.slices, root.as_deref());
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
            text: String::with_capacity(rows * cols.max(1) as usize * 8),
            cells: Vec::with_capacity(rows * cols as usize),
            sources: Vec::with_capacity(rows),
            columns,
            root,
        };
        out.fill();
        out
    }

    fn fill(&mut self) {
        let cols = self.cols as usize;
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
                                    finish(&mut arena, start, rendered)
                                }
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
                                let cell = doc_field(row, name, cols, self.root.as_deref());
                                let meta = match cell {
                                    None => CellMeta::empty(KIND_ABSENT),
                                    Some(v) => {
                                        let start = arena.len();
                                        let r = render_value(v, &mut arena);
                                        finish(&mut arena, start, r)
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
                                        finish(&mut arena, start, r)
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

    fn envelope(&self, row: u64) -> Option<serde_json::Value> {
        let root = self.root.as_deref()?;
        let src = *self.sources.get(row as usize)?;
        let WindowSlice::Docs { docs, .. } = self.slices.get(src.slice as usize)? else {
            return None;
        };
        let DocSegment::Values(values) = docs.as_ref() else {
            return None;
        };
        let Value::Document(doc) = values.get(src.offset as usize)? else {
            return None;
        };
        let mut fields = serde_json::Map::new();
        for (name, value) in doc.iter() {
            if name.as_ref() == root {
                continue;
            }
            fields.insert(name.to_string(), crate::cells::value_to_json(value));
        }
        Some(serde_json::Value::Object(fields))
    }

    fn cell_meta(&self, row: u64, col: u32) -> Option<CellMeta> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let idx = row as usize * self.cols as usize + col as usize;
        self.cells.get(idx).copied()
    }

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
                        doc_field(row, name, self.cols as usize, self.root.as_deref())
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

fn finish(arena: &mut String, start: usize, rendered: Rendered<'_>) -> CellMeta {
    match rendered {
        Rendered::Empty(kind) => CellMeta::empty(kind),
        Rendered::Borrowed(s, kind) => match u32::try_from(s.len()) {
            Ok(len) => CellMeta {
                off: 0,
                len,
                kind,
                ptr: s.as_ptr(),
            },
            Err(_) => CellMeta::empty(kind),
        },
        Rendered::Arena(kind) => {
            let end = arena.len();
            match (u32::try_from(start), u32::try_from(end - start)) {
                (Ok(off), Ok(len)) => CellMeta {
                    off,
                    len,
                    kind,
                    ptr: std::ptr::null(),
                },
                _ => {
                    arena.truncate(start);
                    CellMeta::empty(kind)
                }
            }
        }
    }
}

fn row_root<'a>(row: &'a Value, root: Option<&str>) -> Option<&'a Value> {
    match root {
        None => Some(row),
        Some(name) => match row {
            Value::Document(doc) => doc.get(name),
            _ => None,
        },
    }
}

fn effective_root(slices: &[WindowSlice], root: Option<String>) -> Option<String> {
    let name = root.as_deref()?;
    let carried = slices.iter().any(|slice| match slice {
        WindowSlice::Docs {
            docs, offset, len, ..
        } => match docs.as_ref() {
            DocSegment::Values(values) => values
                .iter()
                .skip(*offset)
                .take(*len)
                .any(|value| row_root(value, Some(name)).is_some()),
            DocSegment::Pairs(_) => false,
        },
        WindowSlice::Table { .. } => false,
    });
    carried.then_some(root).flatten()
}

fn doc_field<'a>(
    row: &'a Value,
    name: Option<&str>,
    cols: usize,
    root: Option<&str>,
) -> Option<&'a Value> {
    let row = row_root(row, root)?;
    match (row, name) {
        (Value::Document(doc), Some(name)) => doc.get(name),
        // Single synthetic "value" column over a non-document row.
        (other, _) if cols <= 1 => Some(other),
        _ => None,
    }
}

fn project_columns(slices: &[WindowSlice], root: Option<&str>) -> Vec<String> {
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
                        if let Some(Value::Document(doc)) = row_root(value, root) {
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

pub(crate) fn doc_columns(segment: &Arc<DocSegment>, root: Option<&str>) -> Vec<String> {
    project_columns(
        &[WindowSlice::Docs {
            first_row: 0,
            docs: segment.clone(),
            offset: 0,
            len: segment.len(),
        }],
        root,
    )
}

// ---- accessors ---------------------------------------------------------

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_count(r: *mut DatagrepRows) -> u64 {
    // SAFETY: r is NULL or a live window from datagrep_query_rows per the contract; rows_ref maps NULL to None.
    guard_quiet(0, || unsafe { rows_ref(r) }.map(|r| r.rows).unwrap_or(0))
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_columns(r: *mut DatagrepRows) -> u32 {
    // SAFETY: as `datagrep_rows_count` — NULL or a live window.
    guard_quiet(0, || unsafe { rows_ref(r) }.map(|r| r.cols).unwrap_or(0))
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_pending(r: *mut DatagrepRows) -> bool {
    // SAFETY: as `datagrep_rows_count` — NULL or a live window.
    guard_quiet(false, || {
        unsafe { rows_ref(r) }.map(|r| r.pending).unwrap_or(false)
    })
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`. The returned pointer is borrowed, length-tagged, NOT NUL-terminated, and dangles after `datagrep_rows_free`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_cell(
    r: *mut DatagrepRows,
    row: u64,
    col: u32,
    len_out: *mut usize,
) -> *const c_char {
    guard_quiet(std::ptr::null(), || {
        // SAFETY: non-NULL (checked) and writable per the contract; length zeroed first so an early return cannot leave a stale count.
        if !len_out.is_null() {
            unsafe { *len_out = 0 };
        }
        // SAFETY: r is NULL or a live window from datagrep_query_rows per the contract; rows_ref maps NULL to None.
        let Some(rows) = (unsafe { rows_ref(r) }) else {
            return std::ptr::null();
        };
        let Some(meta) = rows.cell_meta(row, col) else {
            return std::ptr::null();
        };
        // SAFETY: non-NULL (checked) and writable per the contract.
        if !len_out.is_null() {
            unsafe { *len_out = meta.len as usize };
        }
        if meta.len == 0 {
            return rows.text.as_ptr() as *const c_char;
        }
        if meta.ptr.is_null() {
            // SAFETY: fill/finish guarantee off + len <= text.len() past the u32 round trip, so the pointer lands inside text, owned by rows and dropped only by datagrep_rows_free.
            unsafe { rows.text.as_ptr().add(meta.off as usize) as *const c_char }
        } else {
            meta.ptr as *const c_char
        }
    })
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_cell_kind(r: *mut DatagrepRows, row: u64, col: u32) -> u8 {
    guard_quiet(KIND_ABSENT, || {
        // SAFETY: r is NULL or a live window from datagrep_query_rows per the contract; rows_ref maps NULL to None.
        unsafe { rows_ref(r) }
            .and_then(|rows| rows.cell_meta(row, col))
            .map(|m| m.kind)
            .unwrap_or(KIND_ABSENT)
    })
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_cell_detail_json(
    r: *mut DatagrepRows,
    row: u64,
    col: u32,
) -> *mut c_char {
    guard_quiet(std::ptr::null_mut(), || {
        // SAFETY: r is NULL or a live window from datagrep_query_rows per the contract; rows_ref maps NULL to None.
        let Some(rows) = (unsafe { rows_ref(r) }) else {
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

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_column_names_json(r: *mut DatagrepRows) -> *mut c_char {
    guard_quiet(std::ptr::null_mut(), || {
        // SAFETY: r is NULL or a live window from datagrep_query_rows per the contract; rows_ref maps NULL to None.
        let Some(rows) = (unsafe { rows_ref(r) }) else {
            return std::ptr::null_mut();
        };
        match serde_json::to_string(&rows.columns) {
            Ok(text) => to_c_string(text),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_envelope_json(
    r: *mut DatagrepRows,
    row: u64,
) -> *mut c_char {
    guard_quiet(std::ptr::null_mut(), || {
        // SAFETY: r is NULL or a live window from datagrep_query_rows per the contract; rows_ref maps NULL to None.
        let Some(rows) = (unsafe { rows_ref(r) }) else {
            return std::ptr::null_mut();
        };
        let Some(envelope) = rows.envelope(row) else {
            return std::ptr::null_mut();
        };
        match serde_json::to_string(&envelope) {
            Ok(text) => to_c_string(text),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// # Safety
/// `r` is NULL or an unfreed window from `datagrep_query_rows`. Every cell pointer it handed out dangles after this returns.
#[no_mangle]
pub unsafe extern "C" fn datagrep_rows_free(r: *mut DatagrepRows) {
    guard_quiet((), || {
        if !r.is_null() {
            // SAFETY: non-NULL (checked) and unfreed per the contract; dropping the Box releases the arena, so every cell pointer from datagrep_rows_cell dangles after this — the header says so.
            drop(unsafe { Box::from_raw(r) });
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
        rooted_window(values, None)
    }

    fn rooted_window(values: Vec<Value>, root: Option<&str>) -> DatagrepRows {
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
            root.map(str::to_string),
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
            None,
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
            None,
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
            None,
        );
        assert_eq!(rows.columns, vec!["key", "value"]);
        assert_eq!(text(&rows, 0, 0), "k");
        assert_eq!(text(&rows, 0, 1), "v");
    }

    #[test]
    fn a_root_hint_projects_the_document_and_not_its_envelope() {
        let hit = doc(vec![
            ("_index", Value::Str(Arc::from("events"))),
            ("_id", Value::Str(Arc::from("abc"))),
            ("_seq_no", Value::I64(41)),
            ("_primary_term", Value::I64(3)),
            (
                "_source",
                doc(vec![
                    ("status", Value::Str(Arc::from("open"))),
                    ("n", Value::I64(7)),
                ]),
            ),
        ]);
        let rows = rooted_window(vec![hit], Some("_source"));
        assert_eq!(rows.columns, vec!["status", "n"]);
        assert_eq!(text(&rows, 0, 0), "open");
        assert_eq!(text(&rows, 0, 1), "7");
    }

    #[test]
    fn the_envelope_carries_the_identity_and_the_cas_guard() {
        let hit = doc(vec![
            ("_index", Value::Str(Arc::from("events"))),
            ("_id", Value::Str(Arc::from("abc"))),
            ("_routing", Value::Str(Arc::from("tenant-7"))),
            ("_seq_no", Value::I64(41)),
            ("_primary_term", Value::I64(3)),
            (
                "_source",
                doc(vec![("status", Value::Str(Arc::from("open")))]),
            ),
        ]);
        let rows = rooted_window(vec![hit], Some("_source"));
        let envelope = rows.envelope(0).expect("a rooted row has an envelope");
        assert_eq!(envelope["_id"], serde_json::json!("abc"));
        assert_eq!(envelope["_seq_no"], serde_json::json!(41));
        assert_eq!(envelope["_primary_term"], serde_json::json!(3));
        assert_eq!(envelope["_routing"], serde_json::json!("tenant-7"));
        assert!(
            envelope.get("_source").is_none(),
            "the document itself is not part of its own envelope"
        );
        let plain = docs_window(vec![doc(vec![("a", Value::I64(1))])]);
        assert!(plain.envelope(0).is_none());
    }

    #[test]
    fn a_window_with_no_root_anywhere_is_projected_unrooted() {
        let bare = doc(vec![
            ("_index", Value::Str(Arc::from("events"))),
            ("_id", Value::Str(Arc::from("abc"))),
        ]);
        let rows = rooted_window(vec![bare], Some("_source"));
        assert_eq!(rows.columns, vec!["_index", "_id"]);
        assert_eq!(text(&rows, 0, 1), "abc");
        assert!(rows.envelope(0).is_none());
    }
}
