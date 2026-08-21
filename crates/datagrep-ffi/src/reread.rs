use std::ffi::c_char;
use std::sync::Arc;

use datagrep_api::driver::{FetchHint, Payload};
use datagrep_api::request::{Op, Predicate, Request};
use datagrep_api::shape::{ObjectPath, Shape};
use datagrep_api::value::{FieldPath, PathSeg, Value};
use datagrep_core::session::ConnLease;
use serde::Deserialize;
use serde_json::json;

use crate::cells::value_to_json;
use crate::core::{core_ref, CoreInner, DatagrepCore};
use crate::ffi_util::{cstr, guard, to_c_string};
use crate::runtime::runtime;

#[derive(Debug, Deserialize)]
struct RereadBatch {
    documents: Vec<Address>,
}

#[derive(Debug, Deserialize)]
struct Address {
    key: Vec<(FieldPath, Value)>,
}

/// # Safety
/// `core` is a live handle from `datagrep_core_new`; string arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_reread_documents(
    core: *mut DatagrepCore,
    profile: *const c_char,
    addresses_json: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_reread_documents",
        || {
            // SAFETY: live core handle and strings NULL or NUL-terminated per the contract; core_ref/cstr error before any deref.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let addresses_json = unsafe { cstr(addresses_json, "addresses_json") }?;

            let batch: RereadBatch = serde_json::from_str(addresses_json)
                .map_err(|e| format!("addresses_json is not a valid document address list: {e}"))?;

            let rt = runtime()?;
            let documents = rt.block_on(reread_all(core, profile, &batch.documents))?;

            serde_json::to_string(&json!({ "documents": documents }))
                .map(to_c_string)
                .map_err(|e| format!("could not serialize the re-read: {e}"))
        },
    )
}

async fn reread_all(
    core: &Arc<CoreInner>,
    profile: &str,
    addresses: &[Address],
) -> Result<Vec<serde_json::Value>, String> {
    let (lease, saved) = core.leased(profile).await?;
    let path_field = crate::query::object_path_field(&saved.driver_id).ok_or_else(|| {
        format!(
            "datagrep cannot re-read a document on `{}` by its identity: this engine has not \
             said which identity field names the object a document lives in",
            saved.driver_id
        )
    })?;

    let mut documents = Vec::with_capacity(addresses.len());
    for address in addresses {
        documents.push(match reread_one(&lease, address, path_field).await {
            Ok(found) => found,
            Err(why) => json!({ "found": false, "error": why }),
        });
    }
    Ok(documents)
}

async fn reread_one(
    lease: &ConnLease,
    address: &Address,
    path_field: &str,
) -> Result<serde_json::Value, String> {
    let (object, terms) = split_address(address, path_field)?;

    let mut cursor = lease
        .execute(Request::Op(Op::Scan {
            path: ObjectPath::new(vec![object]),
            filter: Some(Predicate::And(terms)),
            order: Vec::new(),
            limit: Some(2),
            project: None,
            resume: None,
        }))
        .await
        .map_err(|e| e.to_string())?;

    let root = root_of(cursor.shape());
    let mut docs: Vec<Value> = Vec::new();
    while let Some(batch) = cursor
        .next_batch(FetchHint::default())
        .await
        .map_err(|e| e.to_string())?
    {
        if let Payload::Docs(hits) = batch.payload {
            docs.extend(hits);
        }
        if docs.len() > 1 {
            break;
        }
    }
    let _ = cursor.close().await;

    match docs.as_slice() {
        [] => Ok(json!({ "found": false })),
        [hit] => {
            let (envelope, fields) = split_document(hit, root.as_deref());
            Ok(json!({ "found": true, "envelope": envelope, "fields": fields }))
        }
        _ => Err(
            "more than one document answers to this identity, so datagrep will not say which \
             one the server holds now"
                .to_string(),
        ),
    }
}

