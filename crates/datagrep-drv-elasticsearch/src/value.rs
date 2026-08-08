//! `serde_json::Value` -> `datagrep_api::Value`, and the mapping-aware
//! precision rules that keep a `long` or a `scaled_float` from silently
//! becoming a lossy `f64` (design §3.1: "never lose bytes", "decimals stay
//! strings").
//!
//! # `Absent` vs `Null` — by construction
//!
//! Nothing in this module ever *invents* a value. A hit's `_source` is
//! converted by walking the JSON object's own keys, so a field the document
//! does not carry is simply never emitted; `Document::get_path` then returns
//! `None` and the core renders [`Value::Absent`]. A JSON `null` that really is
//! in `_source` becomes [`Value::Null`]. The two can therefore never be
//! confused — see `absent_is_never_synthesised_null` in the tests.
//!
//! # Numbers
//!
//! `serde_json` parses every integer that fits `i64`/`u64` exactly, so
//! Elasticsearch `long` and `unsigned_long` (whose domains are exactly `i64`
//! and `u64`) round-trip without loss through [`Value::I64`]/[`Value::U64`].
//! Two cases still need care, and both are resolved by consulting the index
//! mapping:
//!
//! - **`scaled_float`** is a fixed-point decimal stored as a scaled integer.
//!   Rendering it as an `f64` produces the classic `123.45600000000002`. It
//!   becomes [`Value::Decimal`] carrying Rust's shortest round-tripping
//!   decimal text for the value the server sent.
//! - **A `long`/`unsigned_long` that did *not* parse as an integer** — it
//!   arrived in exponent form, or beyond `u64`, and `serde_json` fell back to
//!   `f64`. That fallback has already lost digits relative to the declared
//!   type, so it becomes [`Value::Decimal`] rather than an `f64` pretending to
//!   be exact.
//!
//! Everything else numeric stays `I64`/`U64`/`F64`: for a field mapped
//! `double`, an `f64` *is* the declared precision and promoting it to a string
//! would be its own kind of lie.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value as Json;

use datagrep_api::value::{Document, Value};

use crate::json::OrderedJson;

/// Elasticsearch field types this driver treats specially. Everything not
/// listed maps structurally (string -> `Str`, object -> `Document`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EsType {
    /// Fixed-point decimal — must not round-trip through `f64`.
    ScaledFloat,
    /// 64-bit signed / unsigned integers.
    Long,
    UnsignedLong,
    /// Base64-encoded bytes in `_source`.
    Binary,
    /// Anything else; carried so the field list and completions can show the
    /// engine's own type name.
    Other,
}

impl EsType {
    pub fn from_mapping_type(name: &str) -> Self {
        match name {
            "scaled_float" => EsType::ScaledFloat,
            "long" => EsType::Long,
            "unsigned_long" => EsType::UnsignedLong,
            "binary" => EsType::Binary,
            _ => EsType::Other,
        }
    }
}

/// Dotted-path -> declared Elasticsearch type, flattened from one index's
/// `_mapping`. Only ever fetched for the index actually being read (design
/// §5.1: never the whole cluster's mappings).
#[derive(Debug, Clone, Default)]
pub struct FieldTypes {
    types: HashMap<String, (EsType, Arc<str>)>,
}

