//! [`MongoConnection`] (ticket item 2): parses `Request::Native` MongoShell
//! text via `datagrep_lang::mongo` (never reimplemented here) and dispatches both
//! that and structured [`Op`]s to the official `mongodb` driver.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bson::{doc, Bson, Document as BsonDocument};
use mongodb::Collection;
use tokio::sync::Mutex;

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::driver::{
    Canceller, Connection, Cursor, Enforcement, ResumeToken, ServerInfo, Transaction, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::{DdlOp, Mutation, MutationBatch, Op, Predicate, Request, SortKey};
use datagrep_api::shape::ObjectPath;
use datagrep_api::value::{Document as DatagrepDocument, FieldPath, Value};

use datagrep_lang::mongo::error::MongoError;
use datagrep_lang::mongo::{parse, MongoStatement, ParsedMongo};

use crate::canceller::MongoCanceller;
use crate::catalog::MongoCatalog;
use crate::cursor::{decode_id_keyset, AckCursor, DocsCursor, MongoCursor, ResumeStrategy};
use crate::driver::mongo_capabilities;
use crate::error::map_mongo_error;
use crate::filter;
use crate::transaction::MongoTransaction;
use crate::value::{bson_to_value, value_to_bson, value_to_bson_for_field};

/// Every request gets a server-side deadline, even when the caller supplied
/// none (design §3.3: "always send maxTimeMS ... so even an uncancellable
/// query is bounded").
const DEFAULT_MAX_TIME: Duration = Duration::from_secs(30);

pub struct MongoConnection {
    client: mongodb::Client,
    default_database: String,
    server_info: ServerInfo,
    transactions_supported: bool,
    closed: AtomicBool,
    /// Client-side-only read-only gate (`set_read_only`); MongoDB has no
    /// per-session server-enforced read-only switch outside of routing reads
    /// to a secondary, which cannot honestly be called `Enforcement::Server`
    /// on a standalone deployment — see `set_read_only`'s doc comment.
    read_only: AtomicBool,
    tag_counter: AtomicU64,
    /// The `comment` tag of whichever `find`/`aggregate` is currently
    /// in-flight on this connection, if any — how [`MongoCanceller`]
    /// correlates a cancel request back to a real `currentOp` entry to
    /// `killOp` (ticket item 6; see `canceller.rs`'s module doc).
    active_comment: Arc<Mutex<Option<Arc<str>>>>,
    /// Cached result of the one-time `killOp`-privilege probe, shared with
    /// every [`MongoCanceller`] this connection hands out.
    killop_probe: Arc<tokio::sync::OnceCell<bool>>,
}

impl MongoConnection {
    pub fn new(
        client: mongodb::Client,
        default_database: String,
        server_info: ServerInfo,
        transactions_supported: bool,
    ) -> Self {
        Self {
            client,
            default_database,
            server_info,
            transactions_supported,
            closed: AtomicBool::new(false),
            read_only: AtomicBool::new(false),
            tag_counter: AtomicU64::new(0),
            active_comment: Arc::new(Mutex::new(None)),
            killop_probe: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    fn guard(&self) -> Result<(), DbError> {
        if self.closed.load(Ordering::Acquire) {
            Err(DbError::Closed)
        } else {
            Ok(())
        }
    }

    fn new_tag(&self) -> Arc<str> {
        let n = self.tag_counter.fetch_add(1, Ordering::Relaxed);
        Arc::from(format!("datagrep-{}-{}", std::process::id(), n))
    }

    /// Tag the connection's currently-in-flight `find`/`aggregate` so a
    /// concurrent [`MongoCanceller::cancel`] can find it in `currentOp`.
    async fn begin_op(&self) -> Arc<str> {
        let tag = self.new_tag();
        *self.active_comment.lock().await = Some(tag.clone());
        tag
    }

    async fn end_op(&self) {
        *self.active_comment.lock().await = None;
    }

    /// `[collection]` addresses a collection under this connection's default
    /// database; `[database, collection]` names one explicitly — Mongo,
    /// unlike Postgres, lets one client reach any database without
    /// reconnecting (ticket item 5's `catalog_levels: [database, collection,
    /// field]`).
    fn resolve_path(&self, path: &ObjectPath) -> Result<(String, String), DbError> {
        resolve_object_path(&self.default_database, path)
    }

    fn collection(&self, db: &str, coll: &str) -> Collection<BsonDocument> {
        self.client.database(db).collection::<BsonDocument>(coll)
    }

    fn read_only_gate(&self, is_write: bool) -> Result<(), DbError> {
        if is_write && self.read_only.load(Ordering::Acquire) {
            return Err(DbError::Unsupported {
                feature: "connection is in read-only mode (client-enforced — MongoDB has no per-session server-side read-only switch outside routing to a secondary)".into(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl Connection for MongoConnection {
    fn capabilities(&self) -> Capabilities {
        mongo_capabilities(self.transactions_supported)
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.guard()?;
        self.client
            .database(&self.default_database)
            .run_command(doc! { "ping": 1 })
            .await
            .map_err(map_mongo_error)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, req))]
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        self.guard()?;
        match req {
            Request::Native { text, params, opts } => {
                if !params.is_empty() {
                    return Err(DbError::Unsupported {
                        feature: "MongoDB native shell text takes no bind parameters (param_style: None) — literal values are already part of the parsed statement".into(),
                    });
                }
                self.execute_text(&text, opts.timeout.unwrap_or(DEFAULT_MAX_TIME))
                    .await
            }
            Request::Op(Op::Scan {
                path,
                filter,
                order,
                project,
                limit,
                resume,
            }) => {
                self.read_only_gate(false)?;
                self.execute_scan(
                    &path,
                    filter.as_ref(),
                    &order,
                    project.as_deref(),
                    limit,
                    resume.as_ref(),
                )
                .await
            }
            Request::Op(Op::Count {
                path,
                filter,
                exact,
            }) => {
                self.read_only_gate(false)?;
                self.execute_count(&path, filter.as_ref(), exact).await
            }
            Request::Op(Op::Mutate(batch)) => {
                self.read_only_gate(true)?;
                self.execute_mutate(&batch).await
            }
            Request::Op(Op::Explain { inner, analyze }) => {
                self.execute_explain(*inner, analyze).await
            }
            Request::Op(Op::Ddl(DdlOp::Native { text })) => {
                self.read_only_gate(true)?;
                self.execute_text(&text, DEFAULT_MAX_TIME).await
            }
        }
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(MongoCanceller::new(
            self.client.clone(),
            self.default_database.clone(),
            self.killop_probe.clone(),
            self.active_comment.clone(),
        ))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        Arc::new(MongoCatalog::new(
            self.client.clone(),
            self.default_database.clone(),
        ))
    }

    async fn begin(&self, opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        self.guard()?;
        if !self.transactions_supported {
            return Err(DbError::Unsupported {
                feature: "this MongoDB deployment does not support multi-document transactions (needs a replica set on 4.0+, or a sharded cluster on 4.2+)".into(),
            });
        }
        // `TxOpts::isolation`/`read_only` have no direct MongoDB transaction
        // equivalent (Mongo transactions are snapshot/majority-based, not
        // configurable per the SQL isolation ladder) — accepted but not
        // mapped, a documented gap rather than a silent downgrade of a flag
        // that was never honorable here.
        let _ = opts;
        let mut session = self.client.start_session().await.map_err(map_mongo_error)?;
        session.start_transaction().await.map_err(map_mongo_error)?;
        Ok(Box::new(MongoTransaction::new(
            self.client.clone(),
            self.default_database.clone(),
            session,
        )))
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        self.guard()?;
        self.read_only.store(on, Ordering::Release);
        Ok(Enforcement::Client)
    }

    async fn close(&self) -> Result<(), DbError> {
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Native MongoShell text dispatch
// ---------------------------------------------------------------------

impl MongoConnection {
    async fn execute_text(
        &self,
        text: &str,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        match parse(text).map_err(map_parse_err)? {
            ParsedMongo::Chain(stmt) => self.dispatch_method(stmt, timeout).await,
            ParsedMongo::RawCommand(Value::Document(doc)) => {
                self.execute_raw_command((*doc).clone(), timeout).await
            }
            // The parser's `ParsedMongo::RawCommand` is documented to always
            // carry a `Value::Document` (its own `parse_object` is the only
            // producer of the `{...}` branch in `parse()`).
            ParsedMongo::RawCommand(_) => Err(DbError::Protocol(
                "parser returned a non-document raw command".into(),
            )),
        }
    }

    async fn dispatch_method(
        &self,
        stmt: MongoStatement,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let method = stmt.method.to_ascii_lowercase();
        self.read_only_gate(is_write_method(&method))?;
        let coll = self.collection(&self.default_database, &stmt.collection);
        match method.as_str() {
            "find" => self.run_find(coll, &stmt, timeout, false).await,
            "findone" => self.run_find(coll, &stmt, timeout, true).await,
            "aggregate" => self.run_aggregate(coll, &stmt, timeout).await,
            "count" | "countdocuments" => self.run_count_shell(coll, &stmt, timeout).await,
            "estimateddocumentcount" => self.run_estimated_shell(coll, timeout).await,
            "distinct" => self.run_distinct_shell(coll, &stmt, timeout).await,
            "insertone" => self.run_insert_one(coll, &stmt).await,
            "insertmany" => self.run_insert_many(coll, &stmt).await,
            "updateone" => self.run_update(coll, &stmt, false).await,
            "updatemany" => self.run_update(coll, &stmt, true).await,
            "deleteone" => self.run_delete(coll, &stmt, false).await,
            "deletemany" => self.run_delete(coll, &stmt, true).await,
            "drop" => self.run_drop(coll).await,
            other => Err(DbError::Unsupported {
                feature: format!(
                    "db.<collection>.{other}(...) is not a supported MongoShell method"
                ),
            }),
        }
    }

    async fn run_find(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
        timeout: Duration,
        one: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let mut limit: Option<i64> = if one { Some(1) } else { None };
        let mut skip: Option<u64> = None;
        let mut sort: Option<BsonDocument> = None;
        let mut projection: Option<BsonDocument> = None;
        let mut batch_size: Option<u32> = None;
        let mut max_time = timeout;
        for (name, margs) in &stmt.modifiers {
            match name.to_ascii_lowercase().as_str() {
                "limit" => limit = Some(int_arg(first_arg(margs, "limit")?)?),
                "skip" => skip = Some(int_arg(first_arg(margs, "skip")?)?.max(0) as u64),
                "sort" => sort = Some(arg_doc(margs, 0)?),
                "project" | "projection" => projection = Some(arg_doc(margs, 0)?),
                "batchsize" => {
                    batch_size = Some(int_arg(first_arg(margs, "batchSize")?)?.max(0) as u32)
                }
                "maxtimems" => {
                    max_time = Duration::from_millis(
                        int_arg(first_arg(margs, "maxTimeMS")?)?.max(0) as u64,
                    )
                }
                _ => {} // e.g. .hint()/.collation() — out of v1 scope, ignored rather than rejected
            }
        }
        let tag = self.begin_op().await;
        let mut builder = coll
            .find(filter_doc)
            .max_time(max_time)
            .comment(Bson::String(tag.to_string()));
        if let Some(l) = limit {
            builder = builder.limit(l);
        }
        if let Some(s) = skip {
            builder = builder.skip(s);
        }
        if let Some(s) = sort {
            builder = builder.sort(s);
        }
        if let Some(p) = projection {
            builder = builder.projection(p);
        }
        if let Some(b) = batch_size {
            builder = builder.batch_size(b);
        }
        let result = builder.await;
        self.end_op().await;
        let cursor = result.map_err(map_mongo_error)?;
        Ok(Box::new(MongoCursor::plain(
            cursor,
            ResumeStrategy::IdKeyset,
        )))
    }

    async fn run_aggregate(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let pipeline = pipeline_arg(&stmt.args)?;
        let mut batch_size = None;
        let mut max_time = timeout;
        for (name, margs) in &stmt.modifiers {
            match name.to_ascii_lowercase().as_str() {
                "batchsize" => {
                    batch_size = Some(int_arg(first_arg(margs, "batchSize")?)?.max(0) as u32)
                }
                "maxtimems" => {
                    max_time = Duration::from_millis(
                        int_arg(first_arg(margs, "maxTimeMS")?)?.max(0) as u64,
                    )
                }
                _ => {}
            }
        }
        let tag = self.begin_op().await;
        let mut builder = coll
            .aggregate(pipeline)
            .max_time(max_time)
            .comment(Bson::String(tag.to_string()));
        if let Some(b) = batch_size {
            builder = builder.batch_size(b);
        }
        let result = builder.await;
        self.end_op().await;
        let cursor = result.map_err(map_mongo_error)?;
        // Design ticket item 3: aggregate has no stable, re-issuable cursor
        // key (pipeline stages like $group/$sort break any positional
        // notion of "the next document after this one"), so resume is
        // always `None`, with the reason documented right here.
        Ok(Box::new(MongoCursor::plain(cursor, ResumeStrategy::None)))
    }

    async fn run_count_shell(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let n = coll
            .count_documents(filter_doc)
            .max_time(timeout)
            .await
            .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(n),
            Some(Arc::from("count_documents (exact)")),
        )))
    }

    async fn run_estimated_shell(
        &self,
        coll: Collection<BsonDocument>,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let n = coll
            .estimated_document_count()
            .max_time(timeout)
            .await
            .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(n),
            Some(Arc::from(
                "estimated_document_count (approximate — EXACT_COUNT_CHEAP is false)",
            )),
        )))
    }

    async fn run_distinct_shell(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let field = match stmt.args.first() {
            Some(Value::Str(s)) => s.to_string(),
            _ => {
                return Err(DbError::Query {
                    code: None,
                    message: "distinct() requires a field name string as its first argument".into(),
                    position: None,
                })
            }
        };
        let filter_doc = arg_doc(&stmt.args, 1)?;
        let values = coll
            .distinct(&field, filter_doc)
            .max_time(timeout)
            .await
            .map_err(map_mongo_error)?;
        let docs = values.iter().map(bson_to_value).collect();
        Ok(Box::new(DocsCursor::new(docs)))
    }

    async fn run_insert_one(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let doc = arg_doc(&stmt.args, 0)?;
        let result = coll.insert_one(doc).await.map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(1),
            Some(Arc::from(format!(
                "inserted _id: {}",
                display_bson(&result.inserted_id)
            ))),
        )))
    }

    async fn run_insert_many(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let docs = match stmt.args.first() {
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items.iter() {
                    out.push(as_bson_doc(v)?);
                }
                out
            }
            _ => {
                return Err(DbError::Query {
                    code: None,
                    message: "insertMany() requires an array of documents".into(),
                    position: None,
                })
            }
        };
        let n = docs.len() as u64;
        coll.insert_many(docs).await.map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(n),
            Some(Arc::from("insert_many")),
        )))
    }

    async fn run_update(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
        many: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let update_doc = arg_doc(&stmt.args, 1)?;
        let result = if many {
            coll.update_many(filter_doc, update_doc).await
        } else {
            coll.update_one(filter_doc, update_doc).await
        }
        .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(result.modified_count),
            Some(Arc::from(format!(
                "matched {}, modified {}",
                result.matched_count, result.modified_count
            ))),
        )))
    }

    async fn run_delete(
        &self,
        coll: Collection<BsonDocument>,
        stmt: &MongoStatement,
        many: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let result = if many {
            coll.delete_many(filter_doc).await
        } else {
            coll.delete_one(filter_doc).await
        }
        .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(result.deleted_count),
            Some(Arc::from("delete")),
        )))
    }

    async fn run_drop(&self, coll: Collection<BsonDocument>) -> Result<Box<dyn Cursor>, DbError> {
        coll.drop().await.map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            None,
            Some(Arc::from("collection dropped")),
        )))
    }

    async fn execute_raw_command(
        &self,
        doc_val: DatagrepDocument,
        timeout: Duration,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let mut bson_doc = as_bson_doc(&Value::Document(Arc::new(doc_val)))?;
        if !bson_doc.contains_key("maxTimeMS") {
            bson_doc.insert("maxTimeMS", timeout.as_millis() as i64);
        }
        let first_key = bson_doc.keys().next().map(|s| s.to_ascii_lowercase());
        let is_write = matches!(
            first_key.as_deref(),
            Some("insert")
                | Some("update")
                | Some("delete")
                | Some("findandmodify")
                | Some("drop")
                | Some("dropdatabase")
                | Some("create")
        );
        self.read_only_gate(is_write)?;
        let db = self.client.database(&self.default_database);
        match first_key.as_deref() {
            Some("find") | Some("aggregate") | Some("listcollections") | Some("listindexes") => {
                let cursor = db
                    .run_cursor_command(bson_doc)
                    .await
                    .map_err(map_mongo_error)?;
                Ok(Box::new(MongoCursor::plain(cursor, ResumeStrategy::None)))
            }
            _ => {
                let result = db.run_command(bson_doc).await.map_err(map_mongo_error)?;
                Ok(Box::new(DocsCursor::new(vec![bson_to_value(
                    &Bson::Document(result),
                )])))
            }
        }
    }
}

