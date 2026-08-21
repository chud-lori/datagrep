//! Query lifecycle: run, status, progress, and **the stop button**.
//!
//! ## `datagrep_query_run` really is non-blocking
//!
//! The header promises *"returns immediately with a handle; rows stream in the
//! background"*, and this implementation takes that literally: **nothing**
//! about opening the profile, resolving its keychain secret, dialling the
//! server, or waiting for the server to accept the statement happens on the
//! calling thread. `datagrep_query_run` allocates a handle, spawns one task on the
//! global runtime, and returns.
//!
//! The consequence is deliberate and worth stating plainly: **connection and
//! SQL errors do not come back through `err_out`.** They cannot — they have
//! not happened yet. They arrive in `datagrep_query_status_json` as
//! `{"state":"failed","error":"…"}`, and the progress callback fires when they
//! do. `err_out` is reserved for what *is* knowable synchronously: NULL
//! pointers, non-UTF-8 arguments, empty SQL.
//!
//! This is a deliberate stance, not a shortcut: opening the app connects to
//! nothing, and startup is never gated on the network. A Run button that
//! freezes AppKit for a three-second TLS handshake is the failure mode this
//! product exists to avoid.
//!
//! ## Cancellation is honest
//!
//! The stop button **always** returns control instantly: drop the feeder, close
//! the cursor, free the store. The status line then tells the truth — "stopped
//! receiving results; the server may still be executing this query" — rather
//! than implying a kill we cannot guarantee.
//!
//! `datagrep_query_cancel` never awaits. It hands back the *pending*
//! [`CancelReport`] — the honest one, with `"outcome": null` — because at that
//! instant nobody knows what the server did. The real answer arrives later as
//! [`QueryEvent::CancelOutcome`]; the supervisor task stores it and fires the
//! progress callback. **Call `datagrep_query_cancel` again to read it**: the call
//! is idempotent, never re-cancels, and always returns the latest known
//! report. That is how the frozen header's one-shot signature carries a
//! two-phase truth.
//!
//! ## No polling
//!
//! The supervisor waits on the result store's `watch` channel and the core's
//! event broadcast in a `select!`. There is no sleep loop anywhere in this
//! crate.

use std::ffi::{c_char, c_void};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use datagrep_api::caps::Caps;
use datagrep_api::shape::Shape;
use datagrep_api::value::PathSeg;
use datagrep_api::{ExecOpts, LanguageId};
use datagrep_core::query::CancelReport;
use datagrep_core::store::{ChunkBody, StorePhase, StoreState};
use datagrep_core::{QueryEvent, QueryId};
use datagrep_lang::StatementClass;
use serde_json::json;
use tokio::task::JoinHandle;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, guard_quiet, to_c_string};
use crate::runtime::runtime;

/// Fired when the query makes progress. Called from a tokio worker thread —
/// the Swift side must hop to the main queue itself.
pub type DatagrepProgressFn = extern "C" fn(ctx: *mut c_void);

/// The callback plus its opaque context.
///
/// `ctx` is an opaque pointer the caller owns; this crate only ever passes it
/// back. It must stay valid until `datagrep_query_free`, which is the contract the
/// header's comment implies and this crate's README states outright.
struct ProgressHook {
    cb: Option<DatagrepProgressFn>,
    ctx: *mut c_void,
}

// SAFETY: `ctx` is never dereferenced by Rust — it is handed straight back to
// the C callback. Moving the pointer between threads is exactly what the
// header documents ("Called from a background thread").
unsafe impl Send for ProgressHook {}

impl ProgressHook {
    fn fire(&self) {
        let Some(cb) = self.cb else { return };
        let ctx = self.ctx;
        // A panic unwinding out of a C callback would be undefined behaviour
        // and would also kill the supervisor task. Contain it.
        let _ = std::panic::catch_unwind(move || cb(ctx));
    }
}

