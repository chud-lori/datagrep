//! `datagrep export` (ticket item 2): stream one statement's result straight to a
//! file, rows/sec progress to **stderr** so stdout stays pipeable (the ticket
//! is explicit stdout must stay clean — export doesn't even use it).
//!
//! Rides [`datagrep_core::CoreApi::run_export`] — the store-free streaming
//! endpoint design §3.2/§5.1 calls for ("export streams
//! driver→Arrow→writer→disk with a fixed buffer, never touching grid state
//! \[...\] 'Export all' ≠ 'load all'"). Each driver chunk is converted,
//! written to the [`crate::format::RowSink`], and dropped before the next
//! chunk is pulled; nothing is ever admitted to a result store, so the
//! process's resident result bytes stay at zero however many rows go by —
//! which is exactly what the streaming test below asserts against
//! `CoreApi::result_bytes` (the documented white-box counter for §3.2's
//! budget).

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Instant;

use datagrep_api::driver::{Batch, Payload};
use datagrep_api::error::DbError;
use datagrep_api::request::ExecOpts;
use datagrep_api::shape::Shape;
use datagrep_api::{Request, Value};
use datagrep_core::{ExportSink, SinkFlow};

use crate::cli::ExportArgs;
use crate::context::Context;
use crate::exit::CliError;
use crate::format::{Row, RowSink, Summary};
use crate::value_text::CellText;

pub async fn run(ctx: &Context, args: &ExportArgs) -> Result<(), CliError> {
    let text = super::read_source(args.file.as_deref(), args.command.as_deref())?;
    let (default_id, default_profile) = ctx.open_profile(&args.profile).await?;
    let dialect =
        crate::drivers::language_for_driver(&default_profile.driver_id).ok_or_else(|| {
            CliError::usage(format!(
                "no query language known for driver `{}`",
                default_profile.driver_id
            ))
        })?;
    let language = datagrep_lang::language_for(dialect);
    let spans = language.split(&text);

    let timeout = args
        .timeout
        .as_deref()
        .map(crate::duration::parse_duration)
        .transpose()
        .map_err(CliError::usage)?;
    let deadline = timeout.map(|d| Instant::now() + d);

    let mut out = std::io::BufWriter::new(std::fs::File::create(&args.out)?);
    let stderr_is_tty = std::io::stderr().is_terminal();

    let mut statement_index = 0usize;
    let mut total_rows = 0u64;

    for span in &spans {
        let stmt_text = span.text(&text).trim();
        if stmt_text.is_empty() {
            continue;
        }

        let opts = ExecOpts {
            timeout,
            row_limit: None,
            read_only_assert: false,
        };
        let req = Request::Native {
            text: Arc::from(stmt_text),
            params: Vec::new(),
            opts,
        };

        let rows_this_statement = {
            let row_sink = super::build_sink(args.format, &mut out, statement_index > 0);
            let mut sink = ExportRowSink::new(row_sink, deadline, stderr_is_tty);
            ctx.core
                .run_export(default_id, req, &mut sink)
                .await
                .map_err(map_export_err)?;
            sink.finish()?;
            if stderr_is_tty {
                eprintln!();
            }
            sink.rows_written
        };
        total_rows += rows_this_statement;
        statement_index += 1;
    }

    if statement_index == 0 {
        return Err(CliError::usage("no statements to run (empty input)"));
    }
    out.flush()?;
    eprintln!("done: {total_rows} rows written to {}", args.out.display());
    Ok(())
}

/// A sink error carries the writer's own I/O failure through `DbError::Io`;
/// everything else is a genuine query error.
fn map_export_err(err: DbError) -> CliError {
    match err {
        DbError::Io(io) => CliError::query(format!("could not write the export file: {io}")),
        other => CliError::from(other),
    }
}

/// Adapter: `datagrep_core::ExportSink` (driver chunks in) → `format::RowSink`
/// (formatted rows out). Holds exactly one chunk's rows at a time — they are
/// converted, written, and dropped before the core pulls the next chunk.
struct ExportRowSink<'w> {
    inner: Box<dyn RowSink + 'w>,
    deadline: Option<Instant>,
    stderr_is_tty: bool,
    started_at: std::time::Instant,
    started: bool,
    columns: Vec<String>,
    /// Set once the shape is known to be an acknowledgement (no rows).
    ack: Option<(Option<u64>, Option<String>)>,
    rows_written: u64,
    note: Option<String>,
}