// ---------------------------------------------------------------------
// Structured `Op` dispatch
// ---------------------------------------------------------------------

impl MongoConnection {
    async fn execute_scan(
        &self,
        path: &ObjectPath,
        filter: Option<&Predicate>,
        order: &[SortKey],
        project: Option<&[FieldPath]>,
        limit: Option<u64>,
        resume: Option<&ResumeToken>,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let (db, coll_name) = self.resolve_path(path)?;
        let coll = self.collection(&db, &coll_name);

        let mut compiled = filter.map(filter::compile_predicate).transpose()?;
        if let Some(token) = resume {
            let last_id = decode_id_keyset(token)?;
            compiled = Some(filter::and_keyset(compiled, last_id));
        }
        let filter_doc = compiled.unwrap_or_default();

        let sort_doc = if order.is_empty() {
            None
        } else {
            let mut d = BsonDocument::new();
            for key in order {
                d.insert(
                    filter::field_path_to_mongo(&key.path),
                    if key.desc { -1_i32 } else { 1_i32 },
                );
            }
            Some(d)
        };
        let projection_doc = project.map(|fields| {
            let mut d = BsonDocument::new();
            for f in fields {
                d.insert(filter::field_path_to_mongo(f), 1_i32);
            }
            d
        });

        let tag = self.begin_op().await;
        let mut builder = coll
            .find(filter_doc)
            .max_time(DEFAULT_MAX_TIME)
            .comment(Bson::String(tag.to_string()));
        if let Some(s) = sort_doc {
            builder = builder.sort(s);
        }
        if let Some(p) = projection_doc {
            builder = builder.projection(p);
        }
        if let Some(l) = limit {
            builder = builder.limit(l as i64);
        }
        let result = builder.await;
        self.end_op().await;
        let cursor = result.map_err(map_mongo_error)?;
        Ok(Box::new(MongoCursor::plain(
            cursor,
            ResumeStrategy::IdKeyset,
        )))
    }