/// Everything the supervisor task and the FFI entry points share.
struct QueryShared {
    core: Arc<CoreInner>,
    /// The saved-profile name this query runs on — the key into the core's
    /// per-profile [`datagrep_api::Enforcement`] record for the status JSON.
    profile: String,
    started: Instant,
    inner: Mutex<QueryInner>,
    progress: Mutex<ProgressHook>,
}

#[derive(Default)]
struct QueryInner {
    /// `None` until the server accepts the statement.
    qid: Option<QueryId>,
    /// Set when the query could not be started at all (unknown profile,
    /// connect failure, bad SQL). Reported as `state: "failed"`.
    start_error: Option<String>,
    /// Last store phase seen by the supervisor.
    phase: Option<StorePhase>,
    rows: u64,
    /// Cached the first time any chunk reveals them.
    ///
    /// **CoreApi gap.** Column names arrive baked into the first admitted
    /// chunk; neither driver populates `Batch::schema_delta`, and
    /// `StoreState.chunks` stays empty forever for a genuinely empty result.
    /// So a `SELECT id, name FROM t WHERE 1=0` reports `"columns": []` — the
    /// header's `columns` field simply cannot be filled for an empty result
    /// set today.
    columns: Vec<(String, String)>,
    /// Affected-row count from an `Ack`-shaped statement (INSERT/UPDATE/DDL),
    /// mirrored from `StoreState::affected` so the GUI can say
    /// "N rows affected" instead of an empty grid.
    affected: Option<u64>,
    /// The profile's read-only flag, learned as soon as the profile opens.
    read_only: bool,
    /// The profile's driver id, learned with `read_only` — needed to say
    /// whether the `datagrep-lang` client-side guard covers this engine.
    driver_id: String,
    /// The field this result's columns are projected from, when the driver
    /// declared one (`Shape::Documents::root_hint` — `_source` for an ES hit).
    /// `None` means the row itself is the projected document.
    root: Option<String>,
    /// The field names that identify one row of this result
    /// (`Shape::Documents::identity` — `_index`/`_id`/`_routing` for an ES hit).
    /// Empty means the stream carries no usable identity, and no edit can be
    /// built from it: we never guess what to mutate.
    identity: Vec<String>,
    /// The user pressed stop, possibly before the query was even accepted.
    cancel_requested: bool,
    /// Latest known report — pending at first, resolved once the server
    /// answers.
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

/// One running (or finished) query.
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
    /// The query id, once the server has accepted the statement.
    pub(crate) fn qid(&self) -> Option<QueryId> {
        self.shared.lock().qid
    }

    pub(crate) fn core(&self) -> &Arc<CoreInner> {
        &self.shared.core
    }

    /// Columns as last learned from the stream — used to size skeleton rows
    /// for a window that has not arrived yet.
    pub(crate) fn column_count(&self) -> u32 {
        self.shared.lock().columns.len() as u32
    }

    /// The projection root this result's driver declared, if any. A window
    /// projects the fields *inside* it and keeps the rest of the row as the
    /// envelope — see [`crate::rows::datagrep_rows_envelope_json`].
    pub(crate) fn projection_root(&self) -> Option<String> {
        self.shared.lock().root.clone()
    }
}

/// Borrow a `DatagrepQuery*` argument.
///
/// # Safety
/// `q` must come from `datagrep_query_run` and not yet be freed.
pub(crate) unsafe fn query_ref<'a>(q: *mut DatagrepQuery) -> Result<&'a DatagrepQuery, String> {
    if q.is_null() {
        return Err("DatagrepQuery* must not be NULL".to_string());
    }
    // SAFETY: `q` is non-NULL (checked above) and, per the contract, a live
    // `Box<DatagrepQuery>` from `datagrep_query_run`. The borrow is shared and
    // every field behind it is either immutable or a `Mutex`, so handing out
    // several `&DatagrepQuery` at once — which the C side can do freely, since
    // it may call from any thread — is sound.
    Ok(unsafe { &*q })
}

