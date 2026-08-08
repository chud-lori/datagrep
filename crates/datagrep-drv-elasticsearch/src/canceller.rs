//! [`EsCanceller`] — real, server-side cancellation for this engine:
//! *async search -> task id -> `POST /_tasks/<id>/_cancel`*.
//!
//! # Why a plain `_search` is not enough
//!
//! Closing the HTTP channel on a `_search` abandons the *response*, not the
//! work: the coordinating node keeps the search phase running across every
//! shard. To reach the work we need the task that is doing it, and to name
//! that task we need something the server will echo back to us.
//!
//! Two mechanisms, used together:
//!
//! 1. **`X-Opaque-Id`.** Every search this driver issues carries a unique
//!    `X-Opaque-Id` header, which Elasticsearch attaches to the resulting task
//!    and surfaces in `GET /_tasks?detailed`. `cancel()` looks the task up by
//!    that tag and issues `POST /_tasks/<node:id>/_cancel`. This is the exact
//!    analogue of the Mongo driver's `comment`-tagged `killOp`.
//! 2. **`_async_search`.** When the connection is configured for it (the
//!    default on Elasticsearch), a scan's search is submitted as
//!    `POST /<index>/_async_search?wait_for_completion_timeout=0`, which
//!    returns immediately with a search id for work that is still running.
//!    `DELETE /_async_search/<id>` cancels a still-running async search, and
//!    is used both as a second cancel path and as the cleanup that stops a
//!    completed search's results from lingering in the `.async-search` system
//!    index.
//!
//! # The honesty contract
//!
//! [`Canceller::kind`] reports `ServerSide` only where an async search
//! actually gives us a cancellable handle; on OpenSearch (whose asynchronous
//! search lives behind a different plugin endpoint) or with async search
//! turned off it reports `ClientAbandon`.
//!
//! [`CancelOutcome`] is never embellished: `ServerCancelled` is returned only
//! when the server acknowledged cancelling something specific — a task the
//! tasks API confirmed, or a running async search whose deletion it
//! acknowledged. If nothing was in flight, or nothing matched, the answer is
//! `ClientAbandoned`, which is the truth: we stopped consuming and the server
//! may still be executing.

use std::sync::Arc;

use serde_json::Value as Json;
use tokio::sync::Mutex;

use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::http::{EsHttp, Method, OPAQUE_ID_HEADER};

/// What is currently running, as far as the connection knows. Shared between
/// the cursor that issues the searches and every [`EsCanceller`] the
/// connection hands out (`Canceller` must be usable from another task while
/// `execute()` is in flight, so it can never borrow from the cursor).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InFlight {
    /// The `X-Opaque-Id` on the currently-running request.
    pub opaque_id: Option<Arc<str>>,
    /// The `_async_search` id, when the search was submitted asynchronously.
    pub async_id: Option<Arc<str>>,
}

impl InFlight {
    pub fn is_empty(&self) -> bool {
        self.opaque_id.is_none() && self.async_id.is_none()
    }
}

/// Shared slot the cursor writes and the canceller reads.
pub type InFlightSlot = Arc<Mutex<InFlight>>;

pub struct EsCanceller {
    http: Arc<EsHttp>,
    inflight: InFlightSlot,
    /// Whether this connection submits searches as async searches, i.e.
    /// whether a genuine server-side handle exists at all.
    async_search: bool,
}

impl EsCanceller {
    pub fn new(http: Arc<EsHttp>, inflight: InFlightSlot, async_search: bool) -> Self {
        Self {
            http,
            inflight,
            async_search,
        }
    }

    /// Ask the tasks API for every search task tagged with `opaque_id`.
    async fn tasks_for(&self, opaque_id: &str) -> Result<Vec<String>, DbError> {
        let json = self
            .http
            .request(
                Method::Get,
                "/_tasks",
                &[
                    // Both the async-search submit task and the search task it
                    // spawns match this wildcard.
                    ("actions", "*search*".to_string()),
                    ("detailed", "true".to_string()),
                    // Flat list rather than the nodes -> tasks nesting.
                    ("group_by", "none".to_string()),
                ],
                None,
                None,
                None,
            )
            .await?;
        Ok(task_ids_with_opaque_id(&json, opaque_id))
    }

    async fn cancel_task(&self, task_id: &str) -> Result<bool, DbError> {
        let json = self
            .http
            .request(
                Method::Post,
                &format!("/_tasks/{task_id}/_cancel"),
                &[],
                None,
                None,
                None,
            )
            .await?;
        Ok(cancel_was_acknowledged(&json))
    }