    async fn execute_count(
        &self,
        path: &ObjectPath,
        filter: Option<&Predicate>,
        exact: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let (db, coll_name) = self.resolve_path(path)?;
        let coll = self.collection(&db, &coll_name);

        if !exact && filter.is_none() {
            let n = coll
                .estimated_document_count()
                .max_time(DEFAULT_MAX_TIME)
                .await
                .map_err(map_mongo_error)?;
            return Ok(Box::new(AckCursor::new(
                Some(n),
                Some(Arc::from(
                    "estimated_document_count (approximate — EXACT_COUNT_CHEAP is false)",
                )),
            )));
        }
        let filter_doc = filter
            .map(filter::compile_predicate)
            .transpose()?
            .unwrap_or_default();
        let n = coll
            .count_documents(filter_doc)
            .max_time(DEFAULT_MAX_TIME)
            .await
            .map_err(map_mongo_error)?;
        let message = if exact {
            "count_documents (exact)"
        } else {
            "count_documents (exact — estimatedDocumentCount does not support a filter)"
        };
        Ok(Box::new(AckCursor::new(Some(n), Some(Arc::from(message)))))
    }

    async fn execute_mutate(&self, batch: &MutationBatch) -> Result<Box<dyn Cursor>, DbError> {
        if batch.mutations.is_empty() {
            return Ok(Box::new(AckCursor::new(Some(0), None)));
        }
        let mut total = 0u64;
        for m in &batch.mutations {
            total += self.execute_one_mutation(m).await?;
        }
        Ok(Box::new(AckCursor::new(Some(total), None)))
    }

