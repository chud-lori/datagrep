use std::sync::Arc;

use arrow_array::types::Int32Type;
use arrow_array::{Array, DictionaryArray};
use arrow_schema::{DataType, TimeUnit};
use datagrep_api::{Bytes, TzSpec, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum CellText {
    Null,
    Absent,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
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
        Value::Json(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| J::String(raw.to_string()))
        }
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
