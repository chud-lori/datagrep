use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use datagrep_api::catalog::{CompletionCtx, Enumeration, ListOpts, ObjectKind, ObjectNode};
use datagrep_api::shape::ObjectPath;
use datagrep_api::{ConfigValue, LanguageId};
use datagrep_core::ProfileId;
use datagrep_lang::sql::complete::{self, Column, Kind, Object, TableRef};
use serde_json::{json, Value};

use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, guard_quiet, to_c_string};
use crate::query::language_for_driver;
use crate::runtime::runtime;

const FRESH_FOR: Duration = Duration::from_secs(300);
const MAX_OBJECTS: usize = 5_000;
const MAX_SCHEMAS: usize = 32;
const MAX_DESCRIBED: usize = 8;

/// Each connection's table list, and the columns of tables a statement has named, read once.
#[derive(Debug, Default)]
pub(crate) struct CompletionCache(Mutex<HashMap<String, Arc<Names>>>);

#[derive(Debug)]
struct Names {
    loaded: Instant,
    objects: Vec<(ObjectPath, Object)>,
    columns: Mutex<HashMap<ObjectPath, Arc<Vec<Column>>>>,
}

impl CompletionCache {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Names>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn forget(&self, profile: &str) {
        self.lock().remove(profile);
    }

    fn fresh(&self, profile: &str) -> Option<Arc<Names>> {
        let mut map = self.lock();
        match map.get(profile) {
            Some(names) if names.loaded.elapsed() < FRESH_FOR => Some(names.clone()),
            Some(_) => {
                map.remove(profile);
                None
            }
            None => None,
        }
    }
}

/// Drops the connection's cached names when a statement that can change them finishes.
pub(crate) struct SchemaChange {
    core: Arc<CoreInner>,
    profile: String,
}

impl SchemaChange {
    pub(crate) fn watch(
        core: &Arc<CoreInner>,
        profile: &str,
        driver_id: &str,
        statements: &[String],
    ) -> Option<Self> {
        let Some(language @ LanguageId::Sql(_)) = language_for_driver(driver_id) else {
            return None;
        };
        let lang = datagrep_lang::language_for(language);
        statements
            .iter()
            .any(|s| lang.classify(s) == datagrep_lang::StatementClass::Ddl)
            .then(|| Self {
                core: core.clone(),
                profile: profile.to_string(),
            })
    }
}

impl Drop for SchemaChange {
    fn drop(&mut self) {
        self.core.completion.forget(&self.profile);
    }
}

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; string arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_complete_json(
    core: *mut DatagrepCore,
    profile: *const c_char,
    text: *const c_char,
    caret: usize,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_complete_json",
        || {
            // SAFETY: live core handle and NUL-terminated strings per the contract; core_ref/cstr reject NULL and non-UTF-8 before any deref.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let text = unsafe { cstr(text, "text") }?;
            let rt = runtime()?;
            let payload = rt.block_on(complete_at(core, profile, text, caret))?;
            Ok(to_c_string(payload.to_string()))
        },
    )
}

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; `profile` is NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn datagrep_complete_forget(core: *mut DatagrepCore, profile: *const c_char) {
    guard_quiet((), || {
        // SAFETY: as datagrep_complete_json; a NULL argument is a no-op.
        let (Ok(core), Ok(profile)) = (unsafe { core_ref(core) }, unsafe {
            cstr(profile, "profile")
        }) else {
            return;
        };
        core.completion.forget(profile);
    })
}

/// # Safety
/// String arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_sql_format(
    driver_id: *const c_char,
    sql: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(err_out, std::ptr::null_mut(), "datagrep_sql_format", || {
        // SAFETY: NUL-terminated strings per the contract; cstr rejects NULL and non-UTF-8 before any deref.
        let driver_id = unsafe { cstr(driver_id, "driver_id") }?;
        let sql = unsafe { cstr(sql, "sql") }?;
        match language_for_driver(driver_id) {
            Some(LanguageId::Sql(dialect)) => Ok(to_c_string(datagrep_lang::sql::format::format(
                sql, dialect,
            ))),
            _ => Err(format!(
                "`{driver_id}` statements are not SQL, so there is no SQL to format"
            )),
        }
    })
}

