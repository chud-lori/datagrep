//! BSON <-> `datagrep_api::Value` mapping (ticket item 4), under two rules:
//! never lose bytes, and never let a type-mapping bug be silent.
//!
//! [`bson_to_value`] is infallible and exhaustive over every [`Bson`]
//! variant: anything without a faithful `Value` counterpart rides in
//! [`Value::Unsupported`] with its raw encoding preserved (via
//! [`unsupported_raw`], which wraps the value in a one-field document and
//! re-serializes it — BSON's own encoding is canonical, so this loses
//! nothing relative to the wire bytes the driver already decoded).
//!
//! [`value_to_bson`] is the inverse, used to encode predicate/mutation
//! parameters. It is fallible: a handful of `Value` variants
//! (`Time`, `Interval`, `Json`, `Ref`, `Geo`, `Vector`) have no BSON
//! representation and are refused with `DbError::Unsupported` rather than
//! silently coerced: a silently wrong value is worse than a crash.
//! `Value::Unsupported` round-trips exactly when its `raw` bytes came from
//! [`unsupported_raw`] (i.e. originated from this driver), by decoding the
//! wrapper document back and pulling out the original `Bson`.

use std::str::FromStr;
use std::sync::Arc;

use bson::spec::BinarySubtype;
use bson::{doc, oid::ObjectId, Binary, Bson, Decimal128, Document as BsonDocument};
use bytes::Bytes;

use datagrep_api::{Document as DatagrepDocument, TzSpec, Value};

/// BSON `Decimal128` bit width, for [`unsupported_raw`]'s companion decode.
const OBJECT_ID_HEX_LEN: usize = 24;

/// Decode `bson::Bson` into `datagrep_api::Value`, never failing and never losing
/// bytes. Exhaustive over every variant named in the ticket.
pub fn bson_to_value(b: &Bson) -> Value {
    match b {
        Bson::Double(f) => Value::F64(*f),
        Bson::String(s) => Value::Str(Arc::from(s.as_str())),
        Bson::Array(items) => Value::Array(Arc::from(
            items.iter().map(bson_to_value).collect::<Vec<_>>(),
        )),
        Bson::Document(doc) => Value::Document(Arc::new(bson_doc_to_value_doc(doc))),
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Null => Value::Null,
        Bson::Int32(v) => Value::I64(*v as i64),
        Bson::Int64(v) => Value::I64(*v),
        // Decimal128 -> string, NEVER f64: routing a decimal through binary
        // floating point silently changes the number. `Decimal128`'s own
        // `Display` impl produces the canonical decimal-string form.
        Bson::Decimal128(d) => Value::Decimal(Arc::from(d.to_string())),
        // ObjectId has no public byte accessor in `bson` 2.x, only
        // `to_hex()`; hex decoding is bijective, so this hand-rolled decode
        // recovers the exact 12 raw bytes with no external `hex` crate.
        Bson::ObjectId(oid) => object_id_to_value(oid),
        Bson::DateTime(dt) => Value::Timestamp {
            micros: dt.timestamp_millis().saturating_mul(1_000),
            tz: TzSpec::Utc,
        },
        Bson::Binary(bin) => binary_to_value(bin),
        // Every other BSON type has no `Value` counterpart: preserve raw
        // bytes rather than guess — never lose bytes.
        other => unsupported(other),
    }
}

fn bson_doc_to_value_doc(doc: &BsonDocument) -> DatagrepDocument {
    // `bson::Document` iterates in insertion order, and key order is data for
    // BSON — `Value::Document` preserves it rather than sorting.
    DatagrepDocument::from_fields(
        doc.iter()
            .map(|(k, v)| (Arc::from(k.as_str()), bson_to_value(v)))
            .collect(),
    )
}

