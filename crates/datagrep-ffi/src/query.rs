use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use datagrep_api::caps::Caps;
use datagrep_api::shape::Shape;
use datagrep_api::value::PathSeg;
use datagrep_api::{DbError, ExecOpts, LanguageId};
use datagrep_core::query::CancelReport;
use datagrep_core::store::{ChunkBody, StorePhase, StoreState};
use datagrep_core::{ProfileId, QueryEvent, QueryId, SafetyDecision};
use datagrep_lang::StatementClass;
use serde_json::json;
use tokio::task::JoinHandle;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, guard_quiet, to_c_string};
use crate::runtime::runtime;

pub type DatagrepProgressFn = extern "C" fn(ctx: *mut c_void);

struct ProgressHook {
    cb: Option<DatagrepProgressFn>,
    ctx: *mut c_void,
}

// SAFETY: ctx is never dereferenced by Rust — handed straight back to the C callback on a background thread, as the header documents.
unsafe impl Send for ProgressHook {}

impl ProgressHook {
    fn fire(&self) {
        let Some(cb) = self.cb else { return };
        let ctx = self.ctx;
        let _ = std::panic::catch_unwind(move || cb(ctx));
    }
}

struct QueryShared {
    core: Arc<CoreInner>,
    profile: String,
    started: Instant,
    inner: Mutex<QueryInner>,
    progress: Mutex<ProgressHook>,
}

#[derive(Default)]
struct QueryInner {
    qid: Option<QueryId>,
    start_error: Option<String>,
    phase: Option<StorePhase>,
    rows: u64,
    columns: Vec<(String, String)>,
    affected: Option<u64>,
    read_only: bool,
    safety: Option<SafetyDecision>,
    driver_id: String,
    root: Option<String>,
    identity: Vec<String>,
    cancel_requested: bool,
    cancel_report: Option<CancelReport>,
}

impl QueryShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueryInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self) {
        let hook = self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hook.fire();
    }

    fn fail(&self, message: String) {
        self.lock().start_error = Some(message);
        self.notify();
    }
}

// A safety refusal is a UI state, not a dead end: keep the challenge the frontend has to clear.
fn refusal(shared: &Arc<QueryShared>, id: ProfileId, err: DbError) -> String {
    if let DbError::Safety { challenge, .. } = &err {
        if let Ok(gate) = shared.core.api.safety_gate(id) {
            shared.lock().safety = gate.decision(challenge);
        }
    }
    err.to_string()
}

pub struct DatagrepQuery {
    shared: Arc<QueryShared>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for DatagrepQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.shared.lock();
        f.debug_struct("DatagrepQuery")
            .field("qid", &inner.qid)
            .field("rows", &inner.rows)
            .field("phase", &inner.phase)
            .finish()
    }
}

impl DatagrepQuery {
    pub(crate) fn qid(&self) -> Option<QueryId> {
        self.shared.lock().qid
    }

    pub(crate) fn core(&self) -> &Arc<CoreInner> {
        &self.shared.core
    }

    pub(crate) fn column_count(&self) -> u32 {
        self.shared.lock().columns.len() as u32
    }

    pub(crate) fn projection_root(&self) -> Option<String> {
        self.shared.lock().root.clone()
    }
}

pub(crate) unsafe fn query_ref<'a>(q: *mut DatagrepQuery) -> Result<&'a DatagrepQuery, String> {
    if q.is_null() {
        return Err("DatagrepQuery* must not be NULL".to_string());
    }
    // SAFETY: non-NULL (checked) and live per the contract; every field behind the shared borrow is immutable or a Mutex, so cross-thread &DatagrepQuery is sound.
    Ok(unsafe { &*q })
}

// ---- run ---------------------------------------------------------------

/// # Safety
/// `core` is a live handle; `profile`/`sql` are NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_run(
    core: *mut DatagrepCore,
    profile: *const c_char,
    sql: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut DatagrepQuery {
    guard(err_out, std::ptr::null_mut(), "datagrep_query_run", || {
        // SAFETY: live core and NUL-terminated strings per the contract; both are copied to owned Strings because the task outlives this call.
        let core = unsafe { core_ref(core) }?.clone();
        let profile = unsafe { cstr(profile, "profile") }?.to_string();
        let sql = unsafe { cstr(sql, "sql") }?.to_string();
        if sql.trim().is_empty() {
            return Err("sql must not be empty".to_string());
        }
        let rt = runtime()?;

        let shared = Arc::new(QueryShared {
            core,
            profile: profile.clone(),
            started: Instant::now(),
            inner: Mutex::new(QueryInner::default()),
            progress: Mutex::new(ProgressHook {
                cb: None,
                ctx: std::ptr::null_mut(),
            }),
        });
        let task = rt.spawn(drive(shared.clone(), profile, sql));
        Ok(Box::into_raw(Box::new(DatagrepQuery {
            shared,
            tasks: Mutex::new(vec![task]),
        })))
    })
}