// ---- run ---------------------------------------------------------------

/// Start a statement. Returns immediately; rows stream in the background.
///
/// # Safety
/// `core` must come from `datagrep_core_new`; `profile`/`sql` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_run(
    core: *mut DatagrepCore,
    profile: *const c_char,
    sql: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut DatagrepQuery {
    guard(err_out, std::ptr::null_mut(), "datagrep_query_run", || {
        // SAFETY: the contract above — a live core handle and NUL-terminated
        // `profile`/`sql`. Both strings are copied to owned `String`s right here
        // because they are moved into a task that outlives the call; borrowing
        // C memory across that boundary would be a use-after-free the instant
        // Swift released the argument.
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

/// The whole life of a query, off the calling thread.
async fn drive(shared: Arc<QueryShared>, profile: String, sql: String) {
    // Subscribe *before* running so no `CancelOutcome` can be missed by a
    // subscriber that started a moment too late.
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

    // Split with `datagrep-lang` rather than shipping a second tokenizer. A script
    // pasted into the editor ("CREATE TABLE …; INSERT …; SELECT …") runs in
    // order and the handle tracks the **last** statement, which is the one
    // that produces the grid — the same shape `datagrep-cli` gives a `-c` script.
    let statements = split_statements(&driver_id, &sql);
    let Some((last, leading)) = statements.split_last() else {
        return shared.fail("sql contains no statement".to_string());
    };

    // Read-only guardrail layer 2: every statement of the script is vetted by
    // `datagrep-lang`'s classifier *before dispatch*, so a write never even
    // reaches the server — the same client-side guard
    // `datagrep-cli`'s `@readonly` uses, and it applies regardless of how
    // strongly the server enforces read-only (layer 1).
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
        Err(e) => return shared.fail(e),
    };

    // The result's shape is fixed the moment the cursor exists, so the editing
    // facts are read once here rather than re-derived on every status call.
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

/// Watch one query's store and republish it into [`QueryInner`], without
/// polling — a sleep loop would burn a wakeup per tick on an idle query.
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
                // Lagged: the store watch is the source of truth for rows, so
                // a missed broadcast event costs nothing but a cancel outcome
                // we then read from the next `cancel` call.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
        }
    }

    // The store is terminal, but the server half of a cancel may still be in
    // flight. Keep listening for exactly that, then stop.
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

/// Fold one store snapshot into the shared state.
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

/// What a result's declared [`Shape`] says about editing it: the projection
/// root, and the field names that identify one row.
///
/// Only a document stream answers both — a `Shape::Table`'s identity is column
/// *indices* into a schema, which addresses a row for a SQL `UPDATE … WHERE`
/// rather than for the named key a [`datagrep_api::request::Mutation`] carries.
/// Wiring that up is the tabular half of the editing work and is not pretended
/// to here: an unknown shape reports no identity, and no identity means the UI
/// offers no edit.
fn editing_facts(shape: &Shape) -> (Option<String>, Vec<String>) {
    let Shape::Documents {
        root_hint,
        identity,
    } = shape
    else {
        return (None, Vec::new());
    };
    // A root hint that is anything other than one plain field name is not
    // something this projection can honour, and honouring it half way would put
    // the wrong values in the grid.
    let root = root_hint.as_ref().and_then(|p| match p.segments() {
        [PathSeg::Field(name)] => Some(name.to_string()),
        _ => None,
    });
    // Same rule for identity: every field of it must be addressable by name at
    // the top of the row, because that is the form a mutation key takes.
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

/// The `"editable"` half of the status JSON: what the UI needs before it may
/// offer an edit at all, or `null`.
///
/// Both halves have to agree. The connection must report `EDITABLE_RESULTS`,
/// and *this* result must carry an identity — a connection whose rows generally
/// have keys still returns aggregate results that have none. `caps` is `None`
/// until a connection has actually answered, and that is reported as "not
/// editable" rather than assumed either way.
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
        // Off means a failing batch can leave a prefix applied. The commit
        // confirmation has to say so BEFORE the click, so it is reported here
        // rather than inferred from the driver id.
        "atomic_batch": caps.contains(Caps::ATOMIC_BATCH),
    })
}

