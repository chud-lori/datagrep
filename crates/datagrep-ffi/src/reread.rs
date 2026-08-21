//! `datagrep_reread_documents` — the read half of a version conflict.
//!
//! A guarded write that comes back `version_conflict_engine_exception` has told
//! the user one thing only: the document moved after they loaded it. It has not
//! said *how* it moved, and without that the only offers left are "try again"
//! (which is the clobber the guard exists to prevent) and "give up". So the
//! conflict flow re-reads the document and puts three readings side by side —
//! the value that was loaded, the value on the server now, and the value that
//! was typed — which is what makes *rebase* (re-apply my edits onto the current
//! version) and *discard mine* real choices rather than a coin toss.
//!
//! This is deliberately **not** `retry_on_conflict`. Nothing here re-sends
//! anything; it reads, and a human decides.
//!
//! ## Addressing, without the frontend knowing the engine
//!
//! The input is the same `key` a [`datagrep_api::request::Mutation`] carries —
//! identity fields paired with this document's values — so the UI re-uses the
//! address it already staged instead of building a second one. Turning that
//! back into a read means splitting it: one identity field names the *object*
//! (`_index` for a hit), the rest are equality terms inside it. Which field is
//! which is engine knowledge, and it stays in Rust, in the same table as the
//! guard field names ([`crate::query::object_path_field`]).
//!
//! The read itself is an ordinary `Op::Scan`, so the guard values come back the
//! way they come back everywhere else: the Elasticsearch cursor asks every page
//! for `seq_no_primary_term`, which is exactly the fresh `_seq_no`/`_primary_term`
//! a rebase needs to re-guard against.
//!
//! ## The shape it returns
//!
//! ```json
//! { "documents": [
//!     { "found": true,
//!       "envelope": {"_index":"events","_id":"abc","_seq_no":45,"_primary_term":3},
//!       "fields":   {"status":"open","retries":4} },
//!     { "found": false },
//!     { "found": false, "error": "…" } ] }
//! ```
//!
//! One entry per address, **in the order they were sent** — the same
//! by-position contract the mutation report follows, and for the same reason:
//! matching by id would need this layer to know which identity field *is* the
//! id.

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

/// The batch of addresses to re-read, as the UI sends it.
#[derive(Debug, Deserialize)]
struct RereadBatch {
    documents: Vec<Address>,
}

/// One document's address: exactly the `key` of the mutation that conflicted.
#[derive(Debug, Deserialize)]
struct Address {
    key: Vec<(FieldPath, Value)>,
}

/// Read what the server holds now for each addressed document.
///
/// `addresses_json` is `{"documents":[{"key":[[FieldPath,Value],…]},…]}`, with
/// `FieldPath`/`Value` in the same serde spelling `datagrep_mutate` takes
/// (`[{"Field":"_id"}]`, `{"Str":"x"}`).
///
/// Blocks, like [`crate::mutate::datagrep_mutate`]: this is a discrete question
/// with an answer, not a stream. Returns an **owned** JSON `char*` (free it with
/// `datagrep_string_free`), or NULL with `*err_out` set when the batch as a
/// whole could not run — an unparseable input, an unknown profile, a connection
/// that could not be leased, an engine that has not said how a document is
/// addressed. A single document that is gone, or that could not be read, is an
/// entry in the result rather than an error: the other conflicts still need
/// resolving.
///
/// # Safety
/// `core` must come from `datagrep_core_new` and be unfreed; `profile` and
/// `addresses_json` must be NULL or valid NUL-terminated strings that outlive
/// the call; `err_out` must be NULL or point at a writable `char*`.
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
            // SAFETY: `core` is a live handle from `datagrep_core_new` and the two
            // strings are NULL or NUL-terminated, per this function's contract;
            // `core_ref`/`cstr` turn NULL (and non-UTF-8) into errors rather than
            // dereferencing them.
            let core = unsafe { core_ref(core) }?;
            let profile = unsafe { cstr(profile, "profile") }?;
            let addresses_json = unsafe { cstr(addresses_json, "addresses_json") }?;

            let batch: RereadBatch = serde_json::from_str(addresses_json)
                .map_err(|e| format!("addresses_json is not a valid document address list: {e}"))?;

            let rt = runtime()?;
            // Synchronous by design, exactly like the commit it follows. The
            // runtime is process-global and this is never called from a worker
            // thread, so `block_on` here is never "block_on from inside the
            // runtime" (see `runtime.rs`).
            let documents = rt.block_on(reread_all(core, profile, &batch.documents))?;

            serde_json::to_string(&json!({ "documents": documents }))
                .map(to_c_string)
                .map_err(|e| format!("could not serialize the re-read: {e}"))
        },
    )
}

/// Lease one connection and read each address through it, in order.
///
/// One scan per document rather than one scan for all of them: a conflicted
/// batch is small (it is what a human is about to read), and an `OR` over
/// identities would come back unordered, which would put this layer back to
/// matching answers to addresses by guessing which field is the id.
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
            // Per document, not per batch: one document that has been dropped
            // from the index must not cost the user the other conflicts.
            Err(why) => json!({ "found": false, "error": why }),
        });
    }
    Ok(documents)
}