async fn drive(shared: Arc<QueryShared>, profile: String, sql: String) {
    let events = shared.core.api.subscribe_events();

    let (id, saved) = match shared.core.open_profile(&profile).await {
        Ok(v) => v,
        Err(e) => return shared.fail(e),
    };
    let driver_id = saved.driver_id;
    let read_only = saved.read_only;
    {
        let mut inner = shared.lock();
        inner.read_only = read_only;
        inner.driver_id = driver_id.clone();
    }

    let statements = split_statements(&driver_id, &sql);
    let Some((last, leading)) = statements.split_last() else {
        return shared.fail("sql contains no statement".to_string());
    };

    if read_only {
        for stmt in &statements {
            if let Err(e) = refuse_writes(&profile, &driver_id, stmt) {
                return shared.fail(e);
            }
        }
    }

    for stmt in leading {
        if shared.lock().cancel_requested {
            return shared.fail("cancelled before the server accepted the query".to_string());
        }
        if let Err(e) = run_to_completion(&shared, id, &profile, read_only, stmt).await {
            return shared.fail(e);
        }
    }

    if shared.lock().cancel_requested {
        return shared.fail("cancelled before the server accepted the query".to_string());
    }

    let qid = match shared
        .core
        .run_request(id, &profile, read_only, request_for(last, read_only))
        .await
    {
        Ok(qid) => qid,
        Err(e) => return shared.fail(refusal(&shared, id, e)),
    };

    let (root, identity) = shared
        .core
        .api
        .queries()
        .store(qid)
        .map(|store| editing_facts(store.shape()))
        .unwrap_or_default();

    // If stop was pressed while the server was still accepting, honour it now.
    let cancel_now = {
        let mut inner = shared.lock();
        inner.qid = Some(qid);
        inner.root = root;
        inner.identity = identity;
        inner.cancel_requested
    };
    shared.notify();
    if cancel_now {
        if let Ok(report) = shared.core.api.cancel(qid).await {
            shared.lock().cancel_report = Some(report);
        }
    }

    supervise(shared, qid, events).await;
}

