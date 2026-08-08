//! `json` (one array, streamed comma-joined — never built as an in-memory
//! `Vec<Value>` and serialized at the end) and `ndjson` (one object per
//! line). Both write one row at a time and retain nothing between calls.
//!
//! A `Null` cell serializes as JSON `null`; an `Absent` cell **omits the
//! key** — the JSON-native way to say "this field truly is not here",
//! distinct from "here, and null" (design §3.1's `Absent`/`Null` split,
//! carried into the one output format that can actually represent it without
//! inventing a sentinel).

use std::io::{self, Write};

use serde_json::{Map, Value as Json};

use super::{Row, RowSink, Summary};
use crate::value_text::CellText;

fn row_to_object(columns: &[String], row: &Row) -> Json {
    let mut map = Map::with_capacity(row.len());
    for (col, cell) in columns.iter().zip(row) {
        match cell {
            CellText::Absent => {} // omitted, not `null`
            CellText::Null => {
                map.insert(col.clone(), Json::Null);
            }
            CellText::Text(s) => {
                map.insert(col.clone(), Json::String(s.clone()));
            }
        }
    }
    Json::Object(map)
}

pub struct JsonArraySink<W: Write> {
    out: W,
    columns: Vec<String>,
    wrote_any: bool,
}

impl<W: Write> JsonArraySink<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            columns: Vec::new(),
            wrote_any: false,
        }
    }
}

impl<W: Write + Send> RowSink for JsonArraySink<W> {
    fn start(&mut self, columns: &[String]) -> io::Result<()> {
        self.columns = columns.to_vec();
        self.out.write_all(b"[")
    }

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()> {
        for row in rows {
            if self.wrote_any {
                self.out.write_all(b",")?;
            }
            self.wrote_any = true;
            let obj = row_to_object(&self.columns, row);
            serde_json::to_writer(&mut self.out, &obj)?;
        }
        self.out.flush()
    }

    fn finish(&mut self, _summary: &Summary) -> io::Result<()> {
        self.out.write_all(b"]\n")?;
        self.out.flush()
    }
}

pub struct NdjsonSink<W: Write> {
    out: W,
    columns: Vec<String>,
}

impl<W: Write> NdjsonSink<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            columns: Vec::new(),
        }
    }
}

impl<W: Write + Send> RowSink for NdjsonSink<W> {
    fn start(&mut self, columns: &[String]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()> {
        for row in rows {
            let obj = row_to_object(&self.columns, row);
            serde_json::to_writer(&mut self.out, &obj)?;
            self.out.write_all(b"\n")?;
        }
        self.out.flush()
    }

    fn finish(&mut self, _summary: &Summary) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<String> {
        vec!["a".into(), "b".into(), "c".into()]
    }

    fn row() -> Row {
        vec![CellText::Text("x".into()), CellText::Null, CellText::Absent]
    }

    #[test]
    fn ndjson_one_object_per_line_absent_key_omitted() {
        let mut out = Vec::new();
        {
            let mut sink = NdjsonSink::new(&mut out);
            sink.start(&cols()).unwrap();
            sink.write_rows(&[row(), row()]).unwrap();
            sink.finish(&Summary {
                rows_shown: 2,
                note: None,
                affected: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: Json = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["a"], serde_json::json!("x"));
        assert_eq!(parsed["b"], serde_json::Value::Null);
        assert!(
            parsed.get("c").is_none(),
            "an Absent cell must omit its key, not serialize null"
        );
    }

    #[test]
    fn json_array_is_one_valid_document() {
        let mut out = Vec::new();
        {
            let mut sink = JsonArraySink::new(&mut out);
            sink.start(&cols()).unwrap();
            sink.write_rows(&[row()]).unwrap();
            sink.write_rows(&[row()]).unwrap();
            sink.finish(&Summary {
                rows_shown: 2,
                note: None,
                affected: None,
            })
            .unwrap();
        }
        let parsed: Json = serde_json::from_slice(&out).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_result_is_an_empty_array() {
        let mut out = Vec::new();
        {
            let mut sink = JsonArraySink::new(&mut out);
            sink.start(&cols()).unwrap();
            sink.finish(&Summary {
                rows_shown: 0,
                note: None,
                affected: None,
            })
            .unwrap();
        }
        let parsed: Json = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed, serde_json::json!([]));
    }
}