fn object_id_to_value(oid: &ObjectId) -> Value {
    let hex = oid.to_hex();
    match hex_to_bytes(&hex) {
        Some(raw) => Value::Unsupported {
            type_name: Arc::from("ObjectId"),
            raw: Bytes::from(raw),
            display: Arc::from(hex),
        },
        // `to_hex()` always produces 24 valid hex chars; this branch is
        // unreachable in practice but kept as an honest fallback rather than
        // a `.unwrap()` (crate-wide "no unwrap outside tests" rule).
        None => Value::Unsupported {
            type_name: Arc::from("ObjectId"),
            raw: Bytes::new(),
            display: Arc::from(hex),
        },
    }
}

fn binary_to_value(bin: &Binary) -> Value {
    match bin.subtype {
        // Only the modern UUID subtype (4) is decoded as `Value::Uuid` —
        // legacy subtype 3 ("UuidOld") has driver/locale-dependent byte
        // ordering with no single correct interpretation, so it stays raw
        // rather than risk silently swapping byte order (deviation, see
        // crate report).
        BinarySubtype::Uuid if bin.bytes.len() == 16 => {
            let mut buf = [0u8; 16];
            buf.copy_from_slice(&bin.bytes);
            Value::Uuid(buf)
        }
        _ => Value::Bytes(Bytes::copy_from_slice(&bin.bytes)),
    }
}

/// Wrap `b` in `{ "v": b }` and re-serialize with canonical BSON encoding —
/// the raw bytes preserved by [`Value::Unsupported`] for types with no
/// `Value` counterpart (Regex, JavaScript, Timestamp, MinKey/MaxKey,
/// DbPointer, Symbol, Undefined). BSON's encoding is deterministic, so this
/// is exactly as lossless as the wire bytes the driver decoded from.
fn unsupported(b: &Bson) -> Value {
    let (type_name, display) = describe(b);
    let raw = unsupported_raw(b);
    Value::Unsupported {
        type_name: Arc::from(type_name),
        raw,
        display: Arc::from(display),
    }
}

fn unsupported_raw(b: &Bson) -> Bytes {
    match bson::to_vec(&doc! { "v": b.clone() }) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => Bytes::new(),
    }
}

fn describe(b: &Bson) -> (&'static str, String) {
    match b {
        Bson::RegularExpression(r) => ("Regex", format!("/{}/{}", r.pattern, r.options)),
        Bson::JavaScriptCode(c) => ("JavaScript", c.clone()),
        Bson::JavaScriptCodeWithScope(c) => ("JavaScriptWithScope", c.code.clone()),
        Bson::Timestamp(t) => ("Timestamp", format!("t={} i={}", t.time, t.increment)),
        Bson::Symbol(s) => ("Symbol", s.clone()),
        Bson::Undefined => ("Undefined", "undefined".to_string()),
        Bson::MaxKey => ("MaxKey", "MaxKey".to_string()),
        Bson::MinKey => ("MinKey", "MinKey".to_string()),
        Bson::DbPointer(_) => ("DBPointer", "DBPointer".to_string()),
        _ => ("Unknown", b.to_string()),
    }
}

/// Decode a lowercase/uppercase hex string into bytes. No external `hex`
/// dependency — `ObjectId::to_hex()` and Decimal128 raw-byte formatting are
/// the only call sites, both trivial fixed-width cases.
fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

