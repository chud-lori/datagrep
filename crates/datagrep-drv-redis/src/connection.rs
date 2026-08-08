//! [`RedisConnection`]: the `Connection` impl (design §3.1 requirement 2).
//!
//! Three request doors, matching `datagrep-api`'s `Request` enum:
//! - [`Request::Native`] — hand-typed redis-cli text. Split into one command
//!   per line with `datagrep_lang::redis` (already built/tested — never
//!   reimplemented here), each dispatched in turn. The *last* command's
//!   reply becomes the returned cursor: `Shape::Ack` for `OK`/nil/integer
//!   replies, `Shape::Pairs` otherwise (design §3.6: text is never
//!   translated, only tokenized and dispatched).
//! - [`Request::Op`]`(Scan)` — the portable browse path. `SCAN`/`HSCAN`/
//!   `SSCAN`/`ZSCAN` only, cursor-paged, **never `KEYS`** (design §5.2).
//!   A 3-part path (one key) dispatches `TYPE` first and routes to the
//!   right bounded reader — `RedisPairsCursor` for hash/set/zset,
//!   `ListCursor` for a list, `StreamCursor` for a stream, or a one-shot
//!   `GET` for a string — so a 1M-field hash pages instead of coming back
//!   whole (design §3.1 requirement 2).
//! - [`Request::Op`]`(Count)` — `DBSIZE` for the whole keyspace (exact,
//!   O(1)); a per-key cardinality command for one key; a SCAN walk
//!   (exact, cancellable) or a single-round SCAN extrapolation (estimate)
//!   for a filtered/prefixed subset, since Redis has no O(1) way to count a
//!   subset (`driver.rs`'s `REDIS_CAPS` doc: "`EXACT_COUNT_CHEAP`").
//! - [`Request::Op`]`(Mutate)` — `SET`/`HSET`/`DEL`, batched through one
//!   `MULTI`/`EXEC` pipeline so the batch is atomic "where the engine
//!   allows" (`MutationBatch`'s own doc), even though interactive
//!   transactions (`begin`) are not offered — see that method's doc for why
//!   those are different claims.
//!
//! Cancellation (design §3.3): every request gets a fresh [`CancelFlag`],
//! stashed behind a mutex so a subsequently-obtained [`Canceller`] targets
//! *this* request. Commands that block the connection waiting on the server
//! (`BLPOP`, `WAIT`, `XREAD BLOCK`, …) additionally get their `CLIENT ID`
//! recorded so the canceller can `CLIENT KILL ID` them — seeded via
//! `crate::cmd::is_blocking_invocation`, already built and tested.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use datagrep_api::{
    Batch, CancelFlag, Canceller, Capabilities, Caps, Catalog, Connection, Cursor, CursorStats,
    DbError, Enforcement, FetchHint, Mutation, MutationBatch, ObjectPath, Op, PathSeg, Payload,
    Predicate, Request, ResumeToken, ServerInfo, Shape, Transaction, TxOpts, Value, ValueKind,
};

use datagrep_lang::Language;

use crate::canceller::{BlockingClientId, RedisCanceller};
use crate::catalog::{RedisCatalog, NO_PREFIX_BUCKET};
use crate::cmd::{cmd_from_args, compile_glob, is_blocking_invocation, prefix_glob};
use crate::cursor::{ListCursor, OneShotCursor, RedisPairsCursor, ScanFamily, StreamCursor};
use crate::driver::redis_capabilities_baseline;
use crate::error::map_redis_error;
use crate::value::from_resp;

/// One live Redis connection. Holds no direct socket state itself — the
/// `redis` crate's own `ConnectionManager` (auto-reconnecting, cheaply
/// cloneable) does that; this type adds only what `datagrep-api` requires on top
/// (capability gating, cancellation bookkeeping, the closed flag).
pub struct RedisConnection {
    manager: redis::aio::ConnectionManager,
    client: redis::Client,
    server_info: ServerInfo,
    /// Whether `KEY_ENUMERATION` is on, decided once at connect time from a
    /// `DBSIZE` probe (`driver.rs`, `catalog::key_enumeration_from_dbsize`).
    /// Redis has no notion of the keyspace shrinking back under the
    /// threshold mid-session that we'd want to react to — this is a
    /// connect-time decision, not a live one.
    key_enumeration: bool,
    /// The `CancelFlag` for whatever request is currently (or was most
    /// recently) dispatched. `execute` replaces it with a fresh flag before
    /// building a cursor — `CancelFlag` has no reset, so "uncancelled for
    /// this request" can only mean "a new flag" (design §3.3).
    active_cancel: Mutex<CancelFlag>,
    blocking_client_id: BlockingClientId,
    closed: AtomicBool,
}

impl RedisConnection {
    pub fn new(
        manager: redis::aio::ConnectionManager,
        client: redis::Client,
        server_info: ServerInfo,
        key_enumeration: bool,
    ) -> Self {
        Self {
            manager,
            client,
            server_info,
            key_enumeration,
            active_cancel: Mutex::new(CancelFlag::new()),
            blocking_client_id: Arc::new(AtomicI64::new(0)),
            closed: AtomicBool::new(false),
        }
    }