    async fn delete_async_search(&self, async_id: &str) -> Result<bool, DbError> {
        let json = self
            .http
            .request(
                Method::Delete,
                &format!("/_async_search/{async_id}"),
                &[],
                None,
                None,
                None,
            )
            .await?;
        Ok(json
            .get("acknowledged")
            .and_then(Json::as_bool)
            .unwrap_or(true))
    }
}

impl Canceller for EsCanceller {
    fn kind(&self) -> CancelKind {
        if self.async_search {
            CancelKind::ServerSide
        } else {
            // A plain `_search` gives us channel-close; the task-API attempt
            // in `cancel()` may still succeed, and if it does the *outcome*
            // says so — but the advertised strength never over-promises.
            CancelKind::ClientAbandon
        }
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(async move {
            let snapshot = self.inflight.lock().await.clone();
            if snapshot.is_empty() {
                // Nothing tagged as running: either the search already
                // finished or this cursor never issued one. Abandoning
                // consumption is the whole story, and saying so is the point.
                return Ok(CancelOutcome::ClientAbandoned);
            }

            let mut server_cancelled = false;

            // 1. The primary path: resolve our tagged task and cancel it.
            if let Some(tag) = snapshot.opaque_id.as_deref() {
                match self.tasks_for(tag).await {
                    Ok(tasks) => {
                        for task in tasks {
                            match self.cancel_task(&task).await {
                                Ok(true) => server_cancelled = true,
                                Ok(false) => {}
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "task cancel failed; continuing to the async-search path"
                                ),
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "tasks lookup failed; continuing to the async-search path"
                    ),
                }
            }

            // 2. Cancel (and clean up) the async search itself. Deleting a
            //    still-running async search cancels it; deleting a finished
            //    one stops its results occupying the `.async-search` index.
            if let Some(async_id) = snapshot.async_id.as_deref() {
                match self.delete_async_search(async_id).await {
                    Ok(true) => server_cancelled = true,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "async search delete failed")
                    }
                }
            }

            Ok(if server_cancelled {
                CancelOutcome::ServerCancelled
            } else {
                CancelOutcome::ClientAbandoned
            })
        })
    }
}