impl<'w> ExportRowSink<'w> {
    fn new(inner: Box<dyn RowSink + 'w>, deadline: Option<Instant>, stderr_is_tty: bool) -> Self {
        Self {
            inner,
            deadline,
            stderr_is_tty,
            started_at: std::time::Instant::now(),
            started: false,
            columns: Vec::new(),
            ack: None,
            rows_written: 0,
            note: None,
        }
    }

    fn ensure_started(&mut self, width_hint: usize) -> Result<(), DbError> {
        if self.started {
            return Ok(());
        }
        if self.columns.is_empty() && width_hint > 0 {
            // A shape that never declared columns (`Shape::Unknown` narrowed
            // by its first chunk): synthesize stable placeholder names, the
            // same convention as the store's `synthesized_schema`.
            self.columns = (0..width_hint).map(|i| format!("col{i}")).collect();
        }
        self.inner.start(&self.columns)?;
        self.started = true;
        Ok(())
    }

    fn progress(&self) {
        let secs = self.started_at.elapsed().as_secs_f64().max(0.001);
        let rate = self.rows_written as f64 / secs;
        if self.stderr_is_tty {
            eprint!("\r{} rows ({rate:.0} rows/sec)   ", self.rows_written);
        } else {
            eprintln!("{} rows ({rate:.0} rows/sec)", self.rows_written);
        }
    }

    /// Write the footer once the export is over. An Ack-shaped statement's
    /// affected count and message were captured from the shape in `begin`.
    fn finish(&mut self) -> Result<(), CliError> {
        self.ensure_started(0)?;
        let affected = self.ack.as_ref().and_then(|(n, _)| *n);
        if self.note.is_none() {
            if let Some((_, Some(message))) = &self.ack {
                self.note = Some(message.clone());
            }
        }
        self.inner.finish(&Summary {
            rows_shown: self.rows_written,
            note: self.note.clone(),
            affected,
        })?;
        Ok(())
    }
}

impl ExportSink for ExportRowSink<'_> {
    fn begin(&mut self, shape: &Shape) -> Result<(), DbError> {
        match shape {
            Shape::Table(schema) => {
                self.columns = schema.fields.iter().map(|f| f.name.to_string()).collect();
            }
            Shape::Documents { .. } => self.columns = vec!["doc".to_string()],
            Shape::Pairs { .. } => {
                self.columns = vec!["key".to_string(), "value".to_string()];
            }
            Shape::Ack { affected, message } => {
                self.ack = Some((*affected, message.as_ref().map(|m| m.to_string())));
            }
            // Graph results have no CLI rendering yet; Unknown narrows with
            // the first chunk (`ensure_started`'s width hint).
            Shape::Graph(_) | Shape::Unknown => {}
        }
        Ok(())
    }

    fn chunk(&mut self, batch: Batch) -> Result<SinkFlow, DbError> {
        if let Some(dl) = self.deadline {
            if Instant::now() >= dl {
                self.note = Some(
                    "stopped: timed out — the server may still be executing this query".to_string(),
                );
                return Ok(SinkFlow::Stop);
            }
        }
        let rows: Vec<Row> = match batch.payload {
            Payload::Rows(rows) => rows
                .iter()
                .map(|row| row.iter().map(CellText::from_value).collect())
                .collect(),
            Payload::Docs(docs) => docs.iter().map(|d| vec![doc_cell(d)]).collect(),
            Payload::Pairs(pairs) => pairs
                .iter()
                .map(|(k, v)| vec![CellText::from_value(k), CellText::from_value(v)])
                .collect(),
            Payload::Graph(_) | Payload::Empty => Vec::new(),
        };
        if !rows.is_empty() {
            self.ensure_started(rows[0].len())?;
            self.inner.write_rows(&rows)?;
            self.rows_written += rows.len() as u64;
            self.progress();
        }
        // `rows` (this chunk's only copy) drops here.
        Ok(SinkFlow::Continue)
    }
}

