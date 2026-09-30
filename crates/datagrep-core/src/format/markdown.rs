use std::io::{self, Write};

use super::{CellText, Row, RowSink, Summary};

#[derive(Debug)]
pub struct MarkdownSink<W: Write> {
    out: W,
    width: usize,
}

impl<W: Write> MarkdownSink<W> {
    pub fn new(out: W) -> Self {
        Self { out, width: 0 }
    }
}

impl<W: Write + Send> RowSink for MarkdownSink<W> {
    fn start(&mut self, columns: &[String]) -> io::Result<()> {
        self.width = columns.len();
        if columns.is_empty() {
            return Ok(());
        }
        let header: Vec<String> = columns.iter().map(|c| escape(c)).collect();
        writeln!(self.out, "| {} |", header.join(" | "))?;
        writeln!(self.out, "|{}", " --- |".repeat(columns.len()))
    }

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()> {
        for row in rows {
            let cells: Vec<String> = (0..self.width.max(row.len()))
                .map(|i| row.get(i).map_or_else(String::new, cell))
                .collect();
            writeln!(self.out, "| {} |", cells.join(" | "))?;
        }
        self.out.flush()
    }

    fn finish(&mut self, _summary: &Summary) -> io::Result<()> {
        self.out.flush()
    }
}

fn cell(value: &CellText) -> String {
    match value {
        CellText::Null => "NULL".to_string(),
        other => other.display_text().map(|t| escape(&t)).unwrap_or_default(),
    }
}

// A pipe ends the cell and a newline ends the table.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(columns: &[&str], rows: Vec<Row>) -> String {
        let mut out = Vec::new();
        {
            let mut sink = MarkdownSink::new(&mut out);
            let columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
            sink.start(&columns).unwrap();
            sink.write_rows(&rows).unwrap();
            sink.finish(&Summary::default()).unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_table_has_a_header_a_rule_and_one_line_per_row() {
        let out = render(
            &["id", "name"],
            vec![vec![CellText::I64(1), CellText::Text("ada".into())]],
        );
        assert_eq!(out, "| id | name |\n| --- | --- |\n| 1 | ada |\n");
    }

    #[test]
    fn pipes_and_newlines_cannot_break_the_table() {
        let out = render(&["a"], vec![vec![CellText::Text("x|y\nz".into())]]);
        assert_eq!(out.lines().nth(2), Some(r"| x\|y<br>z |"));
    }

    #[test]
    fn null_is_named_and_absent_is_blank() {
        let out = render(&["a", "b"], vec![vec![CellText::Null, CellText::Absent]]);
        assert_eq!(out.lines().nth(2), Some("| NULL |  |"));
    }
}
