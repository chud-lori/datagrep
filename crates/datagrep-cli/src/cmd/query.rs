use std::io::Write;
use std::sync::Arc;

use datagrep_api::request::ExecOpts;
use datagrep_api::Request;
use datagrep_lang::StatementClass;
use tokio::time::Instant;

use crate::cli::QueryArgs;
use crate::context::Context;
use crate::exit::CliError;

use super::streaming::stream_result;

pub async fn run(ctx: &Context, args: &QueryArgs) -> Result<(), CliError> {
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

    let cli_timeout = args
        .timeout
        .as_deref()
        .map(crate::duration::parse_duration)
        .transpose()
        .map_err(CliError::usage)?;

    let mut file_out;
    let mut stdout_out;
    let out: &mut (dyn Write + Send) = match &args.out {
        Some(path) => {
            file_out = std::io::BufWriter::new(std::fs::File::create(path)?);
            &mut file_out
        }
        None => {
            stdout_out = std::io::stdout();
            &mut stdout_out
        }
    };

    let mut statement_index = 0usize;
    let mut any_cancelled = false;

    for span in &spans {
        let stmt_text = span.text(&text).trim();
        if stmt_text.is_empty() {
            continue;
        }
        let directives = span
            .directives
            .clone()
            .map_err(|e| CliError::usage(format!("directive error: {e}")))?;

        let (profile_id, profile) = match &directives.connection {
            Some(name) if name != &args.profile => ctx.open_profile(name).await?,
            _ => (default_id, default_profile.clone()),
        };

        let readonly = directives.readonly || profile.read_only;
        if readonly {
            let stmt_language = crate::drivers::language_for_driver(&profile.driver_id)
                .map(datagrep_lang::language_for)
                .unwrap_or(language);
            let class = stmt_language.classify(stmt_text);
            if matches!(
                class,
                StatementClass::Write | StatementClass::Ddl | StatementClass::Admin
            ) {
                return Err(CliError::query(format!(
                    "statement {} blocked by the read-only guard (client-side) \
                     — {class:?} statement: {}",
                    statement_index + 1,
                    preview(stmt_text)
                )));
            }
        }

        super::safety::clear(ctx, profile_id, stmt_text, &args.safety)?;

        let limit = directives.limit.or(args.limit);
        let timeout = directives.timeout.or(cli_timeout);
        let deadline = timeout.map(|d| Instant::now() + d);

        let opts = ExecOpts {
            timeout,
            row_limit: limit,
            read_only_assert: readonly,
        };
        let started_at = std::time::Instant::now();
        let qid = ctx
            .core
            .run_query(
                profile_id,
                Request::Native {
                    text: Arc::from(stmt_text),
                    params: Vec::new(),
                    opts,
                },
            )
            .await?;

        let mut sink = super::build_sink(args.format, &mut *out, statement_index > 0);
        let run = stream_result(ctx, qid, sink.as_mut(), limit, deadline, |_, _| {}).await;
        ctx.core.close_query(qid).await;

        let status = match &run {
            Ok(outcome) if outcome.cancelled => datagrep_profiles::HistoryStatus::Cancelled,
            Ok(_) => datagrep_profiles::HistoryStatus::Ok,
            Err(_) => datagrep_profiles::HistoryStatus::Error,
        };
        let _ = ctx
            .store
            .record_history(datagrep_profiles::NewHistoryEntry {
                profile_id: profile.id.clone(),
                text: stmt_text.to_string(),
                started_at: datagrep_profiles::now_ms(),
                duration_ms: Some(started_at.elapsed().as_millis() as i64),
                row_count: run
                    .as_ref()
                    .ok()
                    .map(|o| o.affected.unwrap_or(o.rows_shown) as i64),
                status,
                error: run.as_ref().err().map(|e| e.message.clone()),
            })
            .await;

        let outcome = run?;
        if outcome.capped {
            let cap = ctx.core.queries().policy().soft_row_cap;
            return Err(CliError::query(format!(
                "statement {} stopped at the soft row cap ({cap} rows) after {} rows — \
                 the output is (or may be) incomplete. Use `datagrep export` for the complete \
                 result, or pass --limit N to raise the cap to exactly N rows",
                statement_index + 1,
                outcome.rows_shown
            )));
        }
        if outcome.cancelled {
            any_cancelled = true;
        }
        statement_index += 1;
    }

    if statement_index == 0 {
        return Err(CliError::usage("no statements to run (empty input)"));
    }

    let _ = any_cancelled;
    Ok(())
}

