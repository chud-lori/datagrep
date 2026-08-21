use std::ffi::c_char;
use std::sync::Arc;

use datagrep_api::catalog::{Enumeration, ListOpts, ObjectDetail, ObjectKind, ObjectNode};
use datagrep_api::shape::{FieldFlags, ObjectPath};
use serde_json::json;

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, parse_path_json, to_c_string};
use crate::runtime::runtime;

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; string arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_catalog_children_json(
    core: *mut DatagrepCore,
    profile: *const c_char,
    path_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_catalog_children_json",
        || {
            // SAFETY: live core handle and NUL-terminated strings per the contract; core_ref/cstr reject NULL and non-UTF-8 before any deref.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let parent = object_path(unsafe { cstr(path_json, "path_json") }?)?;
            let rt = runtime()?;
            let text = rt.block_on(children(core, profile, parent))?;
            Ok(to_c_string(text))
        },
    )
}

async fn children(core: &CoreInner, profile: &str, parent: ObjectPath) -> Result<String, String> {
    let (id, _profile) = core.open_profile(profile).await?;
    let depth = parent.parts().len();

    let page = core
        .api
        .list_catalog(id, &parent, ListOpts::default())
        .await
        .map_err(|e| e.to_string())?;

    let enumeration = enumeration_for_depth(core, id, depth).await;
    let items: Vec<_> = page
        .items
        .iter()
        .map(|node| {
            json!({
                "name": leaf_name(node),
                "kind": kind_str(node.kind),
                "has_children": node.has_children,
                "enumeration": enumeration_str(enumeration),
            })
        })
        .collect();
    serde_json::to_string(&items).map_err(|e| format!("could not encode the catalog page: {e}"))
}

async fn enumeration_for_depth(
    core: &CoreInner,
    id: datagrep_core::ProfileId,
    depth: usize,
) -> Enumeration {
    let levels = async {
        let session = core.api.session(id).ok()?;
        let lease = session.acquire().await.ok()?;
        Some(lease.catalog().levels())
    }
    .await;

    let Some(levels) = levels.filter(|l| !l.is_empty()) else {
        return Enumeration::OnDemand;
    };
    levels[depth.min(levels.len() - 1)].enumeration
}

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; string arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_catalog_describe_json(
    core: *mut DatagrepCore,
    profile: *const c_char,
    path_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_catalog_describe_json",
        || {
            // SAFETY: as datagrep_catalog_children_json — live core, NUL-terminated strings, NULL rejected before deref.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let path = object_path(unsafe { cstr(path_json, "path_json") }?)?;
            let rt = runtime()?;
            let text = rt.block_on(describe(core, profile, path))?;
            Ok(to_c_string(text))
        },
    )
}

async fn describe(core: &CoreInner, profile: &str, path: ObjectPath) -> Result<String, String> {
    let (id, _profile) = core.open_profile(profile).await?;
    let session = core.api.session(id).map_err(|e| e.to_string())?;
    let lease = session.acquire().await.map_err(|e| e.to_string())?;
    let detail = lease
        .catalog()
        .describe(&path)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string(&detail_json(&detail))
        .map_err(|e| format!("could not encode the object detail: {e}"))
}

#[derive(Default)]
struct PromotedExtras {
    indexes: Option<serde_json::Value>,
    inferred_columns: Option<serde_json::Value>,
    column_defaults: serde_json::Map<String, serde_json::Value>,
    row_estimate: Option<i64>,
    size_bytes: Option<i64>,
    inferred: bool,
    sampled_docs: Option<u64>,
    rest: Vec<(String, String)>,
}

fn promote_extras(extra: &[(std::sync::Arc<str>, std::sync::Arc<str>)]) -> PromotedExtras {
    let mut out = PromotedExtras::default();
    for (k, v) in extra {
        let mut unparsed = true;
        match k.as_ref() {
            "indexes" => {
                if let Ok(val @ serde_json::Value::Array(_)) = serde_json::from_str(v) {
                    out.indexes = Some(val);
                    unparsed = false;
                }
            }
            "inferred_columns" => {
                if let Ok(val @ serde_json::Value::Array(_)) = serde_json::from_str(v) {
                    out.inferred_columns = Some(val);
                    unparsed = false;
                }
            }
            "column_defaults" => {
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(v) {
                    out.column_defaults = map;
                    unparsed = false;
                }
            }
            "row_estimate" => {
                if let Ok(n) = v.parse::<i64>() {
                    out.row_estimate = Some(n);
                    unparsed = false;
                }
            }
            "size_bytes" => {
                if let Ok(n) = v.parse::<i64>() {
                    out.size_bytes = Some(n);
                    unparsed = false;
                }
            }
            "inferred_schema" => {
                out.inferred = v.as_ref() == "true";
                unparsed = false;
            }
            "sampled_docs" => {
                if let Ok(n) = v.parse::<u64>() {
                    out.sampled_docs = Some(n);
                    unparsed = false;
                }
            }
            _ => {}
        }
        if unparsed {
            out.rest.push((k.to_string(), v.to_string()));
        }
    }
    out
}

