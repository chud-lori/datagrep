//! MySQL wire values → `datagrep_api::Value` (design §3.1's honest mapping,
//! risk #4: DECIMAL never rides through f64).
//!
//! Decoding is driven by the column metadata, not the wire variant, because
//! the two protocols disagree about representation: the binary (prepared)
//! protocol delivers typed `mysql_async::Value` variants, while the text
//! protocol delivers nearly everything as `Value::Bytes`. Every arm below
//! therefore accepts both the typed form and the textual form.
//!
//! Timezone honesty: `TIMESTAMP` columns are UTC-normalized by the server
//! and rendered in the *session* time zone — this driver pins the session to
//! `+00:00` at connect (see `driver.rs`), so a decoded `TIMESTAMP` really is
//! UTC and is tagged `TzSpec::Utc`. `DATETIME` has no timezone semantics at
//! all and is tagged `TzSpec::Naive`. The two are never conflated.

use std::sync::Arc;

use bytes::Bytes;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::Column;
use mysql_async::Value as MyValue;

use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType};
use datagrep_api::value::{TzSpec, Value};

/// Is this column the conventional MySQL boolean — signed `TINYINT(1)`?
fn is_bool_column(col: &Column) -> bool {
    col.column_type() == ColumnType::MYSQL_TYPE_TINY
        && col.column_length() == 1
        && !col.flags().contains(ColumnFlags::UNSIGNED_FLAG)
}

/// Is this string-ish column binary (BLOB/BINARY/VARBINARY) rather than text?
/// Character set 63 is MySQL's `binary` charset — the reliable signal; the
/// BINARY_FLAG alone is also set on some text collations (`*_bin`).
fn is_binary_column(col: &Column) -> bool {
    col.character_set() == 63
}

/// The engine-neutral type of a column (schema-level mirror of the per-cell
/// decode below).
pub fn logical_type_of(col: &Column) -> LogicalType {
    use ColumnType::*;
    match col.column_type() {
        MYSQL_TYPE_NULL => LogicalType::Null,
        MYSQL_TYPE_TINY if is_bool_column(col) => LogicalType::Bool,
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG => {
            if col.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
                LogicalType::U64
            } else {
                LogicalType::I64
            }
        }
        MYSQL_TYPE_YEAR => LogicalType::I64,
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => LogicalType::F64,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => LogicalType::Decimal,
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => LogicalType::Date,
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => LogicalType::Time,
        MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2
        | MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2 => LogicalType::Timestamp,
        MYSQL_TYPE_JSON => LogicalType::Json,
        MYSQL_TYPE_BIT => LogicalType::Bytes,
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET => LogicalType::Str,
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => {
            // ENUM/SET are transmitted as string types with a marker flag.
            if col.flags().contains(ColumnFlags::ENUM_FLAG)
                || col.flags().contains(ColumnFlags::SET_FLAG)
            {
                LogicalType::Str
            } else if is_binary_column(col) {
                LogicalType::Bytes
            } else {
                LogicalType::Str
            }
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if is_binary_column(col) {
                LogicalType::Bytes
            } else {
                LogicalType::Str // TEXT family shares the BLOB wire types
            }
        }
        MYSQL_TYPE_GEOMETRY => LogicalType::Unknown,
        _ => LogicalType::Unknown,
    }
}

/// A human-readable native type name for the inspector (`FieldDef::native_type`
/// — "what the server said, not what we mapped it to"). Approximate but
/// truthful: built from the wire type plus the unsigned/charset facts the
/// protocol actually carries.
pub fn native_type_name(col: &Column) -> String {
    use ColumnType::*;
    let base = match col.column_type() {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_TINY => {
            if is_bool_column(col) {
                "tinyint(1)"
            } else {
                "tinyint"
            }
        }
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_TINY_BLOB => "tinyblob",
        MYSQL_TYPE_MEDIUM_BLOB => "mediumblob",
        MYSQL_TYPE_LONG_BLOB => "longblob",
        MYSQL_TYPE_BLOB => {
            if is_binary_column(col) {
                "blob"
            } else {
                "text"
            }
        }
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
            if col.flags().contains(ColumnFlags::ENUM_FLAG) {
                "enum"
            } else if col.flags().contains(ColumnFlags::SET_FLAG) {
                "set"
            } else if is_binary_column(col) {
                "varbinary"
            } else {
                "varchar"
            }
        }
        MYSQL_TYPE_STRING => {
            if col.flags().contains(ColumnFlags::ENUM_FLAG) {
                "enum"
            } else if col.flags().contains(ColumnFlags::SET_FLAG) {
                "set"
            } else if is_binary_column(col) {
                "binary"
            } else {
                "char"
            }
        }
        MYSQL_TYPE_GEOMETRY => "geometry",
        other => return format!("{other:?}"),
    };
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG)
        && matches!(
            col.column_type(),
            MYSQL_TYPE_TINY
                | MYSQL_TYPE_SHORT
                | MYSQL_TYPE_INT24
                | MYSQL_TYPE_LONG
                | MYSQL_TYPE_LONGLONG
                | MYSQL_TYPE_DECIMAL
                | MYSQL_TYPE_NEWDECIMAL
        );
    if unsigned {
        format!("{base} unsigned")
    } else {
        base.to_string()
    }
}