    async fn execute_one_mutation(&self, m: &Mutation) -> Result<u64, DbError> {
        match m {
            Mutation::Insert { path, doc } => {
                let (db, coll_name) = self.resolve_path(path)?;
                let coll = self.collection(&db, &coll_name);
                let bson_doc = as_bson_doc(doc)?;
                coll.insert_one(bson_doc).await.map_err(map_mongo_error)?;
                Ok(1)
            }
            Mutation::Update { path, key, sets } => {
                let (db, coll_name) = self.resolve_path(path)?;
                let coll = self.collection(&db, &coll_name);
                let id_filter = self.id_filter(key)?;
                let mut set_doc = BsonDocument::new();
                for (field, value) in sets {
                    let f = filter::field_path_to_mongo(field);
                    let bson = value_to_bson_for_field(&f, value)?;
                    set_doc.insert(f, bson);
                }
                let update = doc! { "$set": set_doc };
                let result = coll
                    .update_one(id_filter, update)
                    .await
                    .map_err(map_mongo_error)?;
                // Design §3.8: every generated mutation must affect exactly
                // one document or it is rejected with "row identity changed
                // — refresh".
                if result.matched_count != 1 {
                    return Err(DbError::Query {
                        code: None,
                        message: format!(
                            "row identity changed — refresh (expected exactly 1 document matched, got {})",
                            result.matched_count
                        ),
                        position: None,
                    });
                }
                Ok(1)
            }
            Mutation::Delete { path, key } => {
                let (db, coll_name) = self.resolve_path(path)?;
                let coll = self.collection(&db, &coll_name);
                let id_filter = self.id_filter(key)?;
                let result = coll.delete_one(id_filter).await.map_err(map_mongo_error)?;
                if result.deleted_count != 1 {
                    return Err(DbError::Query {
                        code: None,
                        message: format!(
                            "row identity changed — refresh (expected exactly 1 document deleted, got {})",
                            result.deleted_count
                        ),
                        position: None,
                    });
                }
                Ok(1)
            }
        }
    }

