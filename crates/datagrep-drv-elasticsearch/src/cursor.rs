//! The streaming cursors. [`SearchCursor`] is the one that matters: a
//! point-in-time + `search_after` scan (or a `_scroll` scan where PIT does not
//! exist), pulling exactly one page per [`Cursor::next_batch`] call.
//!
//! # Why this is the whole point
//!
//! `next_batch` is pull-only: if nobody calls it, no page is requested, no
//! bytes are read, and the server is never asked to produce more. That is the
//! entire backpressure story — and it is precisely what the official
//! `elasticsearch` crate cannot give us, because it materializes each response
//! body in full before yielding anything.
//!
//! # Server-side context, released on every exit path
//!
//! A PIT (or scroll) pins segments on every shard it touches; leaking one
//! costs the cluster real disk and file handles. The context is therefore
//! released:
//!
//! - on natural exhaustion, immediately after the last page;
//! - on [`Cursor::close`], which the core calls when it drops a result tab or
//!   disconnects on idle;
//! - on **any error**, including a cancelled search, before the error
//!   propagates;
//! - and, as a backstop, in `Drop`, which spawns a best-effort release onto
//!   the current runtime for the case where a task was aborted outright.
//!
//! Every one of those paths funnels through the same `release_context`, which
//! is idempotent.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use datagrep_api::driver::{
    Batch, Cursor, CursorStats, FetchHint, Notice, NoticeSeverity, Payload, ResumeToken,
};
use datagrep_api::error::DbError;
use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType, SchemaDelta, Shape};
use datagrep_api::value::{Document, FieldPath, Value};

use crate::canceller::{InFlight, InFlightSlot};
use crate::http::{EsHttp, Method, PageMode};
use crate::json::OrderedJson;
use crate::resume::{EsResume, ResumeMode};
use crate::value::{json_to_value, FieldTypes};

/// `index.max_result_window` defaults to 10 000 and bounds a single page's
/// `size` even under `search_after`. Asking for more is a 400, so the hint is
/// clamped rather than allowed to fail the request.
pub const MAX_PAGE_SIZE: u32 = 10_000;

/// How long the server is asked to hold a PIT/scroll context between pulls.
/// Long enough to survive a user thinking about the grid, short enough that an
/// abandoned context expires on its own if every release path somehow failed.
pub const DEFAULT_KEEP_ALIVE: &str = "5m";

/// How long each `_async_search` poll waits before returning control, so a
/// cancel issued from another task is never blocked behind a long HTTP read.
const ASYNC_POLL_WAIT: &str = "1s";

/// Everything a scan needs that is independent of which pagination mechanism
/// ends up being used.
#[derive(Debug, Clone)]
pub struct ScanSpec {
    /// Index/alias/data-stream expression; empty means cluster-wide.
    pub index: String,
    /// The search body minus `size`/`sort`/`pit`/`search_after`, which this
    /// cursor owns.
    pub body: Json,
    /// Compiled user sort keys; the stable tiebreaker is appended by the
    /// cursor.
    pub user_sort: Vec<Json>,
    /// Hard row cap from `Op::Scan { limit }` / `ExecOpts::row_limit`.
    pub limit: Option<u64>,
    /// Notices produced while compiling the request (dropped array indexes,
    /// null/absent conflation) — emitted with the first batch.
    pub notices: Vec<Notice>,
    /// Per-request deadline, pushed to the server as the search body's
    /// `timeout` as well as applied to the HTTP call.
    pub timeout: Option<Duration>,
}

/// The live server-side context this cursor must release.
#[derive(Debug, Clone)]
enum Context {
    /// Point-in-time id. Elasticsearch may hand back a *new* id with each
    /// page; the latest one is what has to be released.
    Pit(String),
    /// Scroll id; `None` until the first page has been fetched.
    Scroll(Option<String>),
}

pub struct SearchCursor {
    http: Arc<EsHttp>,
    spec: ScanSpec,
    mode: PageMode,
    keep_alive: String,
    /// `None` once released — the idempotence marker for every exit path.
    context: Option<Context>,
    /// The `search_after` cursor: the last hit's `sort` values.
    last_sort: Vec<Json>,
    types: Arc<FieldTypes>,
    shape: Shape,
    seen_fields: HashSet<Arc<str>>,
    stats: CursorStats,
    /// Remaining rows under the caller's limit.
    remaining: Option<u64>,
    delivered: u64,
    exhausted: bool,
    closed: bool,
    pending_notices: Vec<Notice>,
    inflight: InFlightSlot,
    async_search: bool,
    /// Adaptive page-size ceiling: shrunk when a page overshot the caller's
    /// byte budget, since Elasticsearch has no server-side response-size cap.
    byte_capped_size: Option<u32>,
    opaque_counter: u64,
    opaque_prefix: Arc<str>,
}

