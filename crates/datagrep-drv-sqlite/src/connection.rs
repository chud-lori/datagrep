use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use async_trait::async_trait;
use datagrep_api::{
    Batch, Canceller, Capabilities, Catalog, ConnectCtx, Connection, Cursor, CursorStats, DbError,
    Enforcement, FetchHint, FieldDef, FieldFlags, Identity, IsolationLevel, Mutation,
    MutationBatch, Op, Request, ResumeToken, RowSchema, ServerInfo, SortKey, Transaction, TxOpts,
    Value,
};
use tokio::sync::oneshot;

use crate::canceller::SqliteCanceller;
use crate::catalog::SqliteCatalog;
use crate::cursor::SqliteCursor;
use crate::error::map_sqlite_err;
use crate::scan::{ColumnMeta, OpenScan};
use crate::transaction::SqliteTransaction;
use crate::value::{quote_ident, SqlParam};

pub(crate) enum ExecOutcome {
    Ack { affected: Option<u64> },
    Cursor { id: u64, schema: Arc<RowSchema> },
}

pub(crate) struct FetchReply {
    pub batch: Option<Batch>,
    pub resume_token: Option<ResumeToken>,
    pub stats: CursorStats,
}

pub(crate) struct CatalogJob {
    task: Box<dyn FnOnce(&rusqlite::Connection) + Send>,
}

impl CatalogJob {
    fn run(self, conn: &rusqlite::Connection) {
        (self.task)(conn)
    }
}

pub(crate) enum WorkerMsg {
    Execute {
        req: Box<Request>,
        reply: oneshot::Sender<Result<ExecOutcome, DbError>>,
    },
    FetchBatch {
        id: u64,
        hint: FetchHint,
        reply: oneshot::Sender<Result<FetchReply, DbError>>,
    },
    CloseCursor {
        id: u64,
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Begin {
        opts: TxOpts,
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Commit {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Rollback {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    SetReadOnly {
        on: bool,
        reply: oneshot::Sender<Result<Enforcement, DbError>>,
    },
    Ping {
        reply: oneshot::Sender<Result<(), DbError>>,
    },
    Catalog(CatalogJob),
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct JobSender(std_mpsc::Sender<WorkerMsg>);

impl JobSender {
    async fn call<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, DbError>>) -> WorkerMsg,
    ) -> Result<T, DbError> {
        let (tx, rx) = oneshot::channel();
        self.0.send(build(tx)).map_err(|_| DbError::Closed)?;
        rx.await.map_err(|_| DbError::Closed)?
    }

    pub async fn execute(&self, req: Request) -> Result<ExecOutcome, DbError> {
        self.call(|reply| WorkerMsg::Execute {
            req: Box::new(req),
            reply,
        })
        .await
    }

    pub async fn fetch_batch(&self, id: u64, hint: FetchHint) -> Result<FetchReply, DbError> {
        self.call(|reply| WorkerMsg::FetchBatch { id, hint, reply })
            .await
    }

    pub async fn close_cursor(&self, id: u64) -> Result<(), DbError> {
        self.call(|reply| WorkerMsg::CloseCursor { id, reply })
            .await
    }

    pub async fn begin(&self, opts: TxOpts) -> Result<(), DbError> {
        self.call(|reply| WorkerMsg::Begin { opts, reply }).await
    }

    pub async fn commit(&self) -> Result<(), DbError> {
        self.call(|reply| WorkerMsg::Commit { reply }).await
    }

    pub async fn rollback(&self) -> Result<(), DbError> {
        self.call(|reply| WorkerMsg::Rollback { reply }).await
    }

    pub async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        self.call(|reply| WorkerMsg::SetReadOnly { on, reply })
            .await
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        self.call(|reply| WorkerMsg::Ping { reply }).await
    }

    pub async fn run_catalog_job<F, T>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<T, DbError>>();
        let job = CatalogJob {
            task: Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }),
        };
        self.0
            .send(WorkerMsg::Catalog(job))
            .map_err(|_| DbError::Closed)?;
        rx.await.map_err(|_| DbError::Closed)?
    }

    pub fn shutdown(&self) {
        let _ = self.0.send(WorkerMsg::Shutdown);
    }
}

pub(crate) struct WorkerReady {
    pub server_info: ServerInfo,
    pub interrupt_handle: rusqlite::InterruptHandle,
}

