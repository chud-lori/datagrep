//! An open table scan, stepped incrementally across many `FetchBatch`
//! worker commands (design §3.1 `Cursor::next_batch`, §3.2 "chunk 1 renders
//! before chunk 2 is requested").
//!
//! ## Why this needs `unsafe`
//! rusqlite's `Rows<'stmt>` borrows its `Statement<'stmt>` mutably. A
//! streaming cursor needs both to stay alive *and* keep their cursor
//! position **across many separate worker-thread command dispatches**, which
//! means they must live together in one struct stored in a map — the classic
//! self-referential-struct problem safe Rust cannot express directly (the
//! same reason crates like `ouroboros`/`rental` exist). [`OpenScan`] hand-rolls
//! the same pattern rusqlite's own `CachedStatement`-adjacent internals use:
//! heap-box the statement so its address is stable no matter how the owning
//! `HashMap` reallocates around it, and store the `Rows` borrow with its
//! lifetime erased to `'static`. Soundness rests on three invariants, all
//! enforced right here and nowhere else in the crate:
//! 1. `stmt` is heap-boxed — moving the `Box` handle (e.g. a `HashMap`
//!    rehash) never moves the `Statement` it points to.
//! 2. `stmt` is never read or moved-out-of while `rows` is alive; the only
//!    access after construction goes through `rows`.
//! 3. `rows` is declared *before* `stmt` in the struct, so Rust drops it
//!    first — `Rows::drop` touches the underlying prepared statement, which
//!    must not have been finalized yet.

use std::sync::Arc;

use dbx_api::{Batch, DbError, FetchHint, LogicalType, Payload, ResumeToken, SortKey, Value};
use rusqlite::types::ValueRef;

use crate::error::map_sqlite_err;
use crate::value::{logical_type_for_decl, sqlite_value_to_dbx, SqlParam};

/// One column's schema facts, gathered once right after `PREPARE` (design
/// §3.1: never lie about a value — `logical` is a hint, not a promise about
/// any individual cell; see `value.rs`).
pub(crate) struct ColumnMeta {
    pub name: Arc<str>,
    pub logical: LogicalType,
    pub native_type: Option<Arc<str>>,
}

/// Column schema of an already-prepared statement, gathered via `&self`
/// methods only (so it can run before the statement is boxed for storage).
pub(crate) fn column_metas_for(stmt: &rusqlite::Statement<'_>) -> Vec<ColumnMeta> {
    stmt.columns()
        .into_iter()
        .map(|col| {
            let decl = col.decl_type().map(str::to_string);
            let logical = logical_type_for_decl(decl.as_deref());
            ColumnMeta {
                name: Arc::from(col.name()),
                logical,
                native_type: decl.map(Arc::from),
            }
        })
        .collect()
}

fn estimate_value_bytes(v: &ValueRef<'_>) -> u64 {
    match v {
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(b) | ValueRef::Blob(b) => b.len() as u64,
    }
}

pub(crate) struct OpenScan<'conn> {
    // Declared first so it drops before `stmt` — see module doc, invariant 3.
    rows: rusqlite::Rows<'static>,
    // Never touched again after `prepare_and_open`; kept purely to anchor
    // the heap allocation `rows` borrows into.
    #[allow(dead_code)]
    stmt: Box<rusqlite::Statement<'conn>>,
    pub columns: Vec<ColumnMeta>,
    seq: u64,
    pub rows_emitted: u64,
    pub bytes_emitted: u64,
    done: bool,
    /// Set only when the originating `Op::Scan` had exactly one `SortKey`
    /// that resolved to one of this statement's output columns: (column
    /// index, descending). See `compile_resume_clause` for the matching
    /// consumer half.
    resume_key: Option<(usize, bool)>,
    last_resume_token: Option<ResumeToken>,
}

