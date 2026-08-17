//! [`EsConnection`] — the `Connection` impl. Compiles a [`Request`] into an
//! Elasticsearch REST call and hands back a streaming cursor, without ever
//! buffering a result.
//!
//! # What each `Request` becomes
//!
//! | Request | Elasticsearch |
//! |---|---|
//! | `Native` (console text / bare body) | the request as written; searches stream, everything else is one reply document |
//! | `Op::Scan` | `_pit` + `_search` with `search_after` (or `_scroll`) |
//! | `Op::Count { exact: true }` | `_count` — a real count, and it says so |
//! | `Op::Count { exact: false }` | `_search?size=0`, whose `hits.total` is a **lower bound** |
//! | `Op::Explain { analyze: false }` | `_validate/query?explain&rewrite` — the plan without running it |
//! | `Op::Explain { analyze: true }` | `_search` with `"profile": true` — real per-shard timings |
//! | `Op::Mutate` | guarded, serial, halt-and-report single-document `_update`/`_doc` writes (`EDITABLE_RESULTS` is on; `ATOMIC_BATCH` is off — no transaction) |
//! | `Op::Ddl` | refused: `DDL` is off (index/mapping management is a native `PUT /<index>` request, not SQL DDL) |
//!
//! # Read-only enforcement is `Client`, and says so
//!
//! Elasticsearch has no per-session read-only mode. `set_read_only(true)`
//! therefore returns [`Enforcement::Client`] and installs a classifier that
//! refuses any request that is not a read: `GET`/`HEAD` always pass, `POST`
//! passes only for the read endpoints (`_search`, `_count`, `_explain`,
//! `_validate/query`, `_field_caps`, …), everything else is refused before it
//! leaves the process. A cluster-level `index.blocks.read_only` would be a
//! genuinely server-side control, but it applies to *every* client on the
//! cluster, so setting it from a data browser would be indefensible.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value as Json};
use tokio::sync::Mutex;

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::driver::{
    Canceller, Connection, Cursor, Enforcement, Notice, NoticeSeverity, ServerInfo, Transaction,
    TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::{ExecOpts, MutationBatch, Op, Request};
use datagrep_api::shape::ObjectPath;
use datagrep_api::value::Value;

use crate::canceller::{EsCanceller, InFlight, InFlightSlot};
use crate::catalog::{encode_index_expression, EsCatalog};
use crate::console::{self, ConsoleRequest};
use crate::cursor::{AckCursor, DocsCursor, ScanSpec, SearchCursor, DEFAULT_KEEP_ALIVE};
use crate::filter::{compile_predicate, compile_sort};
use crate::http::{EsHttp, Method, PageMode, Product};
use crate::mutate::{
    batch_report, compile_mutation, guard_unsupported_reason, supports_include_source_on_error,
    CompiledWrite, WriteOutcome,
};
use crate::value::{serde_to_value, FieldTypes};

/// POST endpoints that only read. Used by the read-only guardrail; a path is a
/// read if any of these appears as a `/`-delimited segment.
///
/// `_mapping` is deliberately absent: `POST /<index>/_mapping` *writes* the
/// mapping. Reading a mapping is `GET`, which passes as a read on the method
/// alone.
///
/// This table MUST stay in sync with `datagrep-lang`'s `esdsl::READ_ACTIONS`
/// (the editor-side classifier) until the two are unified into one table in
/// `datagrep-lang`, per the plan's recommendation
/// (notes/elasticsearch-plan.md §6, "classify").
const READ_ONLY_POST_ENDPOINTS: &[&str] = &[
    "_search",
    "_msearch",
    "_count",
    "_explain",
    "_validate",
    "_field_caps",
    "_analyze",
    "_pit",
    "_async_search",
    "_terms_enum",
    "_rank_eval",
    "_search_shards",
    "_resolve",
    "_mget",
    "_termvectors",
    "_mtermvectors",
];

pub struct EsConnection {
    http: Arc<EsHttp>,
    server_info: ServerInfo,
    caps: Capabilities,
    page_mode: PageMode,
    async_search: bool,
    default_index: Option<Arc<str>>,
    inflight: InFlightSlot,
    catalog: Arc<EsCatalog>,
    mapping_cache: Arc<Mutex<HashMap<String, Arc<FieldTypes>>>>,
    read_only: AtomicBool,
    closed: AtomicBool,
    /// Monotonic per-connection counter so every request's `X-Opaque-Id` is
    /// unique even across concurrent cursors on one connection.
    request_seq: AtomicU64,
    opaque_root: Arc<str>,
}

impl EsConnection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: Arc<EsHttp>,
        server_info: ServerInfo,
        caps: Capabilities,
        page_mode: PageMode,
        async_search: bool,
        default_index: Option<Arc<str>>,
        application_name: Option<Arc<str>>,
    ) -> Self {
        let mapping_cache = Arc::new(Mutex::new(HashMap::new()));
        let catalog = Arc::new(EsCatalog::new(http.clone(), mapping_cache.clone()));
        // The opaque id is what the tasks API matches on, so it identifies
        // *this application and connection* — never anything user-supplied.
        let app = application_name.as_deref().unwrap_or("datagrep");
        let opaque_root: Arc<str> = Arc::from(
            format!(
                "{app}-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )
            .as_str(),
        );
        Self {
            http,
            server_info,
            caps,
            page_mode,
            async_search,
            default_index,
            inflight: Arc::new(Mutex::new(InFlight::default())),
            catalog,
            mapping_cache,
            read_only: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            request_seq: AtomicU64::new(0),
            opaque_root,
        }
    }

    fn check_open(&self) -> Result<(), DbError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(DbError::Closed);
        }
        Ok(())
    }

    fn next_opaque_prefix(&self) -> Arc<str> {
        let n = self.request_seq.fetch_add(1, Ordering::Relaxed);
        Arc::from(format!("{}-{n}", self.opaque_root).as_str())
    }

    /// The index expression a request targets, honouring the connection's
    /// default and falling back to a stated cluster-wide `_all`.
    fn resolve_index(&self, explicit: Option<&str>) -> String {
        explicit
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.default_index.as_deref().map(str::to_string))
            .unwrap_or_else(|| "_all".to_string())
    }

    fn read_only_active(&self, opts: &ExecOpts) -> bool {
        opts.read_only_assert || self.read_only.load(Ordering::Acquire)
    }

    /// `POST /<index>/_pit` — the one place a point-in-time is opened.
    async fn open_pit(
        &self,
        index: &str,
        timeout: Option<std::time::Duration>,
    ) -> Result<String, DbError> {
        let response = self
            .http
            .request(
                Method::Post,
                &format!("/{}/_pit", encode_index_expression(index)?),
                &[("keep_alive", DEFAULT_KEEP_ALIVE.to_string())],
                None,
                None,
                timeout,
            )
            .await?;
        response
            .get("id")
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(|| DbError::Protocol("_pit returned no point-in-time id".to_string()))
    }

    /// Open the server-side scan context and build the streaming cursor. The
    /// single place a `SearchCursor` is created, so PIT opening and mapping
    /// loading are never duplicated.
    async fn open_scan(
        &self,
        spec: ScanSpec,
        resume: Option<&datagrep_api::driver::ResumeToken>,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let ScanSpec { index, timeout, .. } = &spec;
        let (index, timeout) = (index.clone(), *timeout);
        // The mapping drives the precision rules and the native type names on
        // schema deltas. It is best-effort: a caller with read access to the
        // documents but not to the mapping still gets a correct, if less
        // precise, stream.
        let types = self
            .catalog
            .mapping(&index)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, index, "mapping unavailable; falling back to structural typing");
                Arc::new(FieldTypes::new())
            });

        if let Some(token) = resume {
            let resume = crate::resume::EsResume::decode(token)?;
            // A resumed `Pit` scan needs a *new* point-in-time: closing the
            // original cursor released the old one, which is the whole point
            // of a resume token. The position lives in the `search_after`
            // values, not in the PIT — see `SearchCursor::from_resume`.
            let fresh_pit = match resume.mode {
                crate::resume::ResumeMode::Pit => {
                    Some(self.open_pit(&resume.index, timeout).await?)
                }
                crate::resume::ResumeMode::Scroll => None,
            };
            return Ok(Box::new(SearchCursor::from_resume(
                self.http.clone(),
                resume,
                fresh_pit,
                spec.user_sort.clone(),
                types,
                self.inflight.clone(),
                self.async_search,
                self.next_opaque_prefix(),
                timeout,
            )));
        }

        let pit_id = match self.page_mode {
            PageMode::Pit => Some(self.open_pit(&index, timeout).await?),
            PageMode::Scroll => None,
        };

        Ok(Box::new(SearchCursor::new(
            self.http.clone(),
            spec,
            self.page_mode,
            DEFAULT_KEEP_ALIVE.to_string(),
            pit_id,
            types,
            self.inflight.clone(),
            self.async_search,
            self.next_opaque_prefix(),
        )))
    }

    async fn execute_native(
        &self,
        text: &str,
        params: &[Value],
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let req = console::parse(text, self.default_index.as_deref(), params)?;
        if self.read_only_active(opts) {
            refuse_if_write(&req)?;
        }

        if req.is_search() {
            let index = self.resolve_index(req.search_index());
            let mut body = req.body.clone().unwrap_or_else(|| json!({}));
            if !body.is_object() {
                return Err(DbError::Query {
                    code: None,
                    message: "a search body must be a JSON object".to_string(),
                    position: None,
                });
            }
            // `sort` in the user's own body is honoured; the cursor appends
            // the stable tiebreaker to it.
            let user_sort = body
                .as_object_mut()
                .and_then(|m| m.remove("sort"))
                .map(|s| match s {
                    Json::Array(items) => items,
                    other => vec![other],
                })
                .unwrap_or_default();
            return self
                .open_scan(
                    ScanSpec {
                        index,
                        body,
                        user_sort,
                        limit: opts.row_limit,
                        notices: Vec::new(),
                        timeout: opts.timeout,
                    },
                    None,
                )
                .await;
        }

        // Anything else — cluster health, a mapping read, `_cat`, an aggregation
        // via `_search?size=0`, a scroll continuation — is one round trip whose
        // reply is shown as a single document.
        let (response, _) = self
            .http
            .request_ordered(
                req.method,
                &req.path,
                &req.query
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone()))
                    .collect::<Vec<_>>(),
                req.body.as_ref(),
                None,
                opts.timeout,
            )
            .await?;
        Ok(Box::new(DocsCursor::new(vec![
            crate::value::json_to_value(&response, "", &FieldTypes::new()),
        ])))
    }

    /// Takes the whole `Op::Scan` rather than its seven fields: the compiler
    /// then keeps this in step with the `Op` definition, and there is one
    /// place that knows how a scan is spelled.
    async fn execute_scan(&self, op: &Op, opts: &ExecOpts) -> Result<Box<dyn Cursor>, DbError> {
        let Op::Scan {
            path,
            filter,
            order,
            project,
            limit,
            resume,
        } = op
        else {
            return Err(DbError::Unsupported {
                feature: format!("{} routed to the scan path", op_name(op)),
            });
        };
        let index = self.resolve_index(path.parts().first().map(|p| &**p));
        let mut notices = Vec::new();
        let mut body = serde_json::Map::new();

        if let Some(filter) = filter.as_ref() {
            let compiled = compile_predicate(filter)?;
            notices.extend(compiled.notices);
            body.insert("query".into(), compiled.query);
        }
        if let Some(fields) = project.as_ref() {
            let includes: Vec<Json> = fields
                .iter()
                .map(|f| Json::String(crate::filter::field_path_to_es(f).0))
                .collect();
            body.insert("_source".into(), json!({ "includes": includes }));
        }
        let user_sort = compile_sort(order)?;

        self.open_scan(
            ScanSpec {
                index,
                body: Json::Object(body),
                user_sort,
                limit: limit.or(opts.row_limit),
                notices,
                timeout: opts.timeout,
            },
            resume.as_ref(),
        )
        .await
    }

    async fn execute_count(
        &self,
        path: &ObjectPath,
        filter: Option<&datagrep_api::request::Predicate>,
        exact: bool,
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let index = self.resolve_index(path.parts().first().map(|p| &**p));
        let mut notices = Vec::new();
        let query = match filter {
            Some(f) => {
                let compiled = compile_predicate(f)?;
                notices.extend(compiled.notices);
                Some(compiled.query)
            }
            None => None,
        };

        if exact {
            // `_count` really does count every match. `EXACT_COUNT_CHEAP` is
            // false precisely because this is a second, potentially expensive
            // request rather than something a search hands back for free.
            let body = query.map(|q| json!({ "query": q }));
            let response = self
                .http
                .request(
                    Method::Post,
                    &format!("/{}/_count", encode_index_expression(&index)?),
                    &[],
                    body.as_ref(),
                    None,
                    opts.timeout,
                )
                .await?;
            let count = response.get("count").and_then(Json::as_u64);
            return Ok(Box::new(
                AckCursor::new(count, Some(Arc::from("_count (exact)"))).with_notices(notices),
            ));
        }

        // The cheap estimate: a zero-size search whose `hits.total` stops
        // counting at `track_total_hits` (10 000 by default). The UI shows
        // "≥ N" — `track_total_hits` is deliberately NOT set here, because
        // asking for it turns the cheap estimate into a full count.
        let mut body = serde_json::Map::new();
        body.insert("size".into(), json!(0));
        if let Some(q) = query {
            body.insert("query".into(), q);
        }
        let response = self
            .http
            .request(
                Method::Post,
                &format!("/{}/_search", encode_index_expression(&index)?),
                &[],
                Some(&Json::Object(body)),
                None,
                opts.timeout,
            )
            .await?;
        let total = response.get("hits").and_then(|h| h.get("total"));
        let value = total.and_then(|t| t.get("value")).and_then(Json::as_u64);
        let relation = total
            .and_then(|t| t.get("relation"))
            .and_then(Json::as_str)
            .unwrap_or("eq");
        let message = if relation == "eq" {
            "hits.total (exact — under the tracking limit)"
        } else {
            "hits.total (LOWER BOUND — Elasticsearch stopped counting at track_total_hits)"
        };
        if relation != "eq" {
            notices.push(Notice {
                severity: NoticeSeverity::Warning,
                code: Some(Arc::from("es.total_is_lower_bound")),
                message: Arc::from(
                    format!(
                        "this is at least {} matches, not exactly — request an exact count to run \
                         _count instead",
                        value.unwrap_or(0)
                    )
                    .as_str(),
                ),
            });
        }
        Ok(Box::new(
            AckCursor::new(value, Some(Arc::from(message))).with_notices(notices),
        ))
    }

    async fn execute_explain(
        &self,
        inner: &Request,
        analyze: bool,
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let (index, query) = self.explainable(inner)?;
        let index = encode_index_expression(&index)?;

        let response = if analyze {
            // `profile: true` actually runs the search and reports real
            // per-shard, per-query-component timings — `EXPLAIN_ANALYZE`.
            let mut body = serde_json::Map::new();
            body.insert("profile".into(), json!(true));
            body.insert("size".into(), json!(0));
            if let Some(q) = query {
                body.insert("query".into(), q);
            }
            self.http
                .request(
                    Method::Post,
                    &format!("/{index}/_search"),
                    &[],
                    Some(&Json::Object(body)),
                    None,
                    opts.timeout,
                )
                .await?
        } else {
            // `_validate/query?explain&rewrite` reports the rewritten Lucene
            // query without executing it — the honest non-analyze EXPLAIN.
            let body = query.map(|q| json!({ "query": q }));
            self.http
                .request(
                    Method::Post,
                    &format!("/{index}/_validate/query"),
                    &[
                        ("explain", "true".to_string()),
                        ("rewrite", "true".to_string()),
                    ],
                    body.as_ref(),
                    None,
                    opts.timeout,
                )
                .await?
        };

        Ok(Box::new(DocsCursor::new(vec![serde_to_value(
            &response,
            "",
            &FieldTypes::new(),
        )])))
    }

    /// `Op::Mutate` — guarded, serial, halt-and-report single-document writes.
    ///
    /// The whole batch is compiled *before* a single write leaves the process
    /// (a refused mutation therefore touches no document), the target indices
    /// are checked for the TSDB sentinel-`_seq_no` hazard up front, and the
    /// compiled writes are then issued one at a time. The first failure halts:
    /// Elasticsearch has no transaction, so the applied prefix stays written
    /// and the rest is deliberately never sent. See [`crate::mutate`] for the
    /// model and the per-operation rules.
    async fn execute_mutate(
        &self,
        batch: &MutationBatch,
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
        // Read-only mode refuses generated writes before anything is compiled
        // or sent — the same client-side guarantee `execute_native` gives the
        // console path. Elasticsearch has no server-side read-only session, so
        // this is our enforcement (`Enforcement::Client`) and says so.
        if self.read_only_active(opts) {
            return Err(DbError::Unsupported {
                feature: "generated writes: this connection is in read-only mode (enforced \
                          client-side — Elasticsearch has no read-only session)"
                    .into(),
            });
        }
        if batch.mutations.is_empty() {
            return Ok(Box::new(AckCursor::new(
                Some(0),
                Some(Arc::from("no mutations")),
            )));
        }

        // Pre-flight: compile every mutation before anything is sent. A compile
        // refusal (guard absent, an `Absent` set, an array-element edit, a
        // sentinel guard, an unwritable envelope field) refuses the whole batch
        // with the reason, having touched nothing.
        let include_source_on_error = supports_include_source_on_error(
            product_of(&self.server_info),
            &self.server_info.version,
        );
        let writes: Vec<CompiledWrite> = batch
            .mutations
            .iter()
            .map(|m| compile_mutation(m, include_source_on_error))
            .collect::<Result<_, _>>()?;

        // TSDB hazard (plan §3.2.3, the sharpest one): a time-series-mode index
        // on ES >= 9.4 returns *sentinel* `_seq_no` values and rejects (or, for
        // by-query ops, silently ignores) an `if_seq_no` guard. Index mode is a
        // capability question that belongs in the per-index catalog, but the
        // catalog does not carry it today — so the least-hacky truthful option
        // is to read the target index's settings once, up front, and refuse the
        // whole batch with the reason rather than discover it as a per-document
        // 400. This is best-effort by design: see `refuse_tsdb_indices`.
        self.refuse_tsdb_indices(&writes, opts.timeout).await?;

        // Serial, halt-and-report: issue each write in order, stopping at the
        // first failure. `outcomes` ends up as long as `writes` when everything
        // applied, or exactly one longer than the applied prefix on a halt —
        // everything after the failure is deliberately never sent.
        let mut outcomes = Vec::with_capacity(writes.len());
        for write in &writes {
            let query: Vec<(&str, String)> =
                write.query.iter().map(|(k, v)| (*k, v.clone())).collect();
            let opaque = self.next_opaque_prefix();
            match self
                .http
                .request(
                    write.method,
                    &write.path,
                    &query,
                    write.body.as_ref(),
                    Some(&opaque),
                    opts.timeout,
                )
                .await
            {
                Ok(response) => outcomes.push(WriteOutcome::Applied(response)),
                Err(error) => {
                    // Halt: record the failure and send nothing further. A 409
                    // arrives here as `DbError::Conflict` (recoverable); the
                    // report surfaces it as a per-row conflict.
                    outcomes.push(WriteOutcome::Failed(error));
                    break;
                }
            }
        }

        let (docs, notices) = batch_report(&writes, outcomes);
        Ok(Box::new(DocsCursor::new(docs).with_notices(notices)))
    }

    /// Refuse a batch whose target index cannot honour the optimistic-
    /// concurrency guard (a time-series index on ES >= 9.4, whose `_seq_no` is
    /// a sentinel). Reads `GET /<index>/_settings?flat_settings=true` once per
    /// distinct concrete index and classifies it with
    /// [`guard_unsupported_reason`].
    ///
    /// **Best-effort on purpose.** If the settings are unreadable (no
    /// permission, a transient error) the check is skipped rather than failing
    /// the batch: the compiled write still carries `if_seq_no`, and a
    /// sequence-numbers-disabled index rejects a guarded single-document write
    /// at write time rather than applying it — so a missing settings read
    /// downgrades to that per-document guard, it does not open a silent clobber.
    async fn refuse_tsdb_indices(
        &self,
        writes: &[CompiledWrite],
        timeout: Option<Duration>,
    ) -> Result<(), DbError> {
        let mut checked: HashSet<&str> = HashSet::new();
        for write in writes {
            if !checked.insert(write.index.as_str()) {
                continue;
            }
            let path = format!("/{}/_settings", encode_index_expression(&write.index)?);
            match self
                .http
                .request(
                    Method::Get,
                    &path,
                    &[("flat_settings", "true".to_string())],
                    None,
                    None,
                    timeout,
                )
                .await
            {
                Ok(settings) => {
                    if let Some(reason) =
                        guard_unsupported_reason(&settings, self.server_info.version.as_ref())
                    {
                        return Err(DbError::Unsupported { feature: reason });
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        index = write.index,
                        "index settings unavailable before a guarded write; relying on the \
                         if_seq_no guard, which a sequence-numbers-disabled index rejects rather \
                         than applies"
                    );
                }
            }
        }
        Ok(())
    }

    /// Reduce an inner request to `(index, query)` for `EXPLAIN`.
    fn explainable(&self, inner: &Request) -> Result<(String, Option<Json>), DbError> {
        match inner {
            Request::Op(Op::Scan { path, filter, .. }) => {
                let index = self.resolve_index(path.parts().first().map(|p| &**p));
                let query = filter
                    .as_ref()
                    .map(|f| compile_predicate(f).map(|c| c.query))
                    .transpose()?;
                Ok((index, query))
            }
            Request::Op(Op::Count { path, filter, .. }) => {
                let index = self.resolve_index(path.parts().first().map(|p| &**p));
                let query = filter
                    .as_ref()
                    .map(|f| compile_predicate(f).map(|c| c.query))
                    .transpose()?;
                Ok((index, query))
            }
            Request::Native { text, params, .. } => {
                let req = console::parse(text, self.default_index.as_deref(), params)?;
                if !req.is_search() {
                    return Err(DbError::Unsupported {
                        feature: "EXPLAIN of a non-search request".into(),
                    });
                }
                let index = self.resolve_index(req.search_index());
                let query = req.body.and_then(|b| b.get("query").cloned());
                Ok((index, query))
            }
            Request::Op(other) => Err(DbError::Unsupported {
                feature: format!("EXPLAIN of {}", op_name(other)),
            }),
        }
    }
}

fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Scan { .. } => "Op::Scan",
        Op::Count { .. } => "Op::Count",
        Op::Mutate(_) => "Op::Mutate",
        Op::Explain { .. } => "Op::Explain",
        Op::Ddl(_) => "Op::Ddl",
    }
}

/// The read-only classifier — the second layer of the read-only guardrail,
/// after the UI badge. Deliberately allow-list shaped: an endpoint nobody
/// thought about is refused, not permitted.
pub fn is_read_request(req: &ConsoleRequest) -> bool {
    if req.method.is_read() {
        return true;
    }
    if req.method != Method::Post {
        return false;
    }
    req.path
        .split('/')
        .any(|seg| READ_ONLY_POST_ENDPOINTS.contains(&seg))
}

fn refuse_if_write(req: &ConsoleRequest) -> Result<(), DbError> {
    if is_read_request(req) {
        return Ok(());
    }
    Err(DbError::Unsupported {
        feature: format!(
            "`{} {}` is not a read, and this connection is in read-only mode (enforced \
             client-side — Elasticsearch has no read-only session)",
            req.method.as_str(),
            req.path
        ),
    })
}

#[async_trait]
impl Connection for EsConnection {
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.check_open()?;
        // `HEAD /` is the cheapest liveness check the engine offers.
        self.http
            .request(Method::Head, "/", &[], None, None, None)
            .await
            .map(|_| ())
    }

    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        self.check_open()?;
        match &req {
            Request::Native { text, params, opts } => self.execute_native(text, params, opts).await,
            Request::Op(op) => {
                let opts = ExecOpts::default();
                match op {
                    Op::Scan { .. } => self.execute_scan(op, &opts).await,
                    Op::Count {
                        path,
                        filter,
                        exact,
                    } => {
                        self.execute_count(path, filter.as_ref(), *exact, &opts)
                            .await
                    }
                    Op::Explain { inner, analyze } => {
                        self.execute_explain(inner, *analyze, &opts).await
                    }
                    // Guarded, serial, halt-and-report single-document writes.
                    // `EDITABLE_RESULTS` is on; `ATOMIC_BATCH` stays off because
                    // Elasticsearch has no multi-document transaction.
                    Op::Mutate(batch) => self.execute_mutate(batch, &opts).await,
                    // `DDL` is off: Elasticsearch's index/mapping management is
                    // not SQL DDL, and generating it from a `DdlOp::Native`
                    // blob would be an untyped passthrough pretending to be a
                    // capability.
                    Op::Ddl(_) => Err(DbError::Unsupported {
                        feature: "Op::Ddl (DDL is off for this driver; index and mapping \
                                  management is a native `PUT /<index>` request)"
                            .into(),
                    }),
                }
            }
        }
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(EsCanceller::new(
            self.http.clone(),
            self.inflight.clone(),
            self.async_search,
        ))
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        self.catalog.clone()
    }

    async fn begin(&self, _opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        // Not a downgrade and not a silent no-op: Elasticsearch has no
        // multi-document transactions at all, and `Caps::TRANSACTIONS` is off
        // so the UI never offers this.
        Err(DbError::Unsupported {
            feature: "transactions (Elasticsearch has no multi-document transactions)".into(),
        })
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        self.check_open()?;
        self.read_only.store(on, Ordering::Release);
        // Honest: this is our classifier, not the server's. A read-only
        // badge that is only client-side has to say that it is, or it
        // promises a guarantee nothing is enforcing.
        Ok(Enforcement::Client)
    }

    async fn close(&self) -> Result<(), DbError> {
        self.closed.store(true, Ordering::Release);
        // Idempotent: any still-open PIT/scroll is released by its cursor's
        // own `close()`/`Drop`, and the HTTP pool drains when this connection
        // is dropped.
        self.mapping_cache.lock().await.clear();
        *self.inflight.lock().await = InFlight::default();
        Ok(())
    }
}