fn preview(stmt: &str) -> String {
    const MAX: usize = 80;
    let one_line: String = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > MAX {
        let truncated: String = one_line.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use super::super::streaming::{FETCH_WINDOW, MAX_ROWS_PER_BATCH};
    use crate::cli::OutputFormat;

    async fn temp_sqlite_profile(
        store: &datagrep_profiles::Store,
        name: &str,
        path: &std::path::Path,
    ) {
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "path".to_string(),
            datagrep_api::ConfigValue::Str(path.display().to_string()),
        );
        let now = datagrep_profiles::now_ms();
        let profile = datagrep_profiles::Profile {
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
        };
        store.create_profile(profile).await.expect("create profile");
    }

    #[tokio::test]
    async fn streaming_never_buffers_more_than_one_window() {
        MAX_ROWS_PER_BATCH.store(0, Ordering::SeqCst);

        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("stream.db");
        temp_sqlite_profile(&ctx.store, "streamtest", &db_path).await;

        let (profile_id, _profile) = ctx.open_profile("streamtest").await.expect("open profile");
        let sql = "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 200000) SELECT x FROM cnt";
        let qid = ctx
            .core
            .run_query(profile_id, Request::native(sql))
            .await
            .expect("run query");

        let mut out: Vec<u8> = Vec::new();
        let outcome = {
            let mut sink = super::super::build_sink(OutputFormat::Ndjson, &mut out, false);
            stream_result(&ctx, qid, sink.as_mut(), None, None, |_, _| {})
                .await
                .expect("stream")
        };
        ctx.core.close_query(qid).await;

        assert_eq!(outcome.rows_shown, 200_000);
        let max = MAX_ROWS_PER_BATCH.load(Ordering::SeqCst);
        assert!(
            max <= FETCH_WINDOW as usize,
            "buffered {max} rows at once, expected at most {FETCH_WINDOW}"
        );
        assert!(max > 0, "the counter never observed any rows at all");
        // 200k ndjson lines were actually written.
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 200_000);
    }

    #[tokio::test]
    async fn zero_row_query_still_writes_the_csv_header() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.db");
        temp_sqlite_profile(&ctx.store, "emptyquery", &db_path).await;
        let out_path = dir.path().join("empty.csv");

        let args = QueryArgs {
            safety: Default::default(),
            profile: "emptyquery".to_string(),
            file: None,
            command: Some("SELECT 1 AS a, 'x' AS b WHERE 1 = 0".to_string()),
            stdin: None,
            format: OutputFormat::Csv,
            limit: None,
            timeout: None,
            out: Some(out_path.clone()),
        };
        run(&ctx, &args).await.expect("query should succeed");
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            written, "a,b\r\n",
            "a zero-row result must still name its columns"
        );
    }

    #[tokio::test]
    async fn the_ladder_needs_a_flag_and_the_right_one() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ladder.db");
        temp_sqlite_profile(&ctx.store, "prod", &db_path).await;
        let mut profile = ctx.find_profile("prod").await.unwrap();
        profile.safety = datagrep_api::safety::SafetyLevel::AuthWrites;
        ctx.store.update_profile(profile).await.unwrap();

        let mut args = QueryArgs {
            safety: Default::default(),
            profile: "prod".to_string(),
            file: None,
            command: Some("CREATE TABLE t (id INTEGER PRIMARY KEY)".to_string()),
            stdin: None,
            format: OutputFormat::Json,
            limit: None,
            timeout: None,
            out: Some(dir.path().join("out.json")),
        };

        let err = run(&ctx, &args).await.expect_err("no flag, no write");
        assert!(err.message.contains("--confirm prod"), "{}", err.message);

        args.safety.acknowledge = true;
        let err = run(&ctx, &args)
            .await
            .expect_err("acknowledging is not authenticating");
        assert!(err.message.contains("authentication"), "{}", err.message);

        args.safety.acknowledge = false;
        args.safety.confirm = Some("staging".to_string());
        assert!(
            run(&ctx, &args).await.is_err(),
            "another connection's name must not clear this one"
        );

        args.safety.confirm = Some("prod".to_string());
        run(&ctx, &args)
            .await
            .expect("the connection name clears it");
    }

    #[test]
    fn preview_truncates_long_statements_on_one_line() {
        let long = "select ".to_string() + &"x, ".repeat(100);
        let p = preview(&long);
        assert!(p.chars().count() <= 81);
        assert!(p.ends_with('…'));
        assert!(!p.contains('\n'));
    }

    #[tokio::test]
    async fn readonly_directive_blocks_a_write_statement() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ro.db");
        temp_sqlite_profile(&ctx.store, "rotest", &db_path).await;

        let args = QueryArgs {
            safety: Default::default(),
            profile: "rotest".to_string(),
            file: None,
            command: Some("-- @readonly\nCREATE TABLE t (id INTEGER PRIMARY KEY)".to_string()),
            stdin: None,
            format: OutputFormat::Json,
            limit: None,
            timeout: None,
            out: None,
        };
        let err = run(&ctx, &args).await.unwrap_err();
        assert_eq!(err.kind, crate::exit::ExitKind::QueryError);
        assert!(err.message.contains("read-only guard"));
    }

    #[tokio::test]
    async fn limit_directive_caps_rows_and_still_succeeds() {
        let ctx = crate::context::test_ctx();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("limit.db");
        temp_sqlite_profile(&ctx.store, "limittest", &db_path).await;
        let (profile_id, _) = ctx.open_profile("limittest").await.unwrap();
        ctx.core
            .run_query(
                profile_id,
                Request::native("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1)"),
            )
            .await
            .ok();

        let args = QueryArgs {
            safety: Default::default(),
            profile: "limittest".to_string(),
            file: None,
            command: Some("-- @limit 3\nWITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x<100) SELECT x FROM cnt".to_string()),
            stdin: None,
            format: OutputFormat::Ndjson,
            limit: None,
            timeout: None,
            out: Some(dir.path().join("out.ndjson")),
        };
        run(&ctx, &args)
            .await
            .expect("limited query should succeed");
        let written = std::fs::read_to_string(dir.path().join("out.ndjson")).unwrap();
        assert_eq!(
            written.lines().count(),
            3,
            "expected exactly the @limit 3 rows"
        );
    }
}