/// One document, scanned by its own identity.
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
            // Two, not one: an identity that answers twice is not an identity,
            // and reporting that is worth more than picking the first hit.
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
    // Best-effort: releasing a server-side cursor early is courtesy here, not
    // correctness — the read is already in hand.
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

/// Split an address into the object it lives in and the terms that find it
/// inside that object.
///
/// An identity field this engine cannot address by name is refused rather than
/// dropped: a scan missing one term would come back with the wrong document,
/// which is precisely the mistake a conflict view exists to prevent.
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

/// A single named field, or `None` for anything a mutation key could not be
/// built from — the same rule `editing_facts` applies to identity.
fn plain_field(path: &FieldPath) -> Option<&str> {
    match path.segments() {
        [PathSeg::Field(name)] => Some(name),
        _ => None,
    }
}

/// The projection root this cursor declared, when it is one plain field name.
fn root_of(shape: &Shape) -> Option<String> {
    let Shape::Documents { root_hint, .. } = shape else {
        return None;
    };
    root_hint.as_ref().and_then(|p| match p.segments() {
        [PathSeg::Field(name)] => Some(name.to_string()),
        _ => None,
    })
}

/// Split one hit the way the grid splits it: the projected root is the document
/// the user wrote, everything outside it is the envelope that identifies and
/// guards it.
///
/// With no root there is nothing outside the document, so the envelope is empty
/// rather than invented — and a UI looking there for a fresh guard finds none
/// and says so, which is the honest answer for an engine whose guard fields are
/// ordinary columns.
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
    let fields = doc.get(root).map(value_to_json).unwrap_or(json!({}));
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

    /// **The bytes the macOS grid actually sends**, captured verbatim from
    /// `datagrep-app --dump-reread`.
    ///
    /// Same pinning as the mutation batch next door, for the same reason: the
    /// UI hand-encodes serde's spelling, there is no Swift test target, and a
    /// wrong bracket here would surface only when someone already has a
    /// conflict on screen. The key is re-used from the staged mutation, so this
    /// also proves the two encoders agree.
    #[test]
    fn the_json_the_macos_grid_sends_parses_into_addresses() {
        // Regenerate with: cd ui/macos && swift build -c release &&
        //                  ./.build/release/datagrep-app --dump-reread
        let json = r#"{"documents":[{"key":[[[{"Field":"_index"}],{"Str":"events"}],[[{"Field":"_id"}],{"Str":"abc"}],[[{"Field":"_routing"}],{"Str":"tenant-7"}]]},{"key":[[[{"Field":"_index"}],{"Str":"events"}],[[{"Field":"_id"}],{"Str":"gone"}]]}]}"#;

        let batch: RereadBatch =
            serde_json::from_str(json).expect("the grid's address list must parse");
        assert_eq!(batch.documents.len(), 2);

        // And it splits into exactly the scan a re-read runs: the index is the
        // object, the rest are terms inside it.
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

    /// An address with nothing but the object names an index, not a document,
    /// and is refused rather than scanned (which would return a stranger).
    #[test]
    fn an_address_with_no_terms_is_refused() {
        let err = split_address(
            &address(vec![("_index", Value::Str(Arc::from("events")))]),
            "_index",
        )
        .expect_err("must refuse");
        assert!(err.contains("names an object"), "{err}");
    }

    /// An address with no object cannot say where to look, and says so.
    #[test]
    fn an_address_with_no_object_is_refused() {
        let err = split_address(
            &address(vec![("_id", Value::Str(Arc::from("abc")))]),
            "_index",
        )
        .expect_err("must refuse");
        assert!(err.contains("carries no `_index`"), "{err}");
    }

    /// The hit splits the way the grid splits it: the root is the document,
    /// everything else is the envelope a rebase re-guards against.
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

    /// A hit fetched without its root has no fields to show, and reports an
    /// empty document rather than pretending the envelope is one.
    #[test]
    fn a_hit_missing_its_root_reports_no_fields() {
        let (envelope, fields) = split_document(
            &hit(vec![("_id", Value::Str(Arc::from("abc")))]),
            Some("_source"),
        );
        assert_eq!(envelope["_id"], json!("abc"));
        assert_eq!(fields, json!({}));
    }

    /// With no root declared, nothing is outside the document — so the
    /// envelope is empty rather than a copy of it.
    #[test]
    fn a_rootless_hit_has_no_envelope() {
        let (envelope, fields) = split_document(&hit(vec![("id", Value::I64(7))]), None);
        assert_eq!(envelope, json!({}));
        assert_eq!(fields["id"], json!(7));
    }

    /// Only engines that have said how a document is addressed can be re-read;
    /// the rest are refused by name rather than guessed at.
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

    /// A NULL core is an error string, not a crash.
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

    /// Malformed JSON is refused before any profile lookup or socket.
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

    /// A valid list against an unknown profile fails at profile resolution —
    /// a clear error, NULL return, no panic, still no socket.
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

    /// A NULL `addresses_json` is a checked error, not a deref.
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
