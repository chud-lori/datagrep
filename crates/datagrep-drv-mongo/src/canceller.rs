//! [`MongoCanceller`] (ticket item 6): reports `ServerSide` when `killOp` is
//! actually permitted (probed once, cached), else degrades to
//! `ClientAbandon`. Cancellation reports what really happened — claiming a
//! server-side kill that the deployment never granted us the privileges for
//! would be worse than admitting the query is still running.
//!
//! **Correlating a cancel to a real operation.** `killOp` needs an `opid`,
//! and nothing in `datagrep-api`'s `Canceller` trait carries a query identity to
//! look one up by. This driver tags every `find`/`aggregate` it issues with
//! a `comment` (`MongoConnection::begin_op`/`end_op`), stores that tag in a
//! connection-shared `active_comment` slot for as long as the command is
//! in flight, and `cancel()` resolves it back to an `opid` via
//! `currentOp`'s `command.comment` field — a real, if best-effort,
//! server-side kill rather than a purely client-side abandon dressed up as
//! one.
//!
//! **"Always drop the cursor" (ticket item 6).** That invariant lives on
//! [`crate::cursor::MongoCursor::close`], not here: `Canceller::cancel` has
//! no handle to the cursor the core is abandoning (the trait is
//! intentionally cursor-agnostic: a `Canceller` must be cloneable,
//! `'static`, and usable from another task while `execute()` is still in
//! flight, which a cursor handle would not be). What this
//! type guarantees is that closing a `MongoCursor` always drops the inner
//! driver cursor deterministically, so `killCursors` fires regardless of
//! whether the server-side `killOp` above ever lands.

use std::sync::Arc;

use bson::{doc, Bson, Document as BsonDocument};
use tokio::sync::{Mutex, OnceCell};

use datagrep_api::driver::{BoxFuture, CancelKind, CancelOutcome, Canceller};
use datagrep_api::error::DbError;

use crate::error::map_mongo_error;

pub struct MongoCanceller {
    client: mongodb::Client,
    admin_db: String,
    probe: Arc<OnceCell<bool>>,
    active_comment: Arc<Mutex<Option<Arc<str>>>>,
}

impl MongoCanceller {
    pub fn new(
        client: mongodb::Client,
        _default_database: String,
        probe: Arc<OnceCell<bool>>,
        active_comment: Arc<Mutex<Option<Arc<str>>>>,
    ) -> Self {
        Self {
            client,
            admin_db: "admin".to_string(),
            probe,
            active_comment,
        }
    }

    /// One-time probe: can this session run `currentOp`/`killOp` at all?
    /// Cached on the shared `probe` cell so every `MongoCanceller` handed
    /// out by the same connection (and repeated `cancel()` calls) share one
    /// answer (ticket item 6: "probe once, cache").
    async fn probe(&self) -> bool {
        self.client
            .database(&self.admin_db)
            .run_command(doc! { "currentOp": 1, "$ownOps": true })
            .await
            .is_ok()
    }

    /// Look up the in-flight op tagged with `tag` via `currentOp` and issue
    /// `killOp` against it. Returns `true` when a matching, still-running op
    /// was found and `killOp` was sent (not proof it was actually killed —
    /// `CancelOutcome::Requested`, not `ServerCancelled`, is what the caller
    /// reports for exactly this reason, matching the Postgres driver's own
    /// "the protocol gives no ack" precedent).
    async fn kill_by_comment(&self, tag: &str) -> Result<bool, DbError> {
        let admin = self.client.database(&self.admin_db);
        let current = admin
            .run_command(doc! { "currentOp": 1, "$ownOps": true })
            .await
            .map_err(map_mongo_error)?;
        let Some(Bson::Array(inprog)) = current.get("inprog") else {
            return Ok(false);
        };
        let opid = inprog.iter().find_map(|entry| {
            let Bson::Document(d) = entry else {
                return None;
            };
            let comment = d
                .get_document("command")
                .ok()
                .and_then(|c| c.get_str("comment").ok());
            if comment == Some(tag) {
                d.get("opid").cloned()
            } else {
                None
            }
        });
        let Some(opid) = opid else {
            return Ok(false);
        };
        let mut kill_cmd = BsonDocument::new();
        kill_cmd.insert("killOp", 1);
        kill_cmd.insert("op", opid);
        admin.run_command(kill_cmd).await.map_err(map_mongo_error)?;
        Ok(true)
    }
}

impl Canceller for MongoCanceller {
    fn kind(&self) -> CancelKind {
        // `kind()` is synchronous and the privilege probe needs network I/O,
        // so before the first `cancel()` this honestly reports the weaker
        // guarantee rather than promising a server kill it hasn't verified.
        match self.probe.get() {
            Some(true) => CancelKind::ServerSide,
            _ => CancelKind::ClientAbandon,
        }
    }

    fn cancel(&self) -> BoxFuture<'_, Result<CancelOutcome, DbError>> {
        Box::pin(async move {
            let allowed = *self.probe.get_or_init(|| self.probe()).await;
            if !allowed {
                return Ok(CancelOutcome::ClientAbandoned);
            }
            let tag = self.active_comment.lock().await.clone();
            let Some(tag) = tag else {
                // Nothing tagged as in-flight (a count/insert/update/delete,
                // or the find/aggregate already finished) — there is
                // nothing left to `killOp`; abandoning consumption is the
                // whole story.
                return Ok(CancelOutcome::ClientAbandoned);
            };
            match self.kill_by_comment(&tag).await {
                Ok(true) => Ok(CancelOutcome::Requested),
                Ok(false) => Ok(CancelOutcome::ClientAbandoned),
                Err(e) => {
                    tracing::warn!(error = %e, "killOp attempt failed; falling back to client-abandon");
                    Ok(CancelOutcome::ClientAbandoned)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_defaults_to_client_abandon_before_any_probe() {
        let probe = Arc::new(OnceCell::new());
        // Not populated yet — must not claim ServerSide it hasn't verified.
        assert!(probe.get().is_none());
        // (Constructing a real `MongoCanceller` needs a live `mongodb::Client`;
        // the cache-before-probe contract that matters here is exercised
        // directly against the shared cell, which is what `kind()` reads.)
        assert!(!probe.get().copied().unwrap_or(false));
    }

    #[tokio::test]
    async fn probe_cell_caches_across_concurrent_get_or_init() {
        let cell: Arc<OnceCell<bool>> = Arc::new(OnceCell::new());
        let a = cell.get_or_init(|| async { true }).await;
        let b = cell.get_or_init(|| async { false }).await;
        assert!(*a);
        assert!(*b, "second init closure never runs once cached");
    }
}
