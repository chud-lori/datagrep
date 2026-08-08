//! `json` (one array, streamed comma-joined — never built as an in-memory
//! `Vec<Value>` and serialized at the end) and `ndjson` (one object per
//! line). Both write one row at a time and retain nothing between calls.
//!
//! A `Null` cell serializes as JSON `null`; an `Absent` cell **omits the
//! key** — the JSON-native way to say "this field truly is not here",
//! distinct from "here, and null". A field absent from a document is a
//! different fact from a field that is null, and JSON is the one output
//! format that can carry that split without inventing a sentinel.

use std::io::{self, Write};

use super::{Row, RowSink, Summary};
use crate::value_text::CellText;

/// Serialize one row as a JSON object, streamed straight to the writer.
///
/// Types are real JSON types: booleans are `true`/`false`, integers and
/// (finite) floats are numbers — `{"id":1}`, never `{"id":"1"}` — so the
/// README's `--format json | jq` filters (`.id > 40`, `select(.b)`) work. A
/// `json`/`jsonb` cell is spliced in **verbatim** (validated first): nested
/// JSON stays nested, key order and number formatting untouched, never
/// double-encoded as an escaped string.
fn write_row_object<W: Write>(out: &mut W, columns: &[String], row: &Row) -> io::Result<()> {
    out.write_all(b"{")?;
    let mut first = true;
    for (col, cell) in columns.iter().zip(row) {
        if matches!(cell, CellText::Absent) {
            continue; // omitted, not `null`
        }
        if !first {
            out.write_all(b",")?;
        }
        first = false;
        serde_json::to_writer(&mut *out, col)?;
        out.write_all(b":")?;
        write_cell(out, cell)?;
    }
    out.write_all(b"}")
}

fn write_cell<W: Write>(out: &mut W, cell: &CellText) -> io::Result<()> {
    match cell {
        CellText::Null | CellText::Absent => out.write_all(b"null"),
        CellText::Bool(b) => out.write_all(if *b { b"true" } else { b"false" }),
        CellText::I64(n) => write!(out, "{n}"),
        CellText::U64(n) => write!(out, "{n}"),
        // JSON has no NaN/Infinity; a non-finite float becomes its display
        // text as a string rather than lying with `null`.
        CellText::F64(n) if n.is_finite() => Ok(serde_json::to_writer(&mut *out, n)?),
        CellText::F64(n) => Ok(serde_json::to_writer(&mut *out, &n.to_string())?),
        CellText::Json(raw) => {
            // Verbatim passthrough, gated on the text actually being JSON
            // (it always is for a server-side json/jsonb column; anything
            // else would corrupt the output stream).
            if serde_json::from_str::<serde_json::Value>(raw).is_ok() {
                out.write_all(raw.as_bytes())
            } else {
                Ok(serde_json::to_writer(&mut *out, raw)?)
            }
        }
        CellText::Text(s) => Ok(serde_json::to_writer(&mut *out, s)?),
    }
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
            write_row_object(&mut self.out, &self.columns, row)?;
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
            write_row_object(&mut self.out, &self.columns, row)?;
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
    use serde_json::Value as Json;

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

    /// The README's `| jq` pitch: scalars keep their JSON types — an int is
    /// `1` (not `"1"`), a bool is `true`, a float is `3.5` — and a
    /// `json`/`jsonb` cell arrives as nested JSON, verbatim, never as an
    /// escaped string.
    #[test]
    fn scalars_keep_their_json_types_and_json_cells_nest_verbatim() {
        let mut out = Vec::new();
        {
            let mut sink = NdjsonSink::new(&mut out);
            sink.start(&["i".into(), "b".into(), "f".into(), "js".into()])
                .unwrap();
            sink.write_rows(&[vec![
                CellText::I64(42),
                CellText::Bool(true),
                CellText::F64(3.5),
                CellText::Json(r#"{"k": 1, "nested": [1, 2]}"#.into()),
            ]])
            .unwrap();
            sink.finish(&Summary {
                rows_shown: 1,
                note: None,
                affected: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains(r#""js":{"k": 1, "nested": [1, 2]}"#),
            "jsonb must be spliced verbatim, got: {text}"
        );
        let parsed: Json = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["i"], serde_json::json!(42));
        assert_eq!(parsed["b"], serde_json::json!(true));
        assert_eq!(parsed["f"], serde_json::json!(3.5));
        assert_eq!(parsed["js"]["k"], serde_json::json!(1));
    }

    /// A json cell whose text somehow is not valid JSON (driver bug) must not
    /// corrupt the output stream — it degrades to a truthful string.
    #[test]
    fn invalid_json_cell_degrades_to_a_string() {
        let mut out = Vec::new();
        {
            let mut sink = NdjsonSink::new(&mut out);
            sink.start(&["j".into()]).unwrap();
            sink.write_rows(&[vec![CellText::Json("not json{".into())]])
                .unwrap();
        }
        let parsed: Json = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["j"], serde_json::json!("not json{"));
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
