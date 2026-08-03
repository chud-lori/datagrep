//! Cell rendering — where design §5.1 is either honoured or thrown away.
//!
//! > "Avoiding a `String` per visible cell per frame, in order of leverage:
//! > (1) **`Utf8`/`LargeUtf8` columns need no formatting at all** — the bytes
//! > are already a `&str` slice into the Arrow buffer; borrow, shape, done.
//! > Zero allocation, zero copy."
//!
//! This module implements exactly that ordering:
//!
//! 1. **Borrowed, zero-copy** — Arrow `Utf8`/`LargeUtf8`, the string values of
//!    an `Int32→Utf8` dictionary column (design §5.1: dictionary encoding is
//!    now "load-bearing for P9"), and `Arc<str>`-backed document values
//!    (`Str`/`Decimal`/`Json`). `dbx_rows_cell` returns a pointer straight
//!    into the store's own buffer; the [`crate::rows::DbxRows`] holds the
//!    `Arc` that keeps it alive.
//! 2. **Formatted once per window** into the window's single text arena —
//!    numerics, temporals, UUIDs, bytes. Not per frame: the Swift side gets
//!    pointers and re-reads them for free on every redraw.
//! 3. **Summarised** — documents and arrays render as `{3 fields}` /
//!    `[7 items]` and report kind `3`, because a grid cell is not where a
//!    nested value gets read. `dbx_rows_cell_detail_json` is.
//!
//! ## The distinction this whole ABI exists to carry
//!
//! `Value::Null` → kind **1**. `Value::Absent` → kind **2**. Design §3.1:
//! *"Field NOT PRESENT — distinct from `Null`; […] what makes a Mongo grid
//! truthful rather than a wall of fake empties."* Arrow's validity bitmap
//! cannot represent absence, so kind 2 is reachable only through the document
//! lane — which is precisely why documents are deliberately not Arrow (§3.2).

use std::fmt::Write as _;
use std::sync::Arc;

use arrow_array::types::Int32Type;
use arrow_array::{Array, DictionaryArray};
use arrow_schema::{DataType, TimeUnit};
use dbx_api::{Bytes, TzSpec, Value};

/// A rendered value.
pub const KIND_VALUE: u8 = 0;
/// An explicit SQL `NULL`.
pub const KIND_NULL: u8 = 1;
/// The field is **not present** in this document (`Value::Absent`).
pub const KIND_ABSENT: u8 = 2;
/// A document or array; the cell text is a summary, the detail pane has the
/// real thing.
pub const KIND_NESTED: u8 = 3;

/// Where one cell's text ended up.
#[derive(Debug)]
pub enum Rendered<'a> {
    /// No text at all — `NULL` and `ABSENT` render as zero bytes and are told
    /// apart by their kind, so an empty string stays distinguishable from
    /// both (the three-way distinction design §3.1 insists on).
    Empty(u8),
    /// Borrowed from a buffer the window keeps alive. Zero copy.
    Borrowed(&'a str, u8),
    /// Appended to the window's arena.
    Arena(u8),
}