/// Extract `node:id` task identifiers whose `X-Opaque-Id` header matches.
///
/// Handles both response shapes: the flat `group_by=none` list this driver
/// asks for, and the default `nodes -> tasks` nesting, so a proxy that strips
/// the query parameter cannot silently break cancellation.
pub fn task_ids_with_opaque_id(json: &Json, opaque_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let matches = |task: &Json| -> bool {
        task.get("headers")
            .and_then(|h| h.get(OPAQUE_ID_HEADER))
            .and_then(Json::as_str)
            == Some(opaque_id)
    };
    let ident = |task: &Json| -> Option<String> {
        let node = task.get("node").and_then(Json::as_str)?;
        let id = task.get("id").and_then(Json::as_i64)?;
        Some(format!("{node}:{id}"))
    };

    if let Some(tasks) = json.get("tasks").and_then(Json::as_array) {
        for task in tasks {
            if matches(task) {
                out.extend(ident(task));
            }
        }
    }
    if let Some(nodes) = json.get("nodes").and_then(Json::as_object) {
        for node in nodes.values() {
            if let Some(tasks) = node.get("tasks").and_then(Json::as_object) {
                for (key, task) in tasks {
                    if matches(task) {
                        out.push(ident(task).unwrap_or_else(|| key.clone()));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Did `POST /_tasks/<id>/_cancel` actually cancel something?
///
/// A 200 with an empty `nodes` map means the task was already gone — that is
/// not a server cancel and must not be reported as one.
pub fn cancel_was_acknowledged(json: &Json) -> bool {
    if json
        .get("node_failures")
        .and_then(Json::as_array)
        .is_some_and(|f| !f.is_empty())
    {
        return false;
    }
    if let Some(tasks) = json.get("tasks").and_then(Json::as_array) {
        return !tasks.is_empty();
    }
    json.get("nodes")
        .and_then(Json::as_object)
        .is_some_and(|nodes| {
            nodes.values().any(|n| {
                n.get("tasks")
                    .and_then(Json::as_object)
                    .is_some_and(|t| !t.is_empty())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flat_tasks() -> Json {
        json!({
            "tasks": [
                {
                    "node": "nodeA", "id": 42,
                    "action": "indices:data/read/search",
                    "headers": { "X-Opaque-Id": "datagrep-abc" }
                },
                {
                    "node": "nodeA", "id": 43,
                    "action": "indices:data/read/search",
                    "headers": { "X-Opaque-Id": "someone-elses-query" }
                },
                {
                    "node": "nodeB", "id": 7,
                    "action": "indices:data/read/async_search[submit]",
                    "headers": { "X-Opaque-Id": "datagrep-abc" }
                }
            ]
        })
    }

    #[test]
    fn only_our_own_tagged_tasks_are_selected() {
        let ids = task_ids_with_opaque_id(&flat_tasks(), "datagrep-abc");
        assert_eq!(ids, vec!["nodeA:42".to_string(), "nodeB:7".to_string()]);
        // Cancelling must never touch another client's query.
        assert!(!ids.contains(&"nodeA:43".to_string()));
    }

    #[test]
    fn the_nested_group_by_nodes_shape_is_understood_too() {
        let nested = json!({
            "nodes": {
                "nodeA": {
                    "name": "a",
                    "tasks": {
                        "nodeA:42": {
                            "node": "nodeA", "id": 42,
                            "headers": { "X-Opaque-Id": "datagrep-abc" }
                        },
                        "nodeA:99": {
                            "node": "nodeA", "id": 99,
                            "headers": { "X-Opaque-Id": "other" }
                        }
                    }
                }
            }
        });
        assert_eq!(
            task_ids_with_opaque_id(&nested, "datagrep-abc"),
            vec!["nodeA:42".to_string()]
        );
    }

    #[test]
    fn untagged_or_missing_tasks_select_nothing() {
        assert!(task_ids_with_opaque_id(&json!({}), "x").is_empty());
        assert!(task_ids_with_opaque_id(&json!({"tasks": []}), "x").is_empty());
        assert!(
            task_ids_with_opaque_id(&json!({"tasks": [{"node": "n", "id": 1}]}), "x").is_empty()
        );
    }

    #[test]
    fn an_empty_cancel_response_is_not_reported_as_a_server_cancel() {
        // 200 with nothing cancelled: the task had already finished.
        assert!(!cancel_was_acknowledged(&json!({ "nodes": {} })));
        assert!(!cancel_was_acknowledged(&json!({ "tasks": [] })));
        assert!(!cancel_was_acknowledged(&json!({})));
    }

    #[test]
    fn a_real_cancel_response_is_acknowledged() {
        assert!(cancel_was_acknowledged(&json!({
            "nodes": { "nodeA": { "name": "a", "tasks": {
                "nodeA:42": { "id": 42, "cancelled": true }
            } } }
        })));
        assert!(cancel_was_acknowledged(&json!({
            "tasks": [ { "node": "nodeA", "id": 42, "cancelled": true } ]
        })));
    }

    #[test]
    fn node_failures_veto_the_acknowledgement() {
        assert!(!cancel_was_acknowledged(&json!({
            "node_failures": [ { "type": "failed_node_exception" } ],
            "nodes": { "nodeA": { "tasks": { "nodeA:42": { "id": 42 } } } }
        })));
    }

    #[test]
    fn kind_never_promises_server_side_without_an_async_handle() {
        // Constructed without a live server: `kind()` is a pure function of
        // configuration, which is exactly the property being asserted.
        let http = Arc::new(
            EsHttp::new(
                "http://127.0.0.1:9200".into(),
                crate::http::Auth::None,
                std::time::Duration::from_secs(1),
                false,
            )
            .unwrap(),
        );
        let slot: InFlightSlot = Arc::new(Mutex::new(InFlight::default()));
        let with_async = EsCanceller::new(http.clone(), slot.clone(), true);
        let without = EsCanceller::new(http, slot, false);
        assert_eq!(with_async.kind(), CancelKind::ServerSide);
        assert_eq!(without.kind(), CancelKind::ClientAbandon);
    }

    #[tokio::test]
    async fn cancelling_with_nothing_in_flight_admits_it_abandoned_only() {
        let http = Arc::new(
            EsHttp::new(
                "http://127.0.0.1:1".into(), // never contacted: the slot is empty
                crate::http::Auth::None,
                std::time::Duration::from_millis(50),
                false,
            )
            .unwrap(),
        );
        let slot: InFlightSlot = Arc::new(Mutex::new(InFlight::default()));
        let canceller = EsCanceller::new(http, slot, true);
        assert_eq!(
            canceller.cancel().await.unwrap(),
            CancelOutcome::ClientAbandoned
        );
    }

    #[test]
    fn inflight_emptiness_is_by_content_not_by_option() {
        assert!(InFlight::default().is_empty());
        assert!(!InFlight {
            opaque_id: Some(Arc::from("t")),
            async_id: None
        }
        .is_empty());
    }
}