impl FieldTypes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: impl Into<String>, native: &str) {
        self.types.insert(
            path.into(),
            (EsType::from_mapping_type(native), Arc::from(native)),
        );
    }

    pub fn get(&self, path: &str) -> Option<EsType> {
        self.types.get(path).map(|(t, _)| *t)
    }

    /// The engine's own type name for a path, for the inspector and the field
    /// list — always what the server said, never what we mapped it to.
    pub fn native(&self, path: &str) -> Option<Arc<str>> {
        self.types.get(path).map(|(_, n)| n.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Every mapped path, in no particular order.
    pub fn paths(&self) -> impl Iterator<Item = (&str, EsType, &Arc<str>)> {
        self.types.iter().map(|(p, (t, n))| (p.as_str(), *t, n))
    }

    /// Flatten an Elasticsearch `_mapping` response's `properties` object into
    /// dotted paths. Handles nested `properties` (object/nested fields) and
    /// records multi-fields (`fields`) under their `parent.sub` path, which is
    /// exactly how they are addressed in a query.
    pub fn from_properties(properties: &Json) -> Self {
        let mut out = Self::new();
        flatten_properties(properties, "", &mut out);
        out
    }
}

fn flatten_properties(properties: &Json, prefix: &str, out: &mut FieldTypes) {
    let Some(map) = properties.as_object() else {
        return;
    };
    for (name, def) in map {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(ty) = def.get("type").and_then(Json::as_str) {
            out.insert(path.clone(), ty);
        } else if def.get("properties").is_some() {
            // An object field with no explicit `type` is an implicit `object`.
            out.insert(path.clone(), "object");
        }
        if let Some(nested) = def.get("properties") {
            flatten_properties(nested, &path, out);
        }
        // Multi-fields: `title.keyword` etc.
        if let Some(sub) = def.get("fields") {
            flatten_properties(sub, &path, out);
        }
    }
}

/// Convert one JSON value to a [`Value`], consulting `types` for the declared
/// Elasticsearch type at `path` (dotted, relative to the document root).
///
/// `path` is `""` for the root itself. `types` may be empty — with no mapping
/// available every rule degrades to the structural mapping, which is lossless
/// for everything except the two precision cases documented at module level.
pub fn json_to_value(json: &OrderedJson, path: &str, types: &FieldTypes) -> Value {
    match json {
        OrderedJson::Null => Value::Null,
        OrderedJson::Bool(b) => Value::Bool(*b),
        OrderedJson::Number(n) => number_to_value(n, path, types),
        OrderedJson::String(s) => string_to_value(s, path, types),
        OrderedJson::Array(items) => {
            // Elasticsearch arrays are homogeneous by mapping and are not
            // addressable by index, so every element shares the parent's path.
            let converted: Vec<Value> = items
                .iter()
                .map(|item| json_to_value(item, path, types))
                .collect();
            Value::Array(Arc::from(converted))
        }
        OrderedJson::Object(fields) => {
            // Key order — and duplicate keys — are data (design §3.1), which
            // is why the input is `OrderedJson` and not `serde_json::Value`.
            let mut doc = Document::new();
            for (k, v) in fields {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                doc.push(k.as_str(), json_to_value(v, &child, types));
            }
            Value::Document(Arc::new(doc))
        }
    }
}

/// Convert a `serde_json::Value` — for the control-plane replies whose key
/// order was already lost by the time they got here (a `_cluster/health`
/// document, an `EXPLAIN` plan). Never used for `_source`.
pub fn serde_to_value(json: &Json, path: &str, types: &FieldTypes) -> Value {
    json_to_value(&OrderedJson::from_serde(json), path, types)
}

fn number_to_value(n: &serde_json::Number, path: &str, types: &FieldTypes) -> Value {
    let declared = types.get(path);

    // `scaled_float` is a decimal by definition: hand back the shortest
    // decimal text that round-trips, never an f64 the grid would render as
    // 123.45600000000002.
    if declared == Some(EsType::ScaledFloat) {
        return Value::Decimal(Arc::from(shortest_decimal(n).as_str()));
    }

    if let Some(i) = n.as_i64() {
        return Value::I64(i);
    }
    if let Some(u) = n.as_u64() {
        return Value::U64(u);
    }

    // Fell through to f64. For a field the mapping declares as a 64-bit
    // integer that means digits were already lost relative to the declared
    // type, so keep the text rather than the lossy float.
    if matches!(declared, Some(EsType::Long) | Some(EsType::UnsignedLong)) {
        return Value::Decimal(Arc::from(shortest_decimal(n).as_str()));
    }

    match n.as_f64() {
        Some(f) => Value::F64(f),
        // Unreachable for values produced by `serde_json`'s own parser, but
        // never lose the bytes if it ever happens.
        None => Value::Unsupported {
            type_name: Arc::from("number"),
            raw: datagrep_api::Bytes::from(n.to_string().into_bytes()),
            display: Arc::from(n.to_string().as_str()),
        },
    }
}

/// `serde_json::Number`'s `Display` is the shortest representation that
/// round-trips (it uses `ryu`/`itoa` under the hood), which is exactly the
/// decimal text we want to preserve.
fn shortest_decimal(n: &serde_json::Number) -> String {
    n.to_string()
}

fn string_to_value(s: &str, path: &str, types: &FieldTypes) -> Value {
    if types.get(path) == Some(EsType::Binary) {
        // A `binary` field is base64 in `_source`. Decode to real bytes so the
        // inspector shows a hex dump rather than an opaque base64 blob — and
        // if the server sent something that is not base64 after all, keep the
        // string exactly as received rather than losing it.
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s) {
            return Value::Bytes(datagrep_api::Bytes::from(bytes));
        }
    }
    Value::Str(Arc::from(s))
}

