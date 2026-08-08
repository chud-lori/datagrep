//! One value model for every output format.
//!
//! `RowWindow` hands back two different shapes (`datagrep_core::store::WindowSlice`):
//! Arrow `RecordBatch` slices for `Shape::Table` (postgres/sqlite — this
//! build's only drivers) and `datagrep_api::Value` rows for `Shape::Documents`/
//! `Shape::Pairs` (no driver in this build produces those yet, but the type
//! has to be handled to compile against the real `RowWindow`). Rather than
//! writing separate table/json/csv logic for each, every cell is converted
//! once to a `datagrep_api::Value` ([`arrow_cell_to_value`] for Arrow, a direct
//! pass-through for docs) and every format renders from that one
//! representation via [`CellText`] or [`value_to_json`].
//!
//! # NULL vs Absent vs empty string (ticket: "the grid must be truthful")
//! [`CellText`] keeps the three states [`datagrep_core::convert::display_value`]
//! collapses to `None`/`Some(String)`: a stored `Value::Null` is
//! [`CellText::Null`], a genuinely missing field ([`Value::Absent`] — only
//! reachable through the document lane, since Arrow's validity bitmap cannot
//! represent it — design §3.2) is [`CellText::Absent`], and an empty string
//! is [`CellText::Text(String::new())`], which prints as nothing between
//! delimiters rather than as the word `NULL`.

use std::sync::Arc;

use arrow_array::types::Int32Type;
use arrow_array::{Array, DictionaryArray};
use arrow_schema::{DataType, TimeUnit};
use datagrep_api::{Bytes, TzSpec, Value};

/// A cell's renderable state, truthful about the three-way distinction the
/// ticket calls out — and about scalar *types*: booleans, integers, and
/// floats keep their identity to the JSON formats (`{"id":1}`, never
/// `{"id":"1"}`), and a `json`/`jsonb` cell keeps its raw text so the JSON
/// formats can pass it through verbatim instead of double-encoding it.
/// Decimals stay text deliberately (§risk-4 fidelity: JSON numbers are f64).
#[derive(Debug, Clone, PartialEq)]
pub enum CellText {
    Null,
    Absent,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    /// Raw JSON text from a `json`/`jsonb` column, verbatim from the server.
    Json(String),
    Text(String),
}

impl CellText {
    pub fn from_value(v: &Value) -> Self {
        match v {
            Value::Null => CellText::Null,
            Value::Absent => CellText::Absent,
            Value::Bool(b) => CellText::Bool(*b),
            Value::I64(n) => CellText::I64(*n),
            Value::U64(n) => CellText::U64(*n),
            Value::F64(n) => CellText::F64(*n),
            Value::Json(raw) => CellText::Json(raw.to_string()),
            other => {
                CellText::Text(datagrep_core::convert::display_value(other).unwrap_or_default())
            }
        }
    }

    /// The cell as display text — what `table`/`csv`/`tsv` print. `None` for
    /// NULL/Absent (those formats have their own rendering for that state).
    /// Matches `datagrep_core::convert::display_value` for every variant.
    pub fn display_text(&self) -> Option<std::borrow::Cow<'_, str>> {
        use std::borrow::Cow;
        match self {
            CellText::Null | CellText::Absent => None,
            CellText::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
            CellText::I64(n) => Some(Cow::Owned(n.to_string())),
            CellText::U64(n) => Some(Cow::Owned(n.to_string())),
            CellText::F64(n) => Some(Cow::Owned(n.to_string())),
            CellText::Json(s) | CellText::Text(s) => Some(Cow::Borrowed(s)),
        }
    }
}

/// Convert one Arrow cell to a [`Value`], so table/json/csv/ndjson all render
/// from the same representation regardless of which `WindowSlice` variant
/// produced it. Total by construction: an Arrow type this build's converter
/// never emits (see `datagrep_core::convert::ColKind`) becomes
/// `Value::Unsupported` rather than panicking — the same "never lose bytes,
/// never crash on a driver quirk" stance the design takes everywhere else.
pub fn arrow_cell_to_value(array: &dyn Array, row: usize) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }
    match array.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(downcast::<arrow_array::BooleanArray>(array).value(row)),
        DataType::Int64 => Value::I64(downcast::<arrow_array::Int64Array>(array).value(row)),
        DataType::UInt64 => Value::U64(downcast::<arrow_array::UInt64Array>(array).value(row)),
        DataType::Float64 => Value::F64(downcast::<arrow_array::Float64Array>(array).value(row)),
        DataType::Date32 => Value::Date(downcast::<arrow_array::Date32Array>(array).value(row)),
        DataType::Time64(TimeUnit::Nanosecond) => Value::Time {
            nanos: downcast::<arrow_array::Time64NanosecondArray>(array).value(row),
        },
        DataType::Timestamp(TimeUnit::Microsecond, tz) => Value::Timestamp {
            micros: downcast::<arrow_array::TimestampMicrosecondArray>(array).value(row),
            tz: tz_spec(tz.as_deref()),
        },
        DataType::FixedSizeBinary(16) => {
            let a = downcast::<arrow_array::FixedSizeBinaryArray>(array);
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(a.value(row));
            Value::Uuid(uuid)
        }
        DataType::Binary => {
            let a = downcast::<arrow_array::BinaryArray>(array);
            Value::Bytes(Bytes::copy_from_slice(a.value(row)))
        }
        DataType::Utf8 => Value::Str(Arc::from(
            downcast::<arrow_array::StringArray>(array).value(row),
        )),
        DataType::LargeUtf8 => Value::Str(Arc::from(
            downcast::<arrow_array::LargeStringArray>(array).value(row),
        )),
        DataType::Dictionary(key, value)
            if **key == DataType::Int32 && **value == DataType::Utf8 =>
        {
            let dict = downcast::<DictionaryArray<Int32Type>>(array);
            let keys = dict.keys();
            let values = dict
                .values()
                .as_any()
                .downcast_ref::<arrow_array::StringArray>();
            match values {
                Some(values) if !keys.is_null(row) => {
                    Value::Str(Arc::from(values.value(keys.value(row) as usize)))
                }
                _ => Value::Null,
            }
        }
        other => Value::Unsupported {
            type_name: Arc::from(format!("{other:?}")),
            raw: Bytes::new(),
            display: Arc::from("<unrenderable arrow type>"),
        },
    }
}