    fn ensure_open(&self) -> Result<(), DbError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(DbError::Closed)
        } else {
            Ok(())
        }
    }

    /// Install a brand new [`CancelFlag`] as "the current request's flag"
    /// and return a clone of it for the cursor about to be built.
    fn fresh_cancel(&self) -> CancelFlag {
        let flag = CancelFlag::new();
        let mut slot = self
            .active_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = flag.clone();
        flag
    }

    // ---- Request::Native --------------------------------------------

    /// Split `text` into one redis-cli command per line (`datagrep_lang::redis`,
    /// already tokenizer-tested), dispatch each in turn, and shape the
    /// *last* command's reply. A non-final command that errors aborts the
    /// remaining ones rather than silently skipping past a failure.
    async fn execute_native(
        &self,
        text: &str,
        cancel: CancelFlag,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let spans = datagrep_lang::redis::REDIS.split(text);
        let mut commands: Vec<Vec<String>> = Vec::with_capacity(spans.len());
        for span in &spans {
            let line = span.text(text);
            let args = datagrep_lang::redis::tokenize_args(line).map_err(|e| DbError::Query {
                code: None,
                message: e.to_string(),
                position: None,
            })?;
            let values: Vec<String> = args.into_iter().map(|a| a.value).collect();
            if !values.is_empty() {
                commands.push(values);
            }
        }
        if commands.is_empty() {
            return Ok(Box::new(OneShotCursor::ack(
                None,
                Some(Arc::from("no command")),
            )));
        }

        let mut mgr = self.manager.clone();
        let last_idx = commands.len() - 1;
        let mut result: Option<Box<dyn Cursor>> = None;
        for (i, args) in commands.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(DbError::Cancelled);
            }
            let is_last = i == last_idx;
            if is_last {
                if let Some(cursor) = self.native_scan_cursor(args, cancel.clone())? {
                    result = Some(cursor);
                    break;
                }
            }
            let cmd = cmd_from_args(args);
            let blocking = is_blocking_invocation(args);
            let reply = self.dispatch(&mut mgr, cmd, blocking).await?;
            if is_last {
                result = Some(native_reply_cursor(args, reply));
            }
        }
        Ok(result
            .unwrap_or_else(|| Box::new(OneShotCursor::ack(None, Some(Arc::from("no command"))))))
    }

    /// Recognizes a hand-typed `SCAN`/`HSCAN`/`SSCAN`/`ZSCAN` as the final
    /// statement of a `Request::Native` buffer and routes it through the
    /// same paging [`RedisPairsCursor`] the structured `Op::Scan` path
    /// uses, so a user who types `SCAN 0 MATCH user:*` by hand still gets a
    /// resumable, bounded cursor instead of a single unpaged round trip
    /// (design §3.1 requirement 2/3, §5.2). Returns `Ok(None)` for any other
    /// command, which falls through to the generic one-shot dispatch.
    fn native_scan_cursor(
        &self,
        args: &[String],
        cancel: CancelFlag,
    ) -> Result<Option<Box<dyn Cursor>>, DbError> {
        let Some(cmd_name) = args.first() else {
            return Ok(None);
        };
        let family = if cmd_name.eq_ignore_ascii_case("SCAN") {
            ScanFamily::Keyspace
        } else if cmd_name.eq_ignore_ascii_case("HSCAN") {
            ScanFamily::Hash
        } else if cmd_name.eq_ignore_ascii_case("SSCAN") {
            ScanFamily::Set
        } else if cmd_name.eq_ignore_ascii_case("ZSCAN") {
            ScanFamily::SortedSet
        } else {
            return Ok(None);
        };

        let (key, cursor_idx) = match family {
            ScanFamily::Keyspace => (None, 1usize),
            _ => (
                Some(args.get(1).cloned().ok_or_else(|| DbError::Query {
                    code: None,
                    message: format!("{cmd_name} needs a key argument"),
                    position: None,
                })?),
                2usize,
            ),
        };
        let resume = args
            .get(cursor_idx)
            .map(|c| ResumeToken(datagrep_api::Bytes::from(c.clone().into_bytes())));
        let match_glob = find_option_value(args, "MATCH");

        Ok(Some(Box::new(RedisPairsCursor::new(
            self.manager.clone(),
            family,
            key,
            match_glob,
            resume,
            cancel,
        ))))
    }

    /// Dispatch one already-built command. When `blocking` is set, learns
    /// this connection's `CLIENT ID` immediately beforehand and clears it
    /// immediately after, so `RedisCanceller` can `CLIENT KILL ID` it while
    /// it's in flight and never target a stale id once it returns (design
    /// §3.3, `canceller.rs`'s `blocking_client_id` doc).
    async fn dispatch(
        &self,
        mgr: &mut redis::aio::ConnectionManager,
        cmd: redis::Cmd,
        blocking: bool,
    ) -> Result<redis::Value, DbError> {
        if blocking {
            if let Ok(id) = redis::cmd("CLIENT").arg("ID").query_async::<i64>(mgr).await {
                self.blocking_client_id.store(id, Ordering::Release);
            }
        }
        let result = cmd
            .query_async::<redis::Value>(mgr)
            .await
            .map_err(map_redis_error);
        if blocking {
            self.blocking_client_id.store(0, Ordering::Release);
        }
        result
    }

    // ---- Request::Op(Scan) --------------------------------------------

    async fn scan_cursor(
        &self,
        path: &ObjectPath,
        filter: Option<&Predicate>,
        resume: Option<ResumeToken>,
        cancel: CancelFlag,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let glob = filter.map(compile_glob).transpose()?;
        match path.parts() {
            [] => Err(DbError::Unsupported {
                feature: "Op::Scan at the catalog root — pick a database index first".into(),
            }),
            [_db] => Ok(Box::new(RedisPairsCursor::new(
                self.manager.clone(),
                ScanFamily::Keyspace,
                None,
                glob,
                resume,
                cancel,
            ))),
            [_db, prefix] => {
                if glob.is_some() {
                    return Err(DbError::Unsupported {
                        feature: format!(
                            "Op::Scan filter combined with a keyspace-prefix path (`{prefix}`) \
                             — Redis MATCH takes exactly one glob pattern and the prefix already \
                             supplies it"
                        ),
                    });
                }
                if &**prefix == NO_PREFIX_BUCKET {
                    // The catalog's sentinel bucket for keys with no `:` —
                    // there is no MATCH glob for "does not contain a
                    // character"; scan everything and filter client-side so
                    // this shows exactly the keys `RedisCatalog::list_keys`
                    // promised under this bucket (catalog.rs).
                    return Ok(Box::new(NoColonFilterCursor {
                        inner: RedisPairsCursor::new(
                            self.manager.clone(),
                            ScanFamily::Keyspace,
                            None,
                            None,
                            resume,
                            cancel,
                        ),
                    }));
                }
                Ok(Box::new(RedisPairsCursor::new(
                    self.manager.clone(),
                    ScanFamily::Keyspace,
                    None,
                    Some(prefix_glob(prefix)),
                    resume,
                    cancel,
                )))
            }
            [_db, _prefix, key] => self.key_value_cursor(key.as_ref(), resume, cancel).await,
            _ => Err(DbError::Unsupported {
                feature: "catalog path deeper than the key level (3 levels: db-index, \
                          keyspace-prefix, key)"
                    .into(),
            }),
        }
    }

    /// `TYPE key` first, then route to the bounded reader for that type —
    /// never come back with the whole value in one shot for an aggregate
    /// type (design §3.1 requirement 2). A missing key maps to
    /// [`Value::Absent`], never [`Value::Null`] (value.rs's own doc: this is
    /// exactly the caller who knows *why* it got a Nil).
    async fn key_value_cursor(
        &self,
        key: &str,
        resume: Option<ResumeToken>,
        cancel: CancelFlag,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let mut mgr = self.manager.clone();
        let ty: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut mgr)
            .await
            .map_err(map_redis_error)?;
        match ty.as_str() {
            "none" => Ok(Box::new(OneShotCursor::new(
                Shape::Pairs {
                    value_kind: ValueKind::Unknown,
                },
                Payload::Pairs(vec![(Value::Str(Arc::from(key)), Value::Absent)]),
            ))),
            "hash" => Ok(Box::new(RedisPairsCursor::new(
                mgr,
                ScanFamily::Hash,
                Some(key.to_string()),
                None,
                resume,
                cancel,
            ))),
            "set" => Ok(Box::new(RedisPairsCursor::new(
                mgr,
                ScanFamily::Set,
                Some(key.to_string()),
                None,
                resume,
                cancel,
            ))),
            "zset" => Ok(Box::new(RedisPairsCursor::new(
                mgr,
                ScanFamily::SortedSet,
                Some(key.to_string()),
                None,
                resume,
                cancel,
            ))),
            "list" => Ok(Box::new(ListCursor::new(
                mgr,
                key.to_string(),
                resume,
                cancel,
            ))),
            "stream" => Ok(Box::new(StreamCursor::new(
                mgr,
                key.to_string(),
                resume,
                cancel,
            ))),
            "string" => {
                let v: redis::Value = redis::cmd("GET")
                    .arg(key)
                    .query_async(&mut mgr)
                    .await
                    .map_err(map_redis_error)?;
                // Honest re-mapping: if the key vanished between TYPE and
                // GET, a Nil here means "now absent", not "stored null"
                // (value.rs's documented Nil-is-overloaded caveat).
                let mapped = match from_resp(v) {
                    Value::Null => Value::Absent,
                    other => other,
                };
                Ok(Box::new(OneShotCursor::new(
                    Shape::Pairs {
                        value_kind: ValueKind::Str,
                    },
                    Payload::Pairs(vec![(Value::Str(Arc::from(key)), mapped)]),
                )))
            }
            other => Err(DbError::Unsupported {
                feature: format!(
                    "browsing a Redis key of TYPE {other:?} (unrecognized by this driver)"
                ),
            }),
        }
    }

    // ---- Request::Op(Count) --------------------------------------------

    async fn count(
        &self,
        path: &ObjectPath,
        filter: Option<&Predicate>,
        exact: bool,
        cancel: CancelFlag,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let mut mgr = self.manager.clone();
        match path.parts() {
            [_db, _prefix, key] => {
                if filter.is_some() {
                    return Err(DbError::Unsupported {
                        feature: "Op::Count filter on a single-key cardinality — HLEN/SCARD/\
                                  ZCARD/LLEN/XLEN take no filter"
                            .into(),
                    });
                }
                let ty: String = redis::cmd("TYPE")
                    .arg(key.as_ref())
                    .query_async(&mut mgr)
                    .await
                    .map_err(map_redis_error)?;
                let cmd_name = match ty.as_str() {
                    "none" => return Ok(Box::new(OneShotCursor::ack(Some(0), None))),
                    "hash" => "HLEN",
                    "set" => "SCARD",
                    "zset" => "ZCARD",
                    "list" => "LLEN",
                    "stream" => "XLEN",
                    other => {
                        return Err(DbError::Unsupported {
                            feature: format!(
                                "counting a Redis key of TYPE {other:?} — no natural element count \
                                 (use Op::Scan to inspect a string's value instead)"
                            ),
                        })
                    }
                };
                let n: i64 = redis::cmd(cmd_name)
                    .arg(key.as_ref())
                    .query_async(&mut mgr)
                    .await
                    .map_err(map_redis_error)?;
                Ok(Box::new(OneShotCursor::ack(Some(n.max(0) as u64), None)))
            }
            parts => {
                let glob = filter.map(compile_glob).transpose()?;
                let scoped_glob = match parts {
                    [_db, prefix] => Some(prefix_glob(prefix)),
                    _ => None,
                };
                if glob.is_some() && scoped_glob.is_some() {
                    return Err(DbError::Unsupported {
                        feature: "Op::Count filter combined with a keyspace-prefix path".into(),
                    });
                }
                let effective = glob.or(scoped_glob);
                let Some(effective) = effective else {
                    let dbsize: i64 = redis::cmd("DBSIZE")
                        .query_async(&mut mgr)
                        .await
                        .map_err(map_redis_error)?;
                    return Ok(Box::new(OneShotCursor::ack(
                        Some(dbsize.max(0) as u64),
                        None,
                    )));
                };
                if exact {
                    self.count_exact_scan(&effective, cancel).await
                } else {
                    self.count_estimate_scan(&effective, cancel).await
                }
            }
        }
    }

    /// Full `SCAN` walk to completion, tallying matches. Exact, but O(N) in
    /// keyspace size — the caller asked for `exact: true` knowingly
    /// (`EXACT_COUNT_CHEAP` is false for exactly this reason). Reuses
    /// `RedisPairsCursor` rather than re-parsing SCAN replies, and honors
    /// `cancel` at each round the same way a browse SCAN would.
    async fn count_exact_scan(
        &self,
        glob: &str,
        cancel: CancelFlag,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let mut cursor = RedisPairsCursor::new(
            self.manager.clone(),
            ScanFamily::Keyspace,
            None,
            Some(glob.to_string()),
            None,
            cancel,
        );
        let hint = FetchHint {
            max_rows: 1000,
            ..FetchHint::default()
        };
        while cursor.next_batch(hint).await?.is_some() {}
        Ok(Box::new(OneShotCursor::ack(
            Some(cursor.stats().rows),
            None,
        )))
    }

    /// One bounded `SCAN` round, extrapolated against `DBSIZE`. Cheap, but
    /// explicitly approximate — the returned message says so, per
    /// `Caps::EXACT_COUNT_CHEAP`'s "false → the UI shows '≥ N'" contract.
    async fn count_estimate_scan(
        &self,
        glob: &str,
        cancel: CancelFlag,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let mut mgr = self.manager.clone();
        let dbsize: i64 = redis::cmd("DBSIZE")
            .query_async(&mut mgr)
            .await
            .map_err(map_redis_error)?;
        let sample_count: u32 = 1000;
        let mut cursor = RedisPairsCursor::new(
            self.manager.clone(),
            ScanFamily::Keyspace,
            None,
            Some(glob.to_string()),
            None,
            cancel,
        );
        let batch = cursor
            .next_batch(FetchHint {
                max_rows: sample_count,
                ..FetchHint::default()
            })
            .await?;
        let matched_n = match batch.map(|b| b.payload) {
            Some(Payload::Pairs(p)) => p.len(),
            _ => 0,
        };
        if cursor.resume_token().is_none() {
            // The whole keyspace fit in one SCAN round — the sample IS the
            // exact count, so say so rather than dressing it up as one.
            return Ok(Box::new(OneShotCursor::ack(Some(matched_n as u64), None)));
        }
        let ratio = matched_n as f64 / sample_count as f64;
        let estimate = (ratio * dbsize as f64).round().max(0.0) as u64;
        Ok(Box::new(OneShotCursor::ack(
            Some(estimate),
            Some(Arc::from(format!(
                "estimate — extrapolated from a {sample_count}-key SCAN sample against \
                 DBSIZE={dbsize}; not exact"
            ))),
        )))
    }

    // ---- Request::Op(Mutate) --------------------------------------------

    /// `SET`/`HSET`/`DEL` batched through one `MULTI`/`EXEC` pipeline —
    /// atomic across the batch even though `begin()` (interactive
    /// transactions) is unsupported; MULTI/EXEC is exactly the "single
    /// optimistic pipeline" `driver.rs`'s `REDIS_CAPS` doc already commits
    /// to, just used once per batch instead of exposed interactively.
    async fn mutate(&self, batch: MutationBatch) -> Result<Box<dyn Cursor>, DbError> {
        if batch.mutations.is_empty() {
            return Ok(Box::new(OneShotCursor::ack(Some(0), None)));
        }
        let mut pipe = redis::pipe();
        pipe.atomic();
        for m in &batch.mutations {
            add_mutation_to_pipe(&mut pipe, m)?;
        }
        let mut mgr = self.manager.clone();
        let replies: Vec<redis::Value> =
            pipe.query_async(&mut mgr).await.map_err(map_redis_error)?;
        let mut total: u64 = 0;
        for reply in replies {
            total += mutation_affected(reply)?;
        }
        Ok(Box::new(OneShotCursor::ack(Some(total), None)))
    }
}

