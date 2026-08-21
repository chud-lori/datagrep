use serde_json::{json, Value as Json};

use datagrep_api::{DbError, DdlOp, ObjectKind, ObjectPath};

#[derive(Debug, PartialEq)]
pub struct EsDdl {
    pub kind: EsDdlKind,
    pub absent_code: Option<&'static str>,
    pub ack: &'static str,
}

#[derive(Debug, PartialEq)]
pub enum EsDdlKind {
    DeleteIndex {
        index: String,
        ignore_unavailable: bool,
    },
    Aliases {
        body: Json,
    },
}

fn one_object(path: &ObjectPath) -> Result<&str, DbError> {
    let [name] = path.parts() else {
        return Err(DbError::Unsupported {
            feature: format!(
                "object path {path} does not name a single index or alias (this engine's \
                 catalog is one level deep)"
            ),
        });
    };
    if name.is_empty() {
        return Err(DbError::Unsupported {
            feature: "empty index or alias name".into(),
        });
    }
    if name.contains(['*', ',', '?']) || name.starts_with('<') || name.eq_ignore_ascii_case("_all")
    {
        return Err(DbError::Unsupported {
            feature: format!(
                "{name:?} is a pattern, not one object — a wildcard, comma list or date-math \
                 name expands server-side and could match more than was named"
            ),
        });
    }
    Ok(name)
}

pub fn plan(op: &DdlOp) -> Result<EsDdl, DbError> {
    match op {
        DdlOp::Drop {
            path,
            kind,
            if_exists,
        } => match kind {
            ObjectKind::Collection => Ok(EsDdl {
                kind: EsDdlKind::DeleteIndex {
                    index: one_object(path)?.to_string(),
                    ignore_unavailable: *if_exists,
                },
                absent_code: None,
                ack: "index deleted",
            }),
            ObjectKind::View => {
                let alias = one_object(path)?;
                Ok(EsDdl {
                    kind: EsDdlKind::Aliases {
                        body: json!({
                            "actions": [{ "remove": { "index": "*", "alias": alias } }]
                        }),
                    },
                    // `must_exist: false` does not suppress this.
                    absent_code: if_exists.then_some("aliases_not_found_exception"),
                    ack: "alias removed",
                })
            }
            other => Err(DbError::Unsupported {
                feature: format!(
                    "dropping a {other:?} — this engine's catalog lists indices (Collection) \
                     and aliases (View), and a drop must say which of the two a name means"
                ),
            }),
        },
        DdlOp::Rename { .. } => Err(DbError::Unsupported {
            feature: "renaming an index or alias — this engine has no rename; the equivalent is \
                      to reindex into the new name and re-point an alias, which is several \
                      requests with a data copy in the middle, not one DDL statement"
                .into(),
        }),
        DdlOp::CreateIndex { .. } => Err(DbError::Unsupported {
            feature: "creating a named secondary index — this engine has no such object; a \
                      field is searchable through its mapping, which is authoring and goes as \
                      a native request"
                .into(),
        }),
        DdlOp::Native { .. } => Err(DbError::Unsupported {
            feature: "DdlOp::Native — this engine's own request language is already what \
                      `Request::Native` carries (`PUT /<index>`, `POST /_aliases`), so a \
                      second untyped text door here would only be able to guess at it"
                .into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn path(name: &str) -> ObjectPath {
        ObjectPath::new(vec![Arc::from(name)])
    }

    fn drop_op(name: &str, kind: ObjectKind, if_exists: bool) -> DdlOp {
        DdlOp::Drop {
            path: path(name),
            kind,
            if_exists,
        }
    }

    #[test]
    fn an_index_and_an_alias_take_different_requests() {
        let index = plan(&drop_op("m3_live", ObjectKind::Collection, false)).unwrap();
        assert_eq!(
            index.kind,
            EsDdlKind::DeleteIndex {
                index: "m3_live".to_string(),
                ignore_unavailable: false,
            }
        );

        let alias = plan(&drop_op("m3_live", ObjectKind::View, false)).unwrap();
        let EsDdlKind::Aliases { body } = &alias.kind else {
            panic!("expected an action list, got {:?}", alias.kind)
        };
        assert_eq!(
            body,
            &json!({"actions": [{"remove": {"index": "*", "alias": "m3_live"}}]})
        );
    }

    #[test]
    fn if_exists_uses_whichever_mechanism_the_endpoint_has() {
        let index = plan(&drop_op("m3_live", ObjectKind::Collection, true)).unwrap();
        assert!(matches!(
            index.kind,
            EsDdlKind::DeleteIndex {
                ignore_unavailable: true,
                ..
            }
        ));
        assert_eq!(index.absent_code, None);

        let alias = plan(&drop_op("m3_live", ObjectKind::View, true)).unwrap();
        assert_eq!(alias.absent_code, Some("aliases_not_found_exception"));
        assert_eq!(
            plan(&drop_op("m3_live", ObjectKind::View, false))
                .unwrap()
                .absent_code,
            None
        );
    }

    #[test]
    fn a_name_that_could_match_more_than_one_object_is_refused() {
        for name in ["m3_*", "m3_a,m3_b", "_all", "<m3-{now/d}>", "m3_?", ""] {
            for kind in [ObjectKind::Collection, ObjectKind::View] {
                assert!(
                    plan(&drop_op(name, kind, false)).is_err(),
                    "{name:?} should not be droppable"
                );
            }
        }
        // A deeper path names a field, not an index.
        assert!(plan(&DdlOp::Drop {
            path: ObjectPath::new(vec![Arc::from("m3_live"), Arc::from("title")]),
            kind: ObjectKind::Collection,
            if_exists: false,
        })
        .is_err());
    }

    #[test]
    fn the_verbs_this_engine_does_not_have_are_refused_by_name() {
        let err = plan(&DdlOp::Rename {
            from: path("a"),
            to: path("b"),
            kind: ObjectKind::Collection,
        })
        .unwrap_err();
        assert!(format!("{err}").contains("reindex"), "{err}");

        assert!(plan(&DdlOp::CreateIndex {
            path: path("m3_live"),
            name: Arc::from("by_title"),
            fields: vec![datagrep_api::FieldPath::field("title")],
            unique: false,
            if_not_exists: true,
        })
        .is_err());
        assert!(plan(&DdlOp::Native {
            text: Arc::from("PUT /m3_live")
        })
        .is_err());
        // A table is a kind this catalog never produces.
        assert!(plan(&drop_op("m3_live", ObjectKind::Table, false)).is_err());
    }
}