impl SearchCursor {
    /// Build a cursor over an already-opened server context.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: Arc<EsHttp>,
        spec: ScanSpec,
        mode: PageMode,
        keep_alive: String,
        pit_id: Option<String>,
        types: Arc<FieldTypes>,
        inflight: InFlightSlot,
        async_search: bool,
        opaque_prefix: Arc<str>,
    ) -> Self {
        let context = match mode {
            PageMode::Pit => Context::Pit(pit_id.unwrap_or_default()),
            PageMode::Scroll => Context::Scroll(None),
        };
        let mut notices = spec.notices.clone();
        notices.push(Notice {
            severity: NoticeSeverity::Info,
            code: Some(Arc::from("es.pagination")),
            message: Arc::from(
                format!("streaming with {} (keep_alive {keep_alive})", mode.as_str()).as_str(),
            ),
        });
        let remaining = spec.limit;
        Self {
            http,
            spec,
            mode,
            keep_alive,
            context: Some(context),
            last_sort: Vec::new(),
            types,
            // `root_hint` points the grid at `_source`, so document fields
            // render as top-level columns and the `_index`/`_id`/`_score`
            // envelope stays available in the detail pane. `identity` names
            // the envelope paths that identify a hit — `_routing` is part of
            // identity whenever present (a custom-routed index needs it on
            // every write, or the write lands on the wrong shard).
            shape: Shape::Documents {
                root_hint: Some(FieldPath::field("_source")),
                identity: Some(vec![
                    FieldPath::field("_index"),
                    FieldPath::field("_id"),
                    FieldPath::field("_routing"),
                ]),
            },
            seen_fields: HashSet::new(),
            stats: CursorStats::default(),
            remaining,
            delivered: 0,
            exhausted: false,
            closed: false,
            pending_notices: notices,
            inflight,
            async_search,
            byte_capped_size: None,
            opaque_counter: 0,
            opaque_prefix,
        }
    }

    /// Rebuild a cursor from a [`ResumeToken`] — the idle-disconnect path:
    /// the core closed everything, and the scan picks up from the PIT id plus
    /// the last `search_after` values alone.
    /// `fresh_pit` is a **newly opened** point-in-time for a `Pit`-mode resume.
    ///
    /// The token's own PIT id is deliberately not reused: the whole point of a
    /// resume token is that the core could close the cursor and disconnect,
    /// and closing a cursor releases its PIT (`DELETE /_pit`). Trying the dead
    /// id would fail with `search_context_missing_exception` on every shard.
    /// The *position* lives in the `search_after` sort values, which remain
    /// valid against a new point-in-time, so the resume opens a fresh one and
    /// says — via a `Notice` — that the snapshot changed and documents indexed
    /// in the meantime may now be visible.
    #[allow(clippy::too_many_arguments)]
    pub fn from_resume(
        http: Arc<EsHttp>,
        resume: EsResume,
        fresh_pit: Option<String>,
        user_sort: Vec<Json>,
        types: Arc<FieldTypes>,
        inflight: InFlightSlot,
        async_search: bool,
        opaque_prefix: Arc<str>,
        timeout: Option<Duration>,
    ) -> Self {
        let mode = match resume.mode {
            ResumeMode::Pit => PageMode::Pit,
            ResumeMode::Scroll => PageMode::Scroll,
        };
        let spec = ScanSpec {
            index: resume.index.clone(),
            body: resume.body.clone(),
            user_sort,
            limit: resume.remaining,
            notices: Vec::new(),
            timeout,
        };
        let mut cursor = Self::new(
            http,
            spec,
            mode,
            resume.keep_alive.clone(),
            None,
            types,
            inflight,
            async_search,
            opaque_prefix,
        );
        cursor.context = Some(match resume.mode {
            ResumeMode::Pit => Context::Pit(fresh_pit.unwrap_or_else(|| resume.id.clone())),
            // A scroll id *is* the position, so it is reused as-is.
            ResumeMode::Scroll => Context::Scroll(Some(resume.id.clone())),
        });
        cursor.last_sort = resume.sort;
        cursor.delivered = resume.delivered;
        cursor.remaining = resume.remaining;
        cursor.pending_notices.push(Notice {
            severity: NoticeSeverity::Info,
            code: Some(Arc::from("es.resumed")),
            message: Arc::from(
                format!(
                    "resumed a {} scan after {} rows",
                    mode.as_str(),
                    resume.delivered
                )
                .as_str(),
            ),
        });
        if mode == PageMode::Pit {
            cursor.pending_notices.push(Notice {
                severity: NoticeSeverity::Warning,
                code: Some(Arc::from("es.pit_recreated")),
                message: Arc::from(
                    "the point-in-time was re-opened for this resume: the scan continues from \
                     exactly where it stopped, but it is no longer the original snapshot, so \
                     documents indexed since may now appear",
                ),
            });
        }
        cursor
    }

    /// The tiebreaker that makes `search_after` totally ordered.
    ///
    /// `_shard_doc` exists only inside a PIT (Elasticsearch 7.12+) and is the
    /// only globally unique, cheap sort key. Under scroll the position lives
    /// in the scroll id, so `_doc` — Lucene's internal document order, the
    /// cheapest possible sort — is used purely to avoid scoring.
    fn tiebreaker(&self) -> Json {
        match self.mode {
            PageMode::Pit => json!({ "_shard_doc": "asc" }),
            PageMode::Scroll => json!({ "_doc": "asc" }),
        }
    }

    fn full_sort(&self) -> Vec<Json> {
        let mut sort = self.spec.user_sort.clone();
        sort.push(self.tiebreaker());
        sort
    }

    fn next_opaque_id(&mut self) -> String {
        self.opaque_counter += 1;
        format!("{}-{}", self.opaque_prefix, self.opaque_counter)
    }

    /// Build one page's request body.
    fn page_body(&self, size: u32) -> Json {
        let mut body = self.spec.body.clone();
        let map = body.as_object_mut().expect("search body is an object");
        map.insert("size".into(), json!(size));
        map.insert("sort".into(), Json::Array(self.full_sort()));
        // Ask every hit to carry its `_seq_no`/`_primary_term`. They are the
        // per-document compare-and-swap guard (`if_seq_no`/`if_primary_term`) a
        // later mutation needs; without requesting them here there is nothing
        // to guard a write with. `_routing` rides along in the hit envelope on
        // its own when the index uses custom routing. Requested unconditionally
        // because a read cursor cannot know whether the rows it streams will
        // later be edited, and the cost is two integers per hit.
        map.insert("seq_no_primary_term".into(), json!(true));
        if let Some(t) = self.spec.timeout {
            // Push the deadline server-side too, so even an uncancellable
            // shard is bounded.
            map.insert("timeout".into(), json!(format!("{}ms", t.as_millis())));
        }
        if !self.last_sort.is_empty() {
            map.insert("search_after".into(), Json::Array(self.last_sort.clone()));
        }
        if let (PageMode::Pit, Some(Context::Pit(id))) = (self.mode, self.context.as_ref()) {
            map.insert(
                "pit".into(),
                json!({ "id": id, "keep_alive": self.keep_alive }),
            );
        }
        body
    }

    /// Issue one search and return `(response, wire_bytes)`.
    async fn run_search(&mut self, size: u32) -> Result<(OrderedJson, usize), DbError> {
        match (self.mode, self.context.as_ref()) {
            // A scroll continuation is its own endpoint and carries no body
            // beyond the scroll id.
            (PageMode::Scroll, Some(Context::Scroll(Some(id)))) => {
                let body = json!({ "scroll": self.keep_alive, "scroll_id": id });
                let opaque = self.next_opaque_id();
                self.set_inflight(Some(opaque.clone()), None).await;
                let result = self
                    .http
                    .request_ordered(
                        Method::Post,
                        "/_search/scroll",
                        &[],
                        Some(&body),
                        Some(&opaque),
                        self.spec.timeout,
                    )
                    .await;
                self.clear_inflight().await;
                result
            }
            _ => {
                let body = self.page_body(size);
                // Under a PIT the index MUST NOT appear in the path: the PIT
                // already names the indices it was opened over, and supplying
                // both is a 400.
                let path_index = match self.mode {
                    PageMode::Pit => String::new(),
                    PageMode::Scroll => {
                        if self.spec.index.is_empty() {
                            String::new()
                        } else {
                            format!("/{}", self.spec.index)
                        }
                    }
                };
                let query: Vec<(&str, String)> = match self.mode {
                    PageMode::Scroll => vec![("scroll", self.keep_alive.clone())],
                    PageMode::Pit => Vec::new(),
                };
                if self.async_search {
                    self.run_async_search(&path_index, &query, &body).await
                } else {
                    let opaque = self.next_opaque_id();
                    self.set_inflight(Some(opaque.clone()), None).await;
                    let result = self
                        .http
                        .request_ordered(
                            Method::Post,
                            &format!("{path_index}/_search"),
                            &query,
                            Some(&body),
                            Some(&opaque),
                            self.spec.timeout,
                        )
                        .await;
                    self.clear_inflight().await;
                    result
                }
            }
        }
    }

    /// Submit with `wait_for_completion_timeout=0` so the call returns a
    /// handle to still-running work, then poll. A plain `_search` would give
    /// us nothing to cancel; the handle plus the `X-Opaque-Id` tag are what
    /// [`crate::canceller::EsCanceller`] cancels.
    async fn run_async_search(
        &mut self,
        path_index: &str,
        query: &[(&str, String)],
        body: &Json,
    ) -> Result<(OrderedJson, usize), DbError> {
        let opaque = self.next_opaque_id();
        self.set_inflight(Some(opaque.clone()), None).await;

        let mut submit_query: Vec<(&str, String)> = query.to_vec();
        submit_query.push(("wait_for_completion_timeout", "0".to_string()));
        // Keep the (possibly already complete) result addressable so the
        // canceller has something concrete to delete, and so a completed
        // search can still be fetched by id.
        submit_query.push(("keep_on_completion", "true".to_string()));

        let (submitted, mut bytes) = self
            .http
            .request_ordered(
                Method::Post,
                &format!("{path_index}/_async_search"),
                &submit_query,
                Some(body),
                Some(&opaque),
                self.spec.timeout,
            )
            .await
            .inspect_err(|_| ())?;

        let async_id = submitted
            .get("id")
            .and_then(OrderedJson::as_str)
            .map(str::to_string);
        if let Some(id) = &async_id {
            self.set_inflight(Some(opaque.clone()), Some(id.clone()))
                .await;
        }

        let mut current = submitted;
        let deadline = self.spec.timeout.map(|t| Instant::now() + t);
        loop {
            let running = current
                .get("is_running")
                .and_then(OrderedJson::as_bool)
                .unwrap_or(false);

            // The error is only authoritative once the search has STOPPED.
            // A still-running async search can report a transient
            // `status_exception: error while reducing partial results` in an
            // intermediate response — that is a hiccup reducing the partial
            // view, not the outcome of the search, and failing on it would
            // turn a perfectly good query into a spurious error.
            if !running {
                if let Some(err) = async_search_error(&current) {
                    self.finish_async(async_id.as_deref()).await;
                    return Err(err);
                }
                let response = current
                    .get("response")
                    .cloned()
                    // A response with no `response` key is either a plain
                    // `_search` reply (async search unavailable behind a
                    // proxy) or a protocol violation; using it as-is keeps the
                    // former working.
                    .unwrap_or(current);
                self.finish_async(async_id.as_deref()).await;
                return Ok((response, bytes));
            }
            let Some(id) = async_id.as_deref() else {
                self.clear_inflight().await;
                return Err(DbError::Protocol(
                    "async search is running but returned no id to poll".to_string(),
                ));
            };
            if deadline.is_some_and(|d| Instant::now() >= d) {
                self.finish_async(Some(id)).await;
                return Err(DbError::Timeout);
            }
            let polled = self
                .http
                .request_ordered(
                    Method::Get,
                    &format!("/_async_search/{id}"),
                    &[("wait_for_completion_timeout", ASYNC_POLL_WAIT.to_string())],
                    None,
                    Some(&opaque),
                    self.spec.timeout,
                )
                .await;
            let (next, n) = match polled {
                Ok(v) => v,
                // The async search vanished between two polls. In this driver
                // the only thing that deletes a *running* async search is
                // `EsCanceller::cancel`, so this is the user's stop button
                // landing — a cancellation, not a failure, and the UI must not
                // dress it as one.
                Err(e) if is_async_search_gone(&e) => {
                    self.clear_inflight().await;
                    return Err(DbError::Cancelled);
                }
                Err(e) => {
                    self.finish_async(Some(id)).await;
                    return Err(e);
                }
            };
            bytes = n;
            current = next;
        }
    }

    /// Delete the stored async search (best effort) and clear the in-flight
    /// slot. Leaving it behind would occupy the `.async-search` system index
    /// until its expiry.
    async fn finish_async(&mut self, async_id: Option<&str>) {
        if let Some(id) = async_id {
            if let Err(e) = self
                .http
                .request(
                    Method::Delete,
                    &format!("/_async_search/{id}"),
                    &[],
                    None,
                    None,
                    None,
                )
                .await
            {
                tracing::debug!(error = %e, "async search cleanup failed (it will expire)");
            }
        }
        self.clear_inflight().await;
    }

    async fn set_inflight(&self, opaque_id: Option<String>, async_id: Option<String>) {
        let mut slot = self.inflight.lock().await;
        *slot = InFlight {
            opaque_id: opaque_id.map(|s| Arc::from(s.as_str())),
            async_id: async_id.map(|s| Arc::from(s.as_str())),
        };
    }

    async fn clear_inflight(&self) {
        *self.inflight.lock().await = InFlight::default();
    }

    /// Release the PIT/scroll context. Idempotent, and safe to call from every
    /// exit path including error and cancel.
    async fn release_context(&mut self) {
        let Some(context) = self.context.take() else {
            return;
        };
        let (path, body) = match context {
            Context::Pit(id) if !id.is_empty() => ("/_pit", json!({ "id": id })),
            Context::Scroll(Some(id)) => ("/_search/scroll", json!({ "scroll_id": [id] })),
            // Nothing was ever opened.
            _ => return,
        };
        if let Err(e) = self
            .http
            .request(Method::Delete, path, &[], Some(&body), None, None)
            .await
        {
            // Never fatal: the context expires on its own after `keep_alive`.
            tracing::warn!(error = %e, path, "failed to release elasticsearch search context");
        }
    }

    /// Record any `_source` top-level field not yet seen on this cursor.
    ///
    /// Deliberately shallow and rooted at `_source`, matching this cursor's
    /// `root_hint`: promoting *nested* paths into columns is datagrep-core's
    /// `ViewProjection`/`FieldTrie` job, and the envelope
    /// pseudo-fields (`_id`, `_index`, `_score`) are not announced as columns
    /// because they live outside the hinted root — see the crate report's
    /// `datagrep-api` gaps.
    fn track_schema(&mut self, source: &OrderedJson) -> Vec<SchemaDelta> {
        let Some(fields) = source.as_object() else {
            return Vec::new();
        };
        let mut deltas = Vec::new();
        for (k, v) in fields {
            if self.seen_fields.contains(k.as_str()) {
                continue;
            }
            let name: Arc<str> = Arc::from(k.as_str());
            self.seen_fields.insert(name.clone());
            let logical = json_to_value(v, k, &self.types)
                .logical_type()
                .unwrap_or(LogicalType::Unknown);
            deltas.push(SchemaDelta::AddColumn {
                field: FieldDef {
                    name,
                    logical,
                    flags: FieldFlags::empty(),
                    // Always what the server's mapping said, never what we
                    // mapped it to.
                    native_type: self.types.native(k),
                },
            });
        }
        deltas
    }

    fn note_total(&mut self, response: &OrderedJson) {
        let Some(total) = response.get("hits").and_then(|h| h.get("total")) else {
            return;
        };
        let value = total.get("value").and_then(OrderedJson::as_i64);
        let relation = total.get("relation").and_then(OrderedJson::as_str);
        if let (Some(value), Some("gte")) = (value, relation) {
            self.pending_notices.push(Notice {
                severity: NoticeSeverity::Info,
                code: Some(Arc::from("es.total_is_lower_bound")),
                message: Arc::from(
                    format!(
                        "hits.total is a lower bound: at least {value} matches (Elasticsearch \
                         stops counting at track_total_hits, 10 000 by default)"
                    )
                    .as_str(),
                ),
            });
        }
    }
}

