use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use datagrep_api::catalog::{
    Catalog, Completion, CompletionCtx, InferredSchema, LevelDef, ListOpts, ObjectDetail,
    ObjectNode, Page,
};
use datagrep_api::error::DbError;
use datagrep_api::shape::ObjectPath;

use crate::connection::Link;
use crate::manifest::EngineManifest;
use crate::wire::{path_json, ChildrenReply, DescribeReply};

pub struct SidecarCatalog {
    link: Arc<Link>,
    manifest: &'static EngineManifest,
}

impl SidecarCatalog {
    pub(crate) fn new(link: Arc<Link>, manifest: &'static EngineManifest) -> Self {
        Self { link, manifest }
    }
}

#[async_trait]
impl Catalog for SidecarCatalog {
    fn levels(&self) -> Vec<LevelDef> {
        self.manifest.level_defs()
    }

    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError> {
        let (reply, _) = self
            .link
            .call(
                "children",
                json!({
                    "path": path_json(parent),
                    "prefix": opts.prefix.as_deref(),
                    "limit": opts.limit,
                }),
            )
            .await?;
        let reply: ChildrenReply = serde_json::from_value(reply)
            .map_err(|e| DbError::Protocol(format!("bad children reply: {e}")))?;
        Ok(Page {
            items: reply.items.into_iter().map(ObjectNode::from).collect(),
            next: None,
        })
    }

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError> {
        let (reply, _) = self
            .link
            .call("describe", json!({ "path": path_json(path) }))
            .await?;
        let reply: DescribeReply = serde_json::from_value(reply)
            .map_err(|e| DbError::Protocol(format!("bad describe reply: {e}")))?;
        Ok(reply.into())
    }

    async fn infer_shape(
        &self,
        _path: &ObjectPath,
        _sample_size: u32,
    ) -> Result<InferredSchema, DbError> {
        Err(DbError::Unsupported {
            feature: "shape inference on a sidecar engine".into(),
        })
    }

    async fn complete(&self, _ctx: CompletionCtx) -> Result<Vec<Completion>, DbError> {
        Ok(Vec::new())
    }
}
