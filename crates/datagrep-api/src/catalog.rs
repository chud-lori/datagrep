//! Lazy, incremental catalog (design §3.1, §5.1). Expand-on-demand only —
//! eager whole-catalog indexing is the incumbents' defining mistake, and
//! [`Enumeration`] is what stops a `KEYS *` because someone clicked a triangle.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::driver::ResumeToken;
use crate::error::DbError;
use crate::shape::{LogicalType, ObjectPath, RowSchema};

/// Namespace browser for one connection. Every method is on-demand and
/// cancellable; none may enumerate more than it was asked for.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// The hierarchy this engine exposes, top-down (database → schema → table).
    fn levels(&self) -> Vec<LevelDef>;

    /// One page of children under `parent`. Paged so a 100k-relation server
    /// never materializes at once.
    async fn children(
        &self,
        parent: &ObjectPath,
        opts: ListOpts,
    ) -> Result<Page<ObjectNode>, DbError>;

    /// Full detail for one object (columns, indexes, comments).
    async fn describe(&self, path: &ObjectPath) -> Result<ObjectDetail, DbError>;

    /// Sample-based schema inference for engines without a declared schema
    /// (`SCHEMA_DECLARED` off) — the honest substitute, and labeled as such.
    async fn infer_shape(
        &self,
        path: &ObjectPath,
        sample_size: u32,
    ) -> Result<InferredSchema, DbError>;

    /// Completion candidates for the editor; expected to be a bounded
    /// server-side prefix query plus whatever is already cached (§5.1).
    async fn complete(&self, ctx: CompletionCtx) -> Result<Vec<Completion>, DbError>;
}

/// One level of the catalog hierarchy and how expensive it is to list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelDef {
    /// Display name of the level ("schema", "collection", "key prefix").
    pub name: Arc<str>,
    pub kind: ObjectKind,
    pub enumeration: Enumeration,
}

/// How costly enumerating a level is — the UI adapts (auto-expand, a "Scan
/// for keys…" box, or nothing at all) instead of firing blind queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Enumeration {
    /// Effectively free: `information_schema`, `listCollections`.
    Cheap,
    /// Only via cursor scan (Redis). UI shows a scan control; with
    /// `requires_prefix` it refuses to scan without one.
    ScanOnly { requires_prefix: bool },
    /// Listing costs metered API calls (DynamoDB `ListTables`).
    Paged,
    /// Never auto-expand; the user must explicitly ask.
    OnDemand,
}

/// One node in the catalog tree — deliberately thin; detail is a separate,
/// on-demand call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectNode {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    /// Whether expanding is meaningful (drives the disclosure triangle).
    pub has_children: bool,
    pub comment: Option<Arc<str>>,
}

/// Everything we know about one object once the user actually opens it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectDetail {
    pub node: ObjectNode,
    /// Declared schema, when the engine has one (`SCHEMA_DECLARED`).
    pub schema: Option<RowSchema>,
    /// Engine-specific display facts (row estimate, size, engine) as pairs —
    /// shown, never branched on.
    pub extra: Vec<(Arc<str>, Arc<str>)>,
}

/// One page of results plus the token to fetch the next — the catalog's unit
/// of laziness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// `None` = no more pages.
    pub next: Option<ResumeToken>,
}

/// Bounds for a `children` listing; always bounded, never "everything".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListOpts {
    /// Name prefix filter; required by `ScanOnly { requires_prefix: true }`.
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

/// Result of sampling documents: a trie of observed field paths with type
/// frequencies — the honest schema for schemaless engines, and the seed for
/// the grid's `ViewProjection` (design §3.1).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InferredSchema {
    /// How many documents were sampled; every ratio is relative to this.
    pub sampled: u64,
    /// Children of the document root.
    pub root: Vec<(Arc<str>, FieldTrie)>,
}

/// Per-path statistics from sampling. Presence is counted separately from
/// types because `Absent` is not a type (design §3.1).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FieldTrie {
    /// In how many sampled parents this path was present at all.
    pub present: u64,
    /// Observed types with frequency — heterogeneous columns stay visible
    /// instead of being coerced to the majority type.
    pub types: Vec<(LogicalType, u64)>,
    /// Nested fields (for document-valued paths), insertion-ordered.
    pub children: Vec<(Arc<str>, FieldTrie)>,
}

impl FieldTrie {
    /// Record one observation of this path with the given type.
    pub fn record(&mut self, ty: LogicalType) {
        self.present += 1;
        match self.types.iter_mut().find(|(t, _)| *t == ty) {
            Some((_, n)) => *n += 1,
            None => self.types.push((ty, 1)),
        }
    }

    /// Presence ratio relative to how often the parent was seen — the ranking
    /// key for seeding view columns.
    pub fn presence_ratio(&self, parent_present: u64) -> f64 {
        if parent_present == 0 {
            0.0
        } else {
            self.present as f64 / parent_present as f64
        }
    }
}

/// Where the caret is and what surrounds it — all the catalog needs to offer
/// completions without a resident full schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionCtx {
    /// The statement text being edited.
    pub text: Arc<str>,
    /// Caret byte offset within `text`.
    pub offset: u32,
    /// Namespace scope already established (current database/schema).
    pub scope: Option<ObjectPath>,
}

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub label: Arc<str>,
    pub kind: ObjectKind,
    /// Short annotation (type, table it belongs to).
    pub detail: Option<Arc<str>>,
}

/// What kind of namespace object something is — for icons and completion
/// ranking, not for behavior branches.
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
