//! Value mapping between rusqlite's storage classes and `dbx-api::Value`
//! (design §3.1, risk #4), plus identifier quoting (design §3.8: "identifiers
//! via `quote_ident` per dialect").
//!
//! **SQLite type affinity, stated honestly.** SQLite columns are dynamically
//! typed: a column declared `INTEGER` can still hold a `TEXT` value, because
//! `decl_type` only ever *suggests* an affinity ([sqlite.org/datatype3]). We
//! never coerce a cell's storage class to match its declared type — the one
//! exception is `BOOLEAN`-declared columns holding `0`/`1`, which is a common
//! application convention layered on top of SQLite's untyped `INTEGER`
//! storage, not a real distinct storage class; `Date`/`Time`/`Timestamp`
//! declared columns keep their raw storage-class `Value` (`Str` or `I64`) and
//! only get a [`LogicalType`] hint on the schema side, per design risk #4:
//! "never lie about a value."
//!
//! [sqlite.org/datatype3]: https://www.sqlite.org/datatype3.html

use std::sync::Arc;

use bytes::Bytes;
use dbx_api::{DbError, LogicalType, Value};
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};

/// Double-quote identifier quoting (SQLite/standard SQL style), with embedded
/// `"` doubled and embedded NUL rejected outright.
///
/// NUL rejection matters specifically for the catalog (`catalog.rs`): SQLite
/// identifiers are C strings under the hood, so a NUL byte would silently
/// truncate whatever we build a `PRAGMA table_info("...")` statement from —
/// exactly the "suspicious name" the design calls out, since `PRAGMA`
/// statements cannot bind parameters and must be assembled as text.
pub fn quote_ident(name: &str) -> Result<String, DbError> {
    if name.is_empty() {
        return Err(DbError::Query {
            code: None,
            message: "cannot quote an empty identifier".to_string(),
            position: None,
        });
    }
    if name.as_bytes().contains(&0) {
        return Err(DbError::Query {
            code: None,
            message: format!("identifier contains a NUL byte: {name:?}"),
            position: None,
        });
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for ch in name.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(out)
}

/// Map one SQLite storage-class value to a `dbx-api` [`Value`].
///
/// `decl_type` is the column's declared type text (`stmt.column_decl_type`),
/// used only for the `BOOLEAN` convention described on the module doc — every
/// other case maps the storage class as-is, never the declared type.
pub(crate) fn sqlite_value_to_dbx(v: ValueRef<'_>, decl_type: Option<&str>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => {
            if is_boolean_decl(decl_type) && (i == 0 || i == 1) {
                Value::Bool(i != 0)
            } else {
                Value::I64(i)
            }
        }
        ValueRef::Real(f) => Value::F64(f),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => Value::Str(Arc::from(s)),
            // SQLite's TEXT storage class is documented as UTF-8/16, but
            // nothing stops a BLOB written through a TEXT-affinity column
            // via an extension or a corrupt file. Never lose the bytes.
            Err(_) => Value::Unsupported {
                type_name: Arc::from("sqlite-text-invalid-utf8"),
                raw: Bytes::copy_from_slice(bytes),
                display: Arc::from(String::from_utf8_lossy(bytes).into_owned()),
            },
        },
        ValueRef::Blob(bytes) => Value::Bytes(Bytes::copy_from_slice(bytes)),
    }
}

fn is_boolean_decl(decl_type: Option<&str>) -> bool {
    matches!(decl_type, Some(t) if t.eq_ignore_ascii_case("boolean") || t.eq_ignore_ascii_case("bool"))
}

/// The [`LogicalType`] a declared column type implies, for [`RowSchema`]
/// construction (`dbx_api::RowSchema`) — never for coercing the runtime
/// value, which stays exactly what `sqlite_value_to_dbx` says it is.
///
/// Checks the `BOOLEAN`/`DATE`/`TIME`/`DATETIME`/`TIMESTAMP` conventions
/// first (common application usage, not part of SQLite's own algorithm),
/// then falls back to SQLite's own column-affinity rules
/// (<https://www.sqlite.org/datatype3.html#determination_of_column_affinity>).
/// NUMERIC affinity is genuinely ambiguous (a column can hold INTEGER, REAL,
/// or TEXT) so it maps to `Unknown` rather than guessing.
pub(crate) fn logical_type_for_decl(decl_type: Option<&str>) -> LogicalType {
    let Some(decl) = decl_type else {
        // No declared type at all => BLOB affinity, per SQLite's own rule.
        return LogicalType::Bytes;
    };
    let upper = decl.to_ascii_uppercase();
    match upper.as_str() {
        "BOOLEAN" | "BOOL" => return LogicalType::Bool,
        "DATE" => return LogicalType::Date,
        "TIME" => return LogicalType::Time,
        "DATETIME" | "TIMESTAMP" => return LogicalType::Timestamp,
        _ => {}
    }
    if upper.contains("INT") {
        LogicalType::I64
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        LogicalType::Str
    } else if upper.contains("BLOB") {
        LogicalType::Bytes
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        LogicalType::F64
    } else {
        LogicalType::Unknown
    }
}

/// Adapter so a borrowed `&dbx_api::Value` can be bound as a rusqlite
/// parameter without cloning `dbx-api`'s type into a local one. Values are
/// always bound this way — never spliced into SQL text (design §3.8).
pub(crate) struct SqlParam<'a>(pub &'a Value);