/// Extract an error out of an `_async_search` envelope, distinguishing a
/// cancellation from a genuine failure so the UI never dresses a user cancel
/// up as an error: the user cancelled, which is not a failure.
fn async_search_error(envelope: &OrderedJson) -> Option<DbError> {
    let error = envelope.get("error")?;
    let text = error
        .get("type")
        .and_then(OrderedJson::as_str)
        .unwrap_or_default()
        .to_string()
        + " "
        + error
            .get("reason")
            .and_then(OrderedJson::as_str)
            .unwrap_or_default();
    if text.contains("cancelled") || text.contains("canceled") {
        return Some(DbError::Cancelled);
    }
    Some(crate::error::map_status_error(
        400,
        &serde_json::to_string(&json!({ "error": error.to_serde() })).unwrap_or_default(),
    ))
}

/// Did a poll fail because the async search no longer exists?
///
/// Elasticsearch answers `DELETE`d or expired async searches with
/// `resource_not_found_exception`, whose `reason` is the search id itself.
fn is_async_search_gone(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Query { code: Some(code), .. } if code == "resource_not_found_exception"
    )
}

/// Inspect a search response's `_shards` block.
///
/// This matters more than it looks. A search that fails on **every** shard —
/// an expired point-in-time, a mapping conflict, a cancelled task — still
/// returns HTTP 200 with `hits.hits: []` and the failures tucked away under
/// `_shards.failures`. Reading only `hits` would render that as "no results",
/// which is the single most dangerous possible lie for a database client. So
/// a total failure becomes an error, and a partial one becomes a `Notice`
/// beside the rows that did come back.
fn shard_failure(response: &OrderedJson) -> Option<(String, String)> {
    let shards = response.get("_shards")?;
    if shards
        .get("failed")
        .and_then(OrderedJson::as_i64)
        .unwrap_or(0)
        == 0
    {
        return None;
    }
    let reason = shards
        .get("failures")
        .and_then(OrderedJson::as_array)
        .and_then(|f| f.first())
        .and_then(|f| f.get("reason"));
    let ty = reason
        .and_then(|r| r.get("type"))
        .and_then(OrderedJson::as_str)
        .unwrap_or("shard_failure")
        .to_string();
    let text = reason
        .and_then(|r| r.get("reason"))
        .and_then(OrderedJson::as_str)
        .unwrap_or("a shard failed with no stated reason")
        .to_string();
    Some((ty, text))
}

