use std::ffi::c_char;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datagrep_api::driver::{Batch, Canceller};
use datagrep_api::shape::Shape;
use datagrep_api::{DbError, SqlDialect};
use datagrep_core::format::csv::CsvSink;
use datagrep_core::format::json::JsonArraySink;
use datagrep_core::format::markdown::MarkdownSink;
use datagrep_core::format::sql::{QuoteIdent, SqlInsertSink};
use datagrep_core::format::{self, RowSink, Summary};
use datagrep_core::{run_export_on, ExportSink, SafetyDecision, SinkFlow};
use datagrep_lang::StatementClass;
use serde_json::json;
use tokio::sync::Notify;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, guard_quiet, to_c_string};
use crate::query::{language_for_driver, request_for, split_statements};
use crate::runtime::runtime;

const SERVER_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Csv,
    Json,
    Markdown,
    Sql,
}

impl Format {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "csv" => Ok(Format::Csv),
            "json" => Ok(Format::Json),
            "markdown" => Ok(Format::Markdown),
            "sql" => Ok(Format::Sql),
            other => Err(format!(
                "unknown export format `{other}`; expected csv, json, markdown or sql"
            )),
        }
    }
}

fn formats_for(driver_id: &str) -> Vec<&'static str> {
    let mut formats = vec!["csv", "json", "markdown"];
    if sql_target(driver_id).is_some() {
        formats.push("sql");
    }
    formats
}

// Only engines whose rows an INSERT can recreate; the quoting is the driver's own.
pub(crate) fn sql_target(driver_id: &str) -> Option<(SqlDialect, QuoteIdent)> {
    match driver_id {
        "postgres" => Some((
            SqlDialect::Postgres,
            datagrep_drv_postgres::sql::quote_ident,
        )),
        "mysql" => Some((SqlDialect::Mysql, datagrep_drv_mysql::sql::quote_ident)),
        "sqlite" => Some((SqlDialect::Sqlite, datagrep_drv_sqlite::quote_ident)),
        _ => None,
    }
}

#[derive(Debug, Default)]
enum Phase {
    #[default]
    Running,
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Default)]
struct ExportShared {
    rows: AtomicU64,
    stop: Notify,
    state: Mutex<ExportState>,
}

#[derive(Default)]
struct ExportState {
    phase: Phase,
    safety: Option<SafetyDecision>,
    canceller: Option<Arc<dyn Canceller>>,
}

impl ExportShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, ExportState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cancel(&self) {
        if matches!(self.lock().phase, Phase::Running) {
            self.stop.notify_one();
        }
    }
}

pub struct DatagrepExport {
    shared: Arc<ExportShared>,
}

impl std::fmt::Debug for DatagrepExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatagrepExport")
            .field("rows", &self.shared.rows.load(Ordering::Relaxed))
            .field("phase", &self.shared.lock().phase)
            .finish()
    }
}

unsafe fn export_ref<'a>(e: *mut DatagrepExport) -> Result<&'a DatagrepExport, String> {
    if e.is_null() {
        return Err("DatagrepExport* must not be NULL".to_string());
    }
    // SAFETY: non-NULL (checked) and live per the contract; everything behind it is atomic or a Mutex.
    Ok(unsafe { &*e })
}

struct Job {
    profile: String,
    sql: String,
    format: Format,
    table: String,
    path: PathBuf,
}

/// # Safety
/// `driver_id` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn datagrep_export_formats_json(driver_id: *const c_char) -> *mut c_char {
    guard_quiet(std::ptr::null_mut(), || {
        // SAFETY: NULL or NUL-terminated per the contract; cstr rejects NULL.
        let driver = unsafe { cstr(driver_id, "driver_id") }.unwrap_or_default();
        to_c_string(json!(formats_for(driver)).to_string())
    })
}