/// Columns, dug out of the first chunk that reveals any.
///
/// **CoreApi gap #4 in `datagrep-cli`'s list, hit again here.** There is no way to
/// learn a result's columns before its first chunk lands, so this is where
/// they come from.
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
            // Spilled chunks are Arrow with the store's one settled schema; a
            // later resident chunk carries it. Keep looking.
            ChunkBody::Spilled { .. } => {}
        }
    }
    Vec::new()
}

/// Run one statement and wait for its result set to finish, then give its
/// memory straight back: `close_query`, not `cancel`, is what returns the
/// result budget. Used for the leading statements of a script, whose rows
/// nobody is going to look at.
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
        .await?;

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

/// A `Request` for one statement, with `ExecOpts::read_only_assert` filled in
/// honestly — a future driver that reads it gets the real value.
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

/// The client half of the read-only guard: refuse a statement `datagrep-lang`
/// classifies as `Write`/`Ddl`/`Admin`, naming the profile so the user knows
/// *which* setting refused them. Engines with no classifier pass through —
/// their protection level is reported as `"none"`, never silently claimed.
///
/// `pub` because it is a safety claim rather than an implementation detail:
/// "a write is actually refused under read-only" is only worth stating if it
/// can be checked from outside, per driver, without a live server.
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

/// One line, at most 80 chars, of a refused statement — enough to recognise
/// it, not enough to flood a status field.
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

/// Statement splitting via `datagrep-lang` — never a second tokenizer here.
fn split_statements(driver_id: &str, sql: &str) -> Vec<String> {
    let Some(language) = language_for_driver(driver_id) else {
        // An engine this build has no splitter profile for: send the text
        // through verbatim rather than guessing where a statement ends.
        return vec![sql.to_string()];
    };
    let lang = datagrep_lang::language_for(language);
    lang.split(sql)
        .iter()
        .map(|span| span.text(sql).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The language a profile's engine speaks. One line per driver, like
/// [`crate::drivers::register_drivers`].
pub fn language_for_driver(id: &str) -> Option<LanguageId> {
    match id {
        "sqlite" => Some(LanguageId::Sql(datagrep_api::SqlDialect::Sqlite)),
        "postgres" => Some(LanguageId::Sql(datagrep_api::SqlDialect::Postgres)),
        "redis" => Some(LanguageId::RedisCli),
        "mongodb" => Some(LanguageId::MongoShell),
        "mysql" => Some(LanguageId::Sql(datagrep_api::SqlDialect::Mysql)),
        // datagrep-lang has no ES lexer yet, so this resolves to its inert
        // fallback: no splitting, no highlighting. Honest, not a pretence.
        "elasticsearch" => Some(LanguageId::EsDsl),
        _ => None,
    }
}

/// The envelope fields a generated write for this engine compares against —
/// its `Mutation::expect` precondition.
///
/// One line per driver, the same shape as [`language_for_driver`] above, and
/// here for the same reason: a frontend must not carry engine knowledge, and
/// the guard is engine knowledge. Elasticsearch's precondition is exactly
/// `_seq_no` + `_primary_term` (`if_seq_no`/`if_primary_term`), and its driver
/// refuses an update or delete that arrives without them rather than sending a
/// blind clobber — so a UI that did not know to send them could not edit at
/// all. An engine not listed here gets an empty precondition and its driver
/// decides whether that is acceptable.
fn guard_fields(driver_id: &str) -> Vec<String> {
    match driver_id {
        "elasticsearch" => vec!["_seq_no".to_string(), "_primary_term".to_string()],
        _ => Vec::new(),
    }
}

/// The identity field that names the *object* a document lives in — the one
/// part of a document's address that is a path rather than a filter.
///
/// Third line of the same table, and here for the same reason as
/// [`guard_fields`]: re-reading a document to resolve a version conflict has to
/// turn its identity back into an [`Op::Scan`], and only the engine knows which
/// of `_index`/`_id`/`_routing` is the index. Putting that in the frontend
/// would be the `if driver_id == …` the README bans; putting it here keeps the
/// UI holding an opaque list of identity fields it never has to read.
///
/// An engine not listed here cannot be re-read by identity, and
/// [`crate::reread`] says so rather than guessing which field is the path.
pub(crate) fn object_path_field(driver_id: &str) -> Option<&'static str> {
    match driver_id {
        "elasticsearch" => Some("_index"),
        _ => None,
    }
}

// ---- free --------------------------------------------------------------

/// Stop the query and free the handle.
///
/// # Safety
/// `q` must come from `datagrep_query_run`, freed at most once. Any `DatagrepRows`
/// created from it must be freed first.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_free(q: *mut DatagrepQuery) {
    guard_quiet((), || {
        if q.is_null() {
            return;
        }
        // SAFETY: non-NULL (checked) and, per the contract, a pointer from
        // `datagrep_query_run` not yet freed.
        //
        // The header's free-the-window-first rule is NOT about dangling
        // buffers: a `DatagrepRows` holds `Arc` clones of the store's batches
        // (`datagrep_core::store::WindowSlice`), so it stays valid on its own.
        // The rule exists because this function closes the query, and a window
        // outliving its closed query is a use the API does not define. Do not
        // "simplify" it into a borrow — see `rows.rs`, which states the same
        // invariant from the other side.
        let q = unsafe { Box::from_raw(q) };
        // Detach the callback before anything else: the Swift-owned `ctx` may
        // be freed the instant this returns, and firing into it would be a
        // use-after-free.
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
        // Closing (not cancelling) is what returns the memory: close the tab,
        // get the memory back.
        let qid = q.shared.lock().qid;
        if let (Some(qid), Ok(rt)) = (qid, runtime()) {
            let api = q.shared.core.api.clone();
            rt.spawn(async move { api.close_query(qid).await });
        }
        drop(q);
    })
}