/// Render one [`Value`] — the document lane, and the fallback for Arrow types
/// with no borrowable text.
pub fn render_value<'a>(v: &'a Value, arena: &mut String) -> Rendered<'a> {
    match v {
        Value::Null => Rendered::Empty(KIND_NULL),
        Value::Absent => Rendered::Empty(KIND_ABSENT),
        // `Arc<str>`: already UTF-8, already alive for as long as the window
        // holds its `Arc<DocSegment>`. Borrow it. `Decimal` and `Json` are
        // exact text the design forbids reformatting anyway ("Decimal and
        // Json are already exact text — never reformat them").
        Value::Str(s) | Value::Decimal(s) | Value::Json(s) => {
            Rendered::Borrowed(s, KIND_VALUE)
        }
        Value::Document(doc) => {
            let n = doc.len();
            let _ = write!(arena, "{{{n} field{}}}", plural(n));
            Rendered::Arena(KIND_NESTED)
        }
        Value::Array(items) => {
            let n = items.len();
            let _ = write!(arena, "[{n} item{}]", plural(n));
            Rendered::Arena(KIND_NESTED)
        }
        other => match dbx_core::convert::display_value(other) {
            Some(text) => {
                arena.push_str(&text);
                Rendered::Arena(KIND_VALUE)
            }
            // `display_value` returns `None` only for Null/Absent, both
            // matched above; this arm is unreachable in practice.
            None => Rendered::Empty(KIND_VALUE),
        },
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Render one Arrow cell, taking the borrow fast path wherever Arrow's own
/// layout already holds UTF-8.
///
/// Arrow has no representation for absence — only a validity bitmap — so this
/// never returns [`KIND_ABSENT`]. That is not a shortcut: a tabular result
/// genuinely has no absent cells, because every row has every column.
pub fn render_arrow<'a>(array: &'a dyn Array, row: usize, arena: &mut String) -> Rendered<'a> {
    if array.is_null(row) {
        return Rendered::Empty(KIND_NULL);
    }
    match array.data_type() {
        // (1) The zero-copy path, design §5.1's "single largest structural
        // win": the bytes are already a `&str` in the Arrow buffer.
        DataType::Utf8 => match array.as_any().downcast_ref::<arrow_array::StringArray>() {
            Some(a) => Rendered::Borrowed(a.value(row), KIND_VALUE),
            None => Rendered::Empty(KIND_VALUE),
        },
        DataType::LargeUtf8 => {
            match array.as_any().downcast_ref::<arrow_array::LargeStringArray>() {
                Some(a) => Rendered::Borrowed(a.value(row), KIND_VALUE),
                None => Rendered::Empty(KIND_VALUE),
            }
        }
        // Dictionary-encoded strings: borrow the dictionary entry, not a copy.
        // Design §5.1 — "Shape each distinct value once, key the shaped-run
        // cache by dictionary index": handing back the *same pointer* for
        // every row sharing a value is what lets the Swift side do that.
        DataType::Dictionary(key, value)
            if **key == DataType::Int32 && **value == DataType::Utf8 =>
        {
            match dictionary_str(array, row) {
                Some(s) => Rendered::Borrowed(s, KIND_VALUE),
                None => Rendered::Empty(KIND_NULL),
            }
        }
        // (2) Everything else formats once, into the window's arena.
        //
        // The `Value` here is a temporary, so nothing may be borrowed out of
        // it — which is exactly why every Arrow type whose text *could* be
        // borrowed is matched above. The `Borrowed` arm is unreachable in
        // practice and copies rather than dangling if that ever changes.
        _ => {
            let value = arrow_cell_to_value(array, row);
            let start = arena.len();
            match render_value(&value, arena) {
                Rendered::Empty(kind) => Rendered::Empty(kind),
                Rendered::Arena(kind) => Rendered::Arena(kind),
                Rendered::Borrowed(s, kind) => {
                    let text = s.to_string();
                    arena.truncate(start);
                    arena.push_str(&text);
                    Rendered::Arena(kind)
                }
            }
        }
    }
}

fn dictionary_str(array: &dyn Array, row: usize) -> Option<&str> {
    let dict = array.as_any().downcast_ref::<DictionaryArray<Int32Type>>()?;
    let keys = dict.keys();
    if keys.is_null(row) {
        return None;
    }
    let values = dict
        .values()
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()?;
    Some(values.value(keys.value(row) as usize))
}