/// # Safety
/// `core` is a live handle; string arguments are NUL-terminated (`table` may be NULL); `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_export_start(
    core: *mut DatagrepCore,
    profile: *const c_char,
    sql: *const c_char,
    format: *const c_char,
    table: *const c_char,
    path: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut DatagrepExport {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_export_start",
        || {
            // SAFETY: live core and NUL-terminated strings per the contract; all copied to owned values because the task outlives this call.
            let core = unsafe { core_ref(core) }?.clone();
            let job = Job {
                profile: unsafe { cstr(profile, "profile") }?.to_string(),
                sql: unsafe { cstr(sql, "sql") }?.to_string(),
                format: Format::parse(unsafe { cstr(format, "format") }?)?,
                table: if table.is_null() {
                    String::new()
                } else {
                    unsafe { cstr(table, "table") }?.to_string()
                },
                path: PathBuf::from(unsafe { cstr(path, "path") }?),
            };
            if job.sql.trim().is_empty() {
                return Err("sql must not be empty".to_string());
            }
            if job.format == Format::Sql && job.table.trim().is_empty() {
                return Err("an SQL INSERT export needs a table name".to_string());
            }
            let rt = runtime()?;
            let shared = Arc::new(ExportShared::default());
            rt.spawn(drive(core, shared.clone(), job));
            Ok(Box::into_raw(Box::new(DatagrepExport { shared })))
        },
    )
}

async fn drive(core: Arc<CoreInner>, shared: Arc<ExportShared>, job: Job) {
    let partial = partial_path(&job.path);
    let outcome = tokio::select! {
        biased;
        _ = shared.stop.notified() => None,
        result = export(&core, &shared, &job, &partial) => Some(result),
    };
    let phase = match outcome {
        Some(Ok(())) => match std::fs::rename(&partial, &job.path) {
            Ok(()) => Phase::Done,
            Err(e) => Phase::Failed(format!(
                "could not move the export into place at {}: {e}",
                job.path.display()
            )),
        },
        Some(Err(message)) => Phase::Failed(message),
        None => {
            let canceller = shared.lock().canceller.take();
            if let Some(canceller) = canceller {
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(SERVER_CANCEL_TIMEOUT, canceller.cancel()).await;
                });
            }
            Phase::Cancelled
        }
    };
    if !matches!(phase, Phase::Done) {
        let _ = std::fs::remove_file(&partial);
    }
    let mut state = shared.lock();
    state.canceller = None;
    state.phase = phase;
}

// Written beside the target and renamed on success, so a failed export never clobbers an existing file.
fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

async fn export(
    core: &Arc<CoreInner>,
    shared: &ExportShared,
    job: &Job,
    partial: &Path,
) -> Result<(), String> {
    let saved = core.saved_profile(&job.profile).await?;
    let statements = split_statements(&saved.driver_id, &job.sql);
    let [statement] = statements.as_slice() else {
        return Err(format!(
            "export runs exactly one statement and this has {}; run the one to export on its own",
            statements.len()
        ));
    };
    refuse_changes(&saved.driver_id, statement)?;

    let file = File::create(partial)
        .map_err(|e| format!("could not create {}: {e}", partial.display()))?;
    let out = BufWriter::new(file);
    let rows: Box<dyn RowSink> = match job.format {
        Format::Csv => Box::new(CsvSink::csv(out)),
        Format::Json => Box::new(JsonArraySink::new(out)),
        Format::Markdown => Box::new(MarkdownSink::new(out)),
        Format::Sql => {
            let (dialect, quote) = sql_target(&saved.driver_id).ok_or_else(|| {
                format!(
                    "`{}` is not an SQL engine, so it has no SQL INSERT export",
                    saved.driver_id
                )
            })?;
            Box::new(SqlInsertSink::new(out, &job.table, dialect, quote)?)
        }
    };

    let (lease, _) = core.leased(&job.profile).await?;
    shared.lock().canceller = Some(lease.canceller());
    let mut sink = FileSink {
        rows,
        columns: Vec::new(),
        started: false,
        written: &shared.rows,
    };
    // The same gated execute every run takes: a refused statement never reaches the server.
    let result = run_export_on(&lease, request_for(statement, saved.read_only), &mut sink).await;
    if let Err(DbError::Safety { challenge, .. }) = &result {
        shared.lock().safety = lease.gate().decision(challenge);
    }
    result.map_err(|e| e.to_string())?;
    sink.finish()
        .map_err(|e| format!("could not write the export file: {e}"))
}