// ---- cancel ------------------------------------------------------------

/// **The stop button.** Always returns instantly.
///
/// Idempotent: calling it again does not re-cancel, it just hands back the
/// latest known outcome — which is how the server's answer, arriving later as
/// a `CancelOutcome` event, reaches the caller through a one-shot signature.
///
/// # Safety
/// `q` must come from `datagrep_query_run`; `outcome_json_out` must be NULL or
/// point at a writable `char*`. A non-NULL result must be `datagrep_string_free`d.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_cancel(
    q: *mut DatagrepQuery,
    outcome_json_out: *mut *mut c_char,
) {
    guard_quiet((), || {
        // Null the out-parameter first: the caller must be able to treat a
        // non-NULL result as "there is a report to free" even if we bail out
        // below, and an uninitialised slot would fail that test at random.
        // SAFETY: non-NULL (checked) and writable per the contract.
        if !outcome_json_out.is_null() {
            unsafe { *outcome_json_out = std::ptr::null_mut() };
        }
        // SAFETY: `q` carries this function's contract — from `datagrep_query_run`,
        // not yet freed.
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
            // Already stopped: hand back whatever we know now, which may be
            // the server's real answer.
            (true, Some(report), _) => cancel_json(&report),
            (true, None, _) => pending_json(),
            // First press, query accepted: the local half happens here,
            // synchronously, and the server half is fired and forgotten.
            (false, _, Some(qid)) => {
                let Ok(rt) = runtime() else { return };
                // `CoreApi::cancel` awaits nothing; `block_on`
                // only supplies the runtime context its background task needs.
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
            // First press, not yet accepted: nothing has reached the server,
            // so nothing can be cancelled on it. `drive` will see the flag.
            (false, _, None) => json!({
                "local_stopped": true,
                "kind": "ClientAbandon",
                "outcome": "ClientAbandoned",
                "message": "stopped before the server accepted the query.",
            }),
        };

        if !outcome_json_out.is_null() {
            if let Ok(text) = serde_json::to_string(&report) {
                // SAFETY: non-NULL (checked) and writable per the contract. The
                // slot still holds the NULL written at the top of this call, so
                // overwriting it leaks nothing.
                unsafe { *outcome_json_out = to_c_string(text) };
            }
        }
    })
}