/// A document cell as its true JSON text — same rendering as the `query`
/// path (`streaming::doc_cell`), duplicated here because that one is shaped
/// around `WindowSlice` offsets while this one takes the raw driver value.
fn doc_cell(v: &Value) -> CellText {
    match v {
        Value::Null => CellText::Null,
        Value::Absent => CellText::Absent,
        other => match serde_json::to_string(&crate::value_text::value_to_json(other)) {
            Ok(json) => CellText::Text(json),
            Err(_) => CellText::Text(String::from("<unserializable document>")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputFormat;
    use std::collections::BTreeMap;

    async fn sqlite_profile(store: &datagrep_profiles::Store, name: &str, path: &std::path::Path) {
        let mut values = BTreeMap::new();
        values.insert(
            "path".to_string(),
            datagrep_api::ConfigValue::Str(path.display().to_string()),
        );
        let now = datagrep_profiles::now_ms();
        store
            .create_profile(datagrep_profiles::Profile {
                id: datagrep_profiles::new_id(),
                folder_id: None,
                name: name.to_string(),
                driver_id: "sqlite".to_string(),
                config: datagrep_api::ConnectionConfig {
                    driver: Arc::from("sqlite"),
                    values,
                },
                secret_ref: None,
                tunnel_id: None,
                env: datagrep_profiles::Env::Dev,
                color: None,
                read_only: false,
                confirm_writes: false,
                auto_limit: None,
                idle_timeout_s: None,
                last_used_at: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("create profile");
    }

    #[tokio::test]
    async fn export_streams_to_a_csv_file() {
        let ctx = Context::with_store(datagrep_profiles::Store::open_in_memory());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("export.db");
        sqlite_profile(&ctx.store, "exporttest", &db_path).await;
        let out_path = dir.path().join("out.csv");

        let args = ExportArgs {
            profile: "exporttest".to_string(),
            file: None,
            command: Some("SELECT 1 AS a, 'x' AS b UNION ALL SELECT 2, 'y'".to_string()),
            format: OutputFormat::Csv,
            out: out_path.clone(),
            timeout: None,
        };
        run(&ctx, &args).await.expect("export should succeed");

        let contents = std::fs::read_to_string(&out_path).unwrap();
        assert!(contents.starts_with("a,b\r\n"));
        assert!(contents.contains("1,x"));
        assert!(contents.contains("2,y"));
    }

    /// **Gap 3 proof at the CLI seam**: a ~200k-row export goes through
    /// `CoreApi::run_export` and never touches the result store — the
    /// process-wide resident result byte counter (`CoreApi::result_bytes`,
    /// §3.2's documented budget) stays at zero for the whole export, and
    /// every row still reaches the file.
    #[tokio::test]
    async fn export_of_200k_rows_never_grows_the_result_store() {
        let ctx = Context::with_store(datagrep_profiles::Store::open_in_memory());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("big.db");
        sqlite_profile(&ctx.store, "bigexport", &db_path).await;
        let out_path = dir.path().join("big.ndjson");

        let args = ExportArgs {
            profile: "bigexport".to_string(),
            file: None,
            command: Some(
                "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt \
                 WHERE x < 200000) SELECT x FROM cnt"
                    .to_string(),
            ),
            format: OutputFormat::Ndjson,
            out: out_path.clone(),
            timeout: None,
        };
        run(&ctx, &args).await.expect("export should succeed");

        assert_eq!(
            ctx.core.result_bytes(),
            0,
            "export must never admit anything to the result store (§3.2/§5.1)"
        );
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            written.lines().count(),
            200_000,
            "every row must still reach the file"
        );
    }

    #[tokio::test]
    async fn export_rejects_empty_input() {
        let ctx = Context::with_store(datagrep_profiles::Store::open_in_memory());
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("export2.db");
        sqlite_profile(&ctx.store, "exporttest2", &db_path).await;
        let out_path = dir.path().join("out2.csv");

        let args = ExportArgs {
            profile: "exporttest2".to_string(),
            file: None,
            command: Some("   ".to_string()),
            format: OutputFormat::Csv,
            out: out_path,
            timeout: None,
        };
        let err = run(&ctx, &args).await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }
}