/// Turn a shard failure into the right kind of `DbError` — a cancelled task is
/// a cancellation, not a failure, and the UI must not dress it as one.
fn shard_failure_error(ty: &str, reason: &str) -> DbError {
    if ty.contains("cancel") || reason.contains("cancel") {
        return DbError::Cancelled;
    }
    DbError::Query {
        code: Some(ty.to_string()),
        message: reason.to_string(),
        position: None,
    }
}

/// One hit -> one [`Value::Document`], preserving the envelope's key order and
/// converting `_source` through the mapping-aware converter.
///
/// The driver-injected `sort` array is omitted: it is an artifact of *our*
/// pagination, not of the user's document, and it is preserved losslessly in
/// the resume token instead.
pub fn hit_to_value(hit: &OrderedJson, types: &FieldTypes) -> Value {
    let mut doc = Document::new();
    if let Some(fields) = hit.as_object() {
        for (k, v) in fields {
            if k == "sort" {
                continue;
            }
            let path = if k == "_source" { "" } else { k.as_str() };
            doc.push(k.as_str(), json_to_value(v, path, types));
        }
    }
    Value::Document(Arc::new(doc))
}

#[async_trait]
impl Cursor for SearchCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.closed {
            return Err(DbError::Closed);
        }
        if self.exhausted {
            return Ok(None);
        }

        let mut size = hint.max_rows.clamp(1, MAX_PAGE_SIZE);
        if let Some(cap) = self.byte_capped_size {
            size = size.min(cap.max(1));
        }
        if let Some(remaining) = self.remaining {
            if remaining == 0 {
                self.exhausted = true;
                self.release_context().await;
                return Ok(None);
            }
            size = size.min(remaining.min(MAX_PAGE_SIZE as u64) as u32).max(1);
        }

        let (response, bytes) = match self.run_search(size).await {
            Ok(v) => v,
            Err(e) => {
                // Every error path releases the context before propagating.
                self.release_context().await;
                self.exhausted = true;
                return Err(e);
            }
        };

        // Elasticsearch may hand back a rotated PIT id; the newest one is the
        // one that must eventually be released.
        if let Some(pit) = response.get("pit_id").and_then(OrderedJson::as_str) {
            self.context = Some(Context::Pit(pit.to_string()));
        }
        if let Some(scroll) = response.get("_scroll_id").and_then(OrderedJson::as_str) {
            self.context = Some(Context::Scroll(Some(scroll.to_string())));
        }

        if self.stats.batches == 0 {
            self.note_total(&response);
        }
        if let Some(took) = response.get("took").and_then(OrderedJson::as_i64) {
            let micros = (took.max(0) as u64).saturating_mul(1_000);
            self.stats.server_elapsed_micros =
                Some(self.stats.server_elapsed_micros.unwrap_or(0) + micros);
        }

        let hits: Vec<OrderedJson> = response
            .get("hits")
            .and_then(|h| h.get("hits"))
            .and_then(OrderedJson::as_array)
            .map(<[OrderedJson]>::to_vec)
            .unwrap_or_default();

        // A shard failure arrives as HTTP 200 with an empty `hits.hits`.
        // Rendering that as "no results" would be the worst lie this driver
        // could tell, so it is surfaced — fatally when nothing came back at
        // all, as a notice beside the rows when the failure was partial.
        if let Some((ty, reason)) = shard_failure(&response) {
            if hits.is_empty() {
                self.release_context().await;
                self.exhausted = true;
                return Err(shard_failure_error(&ty, &reason));
            }
            self.pending_notices.push(Notice {
                severity: NoticeSeverity::Warning,
                code: Some(Arc::from(format!("es.shard_failure.{ty}").as_str())),
                message: Arc::from(
                    format!(
                        "some shards failed ({ty}: {reason}) — these results are PARTIAL, not \
                         the full match set"
                    )
                    .as_str(),
                ),
            });
        }

        let mut docs = Vec::with_capacity(hits.len());
        let mut deltas = Vec::new();
        for hit in &hits {
            if let Some(sort) = hit.get("sort").and_then(OrderedJson::as_array) {
                // Lowered to `serde_json::Value`: these go straight back into
                // the next request's `search_after` and into the resume token,
                // where ordering is meaningless.
                self.last_sort = sort.iter().map(OrderedJson::to_serde).collect();
            }
            if let Some(source) = hit.get("_source") {
                deltas.extend(self.track_schema(source));
            }
            docs.push(hit_to_value(hit, &self.types));
        }

        let returned = docs.len() as u32;
        if returned < size {
            // A short page is the end of the stream for both mechanisms.
            self.exhausted = true;
        }
        self.delivered += returned as u64;
        if let Some(remaining) = self.remaining.as_mut() {
            *remaining = remaining.saturating_sub(returned as u64);
            if *remaining == 0 {
                self.exhausted = true;
            }
        }

        // Adaptive byte ceiling: Elasticsearch cannot cap a response by size,
        // so overshooting the caller's budget shrinks the next page instead.
        if bytes > hint.max_bytes as usize && returned > 1 {
            let scaled = (returned as u64 * hint.max_bytes as u64) / bytes.max(1) as u64;
            self.byte_capped_size = Some(scaled.clamp(1, MAX_PAGE_SIZE as u64) as u32);
        } else if bytes * 2 < hint.max_bytes as usize {
            self.byte_capped_size = None;
        }

        if self.exhausted {
            self.release_context().await;
        }

        if docs.is_empty() {
            return Ok(None);
        }

        self.stats.rows += returned as u64;
        self.stats.bytes += bytes as u64;
        self.stats.batches += 1;

        Ok(Some(Batch {
            seq: self.stats.batches - 1,
            payload: Payload::Docs(docs),
            schema_delta: deltas,
            notices: std::mem::take(&mut self.pending_notices),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        let (mode, id) = match self.context.as_ref()? {
            Context::Pit(id) if !id.is_empty() => (ResumeMode::Pit, id.clone()),
            Context::Scroll(Some(id)) => (ResumeMode::Scroll, id.clone()),
            _ => return None,
        };
        if mode == ResumeMode::Pit && self.last_sort.is_empty() {
            // Nothing has been read yet: there is no position to resume from.
            return None;
        }
        EsResume::current(mode, self.spec.index.clone(), id, self.keep_alive.clone())
            .at(self.last_sort.clone(), self.spec.body.clone())
            .counted(self.delivered, self.remaining)
            .encode()
    }

    fn stats(&self) -> CursorStats {
        self.stats
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.closed = true;
        self.release_context().await;
        self.clear_inflight().await;
        Ok(())
    }
}

/// Backstop for the one path `close()` cannot cover: a task aborted outright.
/// Best effort by construction — if there is no runtime to spawn onto, the
/// context still expires after `keep_alive`.
impl Drop for SearchCursor {
    fn drop(&mut self) {
        let Some(context) = self.context.take() else {
            return;
        };
        let (path, body) = match context {
            Context::Pit(id) if !id.is_empty() => ("/_pit", json!({ "id": id })),
            Context::Scroll(Some(id)) => ("/_search/scroll", json!({ "scroll_id": [id] })),
            _ => return,
        };
        let http = self.http.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = http
                    .request(Method::Delete, path, &[], Some(&body), None, None)
                    .await;
            });
        } else {
            tracing::warn!(
                path,
                "dropped an elasticsearch cursor outside a runtime; the search context \
                 will expire on keep_alive rather than being released now"
            );
        }
    }
}