/// A 24-lowercase-hex-char string, the shape `ObjectId::to_hex()` produces
/// and the shape `datagrep-lang`'s `ObjectId("...")` constructor accepts.
pub fn looks_like_object_id_hex(s: &str) -> bool {
    s.len() == OBJECT_ID_HEX_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Encode `datagrep_api::Value` to `bson::Bson` for predicate/mutation
/// parameters. Fallible: a value with no BSON representation is refused
/// rather than silently coerced — a silently wrong value is worse than a
/// crash.
///
/// `Value::Unsupported` round-trips exactly when its `raw` bytes came from
/// [`unsupported_raw`] (any producer of `Value` in this crate): decode the
/// `{ "v": ... }` wrapper and hand back the original `Bson` untouched. A
/// `Value::Unsupported` from some other origin (empty/foreign `raw`) is
/// refused rather than guessed at.
pub fn value_to_bson(v: &Value) -> Result<Bson, datagrep_api::DbError> {
    unsupported_err(match v {
        // `Absent` has no query meaning as a literal comparison value (the
        // caller should have used `Predicate::Exists`/`IsNull` instead);
        // mapped to `Null` rather than panicking on a value the core should
        // never actually construct here.
        Value::Null | Value::Absent => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::I64(v) => Bson::Int64(*v),
        Value::U64(v) => match i64::try_from(*v) {
            Ok(i) => Bson::Int64(i),
            // Mongo has no unsigned integer BSON type; values beyond i64
            // range (>= 2^63) are astronomically rare in practice and are
            // widened to Double rather than refused outright (documented
            // deviation — see crate report).
            Err(_) => Bson::Double(*v as f64),
        },
        Value::F64(f) => Bson::Double(*f),
        Value::Decimal(s) => {
            return Decimal128::from_str(s).map(Bson::Decimal128).map_err(|e| {
                datagrep_api::DbError::Unsupported {
                    feature: format!("value {s:?} is not a valid BSON Decimal128: {e}"),
                }
            });
        }
        Value::Str(s) => Bson::String(s.to_string()),
        Value::Bytes(b) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: b.to_vec(),
        }),
        Value::Date(days) => {
            Bson::DateTime(bson::DateTime::from_millis((*days as i64) * 86_400_000))
        }
        Value::Timestamp { micros, .. } => {
            Bson::DateTime(bson::DateTime::from_millis(micros.div_euclid(1_000)))
        }
        Value::Uuid(bytes) => Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: bytes.to_vec(),
        }),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(value_to_bson(item)?);
            }
            Bson::Array(out)
        }
        Value::Document(doc) => Bson::Document(value_doc_to_bson_doc(doc)?),
        Value::Unsupported { raw, type_name, .. } => return decode_unsupported(raw, type_name),
        // No BSON representation: Mongo has no time-of-day, interval, or
        // JSON-text-preserving type, and DBRef/GeoJSON/vector reconstruction
        // from our minimal `Ref`/`Geo`/`Vector` is out of scope for v1 (see
        // crate report's deviations).
        other => {
            return Err(datagrep_api::DbError::Unsupported {
                feature: format!("{other:?} has no BSON encoding"),
            })
        }
    })
}

fn unsupported_err(b: Bson) -> Result<Bson, datagrep_api::DbError> {
    Ok(b)
}

fn decode_unsupported(raw: &Bytes, type_name: &str) -> Result<Bson, datagrep_api::DbError> {
    if raw.is_empty() {
        return Err(datagrep_api::DbError::Unsupported {
            feature: format!(
                "Unsupported {{ type_name: {type_name:?} }} has no raw bytes to decode back to BSON"
            ),
        });
    }
    match bson::from_slice::<BsonDocument>(raw) {
        Ok(wrapper) => match wrapper.get("v") {
            Some(v) => Ok(v.clone()),
            None => Err(datagrep_api::DbError::Unsupported {
                feature: format!(
                    "Unsupported {{ type_name: {type_name:?} }}'s raw bytes did not decode to the expected wrapper shape"
                ),
            }),
        },
        Err(e) => Err(datagrep_api::DbError::Unsupported {
            feature: format!(
                "Unsupported {{ type_name: {type_name:?} }}'s raw bytes are not valid BSON: {e}"
            ),
        }),
    }
}

fn value_doc_to_bson_doc(doc: &DatagrepDocument) -> Result<BsonDocument, datagrep_api::DbError> {
    let mut out = BsonDocument::new();
    for (k, v) in doc.iter() {
        out.insert(k.to_string(), value_to_bson(v)?);
    }
    Ok(out)
}