    /// `Mutation::Update`/`Delete::key` carries the row identity as named
    /// `(FieldPath, Value)` pairs, so the filter compiles directly from the
    /// mutation — typically `{_id: …}`, but any caller-named field(s) work.
    /// The old "assume a single bare value means `_id`" guess is gone. An
    /// empty identity is refused, never guessed at (design §3.8).
    fn id_filter(&self, key: &[(FieldPath, Value)]) -> Result<BsonDocument, DbError> {
        id_filter_from_key(key)
    }

    async fn execute_explain(
        &self,
        inner: Request,
        analyze: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let command = self.build_explainable_command(inner).await?;
        let verbosity = if analyze {
            "executionStats"
        } else {
            "queryPlanner"
        };
        let explain_cmd = doc! {
            "explain": command,
            "verbosity": verbosity,
            "maxTimeMS": DEFAULT_MAX_TIME.as_millis() as i64,
        };
        let db = self.client.database(&self.default_database);
        let result = db.run_command(explain_cmd).await.map_err(map_mongo_error)?;
        Ok(Box::new(DocsCursor::new(vec![bson_to_value(
            &Bson::Document(result),
        )])))
    }

    /// Build the raw command-document form of a request for `explain`
    /// (v1 scope: `find`/`aggregate` shell chains, raw command documents,
    /// and `Op::Scan` — see the crate report's deviations for what's out).
    async fn build_explainable_command(&self, req: Request) -> Result<BsonDocument, DbError> {
        match req {
            Request::Native { text, params, .. } => {
                if !params.is_empty() {
                    return Err(DbError::Unsupported {
                        feature: "MongoDB native shell text takes no bind parameters".into(),
                    });
                }
                match parse(&text).map_err(map_parse_err)? {
                    ParsedMongo::Chain(stmt) => chain_to_command_doc(&stmt),
                    ParsedMongo::RawCommand(Value::Document(d)) => as_bson_doc(&Value::Document(d)),
                    ParsedMongo::RawCommand(_) => Err(DbError::Protocol(
                        "parser returned a non-document raw command".into(),
                    )),
                }
            }
            Request::Op(Op::Scan {
                path,
                filter,
                order,
                project,
                limit,
                ..
            }) => {
                let (_, coll_name) = self.resolve_path(&path)?;
                let mut cmd = doc! { "find": coll_name.as_str() };
                if let Some(p) = filter {
                    cmd.insert("filter", filter::compile_predicate(&p)?);
                }
                if !order.is_empty() {
                    let mut d = BsonDocument::new();
                    for key in &order {
                        d.insert(
                            filter::field_path_to_mongo(&key.path),
                            if key.desc { -1_i32 } else { 1_i32 },
                        );
                    }
                    cmd.insert("sort", d);
                }
                if let Some(fields) = project {
                    let mut d = BsonDocument::new();
                    for f in &fields {
                        d.insert(filter::field_path_to_mongo(f), 1_i32);
                    }
                    cmd.insert("projection", d);
                }
                if let Some(l) = limit {
                    cmd.insert("limit", l as i64);
                }
                Ok(cmd)
            }
            other => Err(DbError::Unsupported {
                feature: format!("EXPLAIN is not implemented for this request shape ({other:?})"),
            }),
        }
    }
}

