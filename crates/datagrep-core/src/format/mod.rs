pub mod csv;
pub mod json;
pub mod markdown;
pub mod sql;

use std::io;

use datagrep_api::driver::Payload;
use datagrep_api::shape::Shape;
use datagrep_api::Value;

use crate::convert::display_value;

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub rows_shown: u64,
    pub note: Option<String>,
    pub affected: Option<u64>,
}

pub type Row = Vec<CellText>;

pub trait RowSink: Send {
    fn start(&mut self, columns: &[String]) -> io::Result<()>;

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()>;

    fn finish(&mut self, summary: &Summary) -> io::Result<()>;
}

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
            other => CellText::Text(display_value(other).unwrap_or_default()),
        }
    }

    /// A whole document as one JSON cell.
    pub fn document(v: &Value) -> Self {
        match v {
            Value::Null => CellText::Null,
            Value::Absent => CellText::Absent,
            other => match serde_json::to_string(&value_to_json(other)) {
                Ok(json) => CellText::Json(json),
                Err(_) => CellText::Text(String::from("<unserializable document>")),
            },
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
        other => match display_value(other) {
            Some(s) => J::String(s),
            None => J::Null,
        },
    }
}

/// The header a shape names up front; empty when only the rows can tell.
pub fn shape_columns(shape: &Shape) -> Vec<String> {
    match shape {
        Shape::Table(schema) => schema.fields.iter().map(|f| f.name.to_string()).collect(),
        Shape::Documents { .. } => vec!["doc".to_string()],
        Shape::Pairs { .. } => vec!["key".to_string(), "value".to_string()],
        Shape::Ack { .. } | Shape::Graph(_) | Shape::Unknown => Vec::new(),
    }
}

pub fn placeholder_columns(width: usize) -> Vec<String> {
    (0..width).map(|i| format!("col{i}")).collect()
}

pub fn payload_rows(payload: Payload) -> Vec<Row> {
    match payload {
        Payload::Rows(rows) => rows
            .iter()
            .map(|row| row.iter().map(CellText::from_value).collect())
            .collect(),
        Payload::Docs(docs) => docs.iter().map(|d| vec![CellText::document(d)]).collect(),
        Payload::Pairs(pairs) => pairs
            .iter()
            .map(|(k, v)| vec![CellText::from_value(k), CellText::from_value(v)])
            .collect(),
        Payload::Graph(_) | Payload::Empty => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::Document;
    use std::sync::Arc;

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

    #[test]
    fn a_document_becomes_one_json_cell() {
        let doc = Value::Document(Arc::new(Document::from_fields(vec![(
            Arc::from("a"),
            Value::I64(1),
        )])));
        assert_eq!(
            CellText::document(&doc),
            CellText::Json(r#"{"a":1}"#.into())
        );
        assert_eq!(CellText::document(&Value::Null), CellText::Null);
    }
}
