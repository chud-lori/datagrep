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

        super::safety::clear(ctx, default_id, stmt_text, &args.safety)?;

        let opts = ExecOpts {
            timeout,
            row_limit: args.limit,
            read_only_assert: false,
        };
        let req = Request::Native {
            text: Arc::from(stmt_text),
            params: Vec::new(),
            opts,
        };

        let (rows_this_statement, timed_out) = {
            let row_sink = super::build_sink(args.format, &mut out, statement_index > 0);
            let mut sink = ExportRowSink::new(row_sink, deadline, stderr_is_tty, args.limit);
            ctx.core
                .run_export(default_id, req, &mut sink)
                .await
                .map_err(map_export_err)?;
            sink.finish()?;
            if stderr_is_tty {
                eprintln!();
            }
            (sink.rows_written, sink.timed_out)
        };
        total_rows += rows_this_statement;
        statement_index += 1;

        if timed_out {
            out.flush()?;
            return Err(CliError::query(format!(
                "export stopped by --timeout after {rows_this_statement} rows — {} is INCOMPLETE",
                args.out.display()
            )));
        }
    }

    if statement_index == 0 {
        return Err(CliError::usage("no statements to run (empty input)"));
    }
    out.flush()?;
    eprintln!("done: {total_rows} rows written to {}", args.out.display());
    Ok(())
}

fn map_export_err(err: DbError) -> CliError {
    match err {
        DbError::Io(io) => CliError::query(format!("could not write the export file: {io}")),
        other => CliError::from(other),
    }
}

struct ExportRowSink<'w> {
    inner: Box<dyn RowSink + 'w>,
    deadline: Option<Instant>,
    limit: Option<u64>,
    stderr_is_tty: bool,
    started_at: std::time::Instant,
    last_progress: Option<std::time::Instant>,
    started: bool,
    columns: Vec<String>,
    ack: Option<(Option<u64>, Option<String>)>,
    rows_written: u64,
    timed_out: bool,
    note: Option<String>,
}

impl<'w> ExportRowSink<'w> {
    fn new(
        inner: Box<dyn RowSink + 'w>,
        deadline: Option<Instant>,
        stderr_is_tty: bool,
        limit: Option<u64>,
    ) -> Self {
        Self {
            inner,
            deadline,
            limit,
            stderr_is_tty,
            started_at: std::time::Instant::now(),
            last_progress: None,
            started: false,
            columns: Vec::new(),
            ack: None,
            rows_written: 0,
            timed_out: false,
            note: None,
        }
    }

    fn ensure_started(&mut self, width_hint: usize) -> Result<(), DbError> {
        if self.started {
            return Ok(());
        }
        if self.columns.is_empty() && width_hint > 0 {
            self.columns = (0..width_hint).map(|i| format!("col{i}")).collect();
        }
        self.inner.start(&self.columns)?;
        self.started = true;
        Ok(())
    }

    fn progress(&mut self) {
        let min_interval = if self.stderr_is_tty {
            std::time::Duration::from_millis(100)
        } else {
            std::time::Duration::from_secs(1)
        };
        let now = std::time::Instant::now();
        if self
            .last_progress
            .is_some_and(|last| now.duration_since(last) < min_interval)
        {
            return;
        }
        self.last_progress = Some(now);
        let secs = self.started_at.elapsed().as_secs_f64().max(0.001);
        let rate = self.rows_written as f64 / secs;
        if self.stderr_is_tty {
            eprint!("\r{} rows ({rate:.0} rows/sec)   ", self.rows_written);
        } else {
            eprintln!("{} rows ({rate:.0} rows/sec)", self.rows_written);
        }
    }

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
                self.timed_out = true;
                return Ok(SinkFlow::Stop);
            }
        }
        let mut rows: Vec<Row> = match batch.payload {
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
        let mut hit_limit = false;
        if let Some(limit) = self.limit {
            let remaining = limit.saturating_sub(self.rows_written);
            if rows.len() as u64 >= remaining {
                rows.truncate(remaining as usize);
                hit_limit = true;
            }
        }
        if !rows.is_empty() {
            self.ensure_started(rows[0].len())?;
            self.inner.write_rows(&rows)?;
            self.rows_written += rows.len() as u64;
            self.progress();
        }
        // `rows` (this chunk's only copy) drops here.
        if hit_limit {
            self.note = Some(format!(
                "stopped after {} rows (--limit)",
                self.rows_written
            ));
            return Ok(SinkFlow::Stop);
        }
        Ok(SinkFlow::Continue)
    }
}