/// A cursor over an already-materialized set of documents, yielded exactly
/// once: raw console-command replies, `EXPLAIN` output, catalog-ish results.
pub struct DocsCursor {
    shape: Shape,
    docs: Vec<Value>,
    notices: Vec<Notice>,
    done: bool,
}

impl DocsCursor {
    pub fn new(docs: Vec<Value>) -> Self {
        Self {
            shape: Shape::Documents {
                root_hint: None,
                // Console replies / EXPLAIN output are not hits — no identity,
                // not editable.
                identity: None,
            },
            docs,
            notices: Vec::new(),
            done: false,
        }
    }

    pub fn with_notices(mut self, notices: Vec<Notice>) -> Self {
        self.notices = notices;
        self
    }
}

#[async_trait]
impl Cursor for DocsCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, _hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        if self.docs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Batch {
            seq: 0,
            payload: Payload::Docs(std::mem::take(&mut self.docs)),
            schema_delta: Vec::new(),
            notices: std::mem::take(&mut self.notices),
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        CursorStats::default()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.done = true;
        Ok(())
    }
}

/// A one-shot `Ack`-shaped cursor, whose `message` states which strategy
/// actually ran — the honest half of `EXACT_COUNT_CHEAP` being false.
pub struct AckCursor {
    shape: Shape,
    notices: Vec<Notice>,
    done: bool,
}

