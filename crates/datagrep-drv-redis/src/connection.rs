use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use datagrep_api::{
    Batch, CancelFlag, Canceller, Capabilities, Caps, Catalog, Connection, Cursor, CursorStats,
    DbError, Enforcement, FetchHint, FieldPath, Mutation, MutationBatch, ObjectPath, Op, PathSeg,
    Payload, Predicate, Request, ResumeToken, ServerInfo, Shape, Transaction, TxOpts, Value,
    ValueKind,
};

use datagrep_lang::Language;

use crate::canceller::{BlockingClientId, RedisCanceller};
use crate::catalog::{RedisCatalog, NO_PREFIX_BUCKET};
use crate::cmd::{cmd_from_args, compile_glob, is_blocking_invocation, prefix_glob};
use crate::cursor::{ListCursor, OneShotCursor, RedisPairsCursor, ScanFamily, StreamCursor};
use crate::driver::redis_capabilities_baseline;
use crate::error::map_redis_error;
use crate::value::from_resp;

pub struct RedisConnection {
    manager: redis::aio::ConnectionManager,
    client: redis::Client,
    server_info: ServerInfo,
    key_enumeration: bool,
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

    async fn begin(&self, _opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        Err(DbError::Unsupported {
            feature: "interactive transactions — Redis MULTI/EXEC is a single optimistic \
                      pipeline, not an interactive transaction (no mid-transaction reads of \
                      your own writes, no savepoints)"
                .into(),
        })
    }

    async fn set_read_only(&self, _on: bool) -> Result<Enforcement, DbError> {
        self.ensure_open()?;
        Ok(Enforcement::Client)
    }

    async fn close(&self) -> Result<(), DbError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

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

fn find_option_value(args: &[String], option: &str) -> Option<String> {
    args.iter()
        .position(|a| a.eq_ignore_ascii_case(option))
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn mutation_key(path: &ObjectPath, key: &[(FieldPath, Value)]) -> Result<String, DbError> {
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
        [(_, Value::Str(s))] => Ok(s.to_string()),
        [(_, Value::Bytes(b))] => {
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
        Mutation::Delete { path, key, expect } => {
            refuse_expect(expect)?;
            let redis_key = mutation_key(path, key)?;
            pipe.cmd("DEL").arg(&redis_key);
        }
    }
    Ok(())
}

fn refuse_expect(expect: &[(datagrep_api::FieldPath, Value)]) -> Result<(), DbError> {
    if expect.is_empty() {
        return Ok(());
    }
    Err(DbError::Unsupported {
        feature: "conditional mutation (`expect`) — this driver cannot check-and-set".into(),
    })
}

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
            mutation_key(
                &path,
                &[(FieldPath::field("key"), Value::Str(Arc::from("override")))]
            )
            .unwrap(),
            "override"
        );
        assert!(mutation_key(
            &path,
            &[
                (FieldPath::field("a"), Value::I64(1)),
                (FieldPath::field("b"), Value::I64(2)),
            ]
        )
        .is_err());
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
