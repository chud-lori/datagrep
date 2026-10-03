use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use datagrep_api::caps::Capabilities;
use datagrep_api::catalog::Catalog;
use datagrep_api::driver::{
    Canceller, Connection, Cursor, Enforcement, ServerInfo, Transaction, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::Request;

use crate::canceller::SidecarCanceller;
use crate::catalog::SidecarCatalog;
use crate::cursor::SidecarCursor;
use crate::manifest::EngineManifest;
use crate::process::{Process, KILL_GRACE};
use crate::wire::{encode_tagged, ExecuteReply, ReadOnlyReply};

// One protocol connection inside a sidecar process.
pub(crate) struct Link {
    pub process: Arc<Process>,
    pub conn: u64,
}

impl Link {
    pub async fn call(&self, method: &str, mut params: Json) -> Result<(Json, usize), DbError> {
        params["conn"] = json!(self.conn);
        self.process.call(Some(self.conn), method, params).await
    }

    pub async fn cancel(&self) -> Result<(), DbError> {
        self.process
            .notify("cancel", json!({ "conn": self.conn }))
            .await
    }

    // A sidecar that does not settle within the grace period is killed; its connections poison.
    pub async fn settle_or_kill(&self) -> bool {
        if tokio::time::timeout(KILL_GRACE, self.process.settled(self.conn))
            .await
            .is_ok()
        {
            return true;
        }
        tracing::warn!(
            target: "sidecar",
            conn = self.conn,
            "sidecar did not settle after a cancel; killing it"
        );
        self.process.kill();
        false
    }
}

pub struct SidecarConnection {
    link: Arc<Link>,
    info: ServerInfo,
    caps: Capabilities,
    canceller: Arc<SidecarCanceller>,
    catalog: Arc<SidecarCatalog>,
}

impl SidecarConnection {
    pub(crate) fn new(
        process: Arc<Process>,
        conn: u64,
        info: ServerInfo,
        caps: Capabilities,
        manifest: &'static EngineManifest,
    ) -> Self {
        let link = Arc::new(Link { process, conn });
        Self {
            canceller: Arc::new(SidecarCanceller::new(link.clone(), caps.flags)),
            catalog: Arc::new(SidecarCatalog::new(link.clone(), manifest)),
            link,
            info,
            caps,
        }
    }
}

#[async_trait]
impl Connection for SidecarConnection {
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.link.call("ping", json!({})).await.map(|_| ())
    }

    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        let Request::Native { text, params, opts } = req else {
            return Err(DbError::Unsupported {
                feature: "generated operations on a sidecar engine".into(),
            });
        };
        let call = self.link.call(
            "execute",
            json!({
                "text": &*text,
                "params": params.iter().map(encode_tagged).collect::<Vec<_>>(),
                "timeout_ms": opts.timeout.map(|d| d.as_millis() as u64),
                "row_limit": opts.row_limit,
                "read_only_assert": opts.read_only_assert,
            }),
        );
        let (reply, _) = match opts.timeout {
            // The sidecar enforces the timeout itself; this is the backstop if it does not.
            Some(t) => match tokio::time::timeout(t + Duration::from_secs(2), call).await {
                Ok(result) => result?,
                Err(_) => {
                    let _ = self.link.cancel().await;
                    return Err(DbError::Timeout);
                }
            },
            None => call.await?,
        };
        let reply: ExecuteReply = serde_json::from_value(reply)
            .map_err(|e| DbError::Protocol(format!("bad execute reply: {e}")))?;
        Ok(Box::new(SidecarCursor::new(
            self.link.clone(),
            reply.cursor,
            reply.shape.into_shape(),
        )))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.canceller.clone()
    }

    fn catalog(&self) -> Arc<dyn Catalog> {
        self.catalog.clone()
    }

    async fn begin(&self, _opts: TxOpts) -> Result<Box<dyn Transaction>, DbError> {
        Err(DbError::Unsupported {
            feature: "transactions on a sidecar engine".into(),
        })
    }

    async fn set_read_only(&self, on: bool) -> Result<Enforcement, DbError> {
        let (reply, _) = self.link.call("set_read_only", json!({ "on": on })).await?;
        let reply: ReadOnlyReply = serde_json::from_value(reply)
            .map_err(|e| DbError::Protocol(format!("bad set_read_only reply: {e}")))?;
        Ok(reply.enforcement.into())
    }

    async fn close(&self) -> Result<(), DbError> {
        match tokio::time::timeout(KILL_GRACE, self.link.call("disconnect", json!({}))).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => {
                self.link.process.kill();
                Err(DbError::Timeout)
            }
        }
    }
}
