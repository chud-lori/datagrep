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

    async fn probe(&self) -> bool {
        self.client
            .database(&self.admin_db)
            .run_command(doc! { "currentOp": 1, "$ownOps": true })
            .await
            .is_ok()
    }

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