/// Build a [`FieldDef`] from result-set column metadata. Nullability, key and
/// auto-increment facts are all carried by the MySQL column definition
/// packet, unlike Postgres's RowDescription — so this driver can set them
/// honestly per result column.
pub fn field_def_of(col: &Column) -> FieldDef {
    let mut flags = FieldFlags::empty();
    if !col.flags().contains(ColumnFlags::NOT_NULL_FLAG) {
        flags |= FieldFlags::NULLABLE;
    }
    if col.flags().contains(ColumnFlags::PRI_KEY_FLAG) {
        flags |= FieldFlags::PRIMARY_KEY;
    }
    if col.flags().contains(ColumnFlags::UNIQUE_KEY_FLAG) {
        flags |= FieldFlags::UNIQUE;
    }
    if col.flags().contains(ColumnFlags::AUTO_INCREMENT_FLAG) {
        flags |= FieldFlags::AUTO_GENERATED;
    }
    FieldDef {
        name: Arc::from(col.name_str().as_ref()),
        logical: logical_type_of(col),
        flags,
        native_type: Some(Arc::from(native_type_name(col))),
    }
}

/// Days from the Unix epoch for a civil date (Howard Hinnant's algorithm) —
/// no chrono dependency for one conversion.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from((m + 9) % 12); // Mar=0..Feb=11
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

fn timestamp_micros(y: i32, mo: u32, d: u32, h: i64, mi: i64, s: i64, us: i64) -> i64 {
    days_from_civil(y, mo, d) * MICROS_PER_DAY
        + h * 3_600_000_000
        + mi * 60_000_000
        + s * 1_000_000
        + us
}

fn tz_for(col_type: ColumnType) -> TzSpec {
    match col_type {
        // TIMESTAMP is UTC-normalized server-side and rendered in the session
        // tz, which this driver pins to +00:00 at connect.
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => TzSpec::Utc,
        // DATETIME stores exactly what was written; no timezone semantics.
        _ => TzSpec::Naive,
    }
}

/// The all-zero DATE/DATETIME (`0000-00-00[ 00:00:00]`) MySQL emits under
/// permissive sql_modes. It is not a representable date; it survives as
/// `Unsupported` with its text intact rather than being bent into a real day.
fn zero_date_value(type_name: &str, text: &str) -> Value {
    Value::Unsupported {
        type_name: Arc::from(type_name),
        raw: Bytes::copy_from_slice(text.as_bytes()),
        display: Arc::from(text),
    }
}

fn unsupported(col: &Column, raw: &[u8]) -> Value {
    Value::Unsupported {
        type_name: Arc::from(native_type_name(col)),
        raw: Bytes::copy_from_slice(raw),
        display: Arc::from(String::from_utf8_lossy(raw).into_owned()),
    }
}

/// Parse `YYYY-MM-DD` (returns y, m, d).
fn parse_date_text(s: &str) -> Option<(i32, u32, u32)> {
    let mut it = s.splitn(3, '-');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    Some((y, m, d))
}

/// Parse `hh:mm:ss[.ffffff]` into (h, m, s, micros).
fn parse_time_text(s: &str) -> Option<(i64, i64, i64, i64)> {
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    let mut it = hms.splitn(3, ':');
    let h = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let sec = it.next()?.parse().ok()?;
    let us = if frac.is_empty() {
        0
    } else {
        // Right-pad the fraction to microseconds ("5" == 500000 µs).
        let padded = format!("{frac:0<6}");
        padded.get(..6)?.parse().ok()?
    };
    Some((h, m, sec, us))
}