/// One Arrow cell as a [`Value`], for the detail pane and for the formatted
/// path above.
///
/// Total by construction: an Arrow type this build's converter never emits
/// becomes [`Value::Unsupported`] rather than panicking — the same "never lose
/// bytes, never crash on a driver quirk" stance the design takes everywhere.
pub fn arrow_cell_to_value(array: &dyn Array, row: usize) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }
    macro_rules! cast {
        ($t:ty) => {
            match array.as_any().downcast_ref::<$t>() {
                Some(a) => a,
                None => return unsupported(array),
            }
        };
    }
    match array.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(cast!(arrow_array::BooleanArray).value(row)),
        DataType::Int64 => Value::I64(cast!(arrow_array::Int64Array).value(row)),
        DataType::UInt64 => Value::U64(cast!(arrow_array::UInt64Array).value(row)),
        DataType::Float64 => Value::F64(cast!(arrow_array::Float64Array).value(row)),
        DataType::Date32 => Value::Date(cast!(arrow_array::Date32Array).value(row)),
        DataType::Time64(TimeUnit::Nanosecond) => Value::Time {
            nanos: cast!(arrow_array::Time64NanosecondArray).value(row),
        },
        DataType::Timestamp(TimeUnit::Microsecond, tz) => Value::Timestamp {
            micros: cast!(arrow_array::TimestampMicrosecondArray).value(row),
            tz: tz_spec(tz.as_deref()),
        },
        DataType::FixedSizeBinary(16) => {
            let a = cast!(arrow_array::FixedSizeBinaryArray);
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(a.value(row));
            Value::Uuid(uuid)
        }
        DataType::Binary => {
            let a = cast!(arrow_array::BinaryArray);
            Value::Bytes(Bytes::copy_from_slice(a.value(row)))
        }
        DataType::Utf8 => Value::Str(Arc::from(cast!(arrow_array::StringArray).value(row))),
        DataType::LargeUtf8 => {
            Value::Str(Arc::from(cast!(arrow_array::LargeStringArray).value(row)))
        }
        DataType::Dictionary(key, value)
            if **key == DataType::Int32 && **value == DataType::Utf8 =>
        {
            match dictionary_str(array, row) {
                Some(s) => Value::Str(Arc::from(s)),
                None => Value::Null,
            }
        }
        _ => unsupported(array),
    }
}

fn unsupported(array: &dyn Array) -> Value {
    Value::Unsupported {
        type_name: Arc::from(format!("{:?}", array.data_type())),
        raw: Bytes::new(),
        display: Arc::from("<unrenderable arrow type>"),
    }
}

fn tz_spec(tz: Option<&str>) -> TzSpec {
    match tz {
        None => TzSpec::Naive,
        Some("UTC") => TzSpec::Utc,
        Some(name) => TzSpec::Named(Arc::from(name)),
    }
}

