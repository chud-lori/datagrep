//! CSV/TSV — RFC 4180-ish quoting over a configurable delimiter (`,` or
//! `\t`), fully streaming: one row in, one line out, nothing retained.
//!
//! CSV has no way to represent "null" vs "not present" vs "empty string" as
//! three distinct on-the-wire tokens (the ticket's NULL/empty/`Absent`
//! distinction is scoped to `--format table` — see `format::table`'s module
//! docs). Both `NULL` and `Absent` render as an empty, unquoted field here;
//! that loss is inherent to CSV, not a shortcut this module takes.

use std::io::{self, Write};

use super::{Row, RowSink, Summary};
use crate::value_text::CellText;

pub struct CsvSink<W: Write> {
    out: W,
    delim: u8,
}

impl<W: Write> CsvSink<W> {
    pub fn csv(out: W) -> Self {
        Self { out, delim: b',' }
    }

    pub fn tsv(out: W) -> Self {
        Self { out, delim: b'\t' }
    }
}

impl<W: Write> RowSink for CsvSink<W> {
    fn start(&mut self, columns: &[String]) -> io::Result<()> {
        for (i, col) in columns.iter().enumerate() {
            write_csv_field(&mut self.out, self.delim, col, i == 0)?;
        }
        if !columns.is_empty() {
            self.out.write_all(b"\r\n")?;
        }
        Ok(())
    }

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()> {
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                let text = match cell {
                    CellText::Null | CellText::Absent => "",
                    CellText::Text(s) => s.as_str(),
                };
                write_csv_field(&mut self.out, self.delim, text, i == 0)?;
            }
            self.out.write_all(b"\r\n")?;
        }
        self.out.flush()
    }

    fn finish(&mut self, _summary: &Summary) -> io::Result<()> {
        self.out.flush()
    }
}

/// Write one correctly-quoted CSV/TSV field. Quoted whenever the field
/// contains the active delimiter, a quote, or a newline; `"` inside a quoted
/// field is escaped by doubling it, per RFC 4180.
fn write_csv_field<W: Write>(out: &mut W, delim: u8, field: &str, first: bool) -> io::Result<()> {
    if !first {
        out.write_all(&[delim])?;
    }
    let delim_char = delim as char;
    let needs_quoting =
        field.contains(delim_char) || field.contains('"') || field.contains(['\n', '\r']);
    if needs_quoting {
        out.write_all(b"\"")?;
        out.write_all(field.replace('"', "\"\"").as_bytes())?;
        out.write_all(b"\"")?;
    } else {
        out.write_all(field.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(rows: &[Vec<&str>]) -> String {
        let mut out = Vec::new();
        {
            let mut sink = CsvSink::csv(&mut out);
            sink.start(&["a".to_string(), "b".to_string()]).unwrap();
            let owned: Vec<Row> = rows
                .iter()
                .map(|r| r.iter().map(|s| CellText::Text(s.to_string())).collect())
                .collect();
            sink.write_rows(&owned).unwrap();
            sink.finish(&Summary {
                rows_shown: owned.len() as u64,
                note: None,
            })
            .unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_fields_are_unquoted() {
        assert_eq!(render(&[vec!["x", "y"]]), "a,b\r\nx,y\r\n");
    }

    #[test]
    fn fields_with_commas_or_quotes_are_quoted_and_escaped() {
        let out = render(&[vec!["a,b", "she said \"hi\""]]);
        assert_eq!(out, "a,b\r\n\"a,b\",\"she said \"\"hi\"\"\"\r\n");
    }

    #[test]
    fn null_and_absent_both_render_as_empty_field() {
        let mut out = Vec::new();
        let mut sink = CsvSink::csv(&mut out);
        sink.start(&["a".to_string()]).unwrap();
        sink.write_rows(&[vec![CellText::Null], vec![CellText::Absent]])
            .unwrap();
        sink.finish(&Summary {
            rows_shown: 2,
            note: None,
        })
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a\r\n\r\n\r\n");
    }

    #[test]
    fn tsv_uses_tab_delimiter() {
        let mut out = Vec::new();
        let mut sink = CsvSink::tsv(&mut out);
        sink.start(&["a".to_string(), "b".to_string()]).unwrap();
        sink.write_rows(&[vec![CellText::Text("1".into()), CellText::Text("2".into())]])
            .unwrap();
        sink.finish(&Summary {
            rows_shown: 1,
            note: None,
        })
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a\tb\r\n1\t2\r\n");
    }
}