fn chain_to_command_doc(stmt: &MongoStatement) -> Result<BsonDocument, DbError> {
    match stmt.method.to_ascii_lowercase().as_str() {
        "find" => {
            let mut cmd =
                doc! { "find": stmt.collection.as_str(), "filter": arg_doc(&stmt.args, 0)? };
            for (name, margs) in &stmt.modifiers {
                match name.to_ascii_lowercase().as_str() {
                    "limit" => {
                        cmd.insert("limit", int_arg(first_arg(margs, "limit")?)?);
                    }
                    "skip" => {
                        cmd.insert("skip", int_arg(first_arg(margs, "skip")?)?);
                    }
                    "sort" => {
                        cmd.insert("sort", arg_doc(margs, 0)?);
                    }
                    "project" | "projection" => {
                        cmd.insert("projection", arg_doc(margs, 0)?);
                    }
                    _ => {}
                }
            }
            Ok(cmd)
        }
        "aggregate" => Ok(doc! {
            "aggregate": stmt.collection.as_str(),
            "pipeline": pipeline_arg(&stmt.args)?,
            "cursor": {},
        }),
        other => Err(DbError::Unsupported {
            feature: format!("EXPLAIN does not support db.<collection>.{other}(...)"),
        }),
    }
}

/// `[collection]` addresses a collection under `default_database`;
/// `[database, collection]` names one explicitly (shared by
/// [`MongoConnection`] and [`crate::transaction::MongoTransaction`]).
pub(crate) fn resolve_object_path(
    default_database: &str,
    path: &ObjectPath,
) -> Result<(String, String), DbError> {
    match path.parts() {
        [coll] => Ok((default_database.to_string(), coll.to_string())),
        [db, coll] => Ok((db.to_string(), coll.to_string())),
        _ => Err(DbError::Unsupported {
            feature: format!("object path {path} does not name a collection (expected [collection] or [database, collection])"),
        }),
    }
}