/// Decode one cell. `col` is the result-set column the cell belongs to.
pub fn decode_value(col: &Column, v: MyValue) -> Value {
    use ColumnType::*;
    let ct = col.column_type();
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);

    match v {
        MyValue::NULL => Value::Null,
        MyValue::Int(i) => {
            if is_bool_column(col) {
                Value::Bool(i != 0)
            } else if ct == MYSQL_TYPE_YEAR {
                // YEAR is flagged UNSIGNED on the wire but is a year number,
                // mapped to I64 by contract.
                Value::I64(i)
            } else if unsigned && i >= 0 {
                Value::U64(i as u64)
            } else {
                Value::I64(i)
            }
        }
        MyValue::UInt(u) => {
            if is_bool_column(col) {
                Value::Bool(u != 0)
            } else if ct == MYSQL_TYPE_YEAR {
                Value::I64(u as i64)
            } else if unsigned {
                Value::U64(u)
            } else if let Ok(i) = i64::try_from(u) {
                Value::I64(i)
            } else {
                Value::U64(u)
            }
        }
        MyValue::Float(f) => Value::F64(f64::from(f)),
        MyValue::Double(d) => Value::F64(d),
        MyValue::Date(y, mo, d, h, mi, s, us) => {
            if y == 0 && mo == 0 && d == 0 {
                // "0000-00-00": not a real calendar day.
                return zero_date_value(&native_type_name(col), &format!("{y:04}-{mo:02}-{d:02}"));
            }
            match ct {
                MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => {
                    Value::Date(days_from_civil(i32::from(y), u32::from(mo), u32::from(d)) as i32)
                }
                _ => Value::Timestamp {
                    micros: timestamp_micros(
                        i32::from(y),
                        u32::from(mo),
                        u32::from(d),
                        i64::from(h),
                        i64::from(mi),
                        i64::from(s),
                        i64::from(us),
                    ),
                    tz: tz_for(ct),
                },
            }
        }
        MyValue::Time(neg, days, h, m, s, us) => {
            let nanos = (i64::from(days) * 86_400
                + i64::from(h) * 3_600
                + i64::from(m) * 60
                + i64::from(s))
                * 1_000_000_000
                + i64::from(us) * 1_000;
            Value::Time {
                nanos: if neg { -nanos } else { nanos },
            }
        }
        MyValue::Bytes(raw) => decode_bytes(col, raw),
    }
}

/// The text-protocol (and string-typed) side of decoding: everything arrives
/// as bytes and the column type decides what those bytes mean.
fn decode_bytes(col: &Column, raw: Vec<u8>) -> Value {
    use ColumnType::*;
    let ct = col.column_type();
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);

    // Helper: bytes as UTF-8 or bail to Unsupported (never lose bytes).
    macro_rules! text {
        () => {
            match std::str::from_utf8(&raw) {
                Ok(s) => s,
                Err(_) => return unsupported(col, &raw),
            }
        };
    }

    match ct {
        MYSQL_TYPE_NULL => Value::Null,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
            // Design risk #4: DECIMAL/NUMERIC is string-backed, NEVER f64.
            let s = text!();
            Value::Decimal(Arc::from(s))
        }
        MYSQL_TYPE_JSON => {
            // Raw JSON text, never re-serialized (key order and number
            // precision are data).
            let s = text!();
            Value::Json(Arc::from(s))
        }
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            let s = text!();
            if ct == MYSQL_TYPE_TINY && is_bool_column(col) {
                return match s.parse::<i64>() {
                    Ok(i) => Value::Bool(i != 0),
                    Err(_) => unsupported(col, &raw),
                };
            }
            // YEAR is flagged UNSIGNED on the wire but maps to I64.
            if unsigned && ct != MYSQL_TYPE_YEAR {
                match s.parse::<u64>() {
                    Ok(u) => Value::U64(u),
                    Err(_) => unsupported(col, &raw),
                }
            } else {
                match s.parse::<i64>() {
                    Ok(i) => Value::I64(i),
                    Err(_) => unsupported(col, &raw),
                }
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = text!();
            match s.parse::<f64>() {
                Ok(f) => Value::F64(f),
                Err(_) => unsupported(col, &raw),
            }
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => {
            let s = text!();
            if s.starts_with("0000-00-00") {
                return zero_date_value(&native_type_name(col), s);
            }
            match parse_date_text(s) {
                Some((y, m, d)) => Value::Date(days_from_civil(y, m, d) as i32),
                None => unsupported(col, &raw),
            }
        }
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => {
            let s = text!();
            if s.starts_with("0000-00-00") {
                return zero_date_value(&native_type_name(col), s);
            }
            let (date_part, time_part) = match s.split_once(' ') {
                Some((a, b)) => (a, b),
                None => (s, "00:00:00"),
            };
            match (parse_date_text(date_part), parse_time_text(time_part)) {
                (Some((y, mo, d)), Some((h, mi, sec, us))) => Value::Timestamp {
                    micros: timestamp_micros(y, mo, d, h, mi, sec, us),
                    tz: tz_for(ct),
                },
                _ => unsupported(col, &raw),
            }
        }
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => {
            let s = text!();
            let (neg, body) = match s.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, s),
            };
            match parse_time_text(body) {
                Some((h, m, sec, us)) => {
                    let nanos = (h * 3_600 + m * 60 + sec) * 1_000_000_000 + us * 1_000;
                    Value::Time {
                        nanos: if neg { -nanos } else { nanos },
                    }
                }
                None => unsupported(col, &raw),
            }
        }
        MYSQL_TYPE_BIT => Value::Bytes(Bytes::from(raw)),
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET => match String::from_utf8(raw) {
            Ok(s) => Value::Str(Arc::from(s)),
            Err(e) => unsupported(col, e.as_bytes()),
        },
        MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB => {
            let enumish = col.flags().contains(ColumnFlags::ENUM_FLAG)
                || col.flags().contains(ColumnFlags::SET_FLAG);
            if !enumish && is_binary_column(col) {
                Value::Bytes(Bytes::from(raw))
            } else {
                match String::from_utf8(raw) {
                    Ok(s) => Value::Str(Arc::from(s)),
                    Err(e) => unsupported(col, e.as_bytes()),
                }
            }
        }
        // GEOMETRY and anything future/unknown: never lose bytes.
        _ => unsupported(col, &raw),
    }
}