fn open_connection(path: &str, read_only: bool) -> Result<rusqlite::Connection, DbError> {
    use rusqlite::OpenFlags;
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
    };
    let conn = rusqlite::Connection::open_with_flags(path, flags).map_err(map_sqlite_err)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(map_sqlite_err)?;
    if read_only {
        conn.execute_batch("PRAGMA query_only = ON;")
            .map_err(map_sqlite_err)?;
    }
    Ok(conn)
}

fn build_server_info(conn: &rusqlite::Connection, path: &str) -> ServerInfo {
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap_or_else(|_| "unknown".to_string());
    ServerInfo {
        product: Arc::from("SQLite"),
        version: Arc::from(rusqlite::version()),
        details: vec![
            (Arc::from("path"), Arc::from(path)),
            (Arc::from("journal_mode"), Arc::from(journal_mode.as_str())),
        ],
    }
}

pub(crate) fn run_worker(
    path: String,
    read_only: bool,
    rx: std_mpsc::Receiver<WorkerMsg>,
    ready_tx: oneshot::Sender<Result<WorkerReady, DbError>>,
) {
    let conn = match open_connection(&path, read_only) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let interrupt_handle = conn.get_interrupt_handle();
    let server_info = build_server_info(&conn, &path);
    if ready_tx
        .send(Ok(WorkerReady {
            server_info,
            interrupt_handle,
        }))
        .is_err()
    {
        return; // Caller gave up (e.g. connect timeout); nothing to serve.
    }

    let mut cursors: HashMap<u64, OpenScan<'_>> = HashMap::new();
    let mut next_cursor_id: u64 = 1;
    let mut session_read_only = read_only;
    let mut tx_forced_read_only = false;

    while let Ok(msg) = rx.recv() {
        match msg {
            WorkerMsg::Shutdown => break,
            WorkerMsg::Execute { req, reply } => {
                let outcome = handle_execute(&conn, &req, &mut cursors, &mut next_cursor_id);
                let _ = reply.send(outcome);
            }
            WorkerMsg::FetchBatch { id, hint, reply } => {
                let out = match cursors.get_mut(&id) {
                    Some(scan) => scan.fetch_batch(hint).map(|batch| FetchReply {
                        batch,
                        resume_token: scan.resume_token(),
                        stats: CursorStats {
                            rows: scan.rows_emitted,
                            bytes: scan.bytes_emitted,
                            batches: scan.batches_emitted(),
                            server_elapsed_micros: None,
                        },
                    }),
                    None => Err(DbError::Closed),
                };
                let _ = reply.send(out);
            }
            WorkerMsg::CloseCursor { id, reply } => {
                cursors.remove(&id);
                let _ = reply.send(Ok(()));
            }
            WorkerMsg::Begin { opts, reply } => {
                let out = handle_begin(&conn, &opts, &mut tx_forced_read_only, session_read_only);
                let _ = reply.send(out);
            }
            WorkerMsg::Commit { reply } => {
                let out = handle_end_tx(&conn, true, &mut tx_forced_read_only, session_read_only);
                let _ = reply.send(out);
            }
            WorkerMsg::Rollback { reply } => {
                let out = handle_end_tx(&conn, false, &mut tx_forced_read_only, session_read_only);
                let _ = reply.send(out);
            }
            WorkerMsg::SetReadOnly { on, reply } => {
                let out = conn
                    .execute_batch(if on {
                        "PRAGMA query_only = ON"
                    } else {
                        "PRAGMA query_only = OFF"
                    })
                    .map(|()| {
                        session_read_only = on;
                        Enforcement::Server
                    })
                    .map_err(map_sqlite_err);
                let _ = reply.send(out);
            }
            WorkerMsg::Ping { reply } => {
                let out = conn.execute_batch("SELECT 1;").map_err(map_sqlite_err);
                let _ = reply.send(out);
            }
            WorkerMsg::Catalog(job) => job.run(&conn),
        }
    }
}

struct PreparedRequest {
    sql: String,
    params: Vec<Value>,
    order: Vec<SortKey>,
}