pub(crate) fn is_write_method(method_lower: &str) -> bool {
    method_lower.starts_with("insert")
        || method_lower.starts_with("update")
        || method_lower.starts_with("delete")
        || method_lower.starts_with("replace")
        || method_lower.starts_with("drop")
        || method_lower.starts_with("create")
        || method_lower.starts_with("findoneandupdate")
        || method_lower.starts_with("findoneanddelete")
        || method_lower.starts_with("findoneandreplace")
}

/// Compile a mutation's named row identity (`key: Vec<(FieldPath, Value)>`)
/// into a filter document — one entry per named field, typically just
/// `{_id: …}`. Shared by [`MongoConnection`] and `transaction.rs`. An empty
/// identity is refused: we never guess which document to affect (§3.8).
pub(crate) fn id_filter_from_key(key: &[(FieldPath, Value)]) -> Result<BsonDocument, DbError> {
    if key.is_empty() {
        return Err(DbError::Unsupported {
            feature: "mutation with no row identity — refuse to guess which document to affect"
                .into(),
        });
    }
    let mut filter = BsonDocument::new();
    for (field, value) in key {
        let f = crate::filter::field_path_to_mongo(field);
        let bson = crate::value::value_to_bson_for_field(&f, value)?;
        filter.insert(f, bson);
    }
    Ok(filter)
}