fn split_address(
    address: &Address,
    path_field: &str,
) -> Result<(Arc<str>, Vec<Predicate>), String> {
    let mut object: Option<Arc<str>> = None;
    let mut terms: Vec<Predicate> = Vec::new();
    for (field, value) in &address.key {
        let name = plain_field(field).ok_or_else(|| {
            format!("`{field}` is not a plain field name, so it cannot address a document")
        })?;
        if name == path_field {
            let Value::Str(text) = value else {
                return Err(format!(
                    "`{path_field}` names the object this document lives in, so it has to be \
                     text; this one is not"
                ));
            };
            object = Some(text.clone());
            continue;
        }
        terms.push(Predicate::Eq {
            field: field.clone(),
            value: value.clone(),
        });
    }
    let Some(object) = object else {
        return Err(format!(
            "this document's identity carries no `{path_field}`, so datagrep cannot say which \
             object to read it from"
        ));
    };
    if terms.is_empty() {
        return Err(format!(
            "`{path_field}` is the only thing identifying this document, which names an object \
             rather than one document in it"
        ));
    }
    Ok((object, terms))
}

fn plain_field(path: &FieldPath) -> Option<&str> {
    match path.segments() {
        [PathSeg::Field(name)] => Some(name),
        _ => None,
    }
}

fn root_of(shape: &Shape) -> Option<String> {
    let Shape::Documents { root_hint, .. } = shape else {
        return None;
    };
    root_hint.as_ref().and_then(|p| match p.segments() {
        [PathSeg::Field(name)] => Some(name.to_string()),
        _ => None,
    })
}

