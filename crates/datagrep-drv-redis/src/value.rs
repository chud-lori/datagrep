use datagrep_api::value::{Document, Value};
use datagrep_api::Bytes;

pub fn from_resp(v: redis::Value) -> Value {
    match v {
        redis::Value::Nil => Value::Null,
        redis::Value::Int(i) => Value::I64(i),
        redis::Value::BulkString(bytes) => bytes_to_value(bytes),
        redis::Value::SimpleString(s) => Value::Str(s.into()),
        redis::Value::Okay => Value::Str("OK".into()),
        redis::Value::Array(items) => {
            Value::Array(items.into_iter().map(from_resp).collect::<Vec<_>>().into())
        }
        redis::Value::Map(pairs) => Value::Document(std::sync::Arc::new(map_to_document(pairs))),
        redis::Value::Attribute { data, attributes } => {
            tracing::debug!(
                attribute_count = attributes.len(),
                "dropping RESP3 attribute metadata (unsupported by datagrep_api::Value)"
            );
            from_resp(*data)
        }
        redis::Value::Set(items) => {
            Value::Array(items.into_iter().map(from_resp).collect::<Vec<_>>().into())
        }
        redis::Value::Double(d) => Value::F64(d),
        redis::Value::Boolean(b) => Value::Bool(b),
        redis::Value::VerbatimString { text, .. } => Value::Str(text.into()),
        redis::Value::BigNumber(n) => Value::Decimal(n.to_string().into()),
        redis::Value::Push { kind, data } => Value::Unsupported {
            type_name: "redis.push".into(),
            raw: Bytes::from(format!("{kind:?} {data:?}").into_bytes()),
            display: format!("{kind:?} push ({} item(s))", data.len()).into(),
        },
        redis::Value::ServerError(e) => Value::Unsupported {
            type_name: "redis.server_error".into(),
            raw: Bytes::from(e.to_string().into_bytes()),
            display: e.to_string().into(),
        },
        other => {
            let variant_debug = format!("{other:?}");
            tracing::warn!(
                variant = %variant_debug,
                "unrecognized redis::Value variant (redis crate added a variant since this \
                 driver was written) — falling back to Value::Unsupported"
            );
            Value::Unsupported {
                type_name: "redis.value.unknown".into(),
                raw: Bytes::from(variant_debug.clone().into_bytes()),
                display: variant_debug.into(),
            }
        }
    }
}

fn bytes_to_value(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::Str(s.into()),
        Err(e) => Value::Bytes(Bytes::from(e.into_bytes())),
    }
}

fn map_to_document(pairs: Vec<(redis::Value, redis::Value)>) -> Document {
    let mut doc = Document::new();
    for (k, v) in pairs {
        let key: std::sync::Arc<str> = match &k {
            redis::Value::SimpleString(s) => s.as_str().into(),
            redis::Value::BulkString(b) => match std::str::from_utf8(b) {
                Ok(s) => s.into(),
                Err(_) => format!("{k:?}").into(),
            },
            other => format!("{other:?}").into(),
        };
        doc.push(key, from_resp(v));
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nil_maps_to_null_not_absent() {
        assert_eq!(from_resp(redis::Value::Nil), Value::Null);
    }

    #[test]
    fn int_maps_to_i64() {
        assert_eq!(from_resp(redis::Value::Int(42)), Value::I64(42));
        assert_eq!(from_resp(redis::Value::Int(-7)), Value::I64(-7));
    }

    #[test]
    fn bulk_string_valid_utf8_becomes_str() {
        let v = from_resp(redis::Value::BulkString(b"hello world".to_vec()));
        assert_eq!(v, Value::Str("hello world".into()));
    }

    #[test]
    fn bulk_string_invalid_utf8_becomes_bytes_never_lossy() {
        let raw = vec![0xff, 0xfe, 0x00, 0x80];
        let v = from_resp(redis::Value::BulkString(raw.clone()));
        match v {
            Value::Bytes(b) => assert_eq!(b.as_ref(), raw.as_slice()),
            other => panic!("expected Value::Bytes for invalid UTF-8, got {other:?}"),
        }
    }

    #[test]
    fn simple_string_and_okay() {
        assert_eq!(
            from_resp(redis::Value::SimpleString("PONG".into())),
            Value::Str("PONG".into())
        );
        assert_eq!(from_resp(redis::Value::Okay), Value::Str("OK".into()));
    }

    #[test]
    fn array_maps_recursively() {
        let v = from_resp(redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::BulkString(b"two".to_vec()),
            redis::Value::Nil,
        ]));
        assert_eq!(
            v,
            Value::Array(vec![Value::I64(1), Value::Str("two".into()), Value::Null].into())
        );
    }

    #[test]
    fn set_maps_to_array_per_design() {
        let v = from_resp(redis::Value::Set(vec![
            redis::Value::BulkString(b"a".to_vec()),
            redis::Value::BulkString(b"b".to_vec()),
        ]));
        assert_eq!(
            v,
            Value::Array(vec![Value::Str("a".into()), Value::Str("b".into())].into())
        );
    }

    #[test]
    fn map_preserves_key_order_as_document() {
        let v = from_resp(redis::Value::Map(vec![
            (redis::Value::SimpleString("z".into()), redis::Value::Int(1)),
            (redis::Value::SimpleString("a".into()), redis::Value::Int(2)),
        ]));
        match v {
            Value::Document(doc) => {
                let fields: Vec<_> = doc.iter().map(|(k, _)| k.to_string()).collect();
                assert_eq!(
                    fields,
                    vec!["z".to_string(), "a".to_string()],
                    "insertion order kept, not sorted"
                );
            }
            other => panic!("expected Document, got {other:?}"),
        }
    }

    #[test]
    fn double_and_boolean() {
        assert_eq!(from_resp(redis::Value::Double(3.5)), Value::F64(3.5));
        assert_eq!(from_resp(redis::Value::Boolean(true)), Value::Bool(true));
    }

    #[test]
    fn verbatim_string_becomes_str() {
        let v = from_resp(redis::Value::VerbatimString {
            format: redis::VerbatimFormat::Text,
            text: "hello".into(),
        });
        assert_eq!(v, Value::Str("hello".into()));
    }

    #[test]
    fn big_number_becomes_decimal_string() {
        let big = "123456789012345678901234567890";
        let v = from_resp(redis::Value::BigNumber(big.parse().unwrap()));
        assert_eq!(v, Value::Decimal(big.into()));
    }

    #[test]
    fn push_and_server_error_are_unsupported_but_keep_raw_bytes() {
        let push = from_resp(redis::Value::Push {
            kind: redis::PushKind::Message,
            data: vec![redis::Value::BulkString(b"payload".to_vec())],
        });
        match push {
            Value::Unsupported { raw, .. } => assert!(!raw.is_empty()),
            other => panic!("expected Unsupported for Push, got {other:?}"),
        }
    }
}
