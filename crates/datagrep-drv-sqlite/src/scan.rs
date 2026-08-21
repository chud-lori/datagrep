use std::sync::Arc;

use datagrep_api::{Batch, DbError, FetchHint, LogicalType, Payload, ResumeToken, SortKey, Value};
use rusqlite::types::ValueRef;

use crate::error::map_sqlite_err;
use crate::value::{logical_type_for_decl, sqlite_value_to_datagrep, SqlParam};

pub(crate) struct ColumnMeta {
    pub name: Arc<str>,
    pub logical: LogicalType,
    pub native_type: Option<Arc<str>>,
}

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
    #[allow(dead_code)]
    stmt: Box<rusqlite::Statement<'conn>>,
    pub columns: Vec<ColumnMeta>,
    seq: u64,
    pub rows_emitted: u64,
    pub bytes_emitted: u64,
    done: bool,
    resume_key: Option<(usize, bool)>,
    last_resume_token: Option<ResumeToken>,
}

impl<'conn> OpenScan<'conn> {
    pub fn from_prepared(
        mut stmt: Box<rusqlite::Statement<'conn>>,
        columns: Vec<ColumnMeta>,
        params: &[Value],
        resume_key: Option<(usize, bool)>,
    ) -> Result<Self, DbError> {
        let bound: Vec<SqlParam<'_>> = params.iter().map(SqlParam).collect();

        // SAFETY: stmt is heap-boxed (stable address), only accessed via rows after construction, and rows is declared before stmt so it drops first — that trio makes erasing the Rows borrow to 'static sound.
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

    pub fn fetch_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        let max_rows = hint.max_rows.max(1) as usize;
        let mut out_rows: Vec<datagrep_api::Row> = Vec::with_capacity(max_rows.min(4096));
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
                values.push(sqlite_value_to_datagrep(vref, col.native_type.as_deref()));
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
            tz: datagrep_api::TzSpec::Naive,
        },
        _ => return Err(corrupt()),
    })
}

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
                path: datagrep_api::FieldPath::field("a"),
                desc: false,
                nulls_first: false,
            },
            SortKey {
                path: datagrep_api::FieldPath::field("b"),
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