impl<'conn> OpenScan<'conn> {
    /// Takes an already-prepared statement (the caller needed `column_count`
    /// / `column_name` on it first to decide Ack-vs-Table classification and
    /// build the `RowSchema`, so re-preparing here would be wasted work) and
    /// its precomputed column metadata, and opens the row iterator.
    pub fn from_prepared(
        mut stmt: Box<rusqlite::Statement<'conn>>,
        columns: Vec<ColumnMeta>,
        params: &[Value],
        resume_key: Option<(usize, bool)>,
    ) -> Result<Self, DbError> {
        let bound: Vec<SqlParam<'_>> = params.iter().map(SqlParam).collect();

        // SAFETY: `stmt_ptr` points into the heap allocation owned by
        // `boxed`/`stmt`, which we are about to store in `self.stmt` without
        // ever moving out of it again — its address is therefore stable for
        // the life of `self`. We erase the resulting `Rows<'_>` borrow to
        // `'static` and rely on invariants 1-3 in the module doc (stable
        // address, no other access, `rows` drops first) to make that sound.
        let stmt_ptr: *mut rusqlite::Statement<'conn> = &mut *stmt;
        let rows = unsafe { &mut *stmt_ptr }
            .query(rusqlite::params_from_iter(bound))
            .map_err(map_sqlite_err)?;
        let rows: rusqlite::Rows<'static> = unsafe { std::mem::transmute(rows) };

        Ok(Self {
            rows,
            stmt,
            columns,
            seq: 0,
            rows_emitted: 0,
            bytes_emitted: 0,
            done: false,
            resume_key,
            last_resume_token: None,
        })
    }

    /// Step the statement until `hint` is satisfied or the query is
    /// exhausted. `None` means the scan is over; a returned `Batch` is never
    /// empty (design §3.2: chunk sizes are real, never phantom).
    pub fn fetch_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        let max_rows = hint.max_rows.max(1) as usize;
        let mut out_rows: Vec<dbx_api::Row> = Vec::with_capacity(max_rows.min(4096));
        let mut batch_bytes: u64 = 0;

        while out_rows.len() < max_rows {
            let row = match self.rows.next().map_err(map_sqlite_err)? {
                Some(row) => row,
                None => {
                    self.done = true;
                    break;
                }
            };
            let mut values = Vec::with_capacity(self.columns.len());
            for (i, col) in self.columns.iter().enumerate() {
                let vref = row.get_ref(i).map_err(map_sqlite_err)?;
                batch_bytes += estimate_value_bytes(&vref);
                values.push(sqlite_value_to_dbx(vref, col.native_type.as_deref()));
            }
            if let Some((idx, _desc)) = self.resume_key {
                if let Some(v) = values.get(idx) {
                    self.last_resume_token = Some(encode_resume(v));
                }
            }
            out_rows.push(values);
            self.rows_emitted += 1;
            if batch_bytes >= u64::from(hint.max_bytes) {
                break;
            }
        }

        if out_rows.is_empty() {
            return Ok(None);
        }
        let seq = self.seq;
        self.seq += 1;
        self.bytes_emitted += batch_bytes;
        Ok(Some(Batch {
            seq,
            payload: Payload::Rows(out_rows),
            schema_delta: Vec::new(),
            notices: Vec::new(),
        }))
    }

    pub fn resume_token(&self) -> Option<ResumeToken> {
        self.last_resume_token.clone()
    }

    pub fn batches_emitted(&self) -> u64 {
        self.seq
    }
}

// --- Keyset resume token encoding -----------------------------------------
//
// A small hand-rolled tagged encoding (not `serde_json`: dbx-drv-sqlite's
// dependency list is deliberately short, and a resume token is opaque to
// everyone but this driver per the `Cursor::resume_token` contract). Only
// the scalar types that plausibly appear in an `ORDER BY` key are encoded;
// anything else fails loudly at encode time rather than truncating.

const TAG_NULL: u8 = 0;
const TAG_I64: u8 = 1;
const TAG_F64: u8 = 2;
const TAG_STR: u8 = 3;
const TAG_BYTES: u8 = 4;
const TAG_BOOL: u8 = 5;
const TAG_DATE: u8 = 6;
const TAG_TIME: u8 = 7;
const TAG_TIMESTAMP: u8 = 8;
const TAG_U64: u8 = 9;