/// Convert a [`Value`] back to JSON, for compiling typed parameter and
/// predicate values into a query body. This is the only direction in which
/// caller-supplied values enter a request, and it produces *data* — never a
/// clause (see [`crate::filter`]).
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        // A filter that names a field the caller did not supply a value for is
        // a caller bug; JSON has no "absent" literal, so it degrades to null
        // rather than silently dropping the clause.
        Value::Absent => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::I64(i) => Json::Number((*i).into()),
        Value::U64(u) => Json::Number((*u).into()),
        Value::F64(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        // Decimals stay textual: Elasticsearch parses a JSON string into a
        // numeric field fine, and it is the only way to hand it digits an f64
        // cannot hold.
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Str(s) => Json::String(s.to_string()),
        Value::Bytes(b) => Json::String(base64::engine::general_purpose::STANDARD.encode(b)),
        Value::Date(days) => Json::Number((*days).into()),
        Value::Time { nanos } => Json::Number((*nanos).into()),
        Value::Timestamp { micros, .. } => {
            // Elasticsearch's `epoch_millis` is the only epoch format its date
            // parser accepts numerically.
            Json::Number((*micros / 1_000).into())
        }
        Value::Interval {
            months,
            days,
            nanos,
        } => serde_json::json!({
            "months": months, "days": days, "nanos": nanos
        }),
        Value::Uuid(bytes) => Json::String(format_uuid(bytes)),
        // Raw JSON text is re-parsed, not re-serialized, so key order and
        // number precision survive (design §3.1).
        Value::Json(text) => serde_json::from_str(text).unwrap_or(Json::String(text.to_string())),
        Value::Array(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Document(doc) => {
            let mut map = serde_json::Map::new();
            for (k, v) in doc.iter() {
                map.insert(k.to_string(), value_to_json(v));
            }
            Json::Object(map)
        }
        Value::Ref { key, .. } => Json::Array(key.iter().map(value_to_json).collect()),
        Value::Geo(geo) => geometry_to_json(geo),
        Value::Vector(v) => Json::Array(
            v.iter()
                .map(|f| {
                    serde_json::Number::from_f64(*f as f64)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                })
                .collect(),
        ),
        Value::Unsupported { display, .. } => Json::String(display.to_string()),
    }
}

fn geometry_to_json(geo: &datagrep_api::value::Geometry) -> Json {
    use datagrep_api::value::Geometry;
    match geo {
        // GeoJSON order is [lon, lat] — the same order Elasticsearch's
        // `geo_shape`/`geo_point` array form expects.
        Geometry::Point { x, y } => serde_json::json!({
            "type": "Point", "coordinates": [x, y]
        }),
        Geometry::LineString(pts) => serde_json::json!({
            "type": "LineString",
            "coordinates": pts.iter().map(|(x, y)| serde_json::json!([x, y])).collect::<Vec<_>>()
        }),
        Geometry::Polygon(rings) => serde_json::json!({
            "type": "Polygon",
            "coordinates": rings.iter().map(|r| {
                r.iter().map(|(x, y)| serde_json::json!([x, y])).collect::<Vec<_>>()
            }).collect::<Vec<_>>()
        }),
        Geometry::Raw { wkb } => {
            Json::String(base64::engine::general_purpose::STANDARD.encode(wkb))
        }
    }
}