pub(crate) fn as_bson_doc(v: &Value) -> Result<BsonDocument, DbError> {
    match value_to_bson(v)? {
        Bson::Document(d) => Ok(d),
        _ => Err(DbError::Query {
            code: None,
            message: "expected a document value".into(),
            position: None,
        }),
    }
}

pub(crate) fn arg_doc(args: &[Value], idx: usize) -> Result<BsonDocument, DbError> {
    match args.get(idx) {
        None => Ok(BsonDocument::new()),
        Some(v @ Value::Document(_)) => as_bson_doc(v),
        Some(other) => Err(DbError::Query {
            code: None,
            message: format!("expected a document argument, got {other:?}"),
            position: None,
        }),
    }
}

pub(crate) fn pipeline_arg(args: &[Value]) -> Result<Vec<BsonDocument>, DbError> {
    match args.first() {
        Some(Value::Array(items)) => items.iter().map(as_bson_doc).collect(),
        _ => Err(DbError::Query {
            code: None,
            message: "aggregate() requires a pipeline array argument".into(),
            position: None,
        }),
    }
}

pub(crate) fn first_arg<'a>(args: &'a [Value], modifier: &str) -> Result<&'a Value, DbError> {
    args.first().ok_or_else(|| DbError::Query {
        code: None,
        message: format!("{modifier}() requires one argument"),
        position: None,
    })
}

pub(crate) fn int_arg(v: &Value) -> Result<i64, DbError> {
    match v {
        Value::I64(n) => Ok(*n),
        Value::F64(f) if f.fract() == 0.0 => Ok(*f as i64),
        other => Err(DbError::Query {
            code: None,
            message: format!("expected an integer argument, got {other:?}"),
            position: None,
        }),
    }
}

fn display_bson(b: &Bson) -> String {
    match bson_to_value(b) {
        Value::Str(s) => s.to_string(),
        Value::I64(n) => n.to_string(),
        Value::Unsupported { display, .. } => display.to_string(),
        other => format!("{other:?}"),
    }
}

pub(crate) fn map_parse_err(e: MongoError) -> DbError {
    let position = match &e {
        MongoError::UnsupportedJs => None,
        MongoError::UnexpectedEof { at, .. }
        | MongoError::Unexpected { at, .. }
        | MongoError::InvalidLiteral { at, .. }
        | MongoError::TrailingInput { at, .. } => Some(*at as u32),
    };
    DbError::Query {
        code: None,
        message: e.to_string(),
        position,
    }
}