/// What product this connection reported, for the crate's own tests and for
/// callers that want the enum rather than the display string.
pub fn product_of(info: &ServerInfo) -> Product {
    if info
        .details
        .iter()
        .any(|(k, v)| &**k == "distribution" && v.eq_ignore_ascii_case("opensearch"))
    {
        Product::OpenSearch
    } else {
        Product::Elasticsearch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::parse;

    fn console(text: &str) -> ConsoleRequest {
        parse(text, Some("i"), &[]).unwrap()
    }

    #[test]
    fn reads_pass_the_read_only_classifier() {
        for text in [
            "GET /i/_search\n{}",
            "GET /_cluster/health",
            "HEAD /i",
            "POST /i/_search\n{}",
            "POST /i/_count",
            "POST /i/_validate/query",
            "POST /_field_caps",
            "POST /i/_pit",
            "POST /i/_async_search",
        ] {
            assert!(is_read_request(&console(text)), "{text} should be a read");
            assert!(refuse_if_write(&console(text)).is_ok());
        }
    }

    #[test]
    fn writes_are_refused_before_they_leave_the_process() {
        for text in [
            "POST /i/_doc\n{}",
            "PUT /i/_doc/1\n{}",
            "DELETE /i/_doc/1",
            "POST /i/_update/1\n{}",
            "POST /i/_delete_by_query\n{}",
            "POST /i/_update_by_query",
            "PUT /i",
            "DELETE /i",
            "POST /_bulk",
            "POST /_reindex\n{}",
        ] {
            assert!(
                !is_read_request(&console(text)),
                "{text} must not be a read"
            );
            let err = refuse_if_write(&console(text)).unwrap_err();
            assert!(matches!(err, DbError::Unsupported { .. }));
            assert!(
                err.to_string().contains("client-side"),
                "the refusal must admit it is only client-side: {err}"
            );
        }
    }

    /// Regression: `_mapping` used to sit in the POST allow-list, so a
    /// `POST /<index>/_mapping` body — a mapping WRITE — sailed through
    /// read-only mode. Reading a mapping is `GET`, which needs no allow-list
    /// entry.
    #[test]
    fn post_mapping_is_a_write_and_is_refused_in_read_only_mode() {
        let req = console("POST /events/_mapping\n{\"properties\":{\"x\":{\"type\":\"keyword\"}}}");
        assert!(!is_read_request(&req), "POST _mapping writes the mapping");
        let err = refuse_if_write(&req).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        // GET stays a read purely on the method.
        assert!(is_read_request(&console("GET /events/_mapping")));
    }

    /// Regression: `_mget`/`_termvectors`/`_mtermvectors` are genuine reads
    /// (and classified `Read` by `datagrep-lang`'s EsDsl), but were missing
    /// from the driver allow-list, so read-only mode wrongly refused them.
    #[test]
    fn mget_and_termvectors_are_reads_in_read_only_mode() {
        for text in [
            "POST /i/_mget\n{\"ids\":[\"1\",\"2\"]}",
            "POST /i/_termvectors/1",
            "POST /i/_mtermvectors\n{\"ids\":[\"1\"]}",
        ] {
            assert!(is_read_request(&console(text)), "{text} should be a read");
            assert!(refuse_if_write(&console(text)).is_ok());
        }
    }

    /// The allow-list must not be fooled by an endpoint name appearing as a
    /// substring of an index name.
    #[test]
    fn the_allow_list_matches_whole_path_segments_only() {
        assert!(!is_read_request(&console(
            "POST /_search_index_backup/_doc\n{}"
        )));
        assert!(!is_read_request(&console("POST /my_search/_doc\n{}")));
        assert!(is_read_request(&console("POST /my_search/_search\n{}")));
    }

    #[test]
    fn op_names_are_stable_for_error_messages() {
        assert_eq!(
            op_name(&Op::Mutate(datagrep_api::request::MutationBatch::default())),
            "Op::Mutate"
        );
        assert_eq!(
            op_name(&Op::Ddl(datagrep_api::request::DdlOp::Native {
                text: Arc::from("PUT /i")
            })),
            "Op::Ddl"
        );
    }

    #[test]
    fn product_is_read_back_out_of_server_info_details() {
        let os = ServerInfo {
            product: Arc::from("OpenSearch"),
            version: Arc::from("2.11.0"),
            details: vec![(Arc::from("distribution"), Arc::from("opensearch"))],
        };
        assert_eq!(product_of(&os), Product::OpenSearch);
        let es = ServerInfo {
            product: Arc::from("Elasticsearch"),
            version: Arc::from("8.15.0"),
            details: vec![(Arc::from("distribution"), Arc::from("elasticsearch"))],
        };
        assert_eq!(product_of(&es), Product::Elasticsearch);
    }

    /// A non-dialing connection: `EsHttp::new` opens no socket, and the
    /// read-only refusal fires before any request would be built, so this
    /// exercises the guard without a cluster.
    fn offline_connection() -> EsConnection {
        let http = Arc::new(
            EsHttp::new(
                "http://localhost:9200".into(),
                crate::http::Auth::None,
                std::time::Duration::from_secs(5),
                false,
            )
            .unwrap(),
        );
        let server_info = ServerInfo {
            product: Arc::from("Elasticsearch"),
            version: Arc::from("9.0.0"),
            details: Vec::new(),
        };
        let caps = crate::driver::es_capabilities(Product::Elasticsearch, PageMode::Pit);
        EsConnection::new(http, server_info, caps, PageMode::Pit, true, None, None)
    }

    #[tokio::test]
    async fn a_read_only_connection_refuses_generated_writes_before_they_leave_the_process() {
        use datagrep_api::request::Mutation;
        use datagrep_api::value::FieldPath;

        let conn = offline_connection();
        conn.set_read_only(true).await.unwrap();

        let batch = MutationBatch {
            mutations: vec![Mutation::Delete {
                path: ObjectPath::new(vec![Arc::from("events")]),
                key: vec![
                    (FieldPath::field("_index"), Value::Str(Arc::from("events"))),
                    (FieldPath::field("_id"), Value::Str(Arc::from("abc"))),
                ],
                expect: vec![
                    (FieldPath::field("_seq_no"), Value::I64(1)),
                    (FieldPath::field("_primary_term"), Value::I64(1)),
                ],
            }],
        };
        let err = match conn.execute(Request::Op(Op::Mutate(batch))).await {
            Err(e) => e,
            Ok(_) => panic!("a read-only connection must refuse Op::Mutate"),
        };
        assert!(matches!(err, DbError::Unsupported { .. }));
        assert!(
            err.to_string().contains("read-only"),
            "the refusal must name read-only mode: {err}"
        );
    }
}