// Export re-runs the statement, so one that changes anything would change it twice.
fn refuse_changes(driver_id: &str, statement: &str) -> Result<(), String> {
    let Some(language) = language_for_driver(driver_id) else {
        return Ok(());
    };
    let class = datagrep_lang::language_for(language).classify(statement);
    if matches!(
        class,
        StatementClass::Write | StatementClass::Ddl | StatementClass::Tcl | StatementClass::Admin
    ) {
        return Err(format!(
            "export re-runs the statement, and this {class:?} statement would change data again; \
             export a query that reads"
        ));
    }
    Ok(())
}

struct FileSink<'a> {
    rows: Box<dyn RowSink>,
    columns: Vec<String>,
    started: bool,
    written: &'a AtomicU64,
}

impl FileSink<'_> {
    fn ensure_started(&mut self, width: usize) -> std::io::Result<()> {
        if self.started {
            return Ok(());
        }
        if self.columns.is_empty() {
            self.columns = format::placeholder_columns(width);
        }
        self.started = true;
        self.rows.start(&self.columns)
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.ensure_started(0)?;
        self.rows.finish(&Summary {
            rows_shown: self.written.load(Ordering::Relaxed),
            ..Summary::default()
        })
    }
}

impl ExportSink for FileSink<'_> {
    fn begin(&mut self, shape: &Shape) -> Result<(), DbError> {
        self.columns = format::shape_columns(shape);
        Ok(())
    }

    fn chunk(&mut self, batch: Batch) -> Result<SinkFlow, DbError> {
        let rows = format::payload_rows(batch.payload);
        if let Some(first) = rows.first() {
            self.ensure_started(first.len())?;
            self.rows.write_rows(&rows)?;
            self.written.fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        Ok(SinkFlow::Continue)
    }
}

/// # Safety
/// `e` is an unfreed handle from `datagrep_export_start`; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_export_status_json(
    e: *mut DatagrepExport,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_export_status_json",
        || {
            // SAFETY: e is from datagrep_export_start and unfreed per the contract; export_ref turns NULL into an error.
            let e = unsafe { export_ref(e) }?;
            let rows = e.shared.rows.load(Ordering::Relaxed);
            let state = e.shared.lock();
            let (name, error) = match &state.phase {
                Phase::Running => ("running", None),
                Phase::Done => ("done", None),
                Phase::Cancelled => ("cancelled", None),
                Phase::Failed(message) => ("failed", Some(message.clone())),
            };
            let payload = json!({
                "state": name,
                "rows_written": rows,
                "error": error,
                "safety": state.safety.as_ref().map(crate::safety::decision_json),
            });
            Ok(to_c_string(payload.to_string()))
        },
    )
}

/// # Safety
/// `e` is an unfreed handle from `datagrep_export_start`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_export_cancel(e: *mut DatagrepExport) {
    guard_quiet((), || {
        // SAFETY: e is from datagrep_export_start and unfreed per the contract.
        if let Ok(e) = unsafe { export_ref(e) } {
            e.shared.cancel();
        }
    })
}

/// # Safety
/// `e` is an unfreed handle from `datagrep_export_start`. A running export is cancelled.
#[no_mangle]
pub unsafe extern "C" fn datagrep_export_free(e: *mut DatagrepExport) {
    guard_quiet((), || {
        if e.is_null() {
            return;
        }
        // SAFETY: non-NULL (checked) and unfreed per the contract; the task holds its own Arc and cleans up after itself.
        let e = unsafe { Box::from_raw(e) };
        e.shared.cancel();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_insert_is_offered_only_where_an_insert_can_recreate_the_rows() {
        assert_eq!(formats_for("postgres"), ["csv", "json", "markdown", "sql"]);
        assert_eq!(formats_for("sqlite").last(), Some(&"sql"));
        for engine in ["mongodb", "redis", "elasticsearch", "nope"] {
            assert!(!formats_for(engine).contains(&"sql"), "{engine}");
        }
    }

    #[test]
    fn the_partial_file_sits_beside_the_target() {
        assert_eq!(
            partial_path(Path::new("/tmp/out.csv")),
            PathBuf::from("/tmp/out.csv.part")
        );
    }

    #[test]
    fn a_statement_that_changes_data_is_never_re_run_by_export() {
        assert!(refuse_changes("sqlite", "SELECT 1").is_ok());
        assert!(refuse_changes("sqlite", "DELETE FROM t").is_err());
        assert!(refuse_changes("postgres", "DROP TABLE t").is_err());
    }
}
