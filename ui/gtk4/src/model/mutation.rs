use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::{json, Value as Json};

#[derive(Debug, Clone, PartialEq)]
pub enum MutationValue {
    Str(String),
    I64(i64),
    F64(f64),
    Bool(bool),
    Null,
}

impl MutationValue {
    /// The `datagrep_api::Value` externally-tagged spelling the ABI parses.
    pub fn abi_json(&self) -> Json {
        match self {
            MutationValue::Str(s) => json!({ "Str": s }),
            MutationValue::I64(i) => json!({ "I64": i }),
            MutationValue::F64(d) => json!({ "F64": d }),
            MutationValue::Bool(b) => json!({ "Bool": b }),
            MutationValue::Null => json!("Null"),
        }
    }

    pub fn display(&self) -> String {
        match self {
            MutationValue::Str(s) => s.clone(),
            MutationValue::I64(i) => i.to_string(),
            MutationValue::F64(d) => d.to_string(),
            MutationValue::Bool(true) => "true".to_owned(),
            MutationValue::Bool(false) => "false".to_owned(),
            MutationValue::Null => "NULL".to_owned(),
        }
    }

    /// What kind of value this is, in the words the editor puts under the entry.
    pub fn type_name(&self) -> &'static str {
        match self {
            MutationValue::Str(_) => "text",
            MutationValue::I64(_) => "a whole number",
            MutationValue::F64(_) => "a number",
            MutationValue::Bool(_) => "true or false",
            MutationValue::Null => "empty",
        }
    }

    pub fn decode(value: &Json) -> Option<Self> {
        match value {
            Json::String(s) => Some(MutationValue::Str(s.clone())),
            Json::Null => Some(MutationValue::Null),
            Json::Bool(b) => Some(MutationValue::Bool(*b)),
            Json::Number(n) => match n.as_i64() {
                Some(i) => Some(MutationValue::I64(i)),
                None => n.as_f64().map(MutationValue::F64),
            },
            _ => None,
        }
    }

    /// `datagrep_rows_cell_detail_json` hands back one bare JSON value.
    pub fn decode_fragment(json: &str) -> Option<Self> {
        Self::decode(&serde_json::from_str::<Json>(json).ok()?)
    }

    /// Coerces to the loaded value's type — a string would silently retype the field.
    pub fn typed_like(text: &str, loaded: Option<&MutationValue>) -> Result<Self, String> {
        let trimmed = text.trim();
        match loaded.unwrap_or(&MutationValue::Null) {
            MutationValue::Str(_) => Ok(MutationValue::Str(text.to_owned())),
            MutationValue::I64(_) => trimmed
                .parse::<i64>()
                .map(MutationValue::I64)
                .map_err(|_| format!("this field holds a whole number; “{text}” is not one")),
            MutationValue::F64(_) => trimmed
                .parse::<f64>()
                .map(MutationValue::F64)
                .map_err(|_| format!("this field holds a number; “{text}” is not one")),
            MutationValue::Bool(_) => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Ok(MutationValue::Bool(true)),
                "false" | "no" | "0" => Ok(MutationValue::Bool(false)),
                _ => Err(format!(
                    "this field holds true or false; “{text}” is neither"
                )),
            },
            // No type to preserve: read the text the way JSON would.
            MutationValue::Null => Ok(match trimmed {
                "true" => MutationValue::Bool(true),
                "false" => MutationValue::Bool(false),
                _ => match trimmed.parse::<i64>() {
                    Ok(i) => MutationValue::I64(i),
                    Err(_) => match trimmed.parse::<f64>() {
                        Ok(d) => MutationValue::F64(d),
                        Err(_) => MutationValue::Str(text.to_owned()),
                    },
                },
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldValue {
    pub field: String,
    pub value: MutationValue,
}

/// The engine's `editable` block: how this result's documents are named and guarded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditableResult {
    pub identity: Vec<String>,
    pub guard: Vec<String>,
    pub root: String,
    pub atomic_batch: bool,
}

impl<'de> Deserialize<'de> for EditableResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = Json::deserialize(deserializer)?;
        Ok(Self {
            identity: strings(&json, "identity"),
            guard: strings(&json, "guard"),
            root: text(&json, "root"),
            atomic_batch: flag(&json, "atomic_batch"),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Address {
    pub id: String,
    pub key: Vec<FieldValue>,
    pub expect: Vec<FieldValue>,
}

impl EditableResult {
    /// The key and the compare-and-swap guard, read out of one row's envelope.
    pub fn address(&self, envelope: &Json) -> Result<Address, String> {
        let mut key = Vec::new();
        for field in &self.identity {
            match envelope.get(field).and_then(MutationValue::decode) {
                Some(MutationValue::Null) | None => continue,
                Some(value) => key.push(FieldValue {
                    field: field.clone(),
                    value,
                }),
            }
        }
        if key.is_empty() {
            return Err(format!(
                "this row carries none of the fields that identify a document ({}), so there is \
                 nothing to address a write to",
                self.identity.join(", ")
            ));
        }
        let mut expect = Vec::new();
        for field in &self.guard {
            match envelope.get(field).and_then(MutationValue::decode) {
                Some(MutationValue::Null) | None => {
                    return Err(format!(
                        "this document was loaded without `{field}`, so an edit to it could only \
                         be sent unguarded — and an unguarded write would overwrite whatever the \
                         server holds now"
                    ))
                }
                Some(value) => expect.push(FieldValue {
                    field: field.clone(),
                    value,
                }),
            }
        }
        let id = key
            .iter()
            .map(|fv| format!("{}={}", fv.field, fv.value.display()))
            .collect::<Vec<_>>()
            .join("\u{1}");
        Ok(Address { id, key, expect })
    }
}

#[derive(Debug, Clone, Default)]
pub struct DocumentMutation {
    pub path: Vec<String>,
    pub key: Vec<FieldValue>,
    pub expect: Vec<FieldValue>,
    pub sets: Vec<FieldValue>,
    pub is_delete: bool,
}

fn pairs(values: &[FieldValue]) -> Json {
    Json::Array(
        values
            .iter()
            .map(|fv| json!([[{ "Field": fv.field }], fv.value.abi_json()]))
            .collect(),
    )
}

impl DocumentMutation {
    pub fn abi_json(&self) -> Json {
        let mut body = json!({
            "path": self.path,
            "key": pairs(&self.key),
            "expect": pairs(&self.expect),
        });
        if self.is_delete {
            return json!({ "Delete": body });
        }
        body["sets"] = pairs(&self.sets);
        json!({ "Update": body })
    }
}

pub fn mutation_batch_json(mutations: &[DocumentMutation]) -> String {
    let list: Vec<Json> = mutations.iter().map(DocumentMutation::abi_json).collect();
    json!({ "mutations": list }).to_string()
}

#[derive(Debug, Clone, Default)]
pub struct DocumentAddress {
    pub key: Vec<FieldValue>,
}

pub fn document_address_batch_json(addresses: &[DocumentAddress]) -> String {
    let list: Vec<Json> = addresses
        .iter()
        .map(|a| json!({ "key": pairs(&a.key) }))
        .collect();
    json!({ "documents": list }).to_string()
}

/// One field as the server holds it now — which may be a shape a grid cell cannot show.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerValue {
    Value(MutationValue),
    Nested(&'static str),
    Missing,
}

impl ServerValue {
    pub fn decode(value: &Json) -> Self {
        match MutationValue::decode(value) {
            Some(value) => ServerValue::Value(value),
            None => ServerValue::Nested(match value {
                Json::Array(_) => "an array",
                Json::Object(_) => "an object",
                _ => "a value this view cannot show",
            }),
        }
    }

    pub fn display(&self) -> String {
        match self {
            ServerValue::Value(value) => value.display(),
            ServerValue::Nested(what) => (*what).to_owned(),
            ServerValue::Missing => "—".to_owned(),
        }
    }

    pub fn mutation_value(&self) -> Option<&MutationValue> {
        match self {
            ServerValue::Value(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ServerDocument {
    pub found: bool,
    pub error: String,
    pub envelope: HashMap<String, ServerValue>,
    pub fields: HashMap<String, ServerValue>,
}

fn server_map(json: &Json, key: &str) -> HashMap<String, ServerValue> {
    match json.get(key).and_then(Json::as_object) {
        Some(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), ServerValue::decode(v)))
            .collect(),
        None => HashMap::new(),
    }
}

impl ServerDocument {
    pub fn decode_all(json: &str) -> Result<Vec<Self>, String> {
        let parsed: Json = serde_json::from_str(json)
            .map_err(|e| format!("the re-read was not a document list: {e}"))?;
        let Some(documents) = parsed.get("documents").and_then(Json::as_array) else {
            return Err(format!("the re-read was not a document list: {json}"));
        };
        Ok(documents
            .iter()
            .map(|d| Self {
                found: flag(d, "found"),
                error: text(d, "error"),
                envelope: server_map(d, "envelope"),
                fields: server_map(d, "fields"),
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MutationOutcome {
    Applied,
    #[default]
    Failed,
    NotAttempted,
}

#[derive(Debug, Clone, Default)]
pub struct MutationRow {
    pub op: String,
    pub index: String,
    pub document_id: String,
    pub routing: String,
    pub outcome: MutationOutcome,
    pub seq_no: Option<i64>,
    pub primary_term: Option<i64>,
    pub conflict: bool,
    pub error_code: String,
    pub error: String,
    pub forced_refresh: bool,
}

/// A non-fatal message the engine sent along with the batch. Shown, never swallowed.
#[derive(Debug, Clone, Default)]
pub struct MutationNotice {
    pub severity: String,
    pub code: String,
    pub message: String,
}

impl MutationNotice {
    pub fn is_warning(&self) -> bool {
        self.severity == "warning"
    }
}

#[derive(Debug, Clone, Default)]
pub struct MutationReport {
    pub rows: Vec<MutationRow>,
    pub notices: Vec<MutationNotice>,
    pub applied: u32,
    pub failed: u32,
    pub not_attempted: u32,
    pub conflicts: u32,
}

impl MutationReport {
    pub fn is_clean(&self) -> bool {
        self.failed == 0 && self.not_attempted == 0
    }

    pub fn decode(json: &str) -> Result<Self, String> {
        let parsed: Json = serde_json::from_str(json)
            .map_err(|e| format!("the mutation report was not an object: {e}"))?;
        if !parsed.is_object() {
            return Err(format!("the mutation report was not an object: {json}"));
        }
        let rows = parsed
            .get("rows")
            .and_then(Json::as_array)
            .map(|rows| rows.iter().map(decode_report_row).collect())
            .unwrap_or_default();
        let notices = parsed
            .get("notices")
            .and_then(Json::as_array)
            .map(|notices| notices.iter().filter_map(decode_notice).collect())
            .unwrap_or_default();
        let summary = parsed.get("summary").cloned().unwrap_or(Json::Null);
        Ok(Self {
            rows,
            notices,
            applied: count(&summary, "applied"),
            failed: count(&summary, "failed"),
            not_attempted: count(&summary, "not_attempted"),
            conflicts: count(&summary, "conflicts"),
        })
    }
}

fn decode_report_row(row: &Json) -> MutationRow {
    let op = match text(row, "op") {
        op if op.is_empty() => "?".to_owned(),
        op => op,
    };
    MutationRow {
        op,
        index: text(row, "_index"),
        document_id: text(row, "_id"),
        routing: text(row, "_routing"),
        outcome: match text(row, "outcome").as_str() {
            "applied" => MutationOutcome::Applied,
            "not attempted" => MutationOutcome::NotAttempted,
            _ => MutationOutcome::Failed,
        },
        seq_no: row.get("_seq_no").and_then(Json::as_i64),
        primary_term: row.get("_primary_term").and_then(Json::as_i64),
        conflict: flag(row, "conflict"),
        error_code: text(row, "error_code"),
        error: text(row, "error"),
        forced_refresh: flag(row, "forced_refresh"),
    }
}

fn decode_notice(notice: &Json) -> Option<MutationNotice> {
    let message = text(notice, "message");
    if message.is_empty() {
        return None;
    }
    Some(MutationNotice {
        severity: match text(notice, "severity") {
            s if s.is_empty() => "info".to_owned(),
            s => s,
        },
        code: text(notice, "code"),
        message,
    })
}

// A null the engine emits for an absent field must not fail the whole decode.
fn text(json: &Json, key: &str) -> String {
    json.get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn flag(json: &Json, key: &str) -> bool {
    json.get(key).and_then(Json::as_bool).unwrap_or(false)
}

fn count(json: &Json, key: &str) -> u32 {
    json.get(key).and_then(Json::as_u64).unwrap_or(0) as u32
}

fn strings(json: &Json, key: &str) -> Vec<String> {
    match json.get(key).and_then(Json::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_value(s: &str) -> MutationValue {
        MutationValue::Str(s.to_owned())
    }

    #[test]
    fn the_batch_this_grid_builds_parses_as_the_engines_mutation_batch() {
        let update = DocumentMutation {
            path: Vec::new(),
            key: vec![
                FieldValue {
                    field: "_index".to_owned(),
                    value: str_value("events"),
                },
                FieldValue {
                    field: "_id".to_owned(),
                    value: str_value("abc"),
                },
            ],
            expect: vec![
                FieldValue {
                    field: "_seq_no".to_owned(),
                    value: MutationValue::I64(41),
                },
                FieldValue {
                    field: "_primary_term".to_owned(),
                    value: MutationValue::I64(3),
                },
            ],
            sets: vec![
                FieldValue {
                    field: "status".to_owned(),
                    value: str_value("done"),
                },
                FieldValue {
                    field: "score".to_owned(),
                    value: MutationValue::F64(1.5),
                },
                FieldValue {
                    field: "archived".to_owned(),
                    value: MutationValue::Bool(true),
                },
            ],
            is_delete: false,
        };
        let delete = DocumentMutation {
            key: vec![FieldValue {
                field: "_id".to_owned(),
                value: str_value("gone"),
            }],
            is_delete: true,
            ..DocumentMutation::default()
        };

        let json = mutation_batch_json(&[update, delete]);
        let batch: datagrep_api::request::MutationBatch =
            serde_json::from_str(&json).expect("the engine must parse what this grid sends");
        assert_eq!(batch.mutations.len(), 2);
        // Wraps into exactly the request the driver matches on.
        let request = datagrep_api::request::Request::Op(datagrep_api::request::Op::Mutate(batch));
        assert!(matches!(
            request,
            datagrep_api::request::Request::Op(datagrep_api::request::Op::Mutate(_))
        ));
    }

    #[test]
    fn a_delete_carries_no_sets_key_at_all() {
        let json = mutation_batch_json(&[DocumentMutation {
            key: vec![FieldValue {
                field: "_id".to_owned(),
                value: str_value("x"),
            }],
            is_delete: true,
            ..DocumentMutation::default()
        }]);
        assert!(!json.contains("sets"), "{json}");
        assert!(json.contains("\"Delete\""), "{json}");
    }

    #[test]
    fn a_field_pair_is_a_field_path_beside_a_tagged_value() {
        let json = mutation_batch_json(&[DocumentMutation {
            path: Vec::new(),
            key: vec![FieldValue {
                field: "_id".to_owned(),
                value: str_value("abc"),
            }],
            expect: vec![FieldValue {
                field: "_seq_no".to_owned(),
                value: MutationValue::I64(41),
            }],
            sets: vec![FieldValue {
                field: "status".to_owned(),
                value: MutationValue::Null,
            }],
            is_delete: false,
        }]);
        let parsed: Json = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed,
            json!({"mutations":[{"Update":{
                "path": [],
                "key": [[[{"Field":"_id"}], {"Str":"abc"}]],
                "expect": [[[{"Field":"_seq_no"}], {"I64":41}]],
                // A null is the bare tag, not an object — the one value spelled differently.
                "sets": [[[{"Field":"status"}], "Null"]]}}]})
        );
    }

    #[test]
    fn an_address_batch_is_the_shape_reread_documents_takes() {
        let json = document_address_batch_json(&[DocumentAddress {
            key: vec![FieldValue {
                field: "_index".to_owned(),
                value: str_value("events"),
            }],
        }]);
        assert_eq!(
            json,
            r#"{"documents":[{"key":[[[{"Field":"_index"}],{"Str":"events"}]]}]}"#
        );
    }

    #[test]
    fn an_edit_is_coerced_to_the_type_the_cell_was_loaded_with() {
        assert_eq!(
            MutationValue::typed_like("7", Some(&MutationValue::I64(1))),
            Ok(MutationValue::I64(7))
        );
        assert!(MutationValue::typed_like("seven", Some(&MutationValue::I64(1))).is_err());
        assert_eq!(
            MutationValue::typed_like("7", Some(&str_value("x"))),
            Ok(str_value("7")),
            "a text field stays text however numeric the input looks"
        );
        assert_eq!(
            MutationValue::typed_like("no", Some(&MutationValue::Bool(true))),
            Ok(MutationValue::Bool(false))
        );
    }

    #[test]
    fn a_null_cell_is_read_the_way_json_would_read_it() {
        assert_eq!(
            MutationValue::typed_like("12", None),
            Ok(MutationValue::I64(12))
        );
        assert_eq!(
            MutationValue::typed_like("1.5", None),
            Ok(MutationValue::F64(1.5))
        );
        assert_eq!(
            MutationValue::typed_like("true", None),
            Ok(MutationValue::Bool(true))
        );
        assert_eq!(
            MutationValue::typed_like("hello", None),
            Ok(str_value("hello"))
        );
    }

    #[test]
    fn a_row_without_the_guard_refuses_to_produce_an_address() {
        let editable = EditableResult {
            identity: vec!["_index".to_owned(), "_id".to_owned()],
            guard: vec!["_seq_no".to_owned(), "_primary_term".to_owned()],
            root: "_source".to_owned(),
            atomic_batch: false,
        };
        let envelope = json!({"_index":"events","_id":"abc","_seq_no":41});
        let why = editable
            .address(&envelope)
            .expect_err("a missing _primary_term must refuse");
        assert!(why.contains("_primary_term"), "{why}");
        assert!(why.contains("unguarded"), "{why}");
    }

    #[test]
    fn an_address_ids_a_document_by_every_identity_field_it_carries() {
        let editable = EditableResult {
            identity: vec!["_index".to_owned(), "_id".to_owned(), "_routing".to_owned()],
            guard: vec!["_seq_no".to_owned()],
            ..EditableResult::default()
        };
        let address = editable
            .address(&json!({"_index":"events","_id":"abc","_routing":null,"_seq_no":41}))
            .expect("index and id are enough");
        assert_eq!(
            address.key.len(),
            2,
            "a null _routing is not part of the key"
        );
        assert_eq!(address.id, "_index=events\u{1}_id=abc");
        assert_eq!(address.expect.len(), 1);
    }

    #[test]
    fn a_row_with_no_identity_at_all_says_so_rather_than_addressing_nothing() {
        let editable = EditableResult {
            identity: vec!["_id".to_owned()],
            ..EditableResult::default()
        };
        let why = editable
            .address(&json!({"other":1}))
            .expect_err("no identity");
        assert!(why.contains("nothing to address a write to"), "{why}");
    }

    #[test]
    fn the_report_decodes_outcomes_counts_and_notices() {
        let report = MutationReport::decode(
            r#"{"rows":[
                 {"op":"update","_index":"events","_id":"a","outcome":"applied","_seq_no":42,
                  "forced_refresh":true},
                 {"op":"update","_index":"events","_id":"b","outcome":"failed","conflict":true,
                  "error_code":"version_conflict_engine_exception","error":"current version is newer"},
                 {"op":"delete","_index":"events","_id":"c","outcome":"not attempted"}],
               "notices":[{"severity":"warning","code":"es.bulk.partial","message":"applied 1 of 3"},
                          {"severity":"info","code":null,"message":""}],
               "summary":{"applied":1,"failed":1,"not_attempted":1,"conflicts":1}}"#,
        )
        .expect("a report decodes");

        assert_eq!(report.rows.len(), 3);
        assert_eq!(report.rows[0].outcome, MutationOutcome::Applied);
        assert_eq!(report.rows[0].seq_no, Some(42));
        assert!(report.rows[0].forced_refresh);
        assert!(report.rows[1].conflict);
        assert_eq!(report.rows[2].outcome, MutationOutcome::NotAttempted);
        assert_eq!(report.notices.len(), 1, "an empty message is not a notice");
        assert!(report.notices[0].is_warning());
        assert_eq!(report.conflicts, 1);
        assert!(!report.is_clean());
    }

    #[test]
    fn a_null_id_in_the_report_is_empty_text_rather_than_a_failed_decode() {
        let report = MutationReport::decode(
            r#"{"rows":[{"op":"delete","_index":null,"_id":null,"outcome":"applied"}],
                "notices":[],"summary":{"applied":1}}"#,
        )
        .expect("nulls must not lose the report of a write that already happened");
        assert_eq!(report.rows[0].document_id, "");
        assert_eq!(report.applied, 1);
    }

    #[test]
    fn a_re_read_decodes_envelope_fields_and_the_nested_ones_it_cannot_show() {
        let documents = ServerDocument::decode_all(
            r#"{"documents":[
                 {"found":true,"envelope":{"_seq_no":44,"_primary_term":3},
                  "fields":{"status":"claimed","tags":["a"],"note":null}},
                 {"found":false},
                 {"found":false,"error":"more than one document answers to this identity"}]}"#,
        )
        .expect("a re-read decodes");

        assert_eq!(documents.len(), 3);
        assert_eq!(
            documents[0].envelope.get("_seq_no"),
            Some(&ServerValue::Value(MutationValue::I64(44)))
        );
        assert_eq!(
            documents[0].fields.get("tags"),
            Some(&ServerValue::Nested("an array"))
        );
        assert_eq!(
            documents[0].fields.get("note"),
            Some(&ServerValue::Value(MutationValue::Null))
        );
        assert!(!documents[1].found && documents[1].error.is_empty());
        assert!(!documents[2].error.is_empty());
    }

    #[test]
    fn a_missing_field_reads_as_an_em_dash_and_carries_no_value_to_rebase_onto() {
        assert_eq!(ServerValue::Missing.display(), "—");
        assert!(ServerValue::Missing.mutation_value().is_none());
        assert!(ServerValue::Nested("an object").mutation_value().is_none());
    }

    #[test]
    fn the_editable_block_decodes_out_of_a_status_snapshot() {
        let editable: EditableResult = serde_json::from_str(
            r#"{"identity":["_index","_id"],"guard":["_seq_no","_primary_term"],
                "root":"_source","atomic_batch":true}"#,
        )
        .expect("the engine's editable block");
        assert_eq!(editable.identity, ["_index", "_id"]);
        assert_eq!(editable.guard, ["_seq_no", "_primary_term"]);
        assert_eq!(editable.root, "_source");
        assert!(editable.atomic_batch);
    }
}