/// Civil date from days since the Unix epoch (inverse of
/// [`days_from_civil`]; Hinnant's `civil_from_days`).
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    ((y + i64::from(m <= 2)) as i32, m as u32, d as u32)
}

/// Convert a seam [`Value`] into a bindable `mysql_async::Value` — the
/// parameter side of design §3.8: this is the ONLY place request values are
/// encoded, and it produces protocol-level bound parameters, never SQL text.
pub fn to_my_value(v: &Value) -> Result<MyValue, datagrep_api::DbError> {
    use datagrep_api::DbError;
    Ok(match v {
        Value::Null => MyValue::NULL,
        Value::Bool(b) => MyValue::Int(i64::from(*b)),
        Value::I64(i) => MyValue::Int(*i),
        Value::U64(u) => MyValue::UInt(*u),
        Value::F64(f) => MyValue::Double(*f),
        // Decimal binds as its exact text — the server parses it with
        // DECIMAL semantics; f64 never enters the picture (risk #4).
        Value::Decimal(s) => MyValue::Bytes(s.as_bytes().to_vec()),
        Value::Str(s) => MyValue::Bytes(s.as_bytes().to_vec()),
        Value::Bytes(b) => MyValue::Bytes(b.to_vec()),
        Value::Json(s) => MyValue::Bytes(s.as_bytes().to_vec()),
        Value::Date(days) => {
            let (y, m, d) = civil_from_days(i64::from(*days));
            let y = u16::try_from(y).map_err(|_| DbError::Unsupported {
                feature: format!("date out of MySQL's range: epoch day {days}"),
            })?;
            MyValue::Date(y, m as u8, d as u8, 0, 0, 0, 0)
        }
        Value::Time { nanos } => {
            let neg = *nanos < 0;
            let total_us = nanos.unsigned_abs() / 1_000;
            let us = (total_us % 1_000_000) as u32;
            let total_s = total_us / 1_000_000;
            let s = (total_s % 60) as u8;
            let m = ((total_s / 60) % 60) as u8;
            let total_h = total_s / 3_600;
            let days = (total_h / 24) as u32;
            let h = (total_h % 24) as u8;
            MyValue::Time(neg, days, h, m, s, us)
        }
        Value::Timestamp { micros, tz } => {
            // Utc and Naive both bind as their wall-clock reading (the
            // session is pinned to +00:00, so a Utc instant's wall clock IS
            // its UTC reading). A Named/Offset zone would need a calendar
            // conversion this driver refuses to guess at.
            match tz {
                TzSpec::Utc | TzSpec::Naive => {}
                other => {
                    return Err(DbError::Unsupported {
                        feature: format!("binding a timestamp with tz {other:?} is not supported"),
                    })
                }
            }
            let days = micros.div_euclid(MICROS_PER_DAY);
            let in_day = micros.rem_euclid(MICROS_PER_DAY);
            let (y, mo, d) = civil_from_days(days);
            let y = u16::try_from(y).map_err(|_| DbError::Unsupported {
                feature: format!("timestamp out of MySQL's range: {micros} µs"),
            })?;
            let us = (in_day % 1_000_000) as u32;
            let total_s = in_day / 1_000_000;
            let s = (total_s % 60) as u8;
            let mi = ((total_s / 60) % 60) as u8;
            let h = (total_s / 3_600) as u8;
            MyValue::Date(y, mo as u8, d as u8, h, mi, s, us)
        }
        Value::Uuid(bytes) => {
            // MySQL has no UUID type; bind the canonical hyphenated text.
            let b = bytes;
            let s = format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
            );
            MyValue::Bytes(s.into_bytes())
        }
        other => {
            return Err(datagrep_api::DbError::Unsupported {
                feature: format!(
                    "cannot bind a {:?} value as a MySQL parameter",
                    other.logical_type()
                ),
            })
        }
    })
}