impl ToSql for SqlParam<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        use rusqlite::types::Value as SqlV;
        let mapped = match self.0 {
            Value::Null | Value::Absent => SqlV::Null,
            Value::Bool(b) => SqlV::Integer(i64::from(*b)),
            Value::I64(i) => SqlV::Integer(*i),
            Value::U64(u) => {
                let i = i64::try_from(*u).map_err(|_| {
                    rusqlite::Error::ToSqlConversionFailure(
                        format!("u64 {u} exceeds SQLite INTEGER range (i64::MAX)").into(),
                    )
                })?;
                SqlV::Integer(i)
            }
            Value::F64(f) => SqlV::Real(*f),
            Value::Decimal(s) | Value::Str(s) | Value::Json(s) => SqlV::Text(s.to_string()),
            Value::Bytes(b) => SqlV::Blob(b.to_vec()),
            Value::Date(days) => SqlV::Integer(i64::from(*days)),
            Value::Time { nanos } => SqlV::Integer(*nanos),
            Value::Timestamp { micros, .. } => SqlV::Integer(*micros),
            Value::Uuid(bytes) => SqlV::Blob(bytes.to_vec()),
            Value::Unsupported { raw, .. } => SqlV::Blob(raw.to_vec()),
            other => {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    format!("value has no SQLite parameter binding: {other:?}").into(),
                ));
            }
        };
        Ok(ToSqlOutput::Owned(mapped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("users").unwrap(), "\"users\"");
        assert_eq!(quote_ident("weird\"name").unwrap(), "\"weird\"\"name\"");
    }

    #[test]
    fn quote_ident_rejects_nul_and_empty() {
        assert!(quote_ident("").is_err());
        assert!(quote_ident("a\0b").is_err());
    }

    // "every storage-class Value mapping" — one test per SQLite storage class.
    #[test]
    fn maps_null() {
        assert_eq!(sqlite_value_to_dbx(ValueRef::Null, None), Value::Null);
    }

    #[test]
    fn maps_integer() {
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Integer(42), None),
            Value::I64(42)
        );
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Integer(-1), Some("INT")),
            Value::I64(-1)
        );
    }

    #[test]
    fn maps_integer_boolean_decl_to_bool() {
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Integer(1), Some("BOOLEAN")),
            Value::Bool(true)
        );
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Integer(0), Some("boolean")),
            Value::Bool(false)
        );
        // Boolean decl but a value outside {0,1}: honest INTEGER, not a lie.
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Integer(7), Some("BOOLEAN")),
            Value::I64(7)
        );
    }

    #[test]
    fn maps_real() {
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Real(3.5), None),
            Value::F64(3.5)
        );
    }

    #[test]
    fn maps_text() {
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Text(b"hello"), None),
            Value::Str(Arc::from("hello"))
        );
    }

    #[test]
    fn maps_text_invalid_utf8_to_unsupported_without_losing_bytes() {
        let raw: &[u8] = &[0xff, 0xfe, 0x00, 0x41];
        let v = sqlite_value_to_dbx(ValueRef::Text(raw), None);
        match v {
            Value::Unsupported { raw: got, .. } => assert_eq!(&got[..], raw),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn maps_blob() {
        let bytes = b"\x00\x01\x02";
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Blob(bytes), None),
            Value::Bytes(Bytes::copy_from_slice(bytes))
        );
    }

    #[test]
    fn date_time_decl_types_keep_storage_repr() {
        // Date/time decl types only steer the *schema's* LogicalType hint;
        // the actual cell value is never rewritten away from its storage class.
        assert_eq!(logical_type_for_decl(Some("DATE")), LogicalType::Date);
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Text(b"2026-08-02"), Some("DATE")),
            Value::Str(Arc::from("2026-08-02")),
            "DATE-declared column still yields the honest stored TEXT, not a parsed Date value"
        );
        assert_eq!(
            logical_type_for_decl(Some("TIMESTAMP")),
            LogicalType::Timestamp
        );
        assert_eq!(
            sqlite_value_to_dbx(ValueRef::Integer(1_700_000_000), Some("TIMESTAMP")),
            Value::I64(1_700_000_000),
            "TIMESTAMP-declared column still yields the honest stored INTEGER"
        );
    }

    #[test]
    fn logical_type_affinity_rules() {
        assert_eq!(logical_type_for_decl(Some("INTEGER")), LogicalType::I64);
        assert_eq!(logical_type_for_decl(Some("BIGINT")), LogicalType::I64);
        assert_eq!(logical_type_for_decl(Some("VARCHAR(20)")), LogicalType::Str);
        assert_eq!(logical_type_for_decl(Some("TEXT")), LogicalType::Str);
        assert_eq!(logical_type_for_decl(Some("BLOB")), LogicalType::Bytes);
        assert_eq!(logical_type_for_decl(None), LogicalType::Bytes);
        assert_eq!(logical_type_for_decl(Some("REAL")), LogicalType::F64);
        assert_eq!(logical_type_for_decl(Some("DOUBLE")), LogicalType::F64);
        assert_eq!(
            logical_type_for_decl(Some("NUMERIC(10,2)")),
            LogicalType::Unknown,
            "NUMERIC affinity is genuinely ambiguous — never guessed"
        );
    }
}
