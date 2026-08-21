pub mod csv;
pub mod json;
pub mod table;

use std::io;

use crate::value_text::CellText;

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