fn prepare_request(req: &Request) -> Result<PreparedRequest, DbError> {
    match req {
        Request::Native { text, params, .. } => Ok(PreparedRequest {
            sql: text.to_string(),
            params: params.clone(),
            order: Vec::new(),
        }),
        Request::Op(Op::Scan {
            path,
            filter,
            order,
            project,
            limit,
            resume,
        }) => {
            let c = crate::compile::compile_scan(path, filter, order, project, limit, resume)?;
            Ok(PreparedRequest {
                sql: c.sql,
                params: c.params,
                order: order.clone(),
            })
        }
        Request::Op(Op::Count { path, filter, .. }) => {
            let c = crate::compile::compile_count(path, filter)?;
            Ok(PreparedRequest {
                sql: c.sql,
                params: c.params,
                order: Vec::new(),
            })
        }
        Request::Op(Op::Explain { inner, analyze }) => {
            if *analyze {
                return Err(DbError::Unsupported {
                    feature: "EXPLAIN ANALYZE — SQLite has no run-time query analysis, only \
                              static EXPLAIN QUERY PLAN"
                        .to_string(),
                });
            }
            let inner_prepared = prepare_request(inner)?;
            Ok(PreparedRequest {
                sql: format!("EXPLAIN QUERY PLAN {}", inner_prepared.sql),
                params: inner_prepared.params,
                order: Vec::new(),
            })
        }
        Request::Op(Op::Ddl(ddl)) => Ok(PreparedRequest {
            sql: crate::compile::compile_ddl(ddl)?,
            params: Vec::new(),
            order: Vec::new(),
        }),
        Request::Op(Op::Mutate(_)) => {
            unreachable!("Op::Mutate is dispatched separately in handle_execute")
        }
    }
}

fn build_row_schema(columns: &[ColumnMeta], identity: Option<Identity>) -> RowSchema {
    let pk_indices: std::collections::HashSet<u32> = identity
        .as_ref()
        .map(|i| i.field_indices.iter().copied().collect())
        .unwrap_or_default();
    let fields = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut flags = FieldFlags::empty();
            if pk_indices.contains(&(i as u32)) {
                flags |= FieldFlags::PRIMARY_KEY;
            }
            FieldDef {
                name: c.name.clone(),
                logical: c.logical,
                flags,
                native_type: c.native_type.clone(),
            }
        })
        .collect();
    RowSchema { fields, identity }
}

fn detect_identity(
    conn: &rusqlite::Connection,
    req: &Request,
    columns: &[ColumnMeta],
) -> Option<Identity> {
    let Request::Op(Op::Scan {
        path,
        project: None,
        ..
    }) = req
    else {
        return None;
    };
    if path.parts().is_empty() || path.parts().len() > 2 {
        return None;
    }
    let pk_cols = crate::catalog::primary_key_columns(conn, path).ok()?;
    if pk_cols.is_empty() {
        return None;
    }
    let mut indices = Vec::with_capacity(pk_cols.len());
    for pk in &pk_cols {
        let idx = columns
            .iter()
            .position(|c| c.name.as_ref() == pk.as_str())?;
        indices.push(idx as u32);
    }
    Some(Identity {
        field_indices: indices,
    })
}

fn handle_execute<'conn>(
    conn: &'conn rusqlite::Connection,
    req: &Request,
    cursors: &mut HashMap<u64, OpenScan<'conn>>,
    next_cursor_id: &mut u64,
) -> Result<ExecOutcome, DbError> {
    if let Request::Op(Op::Mutate(batch)) = req {
        return handle_mutate(conn, batch);
    }

    let prepared = prepare_request(req)?;
    let mut stmt = conn.prepare(&prepared.sql).map_err(map_sqlite_err)?;
    if stmt.column_count() == 0 {
        let bound: Vec<SqlParam<'_>> = prepared.params.iter().map(SqlParam).collect();
        let affected = stmt
            .execute(rusqlite::params_from_iter(bound))
            .map_err(map_sqlite_err)?;
        return Ok(ExecOutcome::Ack {
            affected: Some(affected as u64),
        });
    }

    let columns_preview = crate::scan::column_metas_for(&stmt);
    let resume_key = if prepared.order.len() == 1 {
        crate::compile::field_name(&prepared.order[0].path)
            .and_then(|name| columns_preview.iter().position(|c| c.name.as_ref() == name))
            .map(|idx| (idx, prepared.order[0].desc))
    } else {
        None
    };
    let identity = detect_identity(conn, req, &columns_preview);
    let schema = Arc::new(build_row_schema(&columns_preview, identity));

    let boxed = Box::new(stmt);
    let scan = OpenScan::from_prepared(boxed, columns_preview, &prepared.params, resume_key)?;
    let id = *next_cursor_id;
    *next_cursor_id += 1;
    cursors.insert(id, scan);
    Ok(ExecOutcome::Cursor { id, schema })
}

