use std::io::{self, Write};

use datagrep_api::{DbError, SqlDialect};

use super::{CellText, Row, RowSink, Summary};

/// The engine's own identifier quoting, as its driver implements it.
pub type QuoteIdent = fn(&str) -> Result<String, DbError>;

#[derive(Debug)]
pub struct SqlInsertSink<W: Write> {
    out: W,
    table: String,
    columns: String,
    quote: QuoteIdent,
    backslash_escapes: bool,
}

impl<W: Write> SqlInsertSink<W> {
    /// `table` may be schema-qualified with dots; each part is quoted on its own.
    pub fn new(
        out: W,
        table: &str,
        dialect: SqlDialect,
        quote: QuoteIdent,
    ) -> Result<Self, String> {
        let table = table.trim();
        if table.is_empty() {
            return Err("an SQL INSERT export needs a table name".to_string());
        }
        let table = table
            .split('.')
            .map(|part| quote(part.trim()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
            .join(".");
        Ok(Self {
            out,
            table,
            columns: String::new(),
            quote,
            // MySQL treats a backslash in a string literal as an escape unless NO_BACKSLASH_ESCAPES is set.
            backslash_escapes: dialect == SqlDialect::Mysql,
        })
    }

    fn literal(&self, cell: &CellText) -> String {
        match cell {
            CellText::Null | CellText::Absent => "NULL".to_string(),
            CellText::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            CellText::I64(n) => n.to_string(),
            CellText::U64(n) => n.to_string(),
            CellText::F64(n) if n.is_finite() => n.to_string(),
            CellText::F64(n) => self.string(&n.to_string()),
            CellText::Json(s) | CellText::Text(s) => self.string(s),
        }
    }

    fn string(&self, text: &str) -> String {
        let mut escaped = text.replace('\'', "''");
        if self.backslash_escapes {
            escaped = escaped.replace('\\', "\\\\");
        }
        format!("'{escaped}'")
    }
}

impl<W: Write + Send> RowSink for SqlInsertSink<W> {
    fn start(&mut self, columns: &[String]) -> io::Result<()> {
        self.columns = columns
            .iter()
            .map(|c| (self.quote)(c))
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?
            .join(", ");
        Ok(())
    }

    fn write_rows(&mut self, rows: &[Row]) -> io::Result<()> {
        for row in rows {
            let values: Vec<String> = row.iter().map(|c| self.literal(c)).collect();
            if self.columns.is_empty() {
                writeln!(
                    self.out,
                    "INSERT INTO {} VALUES ({});",
                    self.table,
                    values.join(", ")
                )?;
            } else {
                writeln!(
                    self.out,
                    "INSERT INTO {} ({}) VALUES ({});",
                    self.table,
                    self.columns,
                    values.join(", ")
                )?;
            }
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

    fn ansi(ident: &str) -> Result<String, DbError> {
        Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
    }

    fn backtick(ident: &str) -> Result<String, DbError> {
        Ok(format!("`{}`", ident.replace('`', "``")))
    }

    fn render(table: &str, dialect: SqlDialect, quote: QuoteIdent, rows: Vec<Row>) -> String {
        let mut out = Vec::new();
        {
            let mut sink = SqlInsertSink::new(&mut out, table, dialect, quote).unwrap();
            sink.start(&["id".to_string(), "note".to_string()]).unwrap();
            sink.write_rows(&rows).unwrap();
            sink.finish(&Summary::default()).unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn one_insert_per_row_with_quoted_identifiers() {
        let out = render(
            "public.users",
            SqlDialect::Postgres,
            ansi,
            vec![
                vec![CellText::I64(1), CellText::Text("it's".into())],
                vec![CellText::I64(2), CellText::Null],
            ],
        );
        assert_eq!(
            out,
            "INSERT INTO \"public\".\"users\" (\"id\", \"note\") VALUES (1, 'it''s');\n\
             INSERT INTO \"public\".\"users\" (\"id\", \"note\") VALUES (2, NULL);\n"
        );
    }

    #[test]
    fn mysql_escapes_backslashes_and_quotes_with_backticks() {
        let out = render(
            "t",
            SqlDialect::Mysql,
            backtick,
            vec![vec![CellText::Bool(true), CellText::Text(r"a\'b".into())]],
        );
        assert_eq!(
            out,
            "INSERT INTO `t` (`id`, `note`) VALUES (TRUE, 'a\\\\''b');\n"
        );
    }

    #[test]
    fn a_missing_table_name_is_refused_up_front() {
        let err = SqlInsertSink::new(Vec::new(), "  ", SqlDialect::Sqlite, ansi).err();
        assert!(err.is_some_and(|e| e.contains("table name")));
    }
}
