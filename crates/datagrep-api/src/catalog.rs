use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::driver::ResumeToken;
use crate::error::DbError;
use crate::shape::{LogicalType, ObjectPath, RowSchema};

#[async_trait]
pub trait Catalog: Send + Sync {
    fn levels(&self) -> Vec<LevelDef>;

    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError>;

    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError>;

    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError>;

    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelDef {
    pub name: Arc<str>,
    pub kind: ObjectKind,
    pub enumeration: Enumeration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Enumeration {
    Cheap,
    ScanOnly { requires_prefix: bool },
    Paged,
    OnDemand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectNode {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    pub has_children: bool,
    pub comment: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectDetail {
    pub node: ObjectNode,
    pub schema: Option<RowSchema>,
    pub extra: Vec<(Arc<str>, Arc<str>)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<ResumeToken>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListOpts {
    pub prefix: Option<Arc<str>>,
    pub limit: u32,
    pub resume: Option<ResumeToken>,
}

impl Default for ListOpts {
    fn default() -> Self {
        Self {
            prefix: None,
            limit: 200,
            resume: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InferredSchema {
    pub sampled: u64,
    pub root: Vec<(Arc<str>, FieldTrie)>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FieldTrie {
    pub present: u64,
    pub types: Vec<(LogicalType, u64)>,
    pub children: Vec<(Arc<str>, FieldTrie)>,
}

impl FieldTrie {
    pub fn record(&mut self, ty: LogicalType) {
        self.present += 1;
        match self.types.iter_mut().find(|(t, _)| *t == ty) {
            Some((_, n)) => *n += 1,
            None => self.types.push((ty, 1)),
        }
    }

    pub fn presence_ratio(&self, parent_present: u64) -> f64 {
        if parent_present == 0 {
            0.0
        } else {
            self.present as f64 / parent_present as f64
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionCtx {
    pub text: Arc<str>,
    pub offset: u32,
    pub scope: Option<ObjectPath>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub label: Arc<str>,
    pub kind: ObjectKind,
    pub detail: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Database,
    Schema,
    Table,
    View,
    Collection,
    Column,
    Field,
    Index,
    Key,
    Function,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn page_round_trips_through_serde() {
        let page = Page {
            items: vec![ObjectNode {
                path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                kind: ObjectKind::Table,
                has_children: true,
                comment: None,
            }],
            next: Some(ResumeToken(Bytes::from_static(b"page-2"))),
        };
        let json = serde_json::to_string(&page).unwrap();
        let back: Page<ObjectNode> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, page);
    }

    #[test]
    fn field_trie_records_types_and_presence() {
        let mut trie = FieldTrie::default();
        trie.record(LogicalType::Str);
        trie.record(LogicalType::Str);
        trie.record(LogicalType::I64); // heterogeneous — stays visible
        assert_eq!(trie.present, 3);
        assert_eq!(
            trie.types,
            vec![(LogicalType::Str, 2), (LogicalType::I64, 1)]
        );
        assert!((trie.presence_ratio(4) - 0.75).abs() < f64::EPSILON);
        assert_eq!(FieldTrie::default().presence_ratio(0), 0.0);
    }

    #[test]
    fn list_opts_default_is_bounded() {
        assert!(
            ListOpts::default().limit > 0,
            "a listing is never unbounded"
        );
    }
}