fn format_uuid(b: &[u8; 16]) -> String {
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datagrep_api::value::FieldPath;

    fn mapping() -> FieldTypes {
        FieldTypes::from_properties(&serde_json::json!({
            "id":       { "type": "long" },
            "price":    { "type": "scaled_float", "scaling_factor": 100 },
            "ratio":    { "type": "double" },
            "blob":     { "type": "binary" },
            "big":      { "type": "unsigned_long" },
            "title":    { "type": "text", "fields": { "keyword": { "type": "keyword" } } },
            "address":  { "properties": { "city": { "type": "keyword" },
                                          "geo":  { "type": "geo_point" } } }
        }))
    }

    #[test]
    fn mapping_flattens_nested_and_multi_fields_to_dotted_paths() {
        let m = mapping();
        assert_eq!(m.get("id"), Some(EsType::Long));
        assert_eq!(m.get("address.city"), Some(EsType::Other));
        assert_eq!(m.native("address.city").as_deref(), Some("keyword"));
        assert_eq!(m.native("title.keyword").as_deref(), Some("keyword"));
        assert_eq!(m.native("address").as_deref(), Some("object"));
        assert_eq!(m.native("address.geo").as_deref(), Some("geo_point"));
        assert!(!m.is_empty());
    }

    #[test]
    fn integers_within_i64_are_exact_not_floats() {
        let m = mapping();
        let json = serde_json::json!({ "id": 9007199254740993_i64 });
        let v = json_to_value(&OrderedJson::from_serde(&json), "", &m);
        let Value::Document(doc) = &v else {
            panic!("expected document")
        };
        // 2^53 + 1: exactly the value an f64 cannot hold.
        assert_eq!(doc.get("id"), Some(&Value::I64(9_007_199_254_740_993)));
    }

    #[test]
    fn scaled_float_becomes_decimal_never_a_lossy_f64() {
        let m = mapping();
        // 123.456 is not representable in binary floating point.
        let json: Json = serde_json::from_str(r#"{"price": 123.456}"#).unwrap();
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        assert_eq!(
            doc.get("price"),
            Some(&Value::Decimal(Arc::from("123.456"))),
            "a scaled_float must keep its decimal text"
        );
        // The same literal on a `double` field legitimately stays an f64:
        // f64 *is* that field's declared precision.
        let json: Json = serde_json::from_str(r#"{"ratio": 123.456}"#).unwrap();
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        assert_eq!(doc.get("ratio"), Some(&Value::F64(123.456)));
    }

    #[test]
    fn long_beyond_f64_precision_stays_decimal_rather_than_losing_digits() {
        let m = mapping();
        // Written in exponent form, so serde_json parses it as f64 — beyond
        // what the declared `long` can survive as a float.
        let json: Json = serde_json::from_str(r#"{"id": 1.2345678901234567e19}"#).unwrap();
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        match doc.get("id") {
            Some(Value::Decimal(text)) => {
                // The point is not the exact rendering but that the value did
                // NOT land in an `F64` cell pretending to be an exact `long`.
                assert!(
                    text.parse::<f64>().is_ok(),
                    "the decimal text must still be a number: {text}"
                );
                assert!(
                    (text.parse::<f64>().unwrap() - 1.2345678901234567e19).abs() < 1e4,
                    "and it must be the same value: {text}"
                );
            }
            other => panic!("expected Decimal for a lossy long, got {other:?}"),
        }
        // A `double` holding the same magnitude legitimately stays an f64:
        // f64 *is* that field's declared precision.
        let json: Json = serde_json::from_str(r#"{"ratio": 1.2345678901234567e19}"#).unwrap();
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        assert!(matches!(doc.get("ratio"), Some(Value::F64(_))));
        // An unsigned_long inside u64 range is still exact.
        let json: Json = serde_json::from_str(r#"{"big": 18446744073709551615}"#).unwrap();
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        assert_eq!(doc.get("big"), Some(&Value::U64(u64::MAX)));
    }

    #[test]
    fn unmapped_numbers_still_split_integral_from_fractional() {
        let empty = FieldTypes::new();
        let json: Json = serde_json::from_str(r#"{"a": 7, "b": 7.5, "c": -3}"#).unwrap();
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &empty)
        else {
            panic!("expected document")
        };
        assert_eq!(doc.get("a"), Some(&Value::I64(7)));
        assert_eq!(doc.get("b"), Some(&Value::F64(7.5)));
        assert_eq!(doc.get("c"), Some(&Value::I64(-3)));
    }

    #[test]
    fn binary_fields_decode_to_bytes_and_never_lose_a_non_base64_string() {
        let m = mapping();
        let json = serde_json::json!({ "blob": "aGVsbG8=" });
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        assert_eq!(
            doc.get("blob"),
            Some(&Value::Bytes(datagrep_api::Bytes::from_static(b"hello")))
        );

        let json = serde_json::json!({ "blob": "!!! not base64 !!!" });
        let Value::Document(doc) = json_to_value(&OrderedJson::from_serde(&json), "", &m) else {
            panic!("expected document")
        };
        assert_eq!(
            doc.get("blob"),
            Some(&Value::Str(Arc::from("!!! not base64 !!!"))),
            "never lose bytes: an undecodable string stays exactly as received"
        );
    }

    /// The distinction this whole driver exists to keep truthful: a field
    /// missing from `_source` resolves to `Absent`, a field holding JSON
    /// `null` resolves to `Null`, and nothing here ever turns one into the
    /// other.
    #[test]
    fn absent_is_never_synthesised_null() {
        let m = mapping();
        let json: Json = serde_json::from_str(r#"{"a": null}"#).unwrap();
        let value = json_to_value(&OrderedJson::from_serde(&json), "", &m);
        let Value::Document(doc) = &value else {
            panic!("expected document")
        };
        assert_eq!(doc.get("a"), Some(&Value::Null), "explicit null is present");
        assert_eq!(doc.get("b"), None, "a missing key is simply not there");
        let missing: FieldPath = "b".parse().unwrap();
        assert_eq!(
            doc.get_path(&missing),
            None,
            "the core maps this None to Value::Absent"
        );
        assert_ne!(Value::Null, Value::Absent);
    }

    #[test]
    fn nested_objects_and_arrays_preserve_structure_and_key_order() {
        let m = mapping();
        // Parsed with the order-preserving parser, and deliberately written
        // reverse-alphabetically so a BTreeMap-backed parse would be caught.
        let json = OrderedJson::parse(
            r#"{"zebra":1,"address":{"city":"sg","geo":[1.5,2.5]},"tags":["a","b"]}"#,
        )
        .unwrap();
        let Value::Document(doc) = json_to_value(&json, "", &m) else {
            panic!("expected document")
        };
        let top: Vec<&str> = doc.iter().map(|(k, _)| &**k).collect();
        assert_eq!(
            top,
            vec!["zebra", "address", "tags"],
            "_source key order is the wire order, not alphabetical"
        );
        let city: FieldPath = "address.city".parse().unwrap();
        assert_eq!(doc.get_path(&city), Some(&Value::Str(Arc::from("sg"))));
        let tag1: FieldPath = "tags[1]".parse().unwrap();
        assert_eq!(doc.get_path(&tag1), Some(&Value::Str(Arc::from("b"))));
        match doc.get("address") {
            Some(Value::Document(a)) => {
                let keys: Vec<&str> = a.iter().map(|(k, _)| &**k).collect();
                assert_eq!(keys, vec!["city", "geo"], "key order is data");
            }
            other => panic!("expected nested document, got {other:?}"),
        }
    }

    #[test]
    fn value_to_json_round_trips_typed_scalars() {
        assert_eq!(value_to_json(&Value::I64(-5)), serde_json::json!(-5));
        assert_eq!(value_to_json(&Value::U64(5)), serde_json::json!(5));
        assert_eq!(value_to_json(&Value::Bool(true)), serde_json::json!(true));
        assert_eq!(
            value_to_json(&Value::Str(Arc::from("x"))),
            serde_json::json!("x")
        );
        assert_eq!(value_to_json(&Value::Null), Json::Null);
        assert_eq!(
            value_to_json(&Value::Decimal(Arc::from("1.10"))),
            serde_json::json!("1.10"),
            "decimals stay textual so trailing zeros survive"
        );
        assert_eq!(
            value_to_json(&Value::Uuid([0xab; 16])),
            serde_json::json!("abababab-abab-abab-abab-abababababab")
        );
        assert_eq!(
            value_to_json(&Value::Json(Arc::from(r#"{"k":1}"#))),
            serde_json::json!({"k": 1}),
            "raw JSON is re-parsed, not double-encoded"
        );
    }

    #[test]
    fn value_to_json_maps_documents_and_arrays_structurally() {
        let doc = Document::from_fields(vec![
            (Arc::from("a"), Value::I64(1)),
            (
                Arc::from("b"),
                Value::Array(Arc::from(vec![Value::Bool(false)])),
            ),
        ]);
        assert_eq!(
            value_to_json(&Value::Document(Arc::new(doc))),
            serde_json::json!({"a": 1, "b": [false]})
        );
    }
}