/// `Value` → JSON for `dbx_rows_cell_detail_json`.
///
/// Lossless where JSON allows it; anything JSON has no native type for
/// (bytes, UUID, decimal, timestamps, intervals, geometry, vectors) becomes
/// its exact display text rather than a JSON encoding this crate would have
/// to invent and the Swift side would have to learn. Documents and arrays
/// keep their structure — the detail pane is the one place nesting is the
/// point.
pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        // Both become `null`. The detail pane already knows which one it is
        // from `dbx_rows_cell_kind`; JSON has no third spelling.
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
        // Raw JSON text: re-parse so the detail pane gets structure, but fall
        // back to the exact text if the server sent something we cannot parse
        // — never lose bytes.
        Value::Json(text) => serde_json::from_str(text).unwrap_or_else(|_| J::String(text.to_string())),
        other => match dbx_core::convert::display_value(other) {
            Some(text) => J::String(text),
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
    use dbx_api::Document;

    fn render(v: &Value) -> (u8, String) {
        let mut arena = String::new();
        match render_value(v, &mut arena) {
            Rendered::Empty(k) => (k, String::new()),
            Rendered::Borrowed(s, k) => (k, s.to_string()),
            Rendered::Arena(k) => (k, arena),
        }
    }

    /// **The reason the document model exists.** NULL, ABSENT and the empty
    /// string are three different things and stay three different things.
    #[test]
    fn null_absent_and_empty_string_are_three_distinct_states() {
        assert_eq!(render(&Value::Null), (KIND_NULL, String::new()));
        assert_eq!(render(&Value::Absent), (KIND_ABSENT, String::new()));
        assert_eq!(
            render(&Value::Str(Arc::from(""))),
            (KIND_VALUE, String::new())
        );
        // Same text, different kind — which is exactly how the ABI keeps the
        // distinction without a sentinel string.
        assert_ne!(KIND_NULL, KIND_ABSENT);
    }

    #[test]
    fn nested_values_summarise_and_report_kind_three() {
        let doc = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("a"), Value::I64(1)),
            (Arc::from("b"), Value::Null),
            (Arc::from("c"), Value::Bool(true)),
        ])));
        assert_eq!(render(&doc), (KIND_NESTED, "{3 fields}".to_string()));
        let one = Value::Document(Arc::new(Document::from_fields(vec![(
            Arc::from("a"),
            Value::I64(1),
        )])));
        assert_eq!(render(&one), (KIND_NESTED, "{1 field}".to_string()));
        let arr = Value::Array(Arc::from(vec![Value::I64(1), Value::I64(2)]));
        assert_eq!(render(&arr), (KIND_NESTED, "[2 items]".to_string()));
    }

    #[test]
    fn exact_text_values_are_borrowed_not_reformatted() {
        let mut arena = String::new();
        let v = Value::Decimal(Arc::from("1.10"));
        // Trailing zeros are data — a reformat would eat them.
        assert!(matches!(
            render_value(&v, &mut arena),
            Rendered::Borrowed("1.10", KIND_VALUE)
        ));
        assert!(arena.is_empty(), "a borrowed cell must not touch the arena");
    }

    #[test]
    fn arrow_utf8_cells_are_borrowed_and_numerics_are_formatted_once() {
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
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(ints.finish()), Arc::new(strs.finish())])
                .unwrap();

        let mut arena = String::new();
        assert!(matches!(
            render_arrow(batch.column(1).as_ref(), 0, &mut arena),
            Rendered::Borrowed("hi", KIND_VALUE)
        ));
        assert!(arena.is_empty(), "a Utf8 cell must be zero-copy (§5.1)");

        assert!(matches!(
            render_arrow(batch.column(0).as_ref(), 0, &mut arena),
            Rendered::Arena(KIND_VALUE)
        ));
        assert_eq!(arena, "7");

        // Arrow's validity bitmap is NULL, never ABSENT.
        assert!(matches!(
            render_arrow(batch.column(0).as_ref(), 1, &mut arena),
            Rendered::Empty(KIND_NULL)
        ));
        assert!(matches!(
            render_arrow(batch.column(1).as_ref(), 1, &mut arena),
            Rendered::Empty(KIND_NULL)
        ));
    }

    #[test]
    fn detail_json_keeps_structure_and_exact_text() {
        let doc = Value::Document(Arc::new(Document::from_fields(vec![
            (Arc::from("a"), Value::I64(1)),
            (Arc::from("b"), Value::Null),
            (Arc::from("c"), Value::Absent),
        ])));
        let json = value_to_json(&doc);
        assert_eq!(json["a"], serde_json::json!(1));
        assert_eq!(json["b"], serde_json::Value::Null);
        assert_eq!(json["c"], serde_json::Value::Null);

        // Decimal keeps its trailing zero rather than round-tripping f64.
        assert_eq!(
            value_to_json(&Value::Decimal(Arc::from("1.10"))),
            serde_json::json!("1.10")
        );
        // Raw JSON text becomes structure.
        assert_eq!(
            value_to_json(&Value::Json(Arc::from(r#"{"k":[1,2]}"#))),
            serde_json::json!({"k": [1, 2]})
        );
        // ...unless it is not parseable, in which case the bytes survive.
        assert_eq!(
            value_to_json(&Value::Json(Arc::from("not json"))),
            serde_json::json!("not json")
        );
    }
}