fn handle_mutate(
    conn: &rusqlite::Connection,
    batch: &MutationBatch,
) -> Result<ExecOutcome, DbError> {
    let manage_tx = conn.is_autocommit();
    if manage_tx {
        conn.execute_batch("BEGIN").map_err(map_sqlite_err)?;
    }
    let mut total_affected: u64 = 0;
    for m in &batch.mutations {
        match apply_mutation(conn, m) {
            Ok(n) => total_affected += n,
            Err(e) => {
                if manage_tx {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                return Err(e);
            }
        }
    }
    if manage_tx {
        conn.execute_batch("COMMIT").map_err(map_sqlite_err)?;
    }
    Ok(ExecOutcome::Ack {
        affected: Some(total_affected),
    })
}

fn keyed_where(
    key: &[(datagrep_api::FieldPath, Value)],
    params: &mut Vec<Value>,
) -> Result<String, DbError> {
    if key.is_empty() {
        return Err(DbError::Unsupported {
            feature: "mutation with no row identity — refuse to guess which row to affect"
                .to_string(),
        });
    }
    let clauses = key
        .iter()
        .map(|(field, value)| {
            params.push(value.clone());
            Ok::<_, DbError>(format!("{} = ?", crate::compile::field_ident(field)?))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(clauses.join(" AND "))
}

fn refuse_expect(expect: &[(datagrep_api::FieldPath, Value)]) -> Result<(), DbError> {
    if expect.is_empty() {
        return Ok(());
    }
    Err(DbError::Unsupported {
        feature: "conditional mutation (`expect`) — this driver cannot check-and-set".into(),
    })
}

fn apply_mutation(conn: &rusqlite::Connection, m: &Mutation) -> Result<u64, DbError> {
    match m {
        Mutation::Insert { path, doc } => {
            let Value::Document(d) = doc else {
                return Err(DbError::Unsupported {
                    feature: "Mutation::Insert with a non-Document value".to_string(),
                });
            };
            let table = crate::compile::compile_object_path(path)?;
            let cols = d
                .iter()
                .map(|(k, _)| quote_ident(k))
                .collect::<Result<Vec<_>, _>>()?;
            let placeholders = vec!["?"; d.len()].join(", ");
            let sql = format!(
                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                cols.join(", ")
            );
            let params: Vec<Value> = d.iter().map(|(_, v)| v.clone()).collect();
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let bound: Vec<SqlParam<'_>> = params.iter().map(SqlParam).collect();
            let n = stmt
                .execute(rusqlite::params_from_iter(bound))
                .map_err(map_sqlite_err)?;
            Ok(n as u64)
        }
        Mutation::Update {
            path,
            key,
            sets,
            expect,
        } => {
            refuse_expect(expect)?;
            if sets.is_empty() {
                return Err(DbError::Query {
                    code: None,
                    message: "Mutation::Update with no `sets`".to_string(),
                    position: None,
                });
            }
            let table = crate::compile::compile_object_path(path)?;
            let mut params: Vec<Value> = Vec::new();
            let set_sql = sets
                .iter()
                .map(|(field, value)| {
                    let f = crate::compile::field_ident(field)?;
                    params.push(value.clone());
                    Ok::<_, DbError>(format!("{f} = ?"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let where_sql = keyed_where(key, &mut params)?;
            let sql = format!(
                "UPDATE {table} SET {} WHERE {where_sql}",
                set_sql.join(", ")
            );
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let bound: Vec<SqlParam<'_>> = params.iter().map(SqlParam).collect();
            let n = stmt
                .execute(rusqlite::params_from_iter(bound))
                .map_err(map_sqlite_err)?;
            expect_exactly_one(n, "change")?;
            Ok(n as u64)
        }
        Mutation::Delete { path, key, expect } => {
            refuse_expect(expect)?;
            let table = crate::compile::compile_object_path(path)?;
            let mut params: Vec<Value> = Vec::new();
            let where_sql = keyed_where(key, &mut params)?;
            let sql = format!("DELETE FROM {table} WHERE {where_sql}");
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let bound: Vec<SqlParam<'_>> = params.iter().map(SqlParam).collect();
            let n = stmt
                .execute(rusqlite::params_from_iter(bound))
                .map_err(map_sqlite_err)?;
            expect_exactly_one(n, "delete")?;
            Ok(n as u64)
        }
    }
}

fn expect_exactly_one(n: usize, verb: &str) -> Result<(), DbError> {
    if n == 1 {
        Ok(())
    } else {
        Err(DbError::Query {
            code: None,
            message: format!(
                "expected exactly 1 row to {verb}, {n} did — row identity changed, refresh"
            ),
            position: None,
        })
    }
}

fn handle_begin(
    conn: &rusqlite::Connection,
    opts: &TxOpts,
    tx_forced_read_only: &mut bool,
    session_read_only: bool,
) -> Result<(), DbError> {
    if let Some(level) = opts.isolation {
        if level != IsolationLevel::Serializable {
            return Err(DbError::Unsupported {
                feature: format!(
                    "SQLite only offers serializable isolation within one connection; {level:?} was requested"
                ),
            });
        }
    }
    conn.execute_batch("BEGIN").map_err(map_sqlite_err)?;
    if opts.read_only && !session_read_only {
        conn.execute_batch("PRAGMA query_only = ON")
            .map_err(map_sqlite_err)?;
        *tx_forced_read_only = true;
    }
    Ok(())
}

fn handle_end_tx(
    conn: &rusqlite::Connection,
    commit: bool,
    tx_forced_read_only: &mut bool,
    session_read_only: bool,
) -> Result<(), DbError> {
    let stmt = if commit { "COMMIT" } else { "ROLLBACK" };
    conn.execute_batch(stmt).map_err(map_sqlite_err)?;
    if *tx_forced_read_only && !session_read_only {
        conn.execute_batch("PRAGMA query_only = OFF")
            .map_err(map_sqlite_err)?;
    }
    *tx_forced_read_only = false;
    Ok(())
}

pub struct SqliteConnection {
    jobs: JobSender,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    interrupt_handle: Arc<rusqlite::InterruptHandle>,
    capabilities: Capabilities,
    server_info: ServerInfo,
    closed: AtomicBool,
}

impl SqliteConnection {
    pub(crate) async fn open(
        path: String,
        read_only: bool,
        ctx: &ConnectCtx,
        capabilities: Capabilities,
    ) -> Result<Self, DbError> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<WorkerMsg>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<WorkerReady, DbError>>();
        let thread_path = path.clone();
        let handle = thread::Builder::new()
            .name(format!("datagrep-sqlite-worker[{path}]"))
            .spawn(move || run_worker(thread_path, read_only, cmd_rx, ready_tx))
            .map_err(|e| DbError::Connect(format!("failed to spawn SQLite worker thread: {e}")))?;

        let ready: WorkerReady = match ctx.connect_timeout {
            Some(d) => tokio::time::timeout(d, ready_rx)
                .await
                .map_err(|_| DbError::Timeout)?
                .map_err(|_| DbError::Closed)??,
            None => ready_rx.await.map_err(|_| DbError::Closed)??,
        };

        Ok(Self {
            jobs: JobSender(cmd_tx),
            worker: Mutex::new(Some(handle)),
            interrupt_handle: Arc::new(ready.interrupt_handle),
            capabilities,
            server_info: ready.server_info,
            closed: AtomicBool::new(false),
        })
    }

    fn ensure_open(&self) -> Result<(), DbError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(DbError::Closed)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl Connection for SqliteConnection {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.ensure_open()?;
        self.jobs.ping().await
    }

    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        self.ensure_open()?;
        let outcome = self.jobs.execute(req).await?;
        Ok(Box::new(SqliteCursor::new(outcome, self.jobs.clone())))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(SqliteCanceller::new(Arc::clone(&self.interrupt_handle)))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        Arc::new(SqliteCatalog {
            jobs: self.jobs.clone(),
        })
    }

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        self.ensure_open()?;
        self.jobs.begin(opts).await?;
        Ok(Box::new(SqliteTransaction::new(self.jobs.clone())))
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        self.ensure_open()?;
        self.jobs.set_read_only(on).await
    }

    async fn close(&self) -> Result<(), DbError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.jobs.shutdown();
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            tokio::task::spawn_blocking(move || handle.join())
                .await
                .map_err(|e| DbError::DriverPanic(e.to_string()))?
                .map_err(|_| DbError::DriverPanic("SQLite worker thread panicked".to_string()))?;
        }
        Ok(())
    }
}