/// The `CancelReport` as JSON, verbatim — including `CancelReport::message`,
/// which is shown to the user exactly as the driver wrote it, never
/// embellished into a stronger claim than the engine can back.
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

/// A status snapshot, in exactly the shape the frozen header documents.
///
/// # Safety
/// `q` must come from `datagrep_query_run`; `err_out` must be NULL or writable.
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
            // SAFETY: `q` is from `datagrep_query_run` and unfreed per the
            // contract; `query_ref` turns NULL into an error string.
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
                // No phase yet: still opening the connection / waiting for the
                // server to accept. "streaming" with zero rows is what the UI
                // should draw — a spinner, not an empty grid.
                (None, None) => ("streaming", None),
            };

            // `total_known` = "rows_loaded is the final count". True exactly
            // when nothing more can ever be admitted. A capped result is
            // final too — it is complete *up to the cap*, and the banner, not
            // this flag, is what says so.
            let total_known = inner.start_error.is_some()
                || inner.phase.as_ref().is_some_and(StorePhase::is_terminal);

            let payload = json!({
                "state": state,
                "rows_loaded": inner.rows,
                // Affected-row count for an Ack-shaped statement (INSERT/
                // UPDATE/DDL); null for row-producing results. Additive to
                // the frozen header's documented shape — the GUI renders
                // "N rows affected" from this.
                "affected_rows": inner.affected,
                // Read-only guard status, stated honestly: null
                // when the profile is writeable, otherwise
                // {"enforcement":"server"|"client"|"none","server_confirmed":bool}
                // — see the header comment on datagrep_connection_info_json.
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
                // What may be edited in this result, or null. Additive to the
                // frozen header's documented shape, like `affected_rows`.
                "editable": editable_json(
                    inner.root.as_deref(),
                    &inner.identity,
                    &inner.driver_id,
                    q.shared.core.caps_for(&q.shared.profile),
                ),
            });
            drop(inner);

            let text = serde_json::to_string(&payload)
                .map_err(|e| format!("could not encode the query status: {e}"))?;
            Ok(to_c_string(text))
        },
    )
}

// ---- progress ----------------------------------------------------------

/// Register a progress callback. Fired from a tokio worker thread.
///
/// Pass a NULL `cb` to detach. `ctx` must outlive the query handle.
///
/// # Safety
/// `q` must come from `datagrep_query_run`. `ctx` is never dereferenced here, but
/// it must remain valid for as long as the callback is attached.
#[no_mangle]
pub unsafe extern "C" fn datagrep_query_on_progress(
    q: *mut DatagrepQuery,
    cb: Option<DatagrepProgressFn>,
    ctx: *mut c_void,
) {
    guard_quiet((), || {
        // SAFETY: `q` is from `datagrep_query_run` and unfreed per the contract.
        // `ctx` is stored, never dereferenced here — keeping it alive while the
        // callback stays attached is the caller's half of the bargain, and
        // `datagrep_query_free` detaches before it can be missed.
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

        // A stream that declares no identity is not editable, and a tabular
        // result is not offered either — its identity is column indices, which
        // no mutation key can be built from.
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

        // Either half missing means no editing at all — including "we have not
        // connected yet", which must never be read as a yes.
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
        // A trailing terminator and whitespace must not invent an empty
        // second statement (`datagrep-lang` spans exclude the `;`).
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