/// Map an `information_schema.columns.data_type` string to a [`LogicalType`]
/// (the catalog path has no wire `Column` to inspect).
pub fn logical_type_of_data_type(data_type: &str, column_type: &str) -> LogicalType {
    match data_type.to_ascii_lowercase().as_str() {
        "tinyint" => {
            if column_type.to_ascii_lowercase().starts_with("tinyint(1)")
                && !column_type.to_ascii_lowercase().contains("unsigned")
            {
                LogicalType::Bool
            } else if column_type.to_ascii_lowercase().contains("unsigned") {
                LogicalType::U64
            } else {
                LogicalType::I64
            }
        }
        "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            if column_type.to_ascii_lowercase().contains("unsigned") {
                LogicalType::U64
            } else {
                LogicalType::I64
            }
        }
        "year" => LogicalType::I64,
        "float" | "double" | "real" => LogicalType::F64,
        "decimal" | "numeric" | "dec" => LogicalType::Decimal,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            LogicalType::Str
        }
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            LogicalType::Bytes
        }
        "date" => LogicalType::Date,
        "time" => LogicalType::Time,
        "datetime" | "timestamp" => LogicalType::Timestamp,
        "json" => LogicalType::Json,
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geomcollection" | "geometrycollection" => LogicalType::Geo,
        _ => LogicalType::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(ct: ColumnType) -> Column {
        Column::new(ct).with_name(b"c")
    }

    fn col_flags(ct: ColumnType, flags: ColumnFlags) -> Column {
        Column::new(ct).with_name(b"c").with_flags(flags)
    }

    #[test]
    fn tinyint1_signed_is_bool_wider_tinyint_is_int() {
        let boolean = Column::new(ColumnType::MYSQL_TYPE_TINY)
            .with_name(b"b")
            .with_column_length(1);
        assert_eq!(logical_type_of(&boolean), LogicalType::Bool);
        assert_eq!(decode_value(&boolean, MyValue::Int(1)), Value::Bool(true));
        assert_eq!(decode_value(&boolean, MyValue::Int(0)), Value::Bool(false));
        // Text protocol delivers the same cell as b"1".
        assert_eq!(
            decode_value(&boolean, MyValue::Bytes(b"1".to_vec())),
            Value::Bool(true)
        );

        // tinyint(4) — the default display width — is an integer, not a bool.
        let tiny4 = Column::new(ColumnType::MYSQL_TYPE_TINY)
            .with_name(b"t")
            .with_column_length(4);
        assert_eq!(logical_type_of(&tiny4), LogicalType::I64);
        assert_eq!(decode_value(&tiny4, MyValue::Int(1)), Value::I64(1));

        // unsigned tinyint(1) is NOT the boolean convention.
        let utiny1 = Column::new(ColumnType::MYSQL_TYPE_TINY)
            .with_name(b"u")
            .with_column_length(1)
            .with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(logical_type_of(&utiny1), LogicalType::U64);
    }

    #[test]
    fn unsigned_bigint_never_overflows_negative() {
        let c = col_flags(ColumnType::MYSQL_TYPE_LONGLONG, ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(logical_type_of(&c), LogicalType::U64);
        assert_eq!(
            decode_value(&c, MyValue::UInt(u64::MAX)),
            Value::U64(u64::MAX)
        );
        // Text protocol form of the same value.
        assert_eq!(
            decode_value(&c, MyValue::Bytes(u64::MAX.to_string().into_bytes())),
            Value::U64(u64::MAX)
        );
        // Signed bigint stays I64.
        let s = col(ColumnType::MYSQL_TYPE_LONGLONG);
        assert_eq!(decode_value(&s, MyValue::Int(-5)), Value::I64(-5));
    }

    #[test]
    fn decimal_is_string_backed_never_f64() {
        let c = col(ColumnType::MYSQL_TYPE_NEWDECIMAL);
        assert_eq!(logical_type_of(&c), LogicalType::Decimal);
        // A value that f64 cannot represent exactly — trailing precision is
        // data and must survive verbatim.
        let v = decode_value(
            &c,
            MyValue::Bytes(b"12345678901234567890.123456789012345678".to_vec()),
        );
        assert_eq!(
            v,
            Value::Decimal(Arc::from("12345678901234567890.123456789012345678"))
        );
        assert!(
            !matches!(v, Value::F64(_)),
            "DECIMAL through f64 is design risk #4"
        );
        // Trailing zeros are data too.
        assert_eq!(
            decode_value(&c, MyValue::Bytes(b"1.10".to_vec())),
            Value::Decimal(Arc::from("1.10"))
        );
    }

    #[test]
    fn float_and_double_are_f64() {
        let c = col(ColumnType::MYSQL_TYPE_DOUBLE);
        assert_eq!(decode_value(&c, MyValue::Double(1.5)), Value::F64(1.5));
        let f = col(ColumnType::MYSQL_TYPE_FLOAT);
        assert_eq!(decode_value(&f, MyValue::Float(2.5)), Value::F64(2.5));
        assert_eq!(
            decode_value(&f, MyValue::Bytes(b"2.5".to_vec())),
            Value::F64(2.5)
        );
    }

    #[test]
    fn date_maps_to_epoch_days() {
        let c = col(ColumnType::MYSQL_TYPE_DATE);
        assert_eq!(logical_type_of(&c), LogicalType::Date);
        // Binary protocol.
        assert_eq!(
            decode_value(&c, MyValue::Date(1970, 1, 1, 0, 0, 0, 0)),
            Value::Date(0)
        );
        assert_eq!(
            decode_value(&c, MyValue::Date(1969, 12, 31, 0, 0, 0, 0)),
            Value::Date(-1)
        );
        // Text protocol; 2024-03-01 is 19783 days after the epoch.
        assert_eq!(
            decode_value(&c, MyValue::Bytes(b"2024-03-01".to_vec())),
            Value::Date(19783)
        );
    }

    #[test]
    fn timestamp_is_utc_datetime_is_naive_never_conflated() {
        let ts = col(ColumnType::MYSQL_TYPE_TIMESTAMP);
        let dt = col(ColumnType::MYSQL_TYPE_DATETIME);
        let from_ts = decode_value(&ts, MyValue::Date(2024, 1, 2, 3, 4, 5, 6));
        let from_dt = decode_value(&dt, MyValue::Date(2024, 1, 2, 3, 4, 5, 6));
        let expected_micros = days_from_civil(2024, 1, 2) * 86_400_000_000
            + 3 * 3_600_000_000
            + 4 * 60_000_000
            + 5 * 1_000_000
            + 6;
        assert_eq!(
            from_ts,
            Value::Timestamp {
                micros: expected_micros,
                tz: TzSpec::Utc
            }
        );
        assert_eq!(
            from_dt,
            Value::Timestamp {
                micros: expected_micros,
                tz: TzSpec::Naive
            }
        );
        assert_ne!(from_ts, from_dt, "the tz qualifier is part of the value");
        // Text protocol with fractional seconds.
        assert_eq!(
            decode_value(&ts, MyValue::Bytes(b"1970-01-01 00:00:01.5".to_vec())),
            Value::Timestamp {
                micros: 1_500_000,
                tz: TzSpec::Utc
            }
        );
    }

    #[test]
    fn zero_date_survives_as_unsupported_not_a_fake_day() {
        let c = col(ColumnType::MYSQL_TYPE_DATE);
        let v = decode_value(&c, MyValue::Bytes(b"0000-00-00".to_vec()));
        match v {
            Value::Unsupported { raw, .. } => assert_eq!(&raw[..], b"0000-00-00"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn time_is_signed_duration_nanos() {
        let c = col(ColumnType::MYSQL_TYPE_TIME);
        assert_eq!(logical_type_of(&c), LogicalType::Time);
        assert_eq!(
            decode_value(&c, MyValue::Time(false, 0, 1, 2, 3, 4)),
            Value::Time {
                nanos: (3_600 + 2 * 60 + 3) * 1_000_000_000 + 4_000
            }
        );
        // MySQL TIME goes beyond 24h and can be negative — it is a duration.
        assert_eq!(
            decode_value(&c, MyValue::Time(true, 1, 2, 0, 0, 0)),
            Value::Time {
                nanos: -(26 * 3_600) * 1_000_000_000
            }
        );
        assert_eq!(
            decode_value(&c, MyValue::Bytes(b"-838:59:59".to_vec())),
            Value::Time {
                nanos: -((838 * 3_600 + 59 * 60 + 59) * 1_000_000_000)
            }
        );
    }

    #[test]
    fn year_is_i64_even_though_the_wire_flags_it_unsigned() {
        // Real servers set UNSIGNED_FLAG on YEAR columns; the mapping
        // contract is still I64.
        let c = col_flags(ColumnType::MYSQL_TYPE_YEAR, ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(logical_type_of(&c), LogicalType::I64);
        assert_eq!(
            decode_value(&c, MyValue::Bytes(b"2024".to_vec())),
            Value::I64(2024)
        );
        assert_eq!(decode_value(&c, MyValue::Int(2024)), Value::I64(2024));
        assert_eq!(decode_value(&c, MyValue::UInt(2024)), Value::I64(2024));
    }

    #[test]
    fn json_is_raw_text() {
        let c = col(ColumnType::MYSQL_TYPE_JSON);
        assert_eq!(logical_type_of(&c), LogicalType::Json);
        let text = br#"{"b":1,"a":2}"#;
        assert_eq!(
            decode_value(&c, MyValue::Bytes(text.to_vec())),
            Value::Json(Arc::from(r#"{"b":1,"a":2}"#)),
            "key order must survive — raw text, never re-serialized"
        );
    }

    #[test]
    fn bit_is_bytes() {
        let c = col(ColumnType::MYSQL_TYPE_BIT);
        assert_eq!(logical_type_of(&c), LogicalType::Bytes);
        assert_eq!(
            decode_value(&c, MyValue::Bytes(vec![0b1010_0001])),
            Value::Bytes(Bytes::from_static(&[0b1010_0001]))
        );
    }

    #[test]
    fn text_vs_blob_split_on_binary_charset() {
        // TEXT and BLOB share wire types; charset 63 (binary) is the split.
        let text_col = Column::new(ColumnType::MYSQL_TYPE_BLOB)
            .with_name(b"t")
            .with_character_set(224); // utf8mb4
        let blob_col = Column::new(ColumnType::MYSQL_TYPE_BLOB)
            .with_name(b"b")
            .with_character_set(63)
            .with_flags(ColumnFlags::BINARY_FLAG);
        assert_eq!(logical_type_of(&text_col), LogicalType::Str);
        assert_eq!(logical_type_of(&blob_col), LogicalType::Bytes);
        assert_eq!(
            decode_value(&text_col, MyValue::Bytes(b"hi".to_vec())),
            Value::Str(Arc::from("hi"))
        );
        assert_eq!(
            decode_value(&blob_col, MyValue::Bytes(vec![0, 159, 146])),
            Value::Bytes(Bytes::from_static(&[0, 159, 146])),
            "non-UTF-8 payloads must not be lossy-converted"
        );
    }

    #[test]
    fn enum_and_set_are_str() {
        let e = Column::new(ColumnType::MYSQL_TYPE_STRING)
            .with_name(b"e")
            .with_flags(ColumnFlags::ENUM_FLAG);
        assert_eq!(logical_type_of(&e), LogicalType::Str);
        assert_eq!(
            decode_value(&e, MyValue::Bytes(b"active".to_vec())),
            Value::Str(Arc::from("active"))
        );
        let s = Column::new(ColumnType::MYSQL_TYPE_STRING)
            .with_name(b"s")
            .with_flags(ColumnFlags::SET_FLAG);
        assert_eq!(logical_type_of(&s), LogicalType::Str);
    }

    #[test]
    fn unknown_type_never_loses_bytes() {
        let c = col(ColumnType::MYSQL_TYPE_GEOMETRY);
        let payload = vec![0x01, 0x02, 0xff, 0xfe];
        match decode_value(&c, MyValue::Bytes(payload.clone())) {
            Value::Unsupported { type_name, raw, .. } => {
                assert_eq!(&*type_name, "geometry");
                assert_eq!(&raw[..], &payload[..], "raw bytes preserved");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn days_from_civil_reference_points() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2024, 2, 29), 19782); // leap day exists
    }

    #[test]
    fn civil_from_days_round_trips() {
        for days in [-1000, -1, 0, 1, 19782, 19783, 40000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
    }

    #[test]
    fn params_bind_as_values_never_text() {
        assert_eq!(to_my_value(&Value::Null).unwrap(), MyValue::NULL);
        assert_eq!(to_my_value(&Value::Bool(true)).unwrap(), MyValue::Int(1));
        assert_eq!(to_my_value(&Value::I64(-7)).unwrap(), MyValue::Int(-7));
        assert_eq!(
            to_my_value(&Value::U64(u64::MAX)).unwrap(),
            MyValue::UInt(u64::MAX)
        );
        assert_eq!(
            to_my_value(&Value::Decimal(Arc::from("1.10"))).unwrap(),
            MyValue::Bytes(b"1.10".to_vec()),
            "decimal binds as exact text, not f64"
        );
        assert_eq!(
            to_my_value(&Value::Date(19783)).unwrap(),
            MyValue::Date(2024, 3, 1, 0, 0, 0, 0)
        );
        assert_eq!(
            to_my_value(&Value::Time {
                nanos: -((26 * 3_600) * 1_000_000_000)
            })
            .unwrap(),
            MyValue::Time(true, 1, 2, 0, 0, 0)
        );
        assert_eq!(
            to_my_value(&Value::Timestamp {
                micros: 1_500_000,
                tz: TzSpec::Utc
            })
            .unwrap(),
            MyValue::Date(1970, 1, 1, 0, 0, 1, 500_000)
        );
        // A zone-named timestamp is refused, not silently reinterpreted.
        assert!(to_my_value(&Value::Timestamp {
            micros: 0,
            tz: TzSpec::Named(Arc::from("Asia/Singapore"))
        })
        .is_err());
        // Structured values a MySQL parameter can't represent are refused.
        assert!(to_my_value(&Value::Absent).is_err());
        let uuid = to_my_value(&Value::Uuid([
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ]))
        .unwrap();
        assert_eq!(
            uuid,
            MyValue::Bytes(b"550e8400-e29b-41d4-a716-446655440000".to_vec())
        );
    }

    #[test]
    fn field_def_carries_nullability_and_keys() {
        let c = Column::new(ColumnType::MYSQL_TYPE_LONG)
            .with_name(b"id")
            .with_flags(
                ColumnFlags::NOT_NULL_FLAG
                    | ColumnFlags::PRI_KEY_FLAG
                    | ColumnFlags::AUTO_INCREMENT_FLAG,
            );
        let f = field_def_of(&c);
        assert_eq!(&*f.name, "id");
        assert!(!f.flags.contains(FieldFlags::NULLABLE));
        assert!(f.flags.contains(FieldFlags::PRIMARY_KEY));
        assert!(f.flags.contains(FieldFlags::AUTO_GENERATED));
        assert_eq!(f.logical, LogicalType::I64);
        assert_eq!(f.native_type.as_deref(), Some("int"));
    }

    #[test]
    fn information_schema_data_type_mapping() {
        use LogicalType as L;
        for (dt, ct, want) in [
            ("tinyint", "tinyint(1)", L::Bool),
            ("tinyint", "tinyint(1) unsigned", L::U64),
            ("tinyint", "tinyint(4)", L::I64),
            ("bigint", "bigint unsigned", L::U64),
            ("bigint", "bigint", L::I64),
            ("decimal", "decimal(38,10)", L::Decimal),
            ("varchar", "varchar(255)", L::Str),
            ("longblob", "longblob", L::Bytes),
            ("datetime", "datetime(6)", L::Timestamp),
            ("timestamp", "timestamp", L::Timestamp),
            ("date", "date", L::Date),
            ("time", "time", L::Time),
            ("json", "json", L::Json),
            ("year", "year", L::I64),
            ("bit", "bit(8)", L::Bytes),
            ("enum", "enum('a','b')", L::Str),
            ("geometry", "geometry", L::Geo),
            ("frobnicator", "frobnicator", L::Unknown),
        ] {
            assert_eq!(logical_type_of_data_type(dt, ct), want, "{dt} / {ct}");
        }
    }
}