fn detail_json(detail: &ObjectDetail) -> serde_json::Value {
    let promoted = promote_extras(&detail.extra);

    let declared: Option<Vec<serde_json::Value>> = detail.schema.as_ref().map(|s| {
        let identity: Vec<usize> = s
            .identity
            .as_ref()
            .map(|i| i.field_indices.iter().map(|&i| i as usize).collect())
            .unwrap_or_default();
        s.fields
            .iter()
            .enumerate()
            .map(|(ordinal, f)| {
                let logical = format!("{:?}", f.logical);
                json!({
                    "name": f.name,
                    "ordinal": ordinal,
                    "native_type": f.native_type,
                    "logical_type": logical,
                    "type": logical,
                    "nullable": f.flags.contains(FieldFlags::NULLABLE),
                    "default": promoted.column_defaults.get(f.name.as_ref()),
                    "primary_key": f.flags.contains(FieldFlags::PRIMARY_KEY)
                        || identity.contains(&ordinal),
                    "unique": f.flags.contains(FieldFlags::UNIQUE),
                    "auto_generated": f.flags.contains(FieldFlags::AUTO_GENERATED),
                    "indexed": f.flags.contains(FieldFlags::INDEXED),
                })
            })
            .collect()
    });
    let columns = match (declared, &promoted.inferred_columns) {
        (Some(cols), _) => serde_json::Value::Array(cols),
        (None, Some(inferred)) => inferred.clone(),
        (None, None) => serde_json::Value::Null,
    };

    json!({
        "path": detail.node.path.parts().iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        "name": leaf_name(&detail.node),
        "kind": kind_str(detail.node.kind),
        "has_children": detail.node.has_children,
        "comment": detail.node.comment,
        "columns": columns,
        "indexes": promoted.indexes,
        "row_estimate": promoted.row_estimate,
        "size_bytes": promoted.size_bytes,
        "inferred": promoted.inferred,
        "sampled_docs": promoted.sampled_docs,
        "extra": promoted
            .rest
            .iter()
            .map(|(k, v)| json!([k, v]))
            .collect::<Vec<_>>(),
    })
}

fn object_path(path_json: &str) -> Result<ObjectPath, String> {
    Ok(ObjectPath::new(
        parse_path_json(path_json)?
            .into_iter()
            .map(Arc::from)
            .collect(),
    ))
}

fn leaf_name(node: &ObjectNode) -> String {
    node.path
        .parts()
        .last()
        .map(|p| p.to_string())
        .unwrap_or_default()
}

fn kind_str(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => "database",
        ObjectKind::Schema => "schema",
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::Collection => "collection",
        ObjectKind::Column => "column",
        ObjectKind::Field => "field",
        ObjectKind::Index => "index",
        ObjectKind::Key => "key",
        ObjectKind::Function => "function",
        ObjectKind::Other => "other",
    }
}

