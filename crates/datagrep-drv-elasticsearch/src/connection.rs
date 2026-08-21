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
use datagrep_api::request::{DdlOp, ExecOpts, MutationBatch, Op, Request};
use datagrep_api::shape::ObjectPath;
use datagrep_api::value::Value;

use crate::canceller::{EsCanceller, InFlight, InFlightSlot};
use crate::catalog::{encode_index_expression, EsCatalog};
use crate::console::{self, ConsoleRequest};
use crate::cursor::{AckCursor, DocsCursor, ScanSpec, SearchCursor, DEFAULT_KEEP_ALIVE};
use crate::ddl::EsDdlKind;
use crate::filter::{compile_predicate, compile_sort};
use crate::http::{EsHttp, Method, PageMode, Product};
use crate::mutate::{
    batch_report, bulk_report, compile_bulk_body, compile_mutation, guard_unsupported_reason,
    supports_include_source_on_error, CompiledWrite, WriteOutcome, MAX_BULK_BODY_BYTES,
};
use crate::value::{serde_to_value, FieldTypes};

// POST-but-read allow-list; _mapping is deliberately absent (POST /_mapping writes). Keep in sync with esdsl::READ_ACTIONS in datagrep-lang.
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

    async fn open_scan(
        &self,
        spec: ScanSpec,
        resume: Option<&datagrep_api::driver::ResumeToken>,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let ScanSpec { index, timeout, .. } = &spec;
        let (index, timeout) = (index.clone(), *timeout);
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

    async fn execute_ddl(&self, op: &DdlOp, opts: &ExecOpts) -> Result<Box<dyn Cursor>, DbError> {
        if self.read_only_active(opts) {
            return Err(DbError::Unsupported {
                feature: "DDL: this connection is in read-only mode (enforced client-side — \
                          Elasticsearch has no read-only session)"
                    .into(),
            });
        }
        let plan = crate::ddl::plan(op)?;
        let sent = match &plan.kind {
            EsDdlKind::DeleteIndex {
                index,
                ignore_unavailable,
            } => {
                let query: Vec<(&str, String)> = if *ignore_unavailable {
                    vec![("ignore_unavailable", "true".to_string())]
                } else {
                    Vec::new()
                };
                self.http
                    .request(
                        Method::Delete,
                        &format!("/{}", encode_index_expression(index)?),
                        &query,
                        None,
                        None,
                        opts.timeout,
                    )
                    .await
            }
            EsDdlKind::Aliases { body } => {
                self.http
                    .request(
                        Method::Post,
                        "/_aliases",
                        &[],
                        Some(body),
                        None,
                        opts.timeout,
                    )
                    .await
            }
        };
        match sent {
            Ok(_) => Ok(Box::new(AckCursor::new(None, Some(Arc::from(plan.ack))))),
            Err(DbError::Query { ref code, .. })
                if plan.absent_code.is_some() && code.as_deref() == plan.absent_code =>
            {
                Ok(Box::new(AckCursor::new(
                    None,
                    Some(Arc::from("nothing to drop")),
                )))
            }
            Err(e) => Err(e),
        }
    }

    async fn execute_mutate(
        &self,
        batch: &MutationBatch,
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
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

        let include_source_on_error = supports_include_source_on_error(
            product_of(&self.server_info),
            &self.server_info.version,
        );
        let writes: Vec<CompiledWrite> = batch
            .mutations
            .iter()
            .map(|m| compile_mutation(m, include_source_on_error))
            .collect::<Result<_, _>>()?;

        self.refuse_tsdb_indices(&writes, opts.timeout).await?;

        if writes.len() == 1 {
            self.execute_mutate_serial(&writes, opts).await
        } else {
            self.execute_mutate_bulk(&writes, opts).await
        }
    }

    async fn execute_mutate_serial(
        &self,
        writes: &[CompiledWrite],
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let mut outcomes = Vec::with_capacity(writes.len());
        for write in writes {
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
                    // Halt: record the failure and send nothing further.
                    outcomes.push(WriteOutcome::Failed(error));
                    break;
                }
            }
        }

        let (docs, notices) = batch_report(writes, outcomes);
        Ok(Box::new(DocsCursor::new(docs).with_notices(notices)))
    }

    async fn execute_mutate_bulk(
        &self,
        writes: &[CompiledWrite],
        opts: &ExecOpts,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let body = compile_bulk_body(writes, MAX_BULK_BODY_BYTES)?;

        let mut query: Vec<(&str, String)> = vec![
            ("refresh", "wait_for".to_string()),
            (
                "filter_path",
                "errors,items.*.status,items.*.error,items.*.result,items.*._id,\
                 items.*._seq_no,items.*._primary_term,items.*.forced_refresh"
                    .to_string(),
            ),
        ];
        if supports_include_source_on_error(
            product_of(&self.server_info),
            &self.server_info.version,
        ) {
            // Never let a malformed-document item error echo the document.
            query.push(("include_source_on_error", "false".to_string()));
        }

        let opaque = self.next_opaque_prefix();
        let response = self
            .http
            .request_ndjson(
                Method::Post,
                "/_bulk",
                &query,
                &body,
                Some(&opaque),
                opts.timeout,
            )
            .await?;

        let (docs, notices) = bulk_report(writes, &response)?;
        Ok(Box::new(DocsCursor::new(docs).with_notices(notices)))
    }

    // TSDB indices (ES >= 9.4) carry sentinel _seq_no and reject or ignore if_seq_no, so refuse up front; best-effort — unreadable settings skip the check and the per-document guard still refuses at write time.
    async fn refuse_tsdb_indices(
        &self,
        writes: &[CompiledWrite],
        timeout: Option<Duration>,
    ) -> Result<(), DbError> {
        let mut checked: HashSet<&str> = HashSet::new();
        for write in writes {
            // Inserts guard with op_type=create, not if_seq_no, so the TSDB hazard does not apply.
            if write.op == "insert" {
                continue;
            }
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
                // Request::Op carries no ExecOpts, so driver defaults apply — no caller row limit or timeout on this path.
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
                    Op::Mutate(batch) => self.execute_mutate(batch, &opts).await,
                    Op::Ddl(ddl) => self.execute_ddl(ddl, &opts).await,
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
        Err(DbError::Unsupported {
            feature: "transactions (Elasticsearch has no multi-document transactions)".into(),
        })
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        self.check_open()?;
        self.read_only.store(on, Ordering::Release);
        Ok(Enforcement::Client)
    }

    async fn close(&self) -> Result<(), DbError> {
        self.closed.store(true, Ordering::Release);
        self.mapping_cache.lock().await.clear();
        *self.inflight.lock().await = InFlight::default();
        Ok(())
    }
}

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

    #[test]
    fn post_mapping_is_a_write_and_is_refused_in_read_only_mode() {
        let req = console("POST /events/_mapping\n{\"properties\":{\"x\":{\"type\":\"keyword\"}}}");
        assert!(!is_read_request(&req), "POST _mapping writes the mapping");
        let err = refuse_if_write(&req).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }));
        // GET stays a read purely on the method.
        assert!(is_read_request(&console("GET /events/_mapping")));
    }

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