fn split_document(hit: &Value, root: Option<&str>) -> (serde_json::Value, serde_json::Value) {
    let Value::Document(doc) = hit else {
        return (json!({}), value_to_json(hit));
    };
    let Some(root) = root else {
        return (json!({}), value_to_json(hit));
    };
    let mut envelope = serde_json::Map::new();
    for (name, value) in doc.iter() {
        if name.as_ref() == root {
            continue;
        }
        envelope.insert(name.to_string(), value_to_json(value));
    }
    let fields = doc
        .get(root)
        .map(value_to_json)
        .unwrap_or_else(|| json!({}));
    (serde_json::Value::Object(envelope), fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_char, CStr, CString};

    use datagrep_api::value::Document;

    fn address(pairs: Vec<(&str, Value)>) -> Address {
        Address {
            key: pairs
                .into_iter()
                .map(|(f, v)| (FieldPath::field(f), v))
                .collect(),
        }
    }

    fn hit(fields: Vec<(&str, Value)>) -> Value {
        Value::Document(Arc::new(Document::from_fields(
            fields.into_iter().map(|(k, v)| (Arc::from(k), v)).collect(),
        )))
    }

    #[test]
    fn the_json_the_macos_grid_sends_parses_into_addresses() {
        let json = r#"{"documents":[{"key":[[[{"Field":"_index"}],{"Str":"events"}],[[{"Field":"_id"}],{"Str":"abc"}],[[{"Field":"_routing"}],{"Str":"tenant-7"}]]},{"key":[[[{"Field":"_index"}],{"Str":"events"}],[[{"Field":"_id"}],{"Str":"gone"}]]}]}"#;

        let batch: RereadBatch =
            serde_json::from_str(json).expect("the grid's address list must parse");
        assert_eq!(batch.documents.len(), 2);

        let (object, terms) = split_address(&batch.documents[0], "_index").expect("splits");
        assert_eq!(&*object, "events");
        assert_eq!(terms.len(), 2);
        assert!(matches!(
            &terms[0],
            Predicate::Eq { field, value }
                if plain_field(field) == Some("_id") && *value == Value::Str(Arc::from("abc"))
        ));
        assert!(matches!(
            &terms[1],
            Predicate::Eq { field, .. } if plain_field(field) == Some("_routing")
        ));
    }

    #[test]
    fn an_address_with_no_terms_is_refused() {
        let err = split_address(
            &address(vec![("_index", Value::Str(Arc::from("events")))]),
            "_index",
        )
        .expect_err("must refuse");
        assert!(err.contains("names an object"), "{err}");
    }

    #[test]
    fn an_address_with_no_object_is_refused() {
        let err = split_address(
            &address(vec![("_id", Value::Str(Arc::from("abc")))]),
            "_index",
        )
        .expect_err("must refuse");
        assert!(err.contains("carries no `_index`"), "{err}");
    }

    #[test]
    fn a_hit_splits_into_envelope_and_fields() {
        let (envelope, fields) = split_document(
            &hit(vec![
                ("_index", Value::Str(Arc::from("events"))),
                ("_id", Value::Str(Arc::from("abc"))),
                ("_seq_no", Value::I64(45)),
                ("_primary_term", Value::I64(3)),
                (
                    "_source",
                    hit(vec![
                        ("status", Value::Str(Arc::from("open"))),
                        ("retries", Value::I64(4)),
                    ]),
                ),
            ]),
            Some("_source"),
        );
        assert_eq!(envelope["_id"], json!("abc"));
        // The fresh guard — the whole point of re-reading.
        assert_eq!(envelope["_seq_no"], json!(45));
        assert_eq!(envelope["_primary_term"], json!(3));
        assert!(envelope.get("_source").is_none());
        assert_eq!(fields["status"], json!("open"));
        assert_eq!(fields["retries"], json!(4));
    }

    #[test]
    fn a_hit_missing_its_root_reports_no_fields() {
        let (envelope, fields) = split_document(
            &hit(vec![("_id", Value::Str(Arc::from("abc")))]),
            Some("_source"),
        );
        assert_eq!(envelope["_id"], json!("abc"));
        assert_eq!(fields, json!({}));
    }

    #[test]
    fn a_rootless_hit_has_no_envelope() {
        let (envelope, fields) = split_document(&hit(vec![("id", Value::I64(7))]), None);
        assert_eq!(envelope, json!({}));
        assert_eq!(fields["id"], json!(7));
    }

    #[test]
    fn only_a_listed_engine_names_its_object_field() {
        assert_eq!(
            crate::query::object_path_field("elasticsearch"),
            Some("_index")
        );
        assert_eq!(crate::query::object_path_field("postgres"), None);
    }

    // ---- FFI-boundary error paths (no socket, no live server) ----------

    fn core() -> *mut DatagrepCore {
        let path = CString::new(":memory:").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let core = unsafe { crate::core::datagrep_core_new(path.as_ptr(), &mut err) };
        assert!(!core.is_null());
        core
    }

    #[test]
    fn a_null_core_is_an_error() {
        let profile = CString::new("p").unwrap();
        let body = CString::new("{\"documents\":[]}").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = unsafe {
            datagrep_reread_documents(
                std::ptr::null_mut(),
                profile.as_ptr(),
                body.as_ptr(),
                &mut err,
            )
        };
        assert!(out.is_null());
        assert!(!err.is_null());
        unsafe { crate::core::datagrep_string_free(err) };
    }

    #[test]
    fn malformed_addresses_json_sets_err_and_returns_null() {
        let core = core();
        let profile = CString::new("anything").unwrap();
        let body = CString::new("{not valid json").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out =
            unsafe { datagrep_reread_documents(core, profile.as_ptr(), body.as_ptr(), &mut err) };
        assert!(out.is_null());
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("not a valid document address list"), "{msg}");
        unsafe { crate::core::datagrep_string_free(err) };
        unsafe { crate::core::datagrep_core_free(core) };
    }

    #[test]
    fn a_valid_list_on_an_unknown_profile_is_an_error() {
        let core = core();
        let profile = CString::new("does-not-exist").unwrap();
        let body = CString::new("{\"documents\":[]}").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out =
            unsafe { datagrep_reread_documents(core, profile.as_ptr(), body.as_ptr(), &mut err) };
        assert!(out.is_null());
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("no profile named"), "{msg}");
        unsafe { crate::core::datagrep_string_free(err) };
        unsafe { crate::core::datagrep_core_free(core) };
    }

    #[test]
    fn a_null_addresses_json_is_an_error() {
        let core = core();
        let profile = CString::new("p").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = unsafe {
            datagrep_reread_documents(core, profile.as_ptr(), std::ptr::null(), &mut err)
        };
        assert!(out.is_null());
        assert!(!err.is_null());
        unsafe { crate::core::datagrep_string_free(err) };
        unsafe { crate::core::datagrep_core_free(core) };
    }
}
