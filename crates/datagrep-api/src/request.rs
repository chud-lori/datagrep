//! What the core asks a driver to do. Two doors only: the engine's own text
//! (never translated) or a structured [`Op`] each driver compiles natively or
//! rejects with a capability error.

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
    /// as parameters — never spliced into the text.
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
    /// Browse: keyset pagination via `resume`, never OFFSET — OFFSET re-scans
    /// everything it skips and drifts when rows change underneath it.
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
/// where the engine supports it, so even uncancellable work is bounded.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExecOpts {
    pub timeout: Option<Duration>,
    /// A hard row cap the driver enforces at the source when it can.
    pub row_limit: Option<u64>,
    /// Caller asserts this request must not write; drivers that can verify it
    /// server-side should — layer 1 of the read-only guardrails.
    pub read_only_assert: bool,
}

/// A batch of mutations applied together — atomically where the engine allows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MutationBatch {
    pub mutations: Vec<Mutation>,
}

/// One generated write. `key` is the full row identity ([`crate::Identity`])
/// as **named** field/value pairs — the same shape as `sets` — so a driver
/// never has to reverse-engineer which columns the values belong to (no
/// `pg_index` lookups, no `PRAGMA table_info` positional conventions, no
/// "assume `_id`"). Each mutation must affect exactly one row or the batch
/// rolls back — a generated write that hits N rows is a data-loss bug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mutation {
    Update {
        path: ObjectPath,
        /// Row identity: identity fields paired with this row's values.
        key: Vec<(FieldPath, Value)>,
        sets: Vec<(FieldPath, Value)>,
        /// Precondition, **not** identity: "only apply if these fields still
        /// hold these values" (check-and-set). ES compiles this to
        /// `if_seq_no`/`if_primary_term`; SQL engines can compile it into
        /// extra `WHERE` conjuncts. A driver that cannot honour a precondition
        /// MUST reject a non-empty `expect` with [`crate::DbError::Unsupported`]
        /// — silently dropping it would turn a guarded write into a clobber.
        #[serde(default)]
        expect: Vec<(FieldPath, Value)>,
    },
    Insert {
        path: ObjectPath,
        /// The new row/document as a `Value` (typically `Value::Document`).
        doc: Value,
    },
    Delete {
        path: ObjectPath,
        /// Row identity: identity fields paired with this row's values.
        key: Vec<(FieldPath, Value)>,
        /// Precondition, same contract as [`Mutation::Update::expect`]:
        /// non-empty means check-and-set or refuse, never drop.
        #[serde(default)]
        expect: Vec<(FieldPath, Value)>,
    },
}

/// DDL. `Native` is not a placeholder: it is the right model for schema and
/// index administration, which is the part of every engine that generalises
/// worst. A structured verb only earns a variant when more than one driver can
/// honour it *and* something in the workspace constructs it — an untyped
/// passthrough is honest, whereas a general-looking variant five drivers
/// refuse is a capability that does not exist.
///
/// Surveyed for M3 across all six drivers: only `Drop` and `Rename` have a
/// meaning in more than one engine. `Create` is blocked on a type vocabulary
/// the api does not have — [`crate::shape::FieldDef`] describes what a *result*
/// held and is deliberately lossy, while authoring cannot be. And a `Drop`
/// needs [`crate::catalog::ObjectKind`] alongside the path, because engines
/// exist where an index and an alias share one namespace and only the kind
/// says which is meant.
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
    fn mutation_key_carries_field_names_and_round_trips_through_serde() {
        // The row identity names its fields, exactly like `sets` — a driver
        // must never have to guess which column a key value belongs to.
        let op = Op::Mutate(MutationBatch {
            mutations: vec![
                Mutation::Update {
                    path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                    key: vec![
                        (FieldPath::field("tenant"), Value::I64(7)),
                        (FieldPath::field("id"), Value::I64(42)),
                    ],
                    sets: vec![(FieldPath::field("name"), Value::Str(Arc::from("amy")))],
                    expect: vec![(FieldPath::field("version"), Value::I64(9))],
                },
                Mutation::Delete {
                    path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                    key: vec![(FieldPath::field("id"), Value::I64(43))],
                    expect: Vec::new(),
                },
            ],
        });
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        assert_eq!(back, op);
        let Op::Mutate(batch) = back else {
            panic!("expected Op::Mutate")
        };
        let Mutation::Update { key, .. } = &batch.mutations[0] else {
            panic!("expected Update")
        };
        assert_eq!(key[0].0, FieldPath::field("tenant"));
        assert_eq!(key[1].1, Value::I64(42));
    }

    #[test]
    fn mutation_without_expect_still_deserializes() {
        // `expect` is `#[serde(default)]`: a payload serialized before the
        // field existed (or by a caller that never sets preconditions) must
        // keep deserializing, as an empty precondition.
        let mutations = vec![
            Mutation::Update {
                path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                key: vec![(FieldPath::field("id"), Value::I64(1))],
                sets: vec![(FieldPath::field("name"), Value::Str(Arc::from("amy")))],
                expect: Vec::new(),
            },
            Mutation::Delete {
                path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                key: vec![(FieldPath::field("id"), Value::I64(2))],
                expect: Vec::new(),
            },
        ];
        for m in mutations {
            // Build the pre-`expect` wire form by dropping the key outright.
            let mut json = serde_json::to_value(&m).unwrap();
            let body = json
                .as_object_mut()
                .and_then(|variant| variant.values_mut().next())
                .and_then(|b| b.as_object_mut())
                .expect("externally tagged enum body");
            assert!(body.remove("expect").is_some(), "expect must serialize");
            let back: Mutation = serde_json::from_value(json).expect("legacy payload");
            assert_eq!(back, m);
        }
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
