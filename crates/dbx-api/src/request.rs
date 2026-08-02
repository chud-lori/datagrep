//! What the core asks a driver to do. Two doors only: the engine's own text
//! (never translated — design §3.6) or a structured [`Op`] each driver compiles
//! natively or rejects with a capability error.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::driver::ResumeToken;
use crate::shape::ObjectPath;
use crate::value::{FieldPath, Value};

/// A unit of work for [`Connection::execute`](crate::Connection::execute).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// User-authored text in the connection's own language, with values bound
    /// as parameters — never spliced (design §3.8).
    Native {
        text: Arc<str>,
        params: Vec<Value>,
        opts: ExecOpts,
    },
    /// A portable structured operation (browse/filter/sort — the real 80% of
    /// cross-engine demand).
    Op(Op),
}

impl Request {
    /// Convenience: native text with no params and default options.
    pub fn native(text: impl Into<Arc<str>>) -> Self {
        Request::Native {
            text: text.into(),
            params: Vec::new(),
            opts: ExecOpts::default(),
        }
    }
}

/// Structured operations every driver compiles natively or rejects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    /// Browse: keyset pagination via `resume`, never OFFSET (design §3.6).
    Scan {
        path: ObjectPath,
        filter: Option<Predicate>,
        order: Vec<SortKey>,
        project: Option<Vec<FieldPath>>,
        limit: Option<u64>,
        resume: Option<ResumeToken>,
    },
    /// `exact: false` allows the cheap estimate; the UI then shows "≥ N"
    /// (see `EXACT_COUNT_CHEAP`).
    Count {
        path: ObjectPath,
        filter: Option<Predicate>,
        exact: bool,
    },
    /// Requires `EDITABLE_RESULTS`.
    Mutate(MutationBatch),
    Explain {
        inner: Box<Request>,
        analyze: bool,
    },
    Ddl(DdlOp),
}

/// A small filter AST drivers compile to their native form (SQL WHERE, Mongo
/// filter document, …). Values are typed [`Value`]s, never spliced text —
/// which is also what blocks `{"$ne": null}`-style NoSQL injection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    Eq {
        field: FieldPath,
        value: Value,
    },
    Ne {
        field: FieldPath,
        value: Value,
    },
    Lt {
        field: FieldPath,
        value: Value,
    },
    Le {
        field: FieldPath,
        value: Value,
    },
    Gt {
        field: FieldPath,
        value: Value,
    },
    Ge {
        field: FieldPath,
        value: Value,
    },
    In {
        field: FieldPath,
        values: Vec<Value>,
    },
    /// SQL LIKE / engine-native pattern match; the pattern is data.
    Like {
        field: FieldPath,
        pattern: Arc<str>,
    },
    /// The field is present — meaningful where `Absent` exists (sparse docs).
    Exists {
        field: FieldPath,
    },
    /// The field holds an explicit NULL (present, but null).
    IsNull {
        field: FieldPath,
    },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
}

/// One sort criterion. `nulls_first` is explicit because engines disagree on
/// the default and a silent difference reorders results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub path: FieldPath,
    pub desc: bool,
    pub nulls_first: bool,
}

/// Per-request execution options. `timeout` should also be pushed server-side
/// where the engine supports it, so even uncancellable work is bounded (§3.3).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExecOpts {
    pub timeout: Option<Duration>,
    /// A hard row cap the driver enforces at the source when it can.
    pub row_limit: Option<u64>,
    /// Caller asserts this request must not write; drivers that can verify it
    /// server-side should (layer 1 of the guardrails, design §3.8).
    pub read_only_assert: bool,
}

/// A batch of mutations applied together — atomically where the engine allows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MutationBatch {
    pub mutations: Vec<Mutation>,
}

/// One generated write. `key` is the full row identity ([`crate::Identity`]):
/// each mutation must affect exactly one row or the batch rolls back (§3.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mutation {
    Update {
        path: ObjectPath,
        key: Vec<Value>,
        sets: Vec<(FieldPath, Value)>,
    },
    Insert {
        path: ObjectPath,
        /// The new row/document as a `Value` (typically `Value::Document`).
        doc: Value,
    },
    Delete {
        path: ObjectPath,
        key: Vec<Value>,
    },
}

/// Placeholder DDL surface — structured variants land with the M3 write path;
/// until then generated DDL travels as engine-native text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DdlOp {
    /// Engine-native DDL text, passed through verbatim.
    Native { text: Arc<str> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_construction() {
        // status = 'active' AND age >= 21 AND NOT (deleted_at exists)
        let p = Predicate::And(vec![
            Predicate::Eq {
                field: FieldPath::field("status"),
                value: Value::Str(Arc::from("active")),
            },
            Predicate::Ge {
                field: FieldPath::field("age"),
                value: Value::I64(21),
            },
            Predicate::Not(Box::new(Predicate::Exists {
                field: FieldPath::field("deleted_at"),
            })),
        ]);
        match &p {
            Predicate::And(parts) => assert_eq!(parts.len(), 3),
            other => panic!("expected And, got {other:?}"),
        }
        // Nested field paths work in predicates.
        let nested = Predicate::In {
            field: "address.tags[0]".parse().unwrap(),
            values: vec![Value::Str(Arc::from("home"))],
        };
        assert_eq!(p.clone(), p);
        assert_ne!(p, nested);
    }

    #[test]
    fn scan_op_round_trips_through_serde() {
        let op = Op::Scan {
            path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
            filter: Some(Predicate::IsNull {
                field: FieldPath::field("deleted_at"),
            }),
            order: vec![SortKey {
                path: FieldPath::field("id"),
                desc: true,
                nulls_first: false,
            }],
            project: None,
            limit: Some(200),
            resume: None,
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        assert_eq!(back, op);
    }
}
