use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog::ObjectKind;
use crate::driver::ResumeToken;
use crate::error::DbError;
use crate::shape::ObjectPath;
use crate::value::{FieldPath, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Native {
        text: Arc<str>,
        params: Vec<Value>,
        opts: ExecOpts,
    },
    Op(Op),
}

impl Request {
    pub fn native(text: impl Into<Arc<str>>) -> Self {
        Request::Native {
            text: text.into(),
            params: Vec::new(),
            opts: ExecOpts::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Scan {
        path: ObjectPath,
        filter: Option<Predicate>,
        order: Vec<SortKey>,
        project: Option<Vec<FieldPath>>,
        limit: Option<u64>,
        resume: Option<ResumeToken>,
    },
    Count {
        path: ObjectPath,
        filter: Option<Predicate>,
        exact: bool,
    },
    Mutate(MutationBatch),
    Explain {
        inner: Box<Request>,
        analyze: bool,
    },
    Ddl(DdlOp),
}

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
    Like {
        field: FieldPath,
        pattern: Arc<str>,
    },
    Exists {
        field: FieldPath,
    },
    IsNull {
        field: FieldPath,
    },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub path: FieldPath,
    pub desc: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExecOpts {
    pub timeout: Option<Duration>,
    pub row_limit: Option<u64>,
    pub read_only_assert: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MutationBatch {
    pub mutations: Vec<Mutation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mutation {
    Update {
        path: ObjectPath,
        key: Vec<(FieldPath, Value)>,
        sets: Vec<(FieldPath, Value)>,
        #[serde(default)]
        expect: Vec<(FieldPath, Value)>,
    },
    Insert {
        path: ObjectPath,
        doc: Value,
    },
    Delete {
        path: ObjectPath,
        key: Vec<(FieldPath, Value)>,
        #[serde(default)]
        expect: Vec<(FieldPath, Value)>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DdlOp {
    Native {
        text: Arc<str>,
    },
    Drop {
        path: ObjectPath,
        kind: ObjectKind,
        if_exists: bool,
    },
    Rename {
        from: ObjectPath,
        to: ObjectPath,
        kind: ObjectKind,
    },
    CreateIndex {
        path: ObjectPath,
        name: Arc<str>,
        fields: Vec<FieldPath>,
        unique: bool,
        if_not_exists: bool,
    },
}

impl DdlOp {
    pub fn rename_target<'a>(
        from: &ObjectPath,
        to: &'a ObjectPath,
    ) -> Result<&'a Arc<str>, DbError> {
        let (from_last, from_parent) =
            from.parts()
                .split_last()
                .ok_or_else(|| DbError::Unsupported {
                    feature: "rename from an empty object path".into(),
                })?;
        let (to_last, to_parent) = to
            .parts()
            .split_last()
            .ok_or_else(|| DbError::Unsupported {
                feature: "rename to an empty object path".into(),
            })?;
        if from_parent != to_parent {
            return Err(DbError::Unsupported {
                feature: format!(
                    "rename moves {from} to a different parent ({to}) — a rename changes the \
                     name, not the namespace"
                ),
            });
        }
        if from_last == to_last {
            return Err(DbError::Unsupported {
                feature: format!("rename of {from} to the name it already has"),
            });
        }
        Ok(to_last)
    }
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
    fn structured_ddl_round_trips_and_native_payloads_still_parse() {
        let ops = vec![
            DdlOp::Drop {
                path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                kind: ObjectKind::Table,
                if_exists: true,
            },
            DdlOp::Rename {
                from: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                to: ObjectPath::new(vec![Arc::from("app"), Arc::from("people")]),
                kind: ObjectKind::Table,
            },
            DdlOp::CreateIndex {
                path: ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]),
                name: Arc::from("users_email"),
                fields: vec![FieldPath::field("email")],
                unique: true,
                if_not_exists: false,
            },
        ];
        for op in ops {
            let json = serde_json::to_string(&Op::Ddl(op.clone())).unwrap();
            let back: Op = serde_json::from_str(&json).unwrap();
            assert_eq!(back, Op::Ddl(op));
        }
        let legacy = r#"{"Ddl":{"Native":{"text":"DROP TABLE t"}}}"#;
        let back: Op = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            back,
            Op::Ddl(DdlOp::Native {
                text: Arc::from("DROP TABLE t")
            })
        );
    }

    #[test]
    fn a_rename_may_only_change_the_last_part() {
        let users = ObjectPath::new(vec![Arc::from("app"), Arc::from("users")]);
        let people = ObjectPath::new(vec![Arc::from("app"), Arc::from("people")]);
        assert_eq!(&**DdlOp::rename_target(&users, &people).unwrap(), "people");

        // Moving to another schema is not a rename, and neither is a no-op.
        let moved = ObjectPath::new(vec![Arc::from("archive"), Arc::from("users")]);
        assert!(DdlOp::rename_target(&users, &moved).is_err());
        assert!(DdlOp::rename_target(&users, &users).is_err());
        assert!(DdlOp::rename_target(&users, &ObjectPath::root()).is_err());
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