async fn supervise(
    shared: Arc<QueryShared>,
    qid: QueryId,
    mut events: tokio::sync::broadcast::Receiver<QueryEvent>,
) {
    let Some(store) = shared.core.api.queries().store(qid) else {
        return;
    };
    let mut watch = store.subscribe();

    loop {
        let state = watch.borrow_and_update().clone();
        let terminal = state.phase.is_terminal();
        absorb(&shared, &state);
        shared.notify();

        if terminal {
            break;
        }
        tokio::select! {
            changed = watch.changed() => {
                if changed.is_err() { break; }
            }
            event = events.recv() => match event {
                Ok(QueryEvent::CancelOutcome { qid: q, report }) if q == qid => {
                    shared.lock().cancel_report = Some(report);
                    shared.notify();
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
        }
    }

    while shared.lock().cancel_requested && shared.lock().cancel_report_is_pending() {
        match events.recv().await {
            Ok(QueryEvent::CancelOutcome { qid: q, report }) if q == qid => {
                shared.lock().cancel_report = Some(report);
                shared.notify();
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
}

impl QueryInner {
    fn cancel_report_is_pending(&self) -> bool {
        self.cancel_report
            .as_ref()
            .map_or(true, |r| r.outcome.is_none())
    }
}

fn absorb(shared: &Arc<QueryShared>, state: &StoreState) {
    let mut inner = shared.lock();
    inner.rows = state.rows;
    inner.phase = Some(state.phase.clone());
    inner.affected = state.affected;
    if inner.columns.is_empty() {
        let root = inner.root.clone();
        inner.columns = columns_of(state, root.as_deref());
    }
}

fn editing_facts(shape: &Shape) -> (Option<String>, Vec<String>) {
    let Shape::Documents {
        root_hint,
        identity,
    } = shape
    else {
        return (None, Vec::new());
    };
    let root = root_hint.as_ref().and_then(|p| match p.segments() {
        [PathSeg::Field(name)] => Some(name.to_string()),
        _ => None,
    });
    let identity = identity
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|p| match p.segments() {
                    [PathSeg::Field(name)] => Some(name.to_string()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default()
        })
        .unwrap_or_default();
    (root, identity)
}

fn editable_json(
    root: Option<&str>,
    identity: &[String],
    driver_id: &str,
    caps: Option<Caps>,
) -> serde_json::Value {
    let Some(caps) = caps else {
        return serde_json::Value::Null;
    };
    if !caps.contains(Caps::EDITABLE_RESULTS) || identity.is_empty() {
        return serde_json::Value::Null;
    }
    json!({
        "identity": identity,
        "guard": guard_fields(driver_id),
        "root": root,
        "atomic_batch": caps.contains(Caps::ATOMIC_BATCH),
    })
}

fn columns_of(state: &StoreState, root: Option<&str>) -> Vec<(String, String)> {
    for chunk in &state.chunks {
        match &chunk.body {
            ChunkBody::Table(batch) => {
                return batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| (f.name().clone(), f.data_type().to_string()))
                    .collect();
            }
            ChunkBody::Docs(segment) => {
                return crate::rows::doc_columns(segment, root)
                    .into_iter()
                    .map(|name| (name, "document-field".to_string()))
                    .collect();
            }
            ChunkBody::Spilled { .. } => {}
        }
    }
    Vec::new()
}

async fn run_to_completion(
    shared: &Arc<QueryShared>,
    id: datagrep_core::ProfileId,
    profile: &str,
    read_only: bool,
    sql: &str,
) -> Result<(), String> {
    let qid = shared
        .core
        .run_request(id, profile, read_only, request_for(sql, read_only))
        .await
        .map_err(|e| refusal(shared, id, e))?;

    let result = match shared.core.api.queries().store(qid) {
        Some(store) => {
            let mut watch = store.subscribe();
            loop {
                let phase = watch.borrow_and_update().phase.clone();
                match phase {
                    StorePhase::Failed(msg) => break Err(msg.to_string()),
                    p if p.is_terminal() => break Ok(()),
                    _ => {}
                }
                if watch.changed().await.is_err() {
                    break Ok(());
                }
            }
        }
        None => Ok(()),
    };
    shared.core.api.close_query(qid).await;
    result
}

fn request_for(sql: &str, read_only: bool) -> datagrep_api::Request {
    datagrep_api::Request::Native {
        text: Arc::from(sql),
        params: Vec::new(),
        opts: ExecOpts {
            read_only_assert: read_only,
            ..ExecOpts::default()
        },
    }
}

pub fn refuse_writes(profile: &str, driver_id: &str, stmt: &str) -> Result<(), String> {
    let Some(language) = language_for_driver(driver_id) else {
        return Ok(());
    };
    let class = datagrep_lang::language_for(language).classify(stmt);
    if matches!(
        class,
        StatementClass::Write | StatementClass::Ddl | StatementClass::Admin
    ) {
        return Err(format!(
            "profile `{profile}` is read-only: refused a {class:?} statement before it \
             reached the server: {}",
            preview(stmt)
        ));
    }
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

fn split_statements(driver_id: &str, sql: &str) -> Vec<String> {
    let Some(language) = language_for_driver(driver_id) else {
        return vec![sql.to_string()];
    };
    let lang = datagrep_lang::language_for(language);
    lang.split(sql)
        .iter()
        .map(|span| span.text(sql).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn language_for_driver(id: &str) -> Option<LanguageId> {
    match id {
        "sqlite" => Some(LanguageId::Sql(datagrep_api::SqlDialect::Sqlite)),
        "postgres" => Some(LanguageId::Sql(datagrep_api::SqlDialect::Postgres)),
        "redis" => Some(LanguageId::RedisCli),
        "mongodb" => Some(LanguageId::MongoShell),
        "mysql" => Some(LanguageId::Sql(datagrep_api::SqlDialect::Mysql)),
        "elasticsearch" => Some(LanguageId::EsDsl),
        _ => None,
    }
}

fn guard_fields(driver_id: &str) -> Vec<String> {
    match driver_id {
        "elasticsearch" => vec!["_seq_no".to_string(), "_primary_term".to_string()],
        _ => Vec::new(),
    }
}

pub(crate) fn object_path_field(driver_id: &str) -> Option<&'static str> {
    match driver_id {
        "elasticsearch" => Some("_index"),
        _ => None,
    }
}

// ---- free --------------------------------------------------------------

/// # Safety
/// `q` is an unfreed query handle from `datagrep_query_run`. Free row windows first; this closes the query.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_free(q: *mut DatagrepQuery) {
    guard_quiet((), || {
        if q.is_null() {
            return;
        }
        // SAFETY: non-NULL (checked), unfreed per the contract. Free-the-window-first is not about dangling buffers (windows hold Arc clones) — this call closes the query; see rows.rs.
        let q = unsafe { Box::from_raw(q) };
        {
            let mut hook = q
                .shared
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hook.cb = None;
            hook.ctx = std::ptr::null_mut();
        }
        for task in q
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        let qid = q.shared.lock().qid;
        if let (Some(qid), Ok(rt)) = (qid, runtime()) {
            let api = q.shared.core.api.clone();
            rt.spawn(async move { api.close_query(qid).await });
        }
        drop(q);
    })
}

// ---- cancel ------------------------------------------------------------

/// # Safety
/// `q` is an unfreed query handle from `datagrep_query_run`.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_cancel(
    q: *mut DatagrepQuery,
    outcome_json_out: *mut *mut c_char,
) {
    guard_quiet((), || {
        // SAFETY: non-NULL (checked) and writable per the contract; nulled first so a bail-out below cannot leave a stale slot.
        if !outcome_json_out.is_null() {
            unsafe { *outcome_json_out = std::ptr::null_mut() };
        }
        // SAFETY: q is from datagrep_query_run and not yet freed, per the contract.
        let Ok(q) = (unsafe { query_ref(q) }) else {
            return;
        };

        let (qid, already, report) = {
            let mut inner = q.shared.lock();
            let already = inner.cancel_requested;
            inner.cancel_requested = true;
            (inner.qid, already, inner.cancel_report.clone())
        };

        let report = match (already, report, qid) {
            (true, Some(report), _) => cancel_json(&report),
            (true, None, _) => pending_json(),
            (false, _, Some(qid)) => {
                let Ok(rt) = runtime() else { return };
                match rt.block_on(q.shared.core.api.cancel(qid)) {
                    Ok(report) => {
                        q.shared.lock().cancel_report = Some(report.clone());
                        cancel_json(&report)
                    }
                    Err(e) => json!({
                        "local_stopped": true,
                        "kind": null,
                        "outcome": null,
                        "message": format!("stopped; the query was already closed ({e})."),
                    }),
                }
            }
            (false, _, None) => json!({
                "local_stopped": true,
                "kind": "ClientAbandon",
                "outcome": "ClientAbandoned",
                "message": "stopped before the server accepted the query.",
            }),
        };

        if !outcome_json_out.is_null() {
            if let Ok(text) = serde_json::to_string(&report) {
                // SAFETY: non-NULL (checked) and writable per the contract; the slot still holds the NULL written at the top, so nothing leaks.
                unsafe { *outcome_json_out = to_c_string(text) };
            }
        }
    })
}

fn cancel_json(report: &CancelReport) -> serde_json::Value {
    json!({
        "local_stopped": report.local_stopped,
        "kind": format!("{:?}", report.kind),
        "outcome": report.outcome.as_ref().map(|o| format!("{o:?}")),
        "message": report.message.as_ref(),
    })
}

fn pending_json() -> serde_json::Value {
    json!({
        "local_stopped": true,
        "kind": null,
        "outcome": null,
        "message": "stopped receiving results; the server may still be executing this query.",
    })
}

// ---- status ------------------------------------------------------------

/// # Safety
/// `q` is an unfreed query handle from `datagrep_query_run`. `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_status_json(
    q: *mut DatagrepQuery,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_query_status_json",
        || {
            // SAFETY: q is from datagrep_query_run and unfreed per the contract; query_ref turns NULL into an error.
            let q = unsafe { query_ref(q) }?;
            let inner = q.shared.lock();
            let elapsed_ms = q.shared.started.elapsed().as_millis() as u64;

            let (state, error) = match (&inner.start_error, &inner.phase) {
                (Some(e), _) => ("failed", Some(e.clone())),
                (None, Some(StorePhase::Loading)) => ("streaming", None),
                (None, Some(StorePhase::Parked(_))) => ("parked", None),
                (None, Some(StorePhase::Capped)) => ("capped", None),
                (None, Some(StorePhase::Complete)) => ("done", None),
                (None, Some(StorePhase::Cancelled)) => ("cancelled", None),
                (None, Some(StorePhase::Failed(m))) => ("failed", Some(m.to_string())),
                (None, None) => ("streaming", None),
            };

            let total_known = inner.start_error.is_some()
                || inner.phase.as_ref().is_some_and(StorePhase::is_terminal);

            let payload = json!({
                "state": state,
                "rows_loaded": inner.rows,
                "affected_rows": inner.affected,
                "read_only": crate::core::read_only_json(
                    inner.read_only,
                    &inner.driver_id,
                    q.shared.core.enforcement_for(&q.shared.profile),
                ),
                "elapsed_ms": elapsed_ms,
                "error": error,
                "columns": inner
                    .columns
                    .iter()
                    .map(|(name, ty)| json!({"name": name, "type": ty}))
                    .collect::<Vec<_>>(),
                "total_known": total_known,
                "editable": editable_json(
                    inner.root.as_deref(),
                    &inner.identity,
                    &inner.driver_id,
                    q.shared.core.caps_for(&q.shared.profile),
                ),
                "safety": inner
                    .safety
                    .as_ref()
                    .map(crate::safety::decision_json),
            });
            drop(inner);

            let text = serde_json::to_string(&payload)
                .map_err(|e| format!("could not encode the query status: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

// ---- progress ----------------------------------------------------------

/// # Safety
/// `q` is an unfreed query handle from `datagrep_query_run`. `ctx` stays alive while the callback is attached.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_on_progress(
    q: *mut DatagrepQuery,
    cb: Option<DatagrepProgressFn>,
    ctx: *mut c_void,
) {
    guard_quiet((), || {
        // SAFETY: q unfreed per the contract; ctx is stored, never dereferenced — datagrep_query_free detaches the callback before it can dangle.
        let Ok(q) = (unsafe { query_ref(q) }) else {
            return;
        };
        let mut hook = q
            .shared
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hook.cb = cb;
        hook.ctx = ctx;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_shape_reports_its_root_and_identity() {
        use datagrep_api::value::FieldPath;
        let shape = Shape::Documents {
            root_hint: Some(FieldPath::field("_source")),
            identity: Some(vec![
                FieldPath::field("_index"),
                FieldPath::field("_id"),
                FieldPath::field("_routing"),
            ]),
        };
        let (root, identity) = editing_facts(&shape);
        assert_eq!(root.as_deref(), Some("_source"));
        assert_eq!(identity, vec!["_index", "_id", "_routing"]);

        let anonymous = Shape::Documents {
            root_hint: None,
            identity: None,
        };
        assert_eq!(editing_facts(&anonymous), (None, Vec::new()));
        assert_eq!(editing_facts(&Shape::Unknown), (None, Vec::new()));
    }

    #[test]
    fn editing_is_offered_only_when_the_connection_and_the_result_both_allow_it() {
        let identity = vec!["_index".to_string(), "_id".to_string()];
        let editable = Caps::EDITABLE_RESULTS;

        let json = editable_json(Some("_source"), &identity, "elasticsearch", Some(editable));
        assert_eq!(json["identity"][1], serde_json::json!("_id"));
        assert_eq!(json["root"], serde_json::json!("_source"));
        assert_eq!(
            json["guard"],
            serde_json::json!(["_seq_no", "_primary_term"]),
            "an edit that cannot name the guard could only be sent unguarded"
        );
        assert_eq!(
            json["atomic_batch"],
            serde_json::json!(false),
            "a batch is only atomic when the connection says so"
        );
        assert_eq!(
            editable_json(
                Some("_source"),
                &identity,
                "elasticsearch",
                Some(editable | Caps::ATOMIC_BATCH)
            )["atomic_batch"],
            serde_json::json!(true)
        );

        assert!(editable_json(Some("_source"), &[], "elasticsearch", Some(editable)).is_null());
        assert!(
            editable_json(Some("_source"), &identity, "elasticsearch", Some(Caps::DDL)).is_null()
        );
        assert!(editable_json(Some("_source"), &identity, "elasticsearch", None).is_null());
    }

    #[test]
    fn a_script_splits_into_statements_via_datagrep_lang() {
        let stmts = split_statements(
            "sqlite",
            "CREATE TABLE t (id INTEGER);\nINSERT INTO t VALUES (1);\nSELECT * FROM t",
        );
        assert_eq!(stmts.len(), 3, "got {stmts:?}");
        assert!(stmts[2].starts_with("SELECT"));
    }

    #[test]
    fn a_single_statement_stays_one_statement() {
        let stmts = split_statements("sqlite", "SELECT 1");
        assert_eq!(stmts, vec!["SELECT 1".to_string()]);
        assert_eq!(
            split_statements("sqlite", "SELECT 1;\n\n"),
            vec!["SELECT 1".to_string()]
        );
    }

    #[test]
    fn an_unknown_engine_is_passed_through_verbatim() {
        assert_eq!(split_statements("mongo", "db.x.find({})").len(), 1);
    }

    #[test]
    fn every_registered_engine_has_a_language() {
        for id in crate::drivers::known_driver_ids() {
            assert!(language_for_driver(id).is_some(), "{id} has no language");
        }
    }
}