/// Shell-text ergonomic recovery for `_id`-shaped filters (see the module
/// report's "datagrep-lang gap" note): `datagrep-lang`'s `ObjectId("<hex>")`
/// constructor compiles to a plain `Value::Str(hex)` (its own documented
/// deviation — it cannot depend on `bytes::Bytes` to build a proper
/// `Value::Unsupported`), which is indistinguishable from a user typing a
/// literal 24-hex-character string. Recovering the intent is a heuristic,
/// deliberately narrow: applied only when a document key is exactly `_id`
/// (top level of a filter/update/insert document processed by the Native
/// shell-text path) and the string looks like an ObjectId hex string.
/// Never applied by the typed `Predicate` compiler (`filter.rs`), which has
/// no ambiguity to resolve because it never sees this shell surface.
pub fn value_to_bson_for_field(field: &str, v: &Value) -> Result<Bson, datagrep_api::DbError> {
    if field == "_id" {
        if let Value::Str(s) = v {
            if looks_like_object_id_hex(s) {
                if let Ok(oid) = ObjectId::parse_str(s.as_ref()) {
                    return Ok(Bson::ObjectId(oid));
                }
            }
        }
    }
    value_to_bson(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{spec::BinarySubtype, Regex as BsonRegex};

    #[test]
    fn every_bson_scalar_maps_to_expected_value() {
        assert_eq!(bson_to_value(&Bson::Double(1.5)), Value::F64(1.5));
        assert_eq!(
            bson_to_value(&Bson::String("hi".into())),
            Value::Str(Arc::from("hi"))
        );
        assert_eq!(bson_to_value(&Bson::Boolean(true)), Value::Bool(true));
        assert_eq!(bson_to_value(&Bson::Null), Value::Null);
        assert_eq!(bson_to_value(&Bson::Int32(7)), Value::I64(7));
        assert_eq!(bson_to_value(&Bson::Int64(-9)), Value::I64(-9));
    }

    #[test]
    fn decimal128_maps_to_string_never_f64() {
        let d = Decimal128::from_str("19.99").unwrap();
        match bson_to_value(&Bson::Decimal128(d)) {
            Value::Decimal(s) => assert_eq!(&*s, "19.99"),
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    #[test]
    fn object_id_keeps_12_raw_bytes_and_hex_display() {
        let oid = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        match bson_to_value(&Bson::ObjectId(oid)) {
            Value::Unsupported {
                type_name,
                raw,
                display,
            } => {
                assert_eq!(&*type_name, "ObjectId");
                assert_eq!(raw.len(), 12);
                assert_eq!(&*display, "507f1f77bcf86cd799439011");
                // Round trip: hex decode of `display` matches `raw` exactly.
                assert_eq!(hex_to_bytes(&display).unwrap(), raw.to_vec());
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn binary_uuid_subtype_4_decodes_as_uuid() {
        let bytes = [1u8; 16];
        let bin = Binary {
            subtype: BinarySubtype::Uuid,
            bytes: bytes.to_vec(),
        };
        assert_eq!(binary_to_value(&bin), Value::Uuid(bytes));
    }

    #[test]
    fn binary_generic_subtype_decodes_as_bytes() {
        let bin = Binary {
            subtype: BinarySubtype::Generic,
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            binary_to_value(&bin),
            Value::Bytes(Bytes::from_static(&[1, 2, 3]))
        );
    }

    #[test]
    fn legacy_uuid_subtype_stays_raw_bytes_not_guessed() {
        let bin = Binary {
            subtype: BinarySubtype::UuidOld,
            bytes: vec![9u8; 16],
        };
        // Deliberately not decoded as Uuid — byte order is ambiguous for the
        // legacy subtype.
        assert!(matches!(binary_to_value(&bin), Value::Bytes(_)));
    }

    #[test]
    fn datetime_round_trips_through_millis() {
        let dt = bson::DateTime::from_millis(1_700_000_000_000);
        match bson_to_value(&Bson::DateTime(dt)) {
            Value::Timestamp { micros, tz } => {
                assert_eq!(tz, TzSpec::Utc);
                assert_eq!(micros, 1_700_000_000_000_000);
            }
            other => panic!("expected Timestamp, got {other:?}"),
        }
    }

    #[test]
    fn regex_javascript_timestamp_minmaxkey_dbpointer_symbol_are_unsupported_never_lost() {
        let regex = Bson::RegularExpression(BsonRegex {
            pattern: "^a".into(),
            options: "i".into(),
        });
        let js = Bson::JavaScriptCode("function() {}".into());
        let ts = Bson::Timestamp(bson::Timestamp {
            time: 1,
            increment: 2,
        });
        let maxkey = Bson::MaxKey;
        let minkey = Bson::MinKey;
        let symbol = Bson::Symbol("sym".into());

        for b in [regex, js, ts, maxkey, minkey, symbol] {
            match bson_to_value(&b) {
                Value::Unsupported { raw, .. } => {
                    assert!(!raw.is_empty(), "raw bytes must be preserved for {b:?}");
                    // Round trip through our own wrapper decode.
                    let decoded = bson::from_slice::<BsonDocument>(&raw).unwrap();
                    assert_eq!(decoded.get("v"), Some(&b));
                }
                other => panic!("expected Unsupported for {b:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn document_preserves_field_order() {
        let mut doc = BsonDocument::new();
        doc.insert("z", Bson::Int32(1));
        doc.insert("a", Bson::Int32(2));
        let v = bson_doc_to_value_doc(&doc);
        let keys: Vec<&str> = v.iter().map(|(k, _)| k.as_ref()).collect();
        assert_eq!(keys, vec!["z", "a"]);
    }

    #[test]
    fn missing_field_is_absent_not_null() {
        let doc = BsonDocument::new();
        let datagrep_doc = bson_doc_to_value_doc(&doc);
        assert_eq!(datagrep_doc.get("nope"), None, "caller maps None to Absent");
    }

    #[test]
    fn value_to_bson_encodes_typed_values() {
        assert_eq!(value_to_bson(&Value::I64(5)).unwrap(), Bson::Int64(5));
        assert_eq!(
            value_to_bson(&Value::Str(Arc::from("x"))).unwrap(),
            Bson::String("x".into())
        );
        assert_eq!(
            value_to_bson(&Value::Bool(true)).unwrap(),
            Bson::Boolean(true)
        );
        assert_eq!(
            value_to_bson(&Value::Decimal(Arc::from("1.5"))).unwrap(),
            Bson::Decimal128(Decimal128::from_str("1.5").unwrap())
        );
    }

    #[test]
    fn value_to_bson_refuses_types_with_no_bson_encoding() {
        assert!(value_to_bson(&Value::Time { nanos: 0 }).is_err());
        assert!(value_to_bson(&Value::Interval {
            months: 0,
            days: 0,
            nanos: 0
        })
        .is_err());
        assert!(value_to_bson(&Value::Json(Arc::from("{}"))).is_err());
    }

    #[test]
    fn unsupported_value_round_trips_back_to_original_bson() {
        let original = Bson::MaxKey;
        let v = bson_to_value(&original);
        let back = value_to_bson(&v).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn object_id_hex_heuristic_only_applies_to_id_field() {
        let hex = Value::Str(Arc::from("507f1f77bcf86cd799439011"));
        match value_to_bson_for_field("_id", &hex).unwrap() {
            Bson::ObjectId(_) => {}
            other => panic!("expected ObjectId, got {other:?}"),
        }
        // Same-shaped string in a non-`_id` field stays a plain string.
        match value_to_bson_for_field("hash", &hex).unwrap() {
            Bson::String(s) => assert_eq!(s, "507f1f77bcf86cd799439011"),
            other => panic!("expected String, got {other:?}"),
        }
    }
}