fn doc_cell(v: &Value) -> CellText {
    match v {
        Value::Null => CellText::Null,
        Value::Absent => CellText::Absent,
        other => match serde_json::to_string(&crate::value_text::value_to_json(other)) {
            Ok(json) => CellText::Json(json),
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
                color: None,
                read_only: false,
                safety: datagrep_api::safety::SafetyLevel::Silent,
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
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("export.db");
        sqlite_profile(&ctx.store, "exporttest", &db_path).await;
        let out_path = dir.path().join("out.csv");

        let args = ExportArgs {
            safety: Default::default(),
            profile: "exporttest".to_string(),
            file: None,
            command: Some("SELECT 1 AS a, 'x' AS b UNION ALL SELECT 2, 'y'".to_string()),
            format: OutputFormat::Csv,
            out: out_path.clone(),
            limit: None,
            timeout: None,
        };
        run(&ctx, &args).await.expect("export should succeed");

        let contents = std::fs::read_to_string(&out_path).unwrap();
        assert!(contents.starts_with("a,b\r\n"));
        assert!(contents.contains("1,x"));
        assert!(contents.contains("2,y"));
    }

    #[tokio::test]
    async fn export_of_200k_rows_never_grows_the_result_store() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("big.db");
        sqlite_profile(&ctx.store, "bigexport", &db_path).await;
        let out_path = dir.path().join("big.ndjson");

        let args = ExportArgs {
            safety: Default::default(),
            profile: "bigexport".to_string(),
            file: None,
            command: Some(
                "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt \
                 WHERE x < 200000) SELECT x FROM cnt"
                    .to_string(),
            ),
            format: OutputFormat::Ndjson,
            out: out_path.clone(),
            limit: None,
            timeout: None,
        };
        run(&ctx, &args).await.expect("export should succeed");

        assert_eq!(
            ctx.core.result_bytes(),
            0,
            "export must never admit anything to the result store"
        );
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            written.lines().count(),
            200_000,
            "every row must still reach the file"
        );
    }

    #[tokio::test]
    async fn export_beyond_the_soft_row_cap_delivers_every_row() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("uncapped.db");
        sqlite_profile(&ctx.store, "uncapped", &db_path).await;
        let out_path = dir.path().join("uncapped.ndjson");

        let args = ExportArgs {
            safety: Default::default(),
            profile: "uncapped".to_string(),
            file: None,
            command: Some(
                "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt \
                 WHERE x < 600000) SELECT x FROM cnt"
                    .to_string(),
            ),
            format: OutputFormat::Ndjson,
            out: out_path.clone(),
            limit: None,
            timeout: None,
        };
        run(&ctx, &args).await.expect("export should succeed");

        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            written.lines().count(),
            600_000,
            "export must deliver the complete result, exactly — never the soft row cap"
        );
    }

    #[tokio::test]
    async fn export_limit_yields_exactly_n_rows() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("limited.db");
        sqlite_profile(&ctx.store, "limited", &db_path).await;
        let out_path = dir.path().join("limited.ndjson");

        let args = ExportArgs {
            safety: Default::default(),
            profile: "limited".to_string(),
            file: None,
            command: Some(
                "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt \
                 WHERE x < 100000) SELECT x FROM cnt"
                    .to_string(),
            ),
            format: OutputFormat::Ndjson,
            out: out_path.clone(),
            limit: Some(1_234),
            timeout: None,
        };
        run(&ctx, &args)
            .await
            .expect("limited export should succeed");

        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            written.lines().count(),
            1_234,
            "--limit N must yield exactly N rows"
        );
    }

    #[tokio::test]
    async fn zero_row_export_still_writes_the_csv_header() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.db");
        sqlite_profile(&ctx.store, "emptyexport", &db_path).await;
        let out_path = dir.path().join("empty.csv");

        let args = ExportArgs {
            safety: Default::default(),
            profile: "emptyexport".to_string(),
            file: None,
            command: Some("SELECT 1 AS a, 'x' AS b WHERE 1 = 0".to_string()),
            format: OutputFormat::Csv,
            out: out_path.clone(),
            limit: None,
            timeout: None,
        };
        run(&ctx, &args).await.expect("export should succeed");

        let contents = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            contents, "a,b\r\n",
            "a zero-row export must still name its columns"
        );
    }

    #[tokio::test]
    async fn export_rejects_empty_input() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("export2.db");
        sqlite_profile(&ctx.store, "exporttest2", &db_path).await;
        let out_path = dir.path().join("out2.csv");

        let args = ExportArgs {
            safety: Default::default(),
            profile: "exporttest2".to_string(),
            file: None,
            command: Some("   ".to_string()),
            format: OutputFormat::Csv,
            out: out_path,
            limit: None,
            timeout: None,
        };
        let err = run(&ctx, &args).await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::UsageError);
    }
}
