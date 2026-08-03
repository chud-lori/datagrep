//! Catalog: **lazy, one level per call** (design §3.1, §5.1).
//!
//! > "On connect issue **exactly one** cheap query: the database/schema list.
//! > […] Nothing else. Expand-on-demand per node."
//!
//! Neither function here recurses. `dbx_catalog_children_json` is one
//! `Catalog::children` call for one path, bounded by `ListOpts`; the
//! disclosure triangle in the Swift outline view is what drives the next one.
//!
//! `enumeration` is the field that stops a `KEYS *` because someone clicked a
//! triangle — design §3.1 calls it "the single most important catalog
//! concept". Getting it into this ABI needs a detour; see
//! [`enumeration_for_depth`].

use std::ffi::c_char;
use std::sync::Arc;

use dbx_api::catalog::{Enumeration, ListOpts, ObjectDetail, ObjectKind, ObjectNode};
use dbx_api::shape::{FieldFlags, ObjectPath};
use serde_json::json;

use crate::core::{core_ref, CoreInner, DbxCore};
use crate::ffi_util::{cstr, guard, parse_path_json, to_c_string};
use crate::runtime::runtime;

/// One level of children under `path_json`.
///
/// Returns `[{"name":..,"kind":..,"has_children":bool,"enumeration":..}, ...]`.
///
/// # Safety
/// `core` must come from `dbx_core_new`; `profile`/`path_json` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn dbx_catalog_children_json(
    core: *mut DbxCore,
    profile: *const c_char,
    path_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "dbx_catalog_children_json",
        || {
            let core = core_ref(core)?;
            let profile = cstr(profile, "profile")?;
            let parent = object_path(cstr(path_json, "path_json")?)?;
            let rt = runtime()?;
            let text = rt.block_on(children(core, profile, parent))?;
            Ok(to_c_string(text))
        },
    )
}

async fn children(
    core: &CoreInner,
    profile: &str,
    parent: ObjectPath,
) -> Result<String, String> {
    let (id, _driver) = core.open_profile(profile).await?;
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

/// How costly enumerating the level *below* `depth` is.
///
/// **CoreApi gap the frozen header forces open.** The header wants
/// `enumeration` **per node**; the engine models it **per level**
/// ([`dbx_api::catalog::LevelDef`], returned by `Catalog::levels()`), and
/// `CoreApi` wraps only `Catalog::children` — there is no `catalog_levels`
/// façade. So this reaches one step further into the same public seam
/// `dbx-cli` uses for `--describe`:
/// `CoreApi::session(id).acquire().await?.catalog().levels()`. Still entirely
/// `dbx-core`/`dbx-api` public API, never a driver crate — but it skips the
/// `guarded(...)` panic isolation `list_catalog` gets for free.
///
/// A failure here is deliberately **not** fatal: the children are real and
/// worth showing. The value degrades to the most conservative honest answer,
/// `on_demand` ("never auto-expand; the user must explicitly ask"), which can
/// cost the user a click but can never fire a `KEYS *`.
async fn enumeration_for_depth(core: &CoreInner, id: dbx_core::ProfileId, depth: usize) -> Enumeration {
    let levels = async {
        let session = core.api.session(id).ok()?;
        let lease = session.acquire().await.ok()?;
        Some(lease.catalog().levels())
    }
    .await;

    let Some(levels) = levels.filter(|l| !l.is_empty()) else {
        return Enumeration::OnDemand;
    };
    // Children of a path of depth `d` are that hierarchy's level `d`. Below
    // the declared hierarchy (columns of a table, say) the last level's cost
    // is the closest truth we have.
    levels[depth.min(levels.len() - 1)].enumeration
}

/// Full detail for one object — columns, comment, engine-specific extras.
///
/// **CoreApi gap.** `CoreApi` wraps `Catalog::children` (as `list_catalog`)
/// but not `describe`/`infer_shape`/`complete`, so this goes through
/// `session().acquire().catalog().describe()` — public `dbx-core`/`dbx-api`
/// API, minus `list_catalog`'s panic isolation. Identical to the detour
/// `dbx-cli` documents.
///
/// # Safety
/// `core` must come from `dbx_core_new`; `profile`/`path_json` must be valid
/// NUL-terminated UTF-8; `err_out` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn dbx_catalog_describe_json(
    core: *mut DbxCore,
    profile: *const c_char,
    path_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "dbx_catalog_describe_json",
        || {
            let core = core_ref(core)?;
            let profile = cstr(profile, "profile")?;
            let path = object_path(cstr(path_json, "path_json")?)?;
            let rt = runtime()?;
            let text = rt.block_on(describe(core, profile, path))?;
            Ok(to_c_string(text))
        },
    )
}

async fn describe(core: &CoreInner, profile: &str, path: ObjectPath) -> Result<String, String> {
    let (id, _driver) = core.open_profile(profile).await?;
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

/// The describe payload's shape is this crate's to choose — the frozen header
/// pins the children JSON but leaves describe open. Hand-shaped rather than
/// `serde(ObjectDetail)` so the Swift side sees `"nullable": true` instead of
/// a bitflags integer it would have to decode.
fn detail_json(detail: &ObjectDetail) -> serde_json::Value {
    let columns: Vec<_> = detail
        .schema
        .iter()
        .flat_map(|s| s.fields.iter())
        .map(|f| {
            json!({
                "name": f.name,
                "type": format!("{:?}", f.logical),
                "native_type": f.native_type,
                "nullable": f.flags.contains(FieldFlags::NULLABLE),
                "primary_key": f.flags.contains(FieldFlags::PRIMARY_KEY),
                "unique": f.flags.contains(FieldFlags::UNIQUE),
                "auto_generated": f.flags.contains(FieldFlags::AUTO_GENERATED),
                "indexed": f.flags.contains(FieldFlags::INDEXED),
            })
        })
        .collect();
    json!({
        "path": detail.node.path.parts().iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        "name": leaf_name(&detail.node),
        "kind": kind_str(detail.node.kind),
        "has_children": detail.node.has_children,
        "comment": detail.node.comment,
        // `null`, not `[]`: "this engine declares no schema" is a different
        // fact from "this object has no columns" (design §3.1
        // `SCHEMA_DECLARED`), and the detail pane should be able to say so.
        "columns": detail.schema.as_ref().map(|_| columns),
        "extra": detail
            .extra
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

/// The header asks for a `name`; `ObjectNode` carries a full [`ObjectPath`].
/// The leaf is the name, and the caller already knows the parent it asked for.
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

/// Exactly the four spellings the frozen header lists. `ScanOnly`'s
/// `requires_prefix` payload has nowhere to go in that vocabulary — a real
/// loss, recorded in the README: a Redis tree cannot tell the Swift side
/// "refuse to scan without a prefix", only "this is a scan".
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
