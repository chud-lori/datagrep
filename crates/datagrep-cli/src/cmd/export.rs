//! `datagrep export` (ticket item 2): stream one statement's result straight to a
//! file, rows/sec progress to **stderr** so stdout stays pipeable (the ticket
//! is explicit stdout must stay clean — export doesn't even use it).
//!
//! Runs on the same [`super::streaming::stream_result`] loop `query` uses;
//! see that module's docs for the honest gap: this is not the store-free
//! `driver→Arrow→writer→disk` path design §3.2/§5.1 describes, because
//! `CoreApi` (our only entry point) has no lower-level façade for it.

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use datagrep_api::request::ExecOpts;
use datagrep_api::Request;
use tokio::time::Instant;

use crate::cli::ExportArgs;
use crate::context::Context;
use crate::exit::CliError;

use super::streaming::stream_result;

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
        let qid = ctx
            .core
            .run_query(
                default_id,
                Request::Native {
                    text: Arc::from(stmt_text),
                    params: Vec::new(),
                    opts,
                },
            )
            .await?;

        let mut sink = super::build_sink(args.format, &mut out, statement_index > 0);
        let progress = |rows: u64, elapsed: std::time::Duration| {
            let secs = elapsed.as_secs_f64().max(0.001);
            let rate = rows as f64 / secs;
            if stderr_is_tty {
                eprint!("\r{rows} rows ({rate:.0} rows/sec)   ");
            } else {
                eprintln!("{rows} rows ({rate:.0} rows/sec)");
            }
        };
        let outcome = stream_result(ctx, qid, sink.as_mut(), None, deadline, progress).await;
        ctx.core.close_query(qid).await;
        let outcome = outcome?;
        if stderr_is_tty {
            eprintln!();
        }
        total_rows += outcome.rows_shown;
        statement_index += 1;
    }

    if statement_index == 0 {
        return Err(CliError::usage("no statements to run (empty input)"));
    }
    out.flush()?;
    eprintln!("done: {total_rows} rows written to {}", args.out.display());
    Ok(())
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
