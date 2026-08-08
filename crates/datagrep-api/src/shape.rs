//! Result shapes. A cursor announces *what kind of thing* it
//! streams; rectangularity for the grid is computed above this seam, so
//! document stores never get forced through fake columns.

use std::fmt;
use std::sync::Arc;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::value::{FieldPath, Value};

/// What a cursor streams. `Unknown` is legal at open and is narrowed by the
/// first batch — some engines only reveal shape once data flows.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// Fixed columns: PG, MySQL, SQLite, ClickHouse.
    Table(Arc<RowSchema>),
    /// Heterogeneous documents: Mongo, ES hits, DynamoDB items. `root_hint`
    /// points at the payload root (e.g. `_source`) when the envelope is noise.
    Documents { root_hint: Option<FieldPath> },
    /// Key/value pairs: Redis SCAN, HGETALL.
    Pairs { value_kind: ValueKind },
    /// Graph results: Neo4j. Designed in from day one, not retrofitted.
    Graph(Arc<GraphSchema>),
    /// A statement acknowledgement: DDL, SET, OK.
    Ack {
        affected: Option<u64>,
        message: Option<Arc<str>>,
    },
    /// Not yet known; narrowed by the first batch.
    Unknown,
}

/// Column schema for [`Shape::Table`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowSchema {
    pub fields: Vec<FieldDef>,
    /// How a row is uniquely identified, when known. Absent identity means
    /// `EDITABLE_RESULTS` must be off — we never guess at which row to mutate.
    pub identity: Option<Identity>,
}

/// One column/field of a schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: Arc<str>,
    pub logical: LogicalType,
    pub flags: FieldFlags,
    /// The engine's own type name (`numeric(38,10)`, `jsonb`) so the inspector
    /// can always show what the server said, not what we mapped it to.
    pub native_type: Option<Arc<str>>,
}

/// Engine-neutral type of a field — the type-level mirror of [`Value`]'s
/// variants, used by schemas and inference rather than per-cell tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogicalType {
    Null,
    Bool,
    I64,
    U64,
    F64,
    Decimal,
    Str,
    Bytes,
    Date,
    Time,
    Timestamp,
    Interval,
    Uuid,
    Json,
    Array,
    Document,
    Ref,
    Geo,
    Vector,
    Unknown,
}

bitflags! {
    /// Per-field facts the UI renders (nullability badge, key icon) — flags,
    /// not driver checks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct FieldFlags: u32 {
        const NULLABLE       = 1 << 0;
        const PRIMARY_KEY    = 1 << 1;
        const UNIQUE         = 1 << 2;
        /// Server-generated (identity/auto-increment/computed) — not editable.
        const AUTO_GENERATED = 1 << 3;
        const INDEXED        = 1 << 4;
    }
}

/// Which fields identify a row. Every generated mutation targets exactly one
/// row through these; no identity, no editing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Indices into [`RowSchema::fields`].
    pub field_indices: Vec<u32>,
}

/// Dotted path naming a namespace object (`db.schema.table`, `db.collection`,
/// a Redis key prefix). The catalog's coordinate system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ObjectPath(pub Vec<Arc<str>>);

impl ObjectPath {
    pub fn new(parts: Vec<Arc<str>>) -> Self {
        Self(parts)
    }

    /// Root of the namespace (no parts).
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn parts(&self) -> &[Arc<str>] {
        &self.0
    }

    /// This path extended by one child part.
    pub fn child(&self, part: impl Into<Arc<str>>) -> Self {
        let mut parts = self.0.clone();
        parts.push(part.into());
        Self(parts)
    }
}

impl fmt::Display for ObjectPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, part) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(part)?;
        }
        Ok(())
    }
}

/// Mid-stream schema evolution. New columns append and never reorder existing
/// ones — the grid grows without refetching, and rows already drawn keep the
/// column positions the user is looking at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaDelta {
    /// A new field crossed the presence threshold; appended on the right.
    AddColumn { field: FieldDef },
    /// A field previously `Unknown` (or too wide) resolved to a concrete type.
    NarrowType { index: u32, to: LogicalType },
}

/// Placeholder graph schema so [`Shape::Graph`] exists from day one; fleshed
/// out when a graph engine lands (deferred past 1.0).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GraphSchema {
    pub node_labels: Vec<Arc<str>>,
    pub edge_types: Vec<Arc<str>>,
}

/// Placeholder chunk of graph data streamed by a graph cursor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphChunk {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
}

/// What the value side of a [`Shape::Pairs`] stream holds (Redis value types,
/// generalized) — lets the UI pick a truthful renderer per key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ValueKind {
    Unknown,
    Bytes,
    Str,
    List,
    Set,
    SortedSet,
    Hash,
    Stream,
    Document,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_path_display_is_dotted() {
        let p = ObjectPath::new(vec![
            Arc::from("app"),
            Arc::from("public"),
            Arc::from("users"),
        ]);
        assert_eq!(p.to_string(), "app.public.users");
        assert_eq!(ObjectPath::root().to_string(), "");
        assert_eq!(
            ObjectPath::root().child("db").child("t").to_string(),
            "db.t"
        );
    }

    #[test]
    fn field_flags_compose() {
        let f = FieldFlags::PRIMARY_KEY | FieldFlags::INDEXED;
        assert!(f.contains(FieldFlags::PRIMARY_KEY));
        assert!(!f.contains(FieldFlags::NULLABLE));
    }
}
