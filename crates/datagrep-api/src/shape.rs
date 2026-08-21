use std::fmt;
use std::sync::Arc;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::value::{FieldPath, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Table(Arc<RowSchema>),
    Documents {
        root_hint: Option<FieldPath>,
        identity: Option<Vec<FieldPath>>,
    },
    Pairs {
        value_kind: ValueKind,
    },
    Graph(Arc<GraphSchema>),
    Ack {
        affected: Option<u64>,
        message: Option<Arc<str>>,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowSchema {
    pub fields: Vec<FieldDef>,
    pub identity: Option<Identity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: Arc<str>,
    pub logical: LogicalType,
    pub flags: FieldFlags,
    pub native_type: Option<Arc<str>>,
}

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
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct FieldFlags: u32 {
        const NULLABLE       = 1 << 0;
        const PRIMARY_KEY    = 1 << 1;
        const UNIQUE         = 1 << 2;
        const AUTO_GENERATED = 1 << 3;
        const INDEXED        = 1 << 4;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub field_indices: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ObjectPath(pub Vec<Arc<str>>);

impl ObjectPath {
    pub fn new(parts: Vec<Arc<str>>) -> Self {
        Self(parts)
    }

    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn parts(&self) -> &[Arc<str>] {
        &self.0
    }

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaDelta {
    AddColumn { field: FieldDef },
    NarrowType { index: u32, to: LogicalType },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GraphSchema {
    pub node_labels: Vec<Arc<str>>,
    pub edge_types: Vec<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphChunk {
    pub nodes: Vec<Value>,
    pub edges: Vec<Value>,
}

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