async fn complete_at(
    core: &CoreInner,
    profile: &str,
    text: &str,
    caret: usize,
) -> Result<Value, String> {
    let (id, saved) = core.open_profile(profile).await?;
    let dialect = match language_for_driver(&saved.driver_id) {
        Some(LanguageId::Sql(dialect)) => dialect,
        _ => return native(core, id, text, caret).await,
    };
    let Some(site) = complete::site(text, caret, dialect) else {
        return Ok(json!({"prefix": "", "items": [], "error": null}));
    };

    let (names, error) = match names(core, id, profile, &saved).await {
        Ok(names) => (Some(names), None),
        Err(e) => (None, Some(e)),
    };
    let (objects, columns, error) = match &names {
        Some(names) => match columns(core, id, names, &site.column_sources()).await {
            Ok(columns) => (objects_of(names), columns, error),
            Err(e) => (objects_of(names), Vec::new(), Some(e)),
        },
        None => (Vec::new(), Vec::new(), error),
    };

    let items: Vec<Value> = complete::candidates(&site, dialect, &objects, &columns)
        .into_iter()
        .map(|c| {
            json!({
                "label": c.label,
                "insert": c.insert,
                "kind": c.kind.as_str(),
                "detail": c.detail,
            })
        })
        .collect();
    Ok(json!({"prefix": site.prefix, "items": items, "error": error}))
}

fn objects_of(names: &Names) -> Vec<Object> {
    names.objects.iter().map(|(_, o)| o.clone()).collect()
}

async fn names(
    core: &CoreInner,
    id: ProfileId,
    profile: &str,
    saved: &datagrep_profiles::Profile,
) -> Result<Arc<Names>, String> {
    if let Some(names) = core.completion.fresh(profile) {
        return Ok(names);
    }
    let database = match saved.config.values.get("database") {
        Some(ConfigValue::Str(db)) if !db.is_empty() => Some(db.as_str()),
        _ => None,
    };
    let names = Arc::new(Names {
        loaded: Instant::now(),
        objects: list_objects(core, id, database).await?,
        columns: Mutex::new(HashMap::new()),
    });
    core.completion
        .lock()
        .insert(profile.to_string(), names.clone());
    Ok(names)
}

/// Walks the catalog down to its table level: the connection's database, then every user schema.
async fn list_objects(
    core: &CoreInner,
    id: ProfileId,
    database: Option<&str>,
) -> Result<Vec<(ObjectPath, Object)>, String> {
    let levels = {
        let session = core.api.session(id).map_err(|e| e.to_string())?;
        let lease = session.acquire().await.map_err(|e| e.to_string())?;
        lease.catalog().levels()
    };
    let mut parents = vec![ObjectPath::root()];
    let mut schemas = false;
    for level in &levels {
        if !matches!(level.enumeration, Enumeration::Cheap | Enumeration::Paged) {
            break;
        }
        let mut nodes = Vec::new();
        for parent in &parents {
            nodes.extend(list_all(core, id, parent).await?);
        }
        match level.kind {
            ObjectKind::Table | ObjectKind::View => {
                return Ok(nodes
                    .into_iter()
                    .filter(|n| matches!(n.kind, ObjectKind::Table | ObjectKind::View))
                    .map(|n| {
                        let parts = n.path.parts();
                        let object = Object {
                            schema: schemas
                                .then(|| parts.len().checked_sub(2).map(|i| parts[i].to_string()))
                                .flatten(),
                            name: parts.last().map(|p| p.to_string()).unwrap_or_default(),
                            kind: if n.kind == ObjectKind::View {
                                Kind::View
                            } else {
                                Kind::Table
                            },
                        };
                        (n.path, object)
                    })
                    .collect());
            }
            ObjectKind::Database => {
                parents = pick_database(nodes, database).into_iter().collect();
            }
            _ => {
                schemas = true;
                parents = nodes
                    .into_iter()
                    .filter(|n| !is_system_schema(leaf(n)))
                    .take(MAX_SCHEMAS)
                    .map(|n| n.path)
                    .collect();
            }
        }
        if parents.is_empty() {
            break;
        }
    }
    Ok(Vec::new())
}

async fn list_all(
    core: &CoreInner,
    id: ProfileId,
    parent: &ObjectPath,
) -> Result<Vec<ObjectNode>, String> {
    let mut opts = ListOpts::default();
    let mut out = Vec::new();
    loop {
        let page = core
            .api
            .list_catalog(id, parent, opts.clone())
            .await
            .map_err(|e| e.to_string())?;
        out.extend(page.items);
        match page.next {
            Some(token) if out.len() < MAX_OBJECTS => opts.resume = Some(token),
            _ => return Ok(out),
        }
    }
}