#[async_trait]
impl Connection for RedisConnection {
    fn capabilities(&self) -> Capabilities {
        let mut caps = redis_capabilities_baseline();
        if !self.key_enumeration {
            caps.flags.remove(Caps::KEY_ENUMERATION);
        }
        caps
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.ensure_open()?;
        let mut mgr = self.manager.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut mgr)
            .await
            .map(|_| ())
            .map_err(map_redis_error)
    }

    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        self.ensure_open()?;
        let cancel = self.fresh_cancel();
        match req {
            Request::Native { text, params, .. } => {
                if !params.is_empty() {
                    return Err(DbError::Unsupported {
                        feature: "parameterized Request::Native params — Redis's protocol has no \
                                  bind-parameter form for CLI text (ParamStyle::None); splice \
                                  values into the command text instead"
                            .into(),
                    });
                }
                self.execute_native(&text, cancel).await
            }
            Request::Op(Op::Scan {
                path,
                filter,
                order,
                project,
                limit,
                resume,
            }) => {
                if !order.is_empty() {
                    return Err(DbError::Unsupported {
                        feature: "ORDER BY — no Redis SCAN-family command guarantees iteration \
                                  order (not hash-table order, and not even a sorted set's score \
                                  order); honoring one would misrepresent the result"
                            .into(),
                    });
                }
                if project.as_ref().is_some_and(|p| !p.is_empty()) {
                    return Err(DbError::Unsupported {
                        feature: "column projection — Shape::Pairs has only a key and a value \
                                  side; refusing rather than silently dropping the requested \
                                  fields"
                            .into(),
                    });
                }
                let cursor = self
                    .scan_cursor(&path, filter.as_ref(), resume, cancel)
                    .await?;
                Ok(match limit {
                    Some(n) => Box::new(LimitedCursor::new(cursor, n)),
                    None => cursor,
                })
            }
            Request::Op(Op::Count {
                path,
                filter,
                exact,
            }) => self.count(&path, filter.as_ref(), exact, cancel).await,
            Request::Op(Op::Mutate(batch)) => self.mutate(batch).await,
            Request::Op(Op::Explain { .. }) => Err(DbError::Unsupported {
                feature: "EXPLAIN — Redis commands are O(1)/O(N) primitives with no query \
                          planner to explain (Caps::EXPLAIN is not set)"
                    .into(),
            }),
            Request::Op(Op::Ddl(_)) => Err(DbError::Unsupported {
                feature: "DDL — Redis has no schema to declare (Caps::DDL is not set)".into(),
            }),
        }
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        let flag = self
            .active_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Arc::new(RedisCanceller::new(
            flag,
            self.blocking_client_id.clone(),
            self.client.clone(),
        ))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        Arc::new(RedisCatalog::new(self.manager.clone()))
    }

    /// Always fails: `MULTI`/`EXEC` is a single optimistic pipeline, not an
    /// interactive transaction — no mid-transaction reads of your own
    /// writes, no savepoints, no partial rollback. Returning `Unsupported`
    /// here (rather than offering a `Transaction` that lies about what it
    /// can do) is what lets the UI grey the "begin transaction" button
    /// instead of erroring at runtime (design §2.6; `driver.rs`'s
    /// `REDIS_CAPS` doc comment).
    async fn begin(&self, _opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        Err(DbError::Unsupported {
            feature: "interactive transactions — Redis MULTI/EXEC is a single optimistic \
                      pipeline, not an interactive transaction (no mid-transaction reads of \
                      your own writes, no savepoints)"
                .into(),
        })
    }

    /// Redis has **no server-side session read-only mode** — there is no
    /// command that puts a connection into a state where the server itself
    /// refuses writes for the rest of the session (unlike SQLite's `PRAGMA
    /// query_only` or a Postgres `SET default_transaction_read_only`).
    /// `Enforcement::Client` is the honest answer: only `datagrep-lang`'s
    /// classifier (layer 2 of design §3.8's three guardrails) stands
    /// between a write command and the server, and the UI's read-only badge
    /// must say exactly that rather than implying a server-side guarantee
    /// that does not exist here.
    async fn set_read_only(&self, _on: bool) -> Result<Enforcement, DbError> {
        self.ensure_open()?;
        Ok(Enforcement::Client)
    }

    async fn close(&self) -> Result<(), DbError> {
        // No explicit teardown call exists on `redis::aio::ConnectionManager`
        // — it has no server-side session state to release beyond the TCP
        // socket itself, which drops when this `RedisConnection` does.
        // Marking `closed` is what makes every *subsequent* call through
        // this handle honestly report `DbError::Closed` instead of quietly
        // continuing to work off a manager the caller believes is gone.
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Shape a hand-typed command's reply. `OK`/nil/integer replies become
/// `Shape::Ack`; a `Map`-shaped reply (e.g. `CONFIG GET`, `HGETALL` under
/// RESP3) becomes field-keyed `Shape::Pairs`; an `Array`-shaped reply
/// (`LRANGE`, `SMEMBERS`, …) becomes index-keyed `Shape::Pairs` — the same
/// convention `ListCursor` uses for its own rows; anything else becomes a
/// single row keyed by the command name so no reply is ever dropped on the
/// floor (design §3.1's "never lose bytes", extended here to "never lose a
/// reply").
fn native_reply_cursor(args: &[String], reply: redis::Value) -> Box<dyn Cursor> {
    let command = args.first().cloned().unwrap_or_default();
    let value = from_resp(reply);
    match value {
        Value::Str(s) if &*s == "OK" => Box::new(OneShotCursor::ack(None, Some(Arc::from("OK")))),
        Value::Null => Box::new(OneShotCursor::ack(None, Some(Arc::from("(nil)")))),
        Value::I64(n) => {
            let affected = (n >= 0).then_some(n as u64);
            Box::new(OneShotCursor::ack(affected, Some(Arc::from(n.to_string()))))
        }
        Value::Document(doc) => {
            let pairs = doc
                .iter()
                .map(|(k, v)| (Value::Str(k.clone()), v.clone()))
                .collect();
            Box::new(OneShotCursor::new(
                Shape::Pairs {
                    value_kind: ValueKind::Document,
                },
                Payload::Pairs(pairs),
            ))
        }
        Value::Array(items) => {
            let pairs = items
                .iter()
                .cloned()
                .enumerate()
                .map(|(i, v)| (Value::I64(i as i64), v))
                .collect();
            Box::new(OneShotCursor::new(
                Shape::Pairs {
                    value_kind: ValueKind::Unknown,
                },
                Payload::Pairs(pairs),
            ))
        }
        other => {
            let pairs = vec![(Value::Str(Arc::from(command.as_str())), other)];
            Box::new(OneShotCursor::new(
                Shape::Pairs {
                    value_kind: ValueKind::Unknown,
                },
                Payload::Pairs(pairs),
            ))
        }
    }
}

/// Case-insensitively find `option`'s following argument (e.g. `MATCH
/// user:*` → `Some("user:*")`).
fn find_option_value(args: &[String], option: &str) -> Option<String> {
    args.iter()
        .position(|a| a.eq_ignore_ascii_case(option))
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Resolve the redis key a `Mutation` targets. The row-identity `key: Vec<Value>`
/// (mirroring the SQL drivers' PK convention) wins when given as a single
/// string/bytes value; otherwise the last segment of `path` is used — the
/// two conventions a caller might reasonably follow, both honored rather
/// than forcing one (deviation noted in the crate report: `datagrep-api` does
/// not pin down which one is canonical for a flat keyspace).
fn mutation_key(path: &ObjectPath, key: &[Value]) -> Result<String, DbError> {
    match key {
        [] => path
            .parts()
            .last()
            .map(|s| s.to_string())
            .ok_or_else(|| DbError::Unsupported {
                feature: "mutation with neither a row-identity `key` nor a path segment naming \
                          the Redis key"
                    .into(),
            }),
        [Value::Str(s)] => Ok(s.to_string()),
        [Value::Bytes(b)] => {
            std::str::from_utf8(b)
                .map(|s| s.to_string())
                .map_err(|_| DbError::Unsupported {
                    feature:
                        "mutation key is not valid UTF-8 — Redis keys through this driver must \
                          be text"
                            .into(),
                })
        }
        other => Err(DbError::Unsupported {
            feature: format!(
                "mutation key {other:?} — a Redis key is a single string, not a composite"
            ),
        }),
    }
}

/// Whether `field` is the sentinel `value` field name used for
/// `Mutation::Update { sets: [(value, x)] }` on a plain string key (as
/// opposed to a named hash field).
fn is_value_field(field: &datagrep_api::FieldPath) -> bool {
    matches!(field.segments(), [PathSeg::Field(name)] if &**name == "value")
}

fn value_to_bytes(v: &Value) -> Result<Vec<u8>, DbError> {
    match v {
        Value::Str(s) => Ok(s.as_bytes().to_vec()),
        Value::Bytes(b) => Ok(b.to_vec()),
        Value::I64(n) => Ok(n.to_string().into_bytes()),
        Value::U64(n) => Ok(n.to_string().into_bytes()),
        Value::F64(f) => Ok(f.to_string().into_bytes()),
        Value::Decimal(d) => Ok(d.as_bytes().to_vec()),
        Value::Bool(b) => Ok(if *b { b"1".to_vec() } else { b"0".to_vec() }),
        Value::Null => Ok(Vec::new()),
        other => Err(DbError::Unsupported {
            feature: format!(
                "writing a {other:?} value to Redis — only scalar text/byte/number-shaped \
                 values map to a Redis string/field value (JSON-encode structured data client-side)"
            ),
        }),
    }
}

fn add_mutation_to_pipe(pipe: &mut redis::Pipeline, m: &Mutation) -> Result<(), DbError> {
    match m {
        Mutation::Insert { path, doc } => {
            let key = mutation_key(path, &[])?;
            match doc {
                Value::Document(d) => {
                    if d.is_empty() {
                        return Err(DbError::Query {
                            code: None,
                            message: "Mutation::Insert with an empty Document has no fields to \
                                      HSET"
                                .to_string(),
                            position: None,
                        });
                    }
                    pipe.cmd("HSET").arg(&key);
                    let mut any = false;
                    for (field, value) in d.iter() {
                        if &**field == "key" {
                            // Redundant with `path`; a Pairs-shaped edit row
                            // for a hash view carries {"key": field, "value": v}.
                            continue;
                        }
                        pipe.arg(field.as_bytes()).arg(value_to_bytes(value)?);
                        any = true;
                    }
                    if !any {
                        return Err(DbError::Query {
                            code: None,
                            message: "Mutation::Insert Document had only the redundant \"key\" \
                                      field — nothing to HSET"
                                .to_string(),
                            position: None,
                        });
                    }
                }
                scalar => {
                    pipe.cmd("SET").arg(&key).arg(value_to_bytes(scalar)?);
                }
            }
        }
        Mutation::Update { path, key, sets } => {
            if sets.is_empty() {
                return Err(DbError::Query {
                    code: None,
                    message: "Mutation::Update with no `sets`".to_string(),
                    position: None,
                });
            }
            let redis_key = mutation_key(path, key)?;
            if sets.len() == 1 && is_value_field(&sets[0].0) {
                pipe.cmd("SET")
                    .arg(&redis_key)
                    .arg(value_to_bytes(&sets[0].1)?);
            } else {
                pipe.cmd("HSET").arg(&redis_key);
                for (field, value) in sets {
                    pipe.arg(field.to_string().as_bytes())
                        .arg(value_to_bytes(value)?);
                }
            }
        }
        Mutation::Delete { path, key } => {
            let redis_key = mutation_key(path, key)?;
            pipe.cmd("DEL").arg(&redis_key);
        }
    }
    Ok(())
}

/// Each mutation's own Redis reply, taken at face value rather than coerced
/// into a uniform "1 row changed" — `DEL`/`HSET` report *their own* native
/// counts (keys removed / new fields added), which is honest but does mean
/// this is not directly comparable to a SQL affected-rows count (e.g.
/// overwriting existing hash fields legitimately reports 0 new fields even
/// though the write succeeded). Documented as a known limitation rather
/// than papered over.
fn mutation_affected(reply: redis::Value) -> Result<u64, DbError> {
    match reply {
        redis::Value::ServerError(e) => Err(DbError::Query {
            code: Some(e.code().to_string()),
            message: e.to_string(),
            position: None,
        }),
        redis::Value::Nil => Ok(0),
        redis::Value::Int(n) => Ok(n.max(0) as u64),
        _ => Ok(1),
    }
}

/// Wraps another cursor and stops once `limit` total rows have been
/// emitted. Redis's SCAN family has no server-side `LIMIT` — `Op::Scan`'s
/// `limit` (design §3.6) would otherwise be silently ignored, which the
/// rest of this crate refuses to do for a filter; enforcing it client-side
/// here keeps that same promise for a row cap.
struct LimitedCursor {
    inner: Box<dyn Cursor>,
    remaining: u64,
    done: bool,
}

impl LimitedCursor {
    fn new(inner: Box<dyn Cursor>, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            done: limit == 0,
        }
    }
}

#[async_trait]
impl Cursor for LimitedCursor {
    fn shape(&self) -> &Shape {
        self.inner.shape()
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        let capped_rows = hint
            .max_rows
            .min(self.remaining.min(u32::MAX as u64) as u32)
            .max(1);
        let capped = FetchHint {
            max_rows: capped_rows,
            ..hint
        };
        let Some(mut batch) = self.inner.next_batch(capped).await? else {
            self.done = true;
            return Ok(None);
        };
        let n = payload_len(&batch.payload) as u64;
        if n >= self.remaining {
            truncate_payload(&mut batch.payload, self.remaining as usize);
            self.done = true;
        } else {
            self.remaining -= n;
        }
        Ok(Some(batch))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        if self.done {
            None
        } else {
            self.inner.resume_token()
        }
    }

    fn stats(&self) -> CursorStats {
        self.inner.stats()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.inner.close().await
    }
}

fn payload_len(p: &Payload) -> usize {
    match p {
        Payload::Rows(r) => r.len(),
        Payload::Docs(d) => d.len(),
        Payload::Pairs(p) => p.len(),
        Payload::Graph(_) | Payload::Empty => 0,
    }
}

fn truncate_payload(p: &mut Payload, n: usize) {
    match p {
        Payload::Rows(r) => r.truncate(n),
        Payload::Docs(d) => d.truncate(n),
        Payload::Pairs(p) => p.truncate(n),
        Payload::Graph(_) | Payload::Empty => {}
    }
}

/// Wraps a keyspace [`RedisPairsCursor`] and drops any pair whose key
/// contains `:` — the client-side complement to `RedisCatalog`'s
/// `"(no prefix)"` bucket (`catalog.rs`'s `NO_PREFIX_BUCKET`), reused here
/// so `Op::Scan` on that bucket shows exactly the keys the catalog tree
/// promised.
struct NoColonFilterCursor {
    inner: RedisPairsCursor,
}

#[async_trait]
impl Cursor for NoColonFilterCursor {
    fn shape(&self) -> &Shape {
        self.inner.shape()
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        let Some(mut batch) = self.inner.next_batch(hint).await? else {
            return Ok(None);
        };
        if let Payload::Pairs(pairs) = &mut batch.payload {
            pairs.retain(|(k, _)| !key_contains_colon(k));
        }
        Ok(Some(batch))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.inner.resume_token()
    }

    fn stats(&self) -> CursorStats {
        self.inner.stats()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.inner.close().await
    }
}

fn key_contains_colon(v: &Value) -> bool {
    match v {
        Value::Str(s) => s.contains(':'),
        Value::Bytes(b) => b.contains(&b':'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_option_value_is_case_insensitive() {
        let args: Vec<String> = ["HSCAN", "k", "0", "match", "user:*"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            find_option_value(&args, "MATCH"),
            Some("user:*".to_string())
        );
        assert_eq!(find_option_value(&args, "COUNT"), None);
    }

    #[test]
    fn mutation_key_prefers_explicit_identity_then_falls_back_to_path() {
        let path = ObjectPath::new(vec![
            Arc::from("0"),
            Arc::from("user:"),
            Arc::from("user:42"),
        ]);
        assert_eq!(mutation_key(&path, &[]).unwrap(), "user:42");
        assert_eq!(
            mutation_key(&path, &[Value::Str(Arc::from("override"))]).unwrap(),
            "override"
        );
        assert!(mutation_key(&path, &[Value::I64(1), Value::I64(2)]).is_err());
    }

    #[test]
    fn value_to_bytes_rejects_structured_values() {
        assert!(value_to_bytes(&Value::Str(Arc::from("hi"))).is_ok());
        assert!(value_to_bytes(&Value::I64(42)).is_ok());
        assert!(value_to_bytes(&Value::Array(Arc::from(vec![Value::I64(1)]))).is_err());
    }

    #[test]
    fn mutation_affected_reads_native_redis_reply_shapes() {
        assert_eq!(mutation_affected(redis::Value::Int(3)).unwrap(), 3);
        assert_eq!(mutation_affected(redis::Value::Nil).unwrap(), 0);
        assert_eq!(
            mutation_affected(redis::Value::SimpleString("OK".into())).unwrap(),
            1
        );
    }

    #[test]
    fn key_contains_colon_checks_str_and_bytes() {
        assert!(key_contains_colon(&Value::Str(Arc::from("user:42"))));
        assert!(!key_contains_colon(&Value::Str(Arc::from("noColon"))));
        assert!(key_contains_colon(&Value::Bytes(
            datagrep_api::Bytes::from_static(b"a:b")
        )));
    }

    #[test]
    fn is_value_field_matches_only_the_literal_value_field() {
        assert!(is_value_field(&datagrep_api::FieldPath::field("value")));
        assert!(!is_value_field(&datagrep_api::FieldPath::field("other")));
    }
}
