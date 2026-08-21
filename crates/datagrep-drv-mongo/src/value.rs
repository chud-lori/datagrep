use std::str::FromStr;
use std::sync::Arc;

use bson::spec::BinarySubtype;
use bson::{doc, oid::ObjectId, Binary, Bson, Decimal128, Document as BsonDocument};
use bytes::Bytes;

use datagrep_api::{Document as DatagrepDocument, TzSpec, Value};

const OBJECT_ID_HEX_LEN: usize = 24;

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
        Bson::Decimal128(d) => Value::Decimal(Arc::from(d.to_string())),
        Bson::ObjectId(oid) => object_id_to_value(oid),
        Bson::DateTime(dt) => Value::Timestamp {
            micros: dt.timestamp_millis().saturating_mul(1_000),
            tz: TzSpec::Utc,
        },
        Bson::Binary(bin) => binary_to_value(bin),
        other => unsupported(other),
    }
}

fn bson_doc_to_value_doc(doc: &BsonDocument) -> DatagrepDocument {
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
        None => Value::Unsupported {
            type_name: Arc::from("ObjectId"),
            raw: Bytes::new(),
            display: Arc::from(hex),
        },
    }
}

fn binary_to_value(bin: &Binary) -> Value {
    match bin.subtype {
        BinarySubtype::Uuid if bin.bytes.len() == 16 => {
            let mut buf = [0u8; 16];
            buf.copy_from_slice(&bin.bytes);
            Value::Uuid(buf)
        }
        _ => Value::Bytes(Bytes::copy_from_slice(&bin.bytes)),
    }
}

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

pub fn looks_like_object_id_hex(s: &str) -> bool {
    s.len() == OBJECT_ID_HEX_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn value_to_bson(v: &Value) -> Result<Bson, datagrep_api::DbError> {
    unsupported_err(match v {
        Value::Null | Value::Absent => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::I64(v) => Bson::Int64(*v),
        Value::U64(v) => match i64::try_from(*v) {
            Ok(i) => Bson::Int64(i),
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