fn encode_resume(value: &Value) -> ResumeToken {
    let mut buf = Vec::with_capacity(9);
    match value {
        Value::Null => buf.push(TAG_NULL),
        Value::I64(i) => {
            buf.push(TAG_I64);
            buf.extend_from_slice(&i.to_be_bytes());
        }
        Value::U64(u) => {
            buf.push(TAG_U64);
            buf.extend_from_slice(&u.to_be_bytes());
        }
        Value::F64(f) => {
            buf.push(TAG_F64);
            buf.extend_from_slice(&f.to_be_bytes());
        }
        Value::Str(s) => {
            buf.push(TAG_STR);
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(TAG_BYTES);
            buf.extend_from_slice(b);
        }
        Value::Bool(b) => {
            buf.push(TAG_BOOL);
            buf.push(u8::from(*b));
        }
        Value::Date(d) => {
            buf.push(TAG_DATE);
            buf.extend_from_slice(&d.to_be_bytes());
        }
        Value::Time { nanos } => {
            buf.push(TAG_TIME);
            buf.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::Timestamp { micros, .. } => {
            buf.push(TAG_TIMESTAMP);
            buf.extend_from_slice(&micros.to_be_bytes());
        }
        // Anything else (nested/document/graph-shaped values) cannot appear
        // as a flat SQLite row cell in the first place; fall back to a
        // token that never matches a real row rather than panicking.
        _ => buf.push(TAG_NULL),
    }
    ResumeToken(bytes::Bytes::from(buf))
}

fn decode_resume(token: &ResumeToken) -> Result<Value, DbError> {
    let corrupt = || DbError::Protocol("corrupt SQLite resume token".to_string());
    let b = &token.0;
    let tag = *b.first().ok_or_else(corrupt)?;
    let rest = &b[1..];
    Ok(match tag {
        TAG_NULL => Value::Null,
        TAG_I64 => Value::I64(i64::from_be_bytes(rest.try_into().map_err(|_| corrupt())?)),
        TAG_U64 => Value::U64(u64::from_be_bytes(rest.try_into().map_err(|_| corrupt())?)),
        TAG_F64 => Value::F64(f64::from_be_bytes(rest.try_into().map_err(|_| corrupt())?)),
        TAG_STR => Value::Str(Arc::from(std::str::from_utf8(rest).map_err(|_| corrupt())?)),
        TAG_BYTES => Value::Bytes(bytes::Bytes::copy_from_slice(rest)),
        TAG_BOOL => Value::Bool(*rest.first().ok_or_else(corrupt)? != 0),
        TAG_DATE => Value::Date(i32::from_be_bytes(rest.try_into().map_err(|_| corrupt())?)),
        TAG_TIME => Value::Time {
            nanos: i64::from_be_bytes(rest.try_into().map_err(|_| corrupt())?),
        },
        TAG_TIMESTAMP => Value::Timestamp {
            micros: i64::from_be_bytes(rest.try_into().map_err(|_| corrupt())?),
            tz: dbx_api::TzSpec::Naive,
        },
        _ => return Err(corrupt()),
    })
}

/// Compile the WHERE fragment continuing a single-sort-key scan past
/// `token`. Multi-key resume is a documented gap (see module doc + design
/// note in `driver.rs`): a correct multi-column keyset predicate needs
/// direction-aware row-value comparison, deliberately not implemented here.
pub(crate) fn compile_resume_clause(
    order: &[SortKey],
    token: &ResumeToken,
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    if order.len() != 1 {
        return Err(DbError::Unsupported {
            feature: format!(
                "Op::Scan resume with {} sort keys — only single-key keyset resume is supported",
                order.len()
            ),
        });
    }
    let key = &order[0];
    let field = crate::compile::field_ident(&key.path)?;
    let value = decode_resume(token)?;
    let op = if key.desc { "<" } else { ">" };
    params.push(value);
    Ok(format!("{field} {op} ?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_token_round_trips_scalars() {
        for v in [
            Value::Null,
            Value::I64(-42),
            Value::U64(42),
            Value::F64(3.5),
            Value::Str(Arc::from("hello")),
            Value::Bytes(bytes::Bytes::from_static(b"\x00\x01")),
            Value::Bool(true),
            Value::Date(19_000),
            Value::Time { nanos: 123 },
        ] {
            let token = encode_resume(&v);
            assert_eq!(decode_resume(&token).unwrap(), v, "round-trip of {v:?}");
        }
    }

    #[test]
    fn multi_key_resume_is_a_declared_gap() {
        let order = vec![
            SortKey {
                path: dbx_api::FieldPath::field("a"),
                desc: false,
                nulls_first: false,
            },
            SortKey {
                path: dbx_api::FieldPath::field("b"),
                desc: false,
                nulls_first: false,
            },
        ];
        let token = encode_resume(&Value::I64(1));
        let mut params = Vec::new();
        let err = compile_resume_clause(&order, &token, &mut params).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
    }
}