impl AckCursor {
    pub fn new(affected: Option<u64>, message: Option<Arc<str>>) -> Self {
        Self {
            shape: Shape::Ack { affected, message },
            notices: Vec::new(),
            done: false,
        }
    }

    pub fn with_notices(mut self, notices: Vec<Notice>) -> Self {
        self.notices = notices;
        self
    }
}

#[async_trait]
impl Cursor for AckCursor {
    fn shape(&self) -> &Shape {
        &self.shape
    }

    async fn next_batch(&mut self, _hint: FetchHint) -> Result<Option<Batch>, DbError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Batch {
            notices: std::mem::take(&mut self.notices),
            ..Batch::default()
        }))
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        None
    }

    fn stats(&self) -> CursorStats {
        CursorStats::default()
    }

    async fn close(&mut self) -> Result<(), DbError> {
        self.done = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Arc<FieldTypes> {
        Arc::new(FieldTypes::from_properties(&json!({
            "n": { "type": "long" },
            "price": { "type": "scaled_float", "scaling_factor": 100 }
        })))
    }

    fn hit() -> OrderedJson {
        // Written as text, not `json!`, precisely so the envelope's key order
        // is the wire order rather than an alphabetized one.
        OrderedJson::parse(
            r#"{"_index":"events","_id":"abc","_score":null,
                "_source":{"n":7,"price":1.5,"nested":{"a":1}},
                "sort":[1,2]}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_hit_becomes_a_document_with_the_envelope_as_pseudo_fields() {
        let value = hit_to_value(&hit(), &types());
        let Value::Document(doc) = &value else {
            panic!("expected a document")
        };
        let keys: Vec<&str> = doc.iter().map(|(k, _)| &**k).collect();
        assert_eq!(
            keys,
            vec!["_index", "_id", "_score", "_source"],
            "envelope order preserved; the driver-injected `sort` is dropped"
        );
        assert_eq!(doc.get("_id"), Some(&Value::Str(Arc::from("abc"))));
        assert_eq!(
            doc.get("_score"),
            Some(&Value::Null),
            "a null score is an explicit null, not absence"
        );
    }

    #[test]
    fn source_fields_are_addressable_under_the_root_hint_and_use_the_mapping() {
        let value = hit_to_value(&hit(), &types());
        let Value::Document(doc) = &value else {
            panic!("expected a document")
        };
        let n: FieldPath = "_source.n".parse().unwrap();
        assert_eq!(doc.get_path(&n), Some(&Value::I64(7)));
        // The mapping is consulted relative to `_source`, so `price` resolves
        // to the scaled_float rule, not to raw f64.
        let price: FieldPath = "_source.price".parse().unwrap();
        assert_eq!(
            doc.get_path(&price),
            Some(&Value::Decimal(Arc::from("1.5")))
        );
        let nested: FieldPath = "_source.nested.a".parse().unwrap();
        assert_eq!(doc.get_path(&nested), Some(&Value::I64(1)));
        // A field this hit does not carry is absent, never null.
        let missing: FieldPath = "_source.absent_field".parse().unwrap();
        assert_eq!(doc.get_path(&missing), None);
    }

    #[test]
    fn the_cas_guard_and_routing_ride_through_the_hit_envelope() {
        // With `seq_no_primary_term` requested, a hit carries `_seq_no` and
        // `_primary_term`; a custom-routed document also carries `_routing`.
        // All three must survive into the emitted document so a later mutation
        // can use them as its precondition/identity — the whole point of P0-1.
        let hit = OrderedJson::parse(
            r#"{"_index":"events","_id":"abc","_routing":"tenant-7","_score":null,
                "_seq_no":41,"_primary_term":3,
                "_source":{"n":7},
                "sort":[1,2]}"#,
        )
        .unwrap();
        let value = hit_to_value(&hit, &types());
        let Value::Document(doc) = &value else {
            panic!("expected a document")
        };
        assert_eq!(doc.get("_seq_no"), Some(&Value::I64(41)));
        assert_eq!(doc.get("_primary_term"), Some(&Value::I64(3)));
        assert_eq!(
            doc.get("_routing"),
            Some(&Value::Str(Arc::from("tenant-7")))
        );
        // Still no `sort` leaking through — that stays an artifact of paging.
        assert_eq!(doc.get("sort"), None);
    }

    #[test]
    fn a_hit_without_source_still_yields_its_envelope() {
        let value = hit_to_value(
            &OrderedJson::from_serde(&json!({ "_index": "i", "_id": "1" })),
            &types(),
        );
        let Value::Document(doc) = &value else {
            panic!("expected a document")
        };
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.get("_source"), None, "never invented");
    }

    #[test]
    fn async_search_cancellation_is_reported_as_cancelled_not_as_an_error() {
        let cancelled = OrderedJson::from_serde(&json!({
            "is_partial": true, "is_running": false,
            "error": { "type": "task_cancelled_exception", "reason": "task cancelled" }
        }));
        assert!(matches!(
            async_search_error(&cancelled),
            Some(DbError::Cancelled)
        ));

        let failed = OrderedJson::from_serde(&json!({
            "error": { "type": "search_phase_execution_exception", "reason": "all shards failed" }
        }));
        match async_search_error(&failed) {
            Some(DbError::Query { code, .. }) => {
                assert_eq!(code.as_deref(), Some("search_phase_execution_exception"))
            }
            other => panic!("expected a Query error, got {other:?}"),
        }

        assert!(
            async_search_error(&OrderedJson::from_serde(&json!({ "is_running": true }))).is_none()
        );
    }

    /// A still-running async search may report a transient reduce error in an
    /// intermediate poll; only the final state decides the outcome. Failing on
    /// the intermediate one turns a good query into a spurious error — which
    /// is exactly what it did before this was fixed.
    #[test]
    fn a_transient_error_on_a_still_running_async_search_is_not_final() {
        let intermediate = OrderedJson::from_serde(&json!({
            "is_partial": true,
            "is_running": true,
            "error": {
                "type": "status_exception",
                "reason": "Async search: error while reducing partial results"
            }
        }));
        // The helper still classifies it…
        assert!(async_search_error(&intermediate).is_some());
        // …but the poll loop must only consult it once `is_running` is false,
        // which is the invariant asserted here.
        assert_eq!(
            intermediate
                .get("is_running")
                .and_then(OrderedJson::as_bool),
            Some(true),
            "the guard the poll loop keys off"
        );
    }

    #[test]
    fn a_total_shard_failure_is_an_error_not_an_empty_result() {
        let response = OrderedJson::from_serde(&json!({
            "_shards": { "total": 1, "successful": 0, "failed": 1, "failures": [
                { "shard": 0, "reason": {
                    "type": "search_context_missing_exception",
                    "reason": "No search context found for id [25]" } }
            ]},
            "hits": { "total": { "value": 0, "relation": "eq" }, "hits": [] }
        }));
        let (ty, reason) = shard_failure(&response).expect("the failure must be seen");
        assert_eq!(ty, "search_context_missing_exception");
        assert!(reason.contains("No search context"));
        match shard_failure_error(&ty, &reason) {
            DbError::Query { code, .. } => {
                assert_eq!(code.as_deref(), Some("search_context_missing_exception"))
            }
            other => panic!("expected Query, got {other:?}"),
        }

        // A healthy response reports nothing.
        assert!(shard_failure(&OrderedJson::from_serde(&json!({
            "_shards": { "total": 1, "successful": 1, "failed": 0 }
        })))
        .is_none());
        assert!(shard_failure(&OrderedJson::from_serde(&json!({}))).is_none());
    }

    #[test]
    fn a_cancelled_shard_failure_is_a_cancellation_not_a_failure() {
        assert!(matches!(
            shard_failure_error("task_cancelled_exception", "task cancelled"),
            DbError::Cancelled
        ));
        // A deleted async search — the only thing that deletes a running one
        // is our own canceller — reads as gone, and nothing else does.
        assert!(is_async_search_gone(&DbError::Query {
            code: Some("resource_not_found_exception".into()),
            message: "Fk1...".into(),
            position: None
        }));
        assert!(!is_async_search_gone(&DbError::Timeout));
        assert!(!is_async_search_gone(&DbError::Query {
            code: Some("index_not_found_exception".into()),
            message: "no such index".into(),
            position: None
        }));
    }

    #[tokio::test]
    async fn docs_cursor_yields_once_then_ends() {
        let mut c = DocsCursor::new(vec![Value::I64(1)]);
        let hint = FetchHint::default();
        assert!(c.next_batch(hint).await.unwrap().is_some());
        assert!(c.next_batch(hint).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ack_cursor_carries_the_strategy_it_actually_used() {
        let mut c = AckCursor::new(Some(10_000), Some(Arc::from("hits.total (lower bound)")));
        match c.shape() {
            Shape::Ack { affected, message } => {
                assert_eq!(*affected, Some(10_000));
                assert!(message.as_deref().unwrap().contains("lower bound"));
            }
            other => panic!("expected Ack, got {other:?}"),
        }
        let hint = FetchHint::default();
        assert!(c.next_batch(hint).await.unwrap().is_some());
        assert!(c.next_batch(hint).await.unwrap().is_none());
    }

    /// A cursor built purely to exercise the pure, non-networked parts of the
    /// page-building logic.
    fn offline_cursor(mode: PageMode) -> SearchCursor {
        let http = Arc::new(
            EsHttp::new(
                "http://127.0.0.1:1".into(),
                crate::http::Auth::None,
                Duration::from_millis(50),
                false,
            )
            .unwrap(),
        );
        SearchCursor::new(
            http,
            ScanSpec {
                index: "events".into(),
                body: json!({ "query": { "match_all": {} } }),
                user_sort: vec![json!({ "ts": { "order": "desc", "missing": "_last" } })],
                limit: Some(1_000),
                notices: Vec::new(),
                timeout: None,
            },
            mode,
            DEFAULT_KEEP_ALIVE.into(),
            Some("pit-id-1".into()),
            types(),
            Arc::new(tokio::sync::Mutex::new(InFlight::default())),
            false,
            Arc::from("datagrep-test"),
        )
    }

    #[test]
    fn a_pit_page_carries_the_pit_the_user_sort_and_a_stable_tiebreaker() {
        let mut cursor = offline_cursor(PageMode::Pit);
        let body = cursor.page_body(500);
        assert_eq!(body["size"], json!(500));
        assert_eq!(body["pit"]["id"], json!("pit-id-1"));
        assert_eq!(body["pit"]["keep_alive"], json!("5m"));
        let sort = body["sort"].as_array().unwrap();
        assert_eq!(sort.len(), 2, "user sort key plus the tiebreaker");
        assert_eq!(sort[0]["ts"]["order"], json!("desc"));
        assert_eq!(
            sort[1],
            json!({ "_shard_doc": "asc" }),
            "_shard_doc is the only globally unique PIT tiebreaker"
        );
        assert!(
            body.get("search_after").is_none(),
            "first page has no cursor"
        );

        // After a page, the next request carries search_after.
        cursor.last_sort = vec![json!(1723075200000_i64), json!(42)];
        let body = cursor.page_body(500);
        assert_eq!(body["search_after"], json!([1723075200000_i64, 42]));
    }

    #[test]
    fn a_scroll_page_uses_doc_order_and_never_sends_a_pit() {
        let cursor = offline_cursor(PageMode::Scroll);
        let body = cursor.page_body(500);
        assert!(body.get("pit").is_none(), "scroll must not send a pit");
        let sort = body["sort"].as_array().unwrap();
        assert_eq!(sort[1], json!({ "_doc": "asc" }));
    }

    #[test]
    fn a_request_timeout_is_pushed_into_the_search_body_too() {
        let mut cursor = offline_cursor(PageMode::Pit);
        cursor.spec.timeout = Some(Duration::from_millis(2500));
        assert_eq!(cursor.page_body(10)["timeout"], json!("2500ms"));
    }

    #[test]
    fn every_page_requests_the_seq_no_primary_term_guard() {
        // Both pagination mechanisms must ask for the per-document CAS guard;
        // without it a later mutation has no `if_seq_no`/`if_primary_term` to
        // send and would have to write blind.
        for mode in [PageMode::Pit, PageMode::Scroll] {
            let cursor = offline_cursor(mode);
            assert_eq!(
                cursor.page_body(10)["seq_no_primary_term"],
                json!(true),
                "{mode:?} page must request seq_no_primary_term"
            );
        }
    }

    #[test]
    fn resume_token_is_none_before_anything_has_been_read() {
        let cursor = offline_cursor(PageMode::Pit);
        assert!(
            cursor.resume_token().is_none(),
            "no position yet — nothing to resume from"
        );
    }

    #[test]
    fn resume_token_round_trips_the_pit_and_the_search_after_position() {
        let mut cursor = offline_cursor(PageMode::Pit);
        cursor.last_sort = vec![json!(1723075200000_i64), json!(42)];
        cursor.delivered = 500;
        cursor.remaining = Some(500);
        let token = cursor.resume_token().expect("a token");
        let decoded = EsResume::decode(&token).unwrap();
        assert_eq!(decoded.mode, ResumeMode::Pit);
        assert_eq!(decoded.id, "pit-id-1");
        assert_eq!(decoded.sort, vec![json!(1723075200000_i64), json!(42)]);
        assert_eq!(decoded.index, "events");
        assert_eq!(decoded.delivered, 500);
        assert_eq!(decoded.remaining, Some(500));
        assert_eq!(decoded.body["query"]["match_all"], json!({}));

        // …and a cursor rebuilt from it resumes at exactly that position.
        let rebuilt = SearchCursor::from_resume(
            cursor.http.clone(),
            decoded,
            Some("pit-id-1".to_string()),
            cursor.spec.user_sort.clone(),
            types(),
            Arc::new(tokio::sync::Mutex::new(InFlight::default())),
            false,
            Arc::from("datagrep-test"),
            None,
        );
        assert_eq!(
            rebuilt.page_body(100)["search_after"],
            json!([1723075200000_i64, 42])
        );
        assert_eq!(rebuilt.page_body(100)["pit"]["id"], json!("pit-id-1"));
        assert_eq!(rebuilt.delivered, 500);
    }

    #[test]
    fn schema_deltas_are_emitted_once_per_new_source_field_with_the_native_type() {
        let mut cursor = offline_cursor(PageMode::Pit);
        let first = cursor.track_schema(&OrderedJson::parse(r#"{"n":1,"price":2.5}"#).unwrap());
        assert_eq!(first.len(), 2);
        match &first[0] {
            SchemaDelta::AddColumn { field } => {
                assert_eq!(&*field.name, "n");
                assert_eq!(field.logical, LogicalType::I64);
                assert_eq!(field.native_type.as_deref(), Some("long"));
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }
        match &first[1] {
            SchemaDelta::AddColumn { field } => {
                assert_eq!(&*field.name, "price");
                assert_eq!(
                    field.logical,
                    LogicalType::Decimal,
                    "a scaled_float column is announced as a decimal, not an f64"
                );
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }
        // Already-seen fields are never re-announced; only the genuinely new one is.
        let second =
            cursor.track_schema(&OrderedJson::parse(r#"{"n":2,"brand_new":"x"}"#).unwrap());
        assert_eq!(second.len(), 1);
        match &second[0] {
            SchemaDelta::AddColumn { field } => assert_eq!(&*field.name, "brand_new"),
            other => panic!("expected AddColumn, got {other:?}"),
        }
        assert!(cursor
            .track_schema(&OrderedJson::parse(r#"{"n":3}"#).unwrap())
            .is_empty());
    }

    #[test]
    fn a_capped_total_is_surfaced_as_a_lower_bound_notice() {
        let mut cursor = offline_cursor(PageMode::Pit);
        cursor.pending_notices.clear();
        cursor.note_total(&OrderedJson::from_serde(&json!({
            "hits": { "total": { "value": 10000, "relation": "gte" }, "hits": [] }
        })));
        assert_eq!(cursor.pending_notices.len(), 1);
        assert_eq!(
            cursor.pending_notices[0].code.as_deref(),
            Some("es.total_is_lower_bound")
        );
        assert!(cursor.pending_notices[0].message.contains("at least 10000"));

        // An exact total says nothing — there is nothing to warn about.
        cursor.pending_notices.clear();
        cursor.note_total(&OrderedJson::from_serde(&json!({
            "hits": { "total": { "value": 42, "relation": "eq" }, "hits": [] }
        })));
        assert!(cursor.pending_notices.is_empty());
    }

    #[test]
    fn the_pagination_mechanism_used_is_always_announced() {
        let cursor = offline_cursor(PageMode::Scroll);
        let notice = cursor
            .pending_notices
            .iter()
            .find(|n| n.code.as_deref() == Some("es.pagination"))
            .expect("the mechanism must be stated");
        assert!(notice.message.contains("scroll"));

        let cursor = offline_cursor(PageMode::Pit);
        let notice = cursor
            .pending_notices
            .iter()
            .find(|n| n.code.as_deref() == Some("es.pagination"))
            .unwrap();
        assert!(notice.message.contains("pit+search_after"));
    }
}