fn leaf(node: &ObjectNode) -> &str {
    node.path.parts().last().map_or("", |p| p)
}

fn pick_database(nodes: Vec<ObjectNode>, configured: Option<&str>) -> Option<ObjectPath> {
    let named = |name: &str| {
        nodes
            .iter()
            .find(|n| leaf(n) == name)
            .map(|n| n.path.clone())
    };
    match configured {
        Some(db) => named(db),
        None => named("main").or_else(|| match nodes.as_slice() {
            [only] => Some(only.path.clone()),
            _ => None,
        }),
    }
}

fn is_system_schema(name: &str) -> bool {
    name.starts_with("pg_") || name == "information_schema"
}

async fn columns(
    core: &CoreInner,
    id: ProfileId,
    names: &Names,
    sources: &[&TableRef],
) -> Result<Vec<Column>, String> {
    let matches = |o: &Object, r: &TableRef| {
        o.name.eq_ignore_ascii_case(&r.name)
            && r.schema.as_deref().map_or(true, |s| {
                o.schema
                    .as_deref()
                    .is_some_and(|os| os.eq_ignore_ascii_case(s))
            })
    };
    let paths: Vec<(&ObjectPath, &Object)> = names
        .objects
        .iter()
        .filter(|(_, o)| sources.iter().any(|r| matches(o, r)))
        .take(MAX_DESCRIBED)
        .map(|(p, o)| (p, o))
        .collect();
    let mut out = Vec::new();
    for (path, object) in paths {
        let cached = names
            .columns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .cloned();
        let cols = match cached {
            Some(cols) => cols,
            None => {
                let cols = Arc::new(describe(core, id, path, &object.name).await?);
                names
                    .columns
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(path.clone(), cols.clone());
                cols
            }
        };
        out.extend(cols.iter().cloned());
    }
    Ok(out)
}

async fn describe(
    core: &CoreInner,
    id: ProfileId,
    path: &ObjectPath,
    table: &str,
) -> Result<Vec<Column>, String> {
    let session = core.api.session(id).map_err(|e| e.to_string())?;
    let lease = session.acquire().await.map_err(|e| e.to_string())?;
    let detail = lease
        .catalog()
        .describe(path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(detail
        .schema
        .map(|s| s.fields)
        .unwrap_or_default()
        .into_iter()
        .map(|f| Column {
            table: table.to_string(),
            name: f.name.to_string(),
            type_name: f.native_type.map(|t| t.to_string()),
        })
        .collect())
}

/// Engines without SQL answer from their own driver, which reads its own idea of the prefix.
async fn native(
    core: &CoreInner,
    id: ProfileId,
    text: &str,
    caret: usize,
) -> Result<Value, String> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return Err(format!(
            "caret {caret} is not a character boundary in the text"
        ));
    }
    let session = core.api.session(id).map_err(|e| e.to_string())?;
    let lease = session.acquire().await.map_err(|e| e.to_string())?;
    let found = lease
        .catalog()
        .complete(CompletionCtx {
            text: Arc::from(text),
            offset: u32::try_from(caret).map_err(|_| "the text is too long to complete in")?,
            scope: None,
        })
        .await
        .map_err(|e| e.to_string())?;
    let before = &text[..caret];
    let prefix = found
        .first()
        .map(|c| typed_part(before, &c.label))
        .unwrap_or_default();
    let items: Vec<Value> = found
        .iter()
        .filter(|c| c.label.starts_with(prefix) && *c.label != *prefix)
        .map(|c| {
            json!({
                "label": c.label.as_ref(),
                "insert": c.label.as_ref(),
                "kind": crate::catalog::kind_str(c.kind),
                "detail": c.detail.as_deref(),
            })
        })
        .collect();
    Ok(json!({"prefix": prefix, "items": items, "error": null}))
}

// The longest tail of `before` that `label` starts with: what accepting it replaces.
fn typed_part<'a>(before: &'a str, label: &str) -> &'a str {
    (before.len().saturating_sub(256)..before.len())
        .filter(|&i| before.is_char_boundary(i))
        .map(|i| &before[i..])
        .find(|tail| label.starts_with(tail))
        .unwrap_or("")
}