fn downcast<T: 'static>(array: &dyn Array) -> &T {
    array
        .as_any()
        .downcast_ref::<T>()
        .expect("arrow_cell_to_value's DataType match must agree with the concrete array type")
}

fn tz_spec(tz: Option<&str>) -> TzSpec {
    match tz {
        None => TzSpec::Naive,
        Some("UTC") => TzSpec::Utc,
        Some(name) => TzSpec::Named(Arc::from(name)),
    }
}

/// `Value` -> `serde_json::Value`, for `--format json`/`ndjson`. Lossless
/// where JSON allows it; anything JSON has no native type for (bytes, UUID,
/// decimal, timestamps, …) becomes its display text, same as the table
/// format, rather than a JSON type we'd have to invent and document.
pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null | Value::Absent => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::I64(n) => J::Number((*n).into()),
        Value::U64(n) => J::Number((*n).into()),
        Value::F64(n) => serde_json::Number::from_f64(*n)
            .map(J::Number)
            .unwrap_or(J::Null),
        Value::Array(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Document(doc) => J::Object(
            doc.iter()
                .map(|(k, v)| (k.to_string(), value_to_json(v)))
                .collect(),
        ),
        // A json/jsonb value nests as real JSON (it *is* JSON); only text
        // that fails to parse — a driver bug — falls back to a string.
        Value::Json(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| J::String(raw.to_string()))
        }
        // Decimal, Str, Bytes, dates/times, Uuid, Interval, Ref, Geo,
        // Vector, Unsupported: all render as their truthful display text.
        other => match datagrep_core::convert::display_value(other) {
            Some(s) => J::String(s),
            None => J::Null,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{Int64Builder, StringBuilder};
    use arrow_array::RecordBatch;
    use arrow_schema::{Field, Schema};
    use datagrep_api::Document;

    #[test]
    fn null_absent_and_empty_string_are_distinct() {
        assert_eq!(CellText::from_value(&Value::Null), CellText::Null);
        assert_eq!(CellText::from_value(&Value::Absent), CellText::Absent);
        assert_eq!(
            CellText::from_value(&Value::Str(Arc::from(""))),
            CellText::Text(String::new())
        );
        assert_ne!(CellText::Null, CellText::Absent);
        assert_ne!(CellText::Null, CellText::Text(String::new()));
        assert_ne!(CellText::Absent, CellText::Text(String::new()));
    }

    #[test]
    fn arrow_cell_round_trips_int_and_string_columns() {
        let mut ints = Int64Builder::new();
        ints.append_value(7);
        ints.append_null();
        let mut strs = StringBuilder::new();
        strs.append_value("hi");
        strs.append_null();
        let schema = Arc::new(Schema::new(vec![
            Field::new("i", DataType::Int64, true),
            Field::new("s", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(ints.finish()), Arc::new(strs.finish())],
        )
        .unwrap();

        assert_eq!(
            arrow_cell_to_value(batch.column(0).as_ref(), 0),
            Value::I64(7)
        );
        assert_eq!(
            arrow_cell_to_value(batch.column(0).as_ref(), 1),
            Value::Null
        );
        assert_eq!(
            arrow_cell_to_value(batch.column(1).as_ref(), 0),
            Value::Str(Arc::from("hi"))
        );
        assert_eq!(
            arrow_cell_to_value(batch.column(1).as_ref(), 1),
            Value::Null
        );
    }

    #[test]
    fn json_conversion_keeps_documents_and_arrays_structured() {
        let doc = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("a"), Value::I64(1)),
            (Arc::from("b"), Value::Null),
        ])));
        let json = value_to_json(&doc);
        assert_eq!(json["a"], serde_json::json!(1));
        assert_eq!(json["b"], serde_json::Value::Null);

        let arr = Value::Array(Arc::from(vec![Value::I64(1), Value::Absent]));
        assert_eq!(value_to_json(&arr), serde_json::json!([1, null]));
    }
}