fn enumeration_str(e: Enumeration) -> &'static str {
    match e {
        Enumeration::Cheap => "cheap",
        Enumeration::ScanOnly { .. } => "scan_only",
        Enumeration::Paged => "paged",
        Enumeration::OnDemand => "on_demand",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_spells_exactly_the_four_header_values() {
        assert_eq!(enumeration_str(Enumeration::Cheap), "cheap");
        assert_eq!(
            enumeration_str(Enumeration::ScanOnly {
                requires_prefix: true
            }),
            "scan_only"
        );
        assert_eq!(enumeration_str(Enumeration::Paged), "paged");
        assert_eq!(enumeration_str(Enumeration::OnDemand), "on_demand");
    }

    #[test]
    fn a_path_json_array_becomes_an_object_path() {
        assert_eq!(object_path("[]").unwrap().parts().len(), 0);
        assert_eq!(
            object_path(r#"["main","users"]"#).unwrap().to_string(),
            "main.users"
        );
        assert!(object_path("nonsense").is_err());
    }

    fn detail_with(
        schema: Option<datagrep_api::shape::RowSchema>,
        extra: Vec<(&str, &str)>,
    ) -> ObjectDetail {
        ObjectDetail {
            node: ObjectNode {
                path: ObjectPath::new(vec![Arc::from("main"), Arc::from("users")]),
                kind: ObjectKind::Table,
                has_children: true,
                comment: None,
            },
            schema,
            extra: extra
                .into_iter()
                .map(|(k, v)| (Arc::from(k), Arc::from(v)))
                .collect(),
        }
    }

    fn users_schema() -> datagrep_api::shape::RowSchema {
        use datagrep_api::shape::{FieldDef, Identity, LogicalType, RowSchema};
        RowSchema {
            fields: vec![
                FieldDef {
                    name: Arc::from("id"),
                    logical: LogicalType::I64,
                    flags: FieldFlags::empty(),
                    native_type: Some(Arc::from("INTEGER")),
                },
                FieldDef {
                    name: Arc::from("age"),
                    logical: LogicalType::I64,
                    flags: FieldFlags::NULLABLE,
                    native_type: Some(Arc::from("INTEGER")),
                },
            ],
            identity: Some(Identity {
                field_indices: vec![0],
            }),
        }
    }

    #[test]
    fn detail_json_promotes_reserved_extras_into_the_documented_shape() {
        let detail = detail_with(
            Some(users_schema()),
            vec![
                (
                    "indexes",
                    r#"[{"name":"idx_a","columns":[{"name":"age","order":"asc"}],"unique":true,"primary":false,"type":"btree","partial":false,"filter":null,"size_bytes":null,"definition":null,"sparse":false,"expire_after_seconds":null}]"#,
                ),
                ("column_defaults", r#"{"age":"18"}"#),
                ("row_estimate", "42"),
                ("size_bytes", "8192"),
                ("engine", "sqlite"),
            ],
        );
        let v = detail_json(&detail);

        assert_eq!(v["indexes"][0]["name"], "idx_a");
        assert_eq!(v["indexes"][0]["unique"], true);
        assert_eq!(v["row_estimate"], 42);
        assert_eq!(v["size_bytes"], 8192);
        assert_eq!(v["inferred"], false);
        assert_eq!(v["sampled_docs"], serde_json::Value::Null);

        let cols = v["columns"].as_array().expect("declared columns");
        assert_eq!(cols[0]["name"], "id");
        assert_eq!(cols[0]["ordinal"], 0);
        assert_eq!(
            cols[0]["primary_key"], true,
            "identity indices count as primary even without the field flag"
        );
        assert_eq!(cols[0]["default"], serde_json::Value::Null);
        assert_eq!(cols[1]["name"], "age");
        assert_eq!(cols[1]["default"], "18");
        assert_eq!(cols[1]["logical_type"], "I64");
        assert_eq!(cols[1]["type"], "I64", "legacy alias kept");

        // Promoted pairs leave `extra`; unrecognized ones stay.
        let extra = v["extra"].as_array().expect("extra array");
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0][0], "engine");
    }

    #[test]
    fn detail_json_uses_inferred_columns_only_without_a_declared_schema() {
        let detail = detail_with(
            None,
            vec![
                ("inferred_schema", "true"),
                ("sampled_docs", "500"),
                (
                    "inferred_columns",
                    r#"[{"name":"_id","ordinal":0,"native_type":null,"logical_type":"ObjectId","nullable":false,"default":null,"primary_key":true,"unique":true,"indexed":true,"auto_generated":false,"presence_ratio":1.0}]"#,
                ),
                ("indexes", "[]"),
            ],
        );
        let v = detail_json(&detail);
        assert_eq!(v["inferred"], true);
        assert_eq!(v["sampled_docs"], 500);
        assert_eq!(v["columns"][0]["name"], "_id");
        assert_eq!(
            v["indexes"],
            serde_json::json!([]),
            "an empty array is 'none', distinct from null 'not reported'"
        );
    }

    #[test]
    fn detail_json_without_reserved_extras_reports_null_not_fabrication() {
        let detail = detail_with(None, vec![("type", "string"), ("ttl_seconds", "no expiry")]);
        let v = detail_json(&detail);
        assert_eq!(v["columns"], serde_json::Value::Null);
        assert_eq!(v["indexes"], serde_json::Value::Null);
        assert_eq!(v["row_estimate"], serde_json::Value::Null);
        assert_eq!(v["inferred"], false);
        assert_eq!(v["extra"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn detail_json_keeps_unparseable_reserved_pairs_in_extra() {
        let detail = detail_with(
            None,
            vec![("indexes", "not json"), ("row_estimate", "many")],
        );
        let v = detail_json(&detail);
        assert_eq!(v["indexes"], serde_json::Value::Null);
        assert_eq!(v["row_estimate"], serde_json::Value::Null);
        let extra = v["extra"].as_array().expect("extra array");
        assert_eq!(extra.len(), 2, "both malformed pairs stay visible");
    }

    #[test]
    fn the_name_is_the_leaf_of_the_path() {
        let node = ObjectNode {
            path: ObjectPath::new(vec![Arc::from("main"), Arc::from("users")]),
            kind: ObjectKind::Table,
            has_children: true,
            comment: None,
        };
        assert_eq!(leaf_name(&node), "users");
        assert_eq!(
            leaf_name(&ObjectNode {
                path: ObjectPath::root(),
                ..node
            }),
            ""
        );
    }
}
