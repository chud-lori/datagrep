//! One module per `datagrep` subcommand (ticket: "FOLLOW WHAT cli.rs DECLARES").
//! `streaming` is the window-by-window execution loop `query` drives;
//! `export` streams through `CoreApi::run_export` (the store-free §3.2/§5.1
//! path) instead.

pub mod catalog;
pub mod doctor;
pub mod export;
pub mod history;
pub mod profiles;
pub mod query;
mod streaming;

use std::io::Write;
use std::path::Path;

use crate::cli::OutputFormat;
use crate::exit::CliError;
use crate::format::{csv, json, table, RowSink};

/// Build the streaming sink for one statement's result set. `leading_blank`
/// only matters to [`table::TableSink`] (a separator between multiple
/// result sets in a multi-statement script); the machine-readable formats
/// concatenate seamlessly by design (repeated CSV/TSV headers are the normal
/// way those formats show "a new result set started").
pub(crate) fn build_sink<'w>(
    format: OutputFormat,
    out: &'w mut (dyn Write + Send),
    leading_blank: bool,
) -> Box<dyn RowSink + 'w> {
    match format {
        OutputFormat::Table => Box::new(table::TableSink::new(
            out,
            leading_blank,
            table::color_enabled(),
        )),
        OutputFormat::Json => Box::new(json::JsonArraySink::new(out)),
        OutputFormat::Ndjson => Box::new(json::NdjsonSink::new(out)),
        OutputFormat::Csv => Box::new(csv::CsvSink::csv(out)),
        OutputFormat::Tsv => Box::new(csv::CsvSink::tsv(out)),
    }
}

/// Read the statement source for `query`/`export`: exactly one of a file, an
/// inline command, or (when neither is given) stdin.
pub(crate) fn read_source(file: Option<&Path>, command: Option<&str>) -> Result<String, CliError> {
    match (file, command) {
        (Some(_), Some(_)) => Err(CliError::usage(
            "pass only one of -f/--file or -c/--command",
        )),
        (Some(path), None) => std::fs::read_to_string(path)
            .map_err(|e| CliError::usage(format!("could not read {}: {e}", path.display()))),
        (None, Some(cmd)) => Ok(cmd.to_string()),
        (None, None) => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| CliError::usage(format!("could not read stdin: {e}")))?;
            Ok(buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_source_rejects_both_file_and_command() {
        let err = read_source(Some(Path::new("x.sql")), Some("select 1")).unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }

    #[test]
    fn read_source_prefers_command_when_given() {
        assert_eq!(
            read_source(None, Some("select 1")).unwrap(),
            "select 1".to_string()
        );
    }
}
