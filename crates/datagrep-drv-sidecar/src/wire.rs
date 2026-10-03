use std::collections::BTreeMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as Json};

use datagrep_api::catalog::{ObjectDetail, ObjectKind, ObjectNode};
use datagrep_api::config::ConfigValue;
use datagrep_api::driver::{Enforcement, ServerInfo};
use datagrep_api::error::DbError;
use datagrep_api::shape::{FieldDef, FieldFlags, LogicalType, ObjectPath, RowSchema, Shape};
use datagrep_api::value::{Document, Geometry, TzSpec, Value};

pub const PROTOCOL_MIN: u32 = 1;
pub const PROTOCOL_MAX: u32 = 1;

#[derive(Debug, Serialize)]
pub struct Outgoing<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub m: &'a str,
    pub p: Json,
}

#[derive(Debug, Deserialize)]
pub struct Incoming {
    pub id: u64,
    #[serde(default)]
    pub ok: Option<Json>,
    #[serde(default)]
    pub err: Option<WireError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireError {
    pub kind: String,
    #[serde(default)]
    pub code: Option<String>,
    pub message: String,
    #[serde(default)]
    pub position: Option<u32>,
}

impl WireError {
    // A sidecar never mints `safety`: only the parent's gate does.
    pub fn into_db_error(self) -> DbError {
        let WireError {
            kind,
            code,
            message,
            position,
        } = self;
        match kind.as_str() {
            "connect" => DbError::Connect(message),
            "auth" => DbError::Auth(message),
            "tls" => DbError::Tls(message),
            "query" => DbError::Query {
                code,
                message,
                position,
            },
            "conflict" => DbError::Conflict { code, message },
            "timeout" => DbError::Timeout,
            "cancelled" => DbError::Cancelled,
            "unsupported" => DbError::Unsupported { feature: message },
            "resource" => DbError::ResourceExhausted(message),
            "config" => DbError::Config(datagrep_api::config::ConfigError::InvalidValue {
                key: code.unwrap_or_default(),
                reason: message,
            }),
            "panic" => DbError::DriverPanic(message),
            other => DbError::Protocol(format!("sidecar sent error kind `{other}`: {message}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HelloReply {
    pub protocol: u32,
    pub engine: String,
    pub engine_version: String,
    pub language: Json,
    pub caps: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConnectReply {
    pub server: WireServer,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WireServer {
    pub product: String,
    pub version: String,
    #[serde(default)]
    pub details: Vec<(String, String)>,
}

impl From<WireServer> for ServerInfo {
    fn from(s: WireServer) -> Self {
        ServerInfo {
            product: Arc::from(s.product),
            version: Arc::from(s.version),
            details: s
                .details
                .into_iter()
                .map(|(k, v)| (Arc::from(k), Arc::from(v)))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExecuteReply {
    pub cursor: u64,
    pub shape: WireShape,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireShape {
    Table {
        fields: Vec<WireField>,
    },
    Ack {
        #[serde(default)]
        affected: Option<u64>,
        #[serde(default)]
        message: Option<String>,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WireField {
    pub name: String,
    pub logical: LogicalType,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub native_type: Option<String>,
}

impl From<WireField> for FieldDef {
    fn from(f: WireField) -> Self {
        FieldDef {
            name: Arc::from(f.name),
            logical: f.logical,
            flags: if f.nullable {
                FieldFlags::NULLABLE
            } else {
                FieldFlags::empty()
            },
            native_type: f.native_type.map(Arc::from),
        }
    }
}

impl WireShape {
    pub fn into_shape(self) -> Shape {
        match self {
            WireShape::Table { fields } => Shape::Table(Arc::new(RowSchema {
                fields: fields.into_iter().map(FieldDef::from).collect(),
                identity: None,
            })),
            WireShape::Ack { affected, message } => Shape::Ack {
                affected,
                message: message.map(Arc::from),
            },
            WireShape::Unknown => Shape::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FetchReply {
    pub batch: Option<WireBatch>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WireBatch {
    pub rows: Vec<Vec<Json>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReadOnlyReply {
    pub enforcement: WireEnforcement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEnforcement {
    Server,
    Client,
    None,
}

impl From<WireEnforcement> for Enforcement {
    fn from(e: WireEnforcement) -> Self {
        match e {
            WireEnforcement::Server => Enforcement::Server,
            WireEnforcement::Client => Enforcement::Client,
            WireEnforcement::None => Enforcement::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChildrenReply {
    pub items: Vec<WireNode>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WireNode {
    pub path: Vec<String>,
    pub kind: ObjectKind,
    #[serde(default)]
    pub has_children: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

impl From<WireNode> for ObjectNode {
    fn from(n: WireNode) -> Self {
        ObjectNode {
            path: ObjectPath::new(n.path.into_iter().map(Arc::from).collect()),
            kind: n.kind,
            has_children: n.has_children,
            comment: n.comment.map(Arc::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DescribeReply {
    pub node: WireNode,
    #[serde(default)]
    pub fields: Option<Vec<WireField>>,
    #[serde(default)]
    pub extra: Vec<(String, String)>,
}

impl From<DescribeReply> for ObjectDetail {
    fn from(d: DescribeReply) -> Self {
        ObjectDetail {
            node: d.node.into(),
            schema: d.fields.map(|fields| RowSchema {
                fields: fields.into_iter().map(FieldDef::from).collect(),
                identity: None,
            }),
            extra: d
                .extra
                .into_iter()
                .map(|(k, v)| (Arc::from(k), Arc::from(v)))
                .collect(),
        }
    }
}

pub fn config_json(values: &BTreeMap<String, ConfigValue>) -> Map<String, Json> {
    values
        .iter()
        .map(|(k, v)| {
            let v = match v {
                ConfigValue::Str(s) => Json::from(s.as_str()),
                ConfigValue::Num(n) => json!(n),
                ConfigValue::Bool(b) => Json::from(*b),
            };
            (k.clone(), v)
        })
        .collect()
}

pub fn path_json(path: &ObjectPath) -> Json {
    Json::from(path.parts().iter().map(|p| &**p).collect::<Vec<_>>())
}

// Table cells are untagged and read against the declared column type; `{"$t":..}` overrides it.
pub fn decode_cell(cell: Json, logical: LogicalType) -> Result<Value, String> {
    if cell.is_null() {
        return Ok(Value::Null);
    }
    if is_tagged(&cell) {
        return decode_tagged(cell);
    }
    let bad = |cell: &Json| format!("{cell} is not a valid {logical:?} cell");
    Ok(match logical {
        LogicalType::Null => return Err(bad(&cell)),
        LogicalType::Bool => Value::Bool(cell.as_bool().ok_or_else(|| bad(&cell))?),
        LogicalType::I64 => Value::I64(cell.as_i64().ok_or_else(|| bad(&cell))?),
        LogicalType::U64 => Value::U64(cell.as_u64().ok_or_else(|| bad(&cell))?),
        LogicalType::F64 => Value::F64(decode_f64(&cell).ok_or_else(|| bad(&cell))?),
        LogicalType::Decimal => Value::Decimal(str_of(&cell).ok_or_else(|| bad(&cell))?),
        LogicalType::Str => Value::Str(str_of(&cell).ok_or_else(|| bad(&cell))?),
        LogicalType::Json => Value::Json(str_of(&cell).ok_or_else(|| bad(&cell))?),
        LogicalType::Bytes => Value::Bytes(bytes_of(&cell).ok_or_else(|| bad(&cell))?),
        LogicalType::Date => Value::Date(
            cell.as_i64()
                .and_then(|d| i32::try_from(d).ok())
                .ok_or_else(|| bad(&cell))?,
        ),
        LogicalType::Time => Value::Time {
            nanos: cell.as_i64().ok_or_else(|| bad(&cell))?,
        },
        LogicalType::Timestamp => decode_timestamp(&cell).ok_or_else(|| bad(&cell))?,
        LogicalType::Interval => decode_interval(&cell).ok_or_else(|| bad(&cell))?,
        LogicalType::Uuid => Value::Uuid(
            cell.as_str()
                .and_then(parse_uuid)
                .ok_or_else(|| bad(&cell))?,
        ),
        LogicalType::Array => Value::Array(decode_list(cell)?),
        LogicalType::Document => Value::Document(Arc::new(decode_document(cell)?)),
        LogicalType::Vector => decode_vector(&cell).ok_or_else(|| bad(&cell))?,
        LogicalType::Geo => Value::Geo(Arc::new(Geometry::Raw {
            wkb: bytes_of(&cell).ok_or_else(|| bad(&cell))?,
        })),
        LogicalType::Ref | LogicalType::Unknown => return Err(bad(&cell)),
    })
}

fn is_tagged(cell: &Json) -> bool {
    cell.as_object().is_some_and(|o| o.contains_key("$t"))
}

// Nested values carry no declared type, so they are tagged unless they are plain JSON scalars.
fn decode_nested(cell: Json) -> Result<Value, String> {
    match cell {
        Json::Null => Ok(Value::Null),
        Json::Bool(b) => Ok(Value::Bool(b)),
        Json::String(s) => Ok(Value::Str(Arc::from(s))),
        Json::Number(n) => Ok(n
            .as_i64()
            .map(Value::I64)
            .or_else(|| n.as_u64().map(Value::U64))
            .unwrap_or_else(|| Value::F64(n.as_f64().unwrap_or(f64::NAN)))),
        other if is_tagged(&other) => decode_tagged(other),
        other => Err(format!("untagged nested value {other}")),
    }
}

fn decode_tagged(cell: Json) -> Result<Value, String> {
    let Json::Object(mut obj) = cell else {
        return Err("tagged value is not an object".into());
    };
    let tag = obj
        .get("$t")
        .and_then(Json::as_str)
        .ok_or("tag is not a string")?
        .to_string();
    let v = obj.remove("v").unwrap_or(Json::Null);
    let logical = match tag.as_str() {
        "absent" => return Ok(Value::Absent),
        "null" => return Ok(Value::Null),
        "unsupported" => {
            let field = |k: &str| obj.get(k).and_then(Json::as_str).map(Arc::from);
            return Ok(Value::Unsupported {
                type_name: field("type").unwrap_or_else(|| Arc::from("?")),
                raw: obj.get("raw").and_then(bytes_of).unwrap_or_default(),
                display: field("display").unwrap_or_else(|| Arc::from("")),
            });
        }
        "ref" => {
            let target = obj
                .remove("target")
                .and_then(|t| serde_json::from_value::<Vec<String>>(t).ok())
                .ok_or("ref without a target")?;
            let key = decode_list(obj.remove("key").unwrap_or(Json::Array(Vec::new())))?;
            return Ok(Value::Ref {
                target: ObjectPath::new(target.into_iter().map(Arc::from).collect()),
                key,
            });
        }
        "bool" => LogicalType::Bool,
        "i64" => LogicalType::I64,
        "u64" => LogicalType::U64,
        "f64" => LogicalType::F64,
        "decimal" => LogicalType::Decimal,
        "str" => LogicalType::Str,
        "bytes" => LogicalType::Bytes,
        "date" => LogicalType::Date,
        "time" => LogicalType::Time,
        "timestamp" => LogicalType::Timestamp,
        "interval" => LogicalType::Interval,
        "uuid" => LogicalType::Uuid,
        "json" => LogicalType::Json,
        "array" => LogicalType::Array,
        "document" => LogicalType::Document,
        "geo" => LogicalType::Geo,
        "vector" => LogicalType::Vector,
        other => return Err(format!("unknown value tag `{other}`")),
    };
    if v.is_null() || is_tagged(&v) {
        return Err(format!("tag `{tag}` without a plain value"));
    }
    decode_cell(v, logical)
}

fn decode_list(cell: Json) -> Result<Arc<[Value]>, String> {
    let Json::Array(items) = cell else {
        return Err(format!("{cell} is not an array"));
    };
    items.into_iter().map(decode_nested).collect()
}

fn decode_document(cell: Json) -> Result<Document, String> {
    let Json::Array(pairs) = cell else {
        return Err(format!("{cell} is not a [[key, value], ...] document"));
    };
    let mut doc = Document::new();
    for pair in pairs {
        let Json::Array(mut kv) = pair else {
            return Err("document entry is not a pair".into());
        };
        if kv.len() != 2 {
            return Err("document entry is not a pair".into());
        }
        let v = kv.pop().unwrap_or(Json::Null);
        let k = kv.pop().unwrap_or(Json::Null);
        let k = k
            .as_str()
            .ok_or("document key is not a string")?
            .to_string();
        doc.push(k, decode_nested(v)?);
    }
    Ok(doc)
}

fn str_of(cell: &Json) -> Option<Arc<str>> {
    cell.as_str().map(Arc::from)
}

fn bytes_of(cell: &Json) -> Option<bytes::Bytes> {
    B64.decode(cell.as_str()?).ok().map(bytes::Bytes::from)
}

fn decode_f64(cell: &Json) -> Option<f64> {
    match cell {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => match s.as_str() {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ => None,
        },
        _ => None,
    }
}

fn decode_timestamp(cell: &Json) -> Option<Value> {
    let micros = cell.get("us")?.as_i64()?;
    let tz = match cell.get("tz")?.as_str()? {
        "utc" => TzSpec::Utc,
        "naive" => TzSpec::Naive,
        s if s.starts_with(['+', '-']) => TzSpec::Offset(parse_offset(s)?),
        name => TzSpec::Named(Arc::from(name)),
    };
    Some(Value::Timestamp { micros, tz })
}

fn parse_offset(s: &str) -> Option<i16> {
    let sign: i16 = if s.starts_with('-') { -1 } else { 1 };
    let (h, m) = s[1..].split_once(':')?;
    Some(sign * (h.parse::<i16>().ok()? * 60 + m.parse::<i16>().ok()?))
}

fn decode_interval(cell: &Json) -> Option<Value> {
    let parts = cell.as_array()?;
    let [months, days, nanos] = parts.as_slice() else {
        return None;
    };
    Some(Value::Interval {
        months: i32::try_from(months.as_i64()?).ok()?,
        days: i32::try_from(days.as_i64()?).ok()?,
        nanos: nanos.as_i64()?,
    })
}

fn decode_vector(cell: &Json) -> Option<Value> {
    let items = cell.as_array()?;
    let floats: Option<Vec<f32>> = items.iter().map(|x| x.as_f64().map(|f| f as f32)).collect();
    Some(Value::Vector(Arc::from(floats?)))
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 || s.len() != 36 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

fn format_uuid(b: &[u8; 16]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn tagged(tag: &str, v: Json) -> Json {
    json!({ "$t": tag, "v": v })
}

// Parameters always travel tagged: the sidecar has no declared type to read them against.
pub fn encode_tagged(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Absent => json!({ "$t": "absent" }),
        Value::Bool(b) => tagged("bool", Json::from(*b)),
        Value::I64(n) => tagged("i64", Json::from(*n)),
        Value::U64(n) => tagged("u64", Json::from(*n)),
        Value::F64(f) => tagged(
            "f64",
            if f.is_nan() {
                Json::from("NaN")
            } else if f.is_infinite() {
                Json::from(if *f > 0.0 { "Infinity" } else { "-Infinity" })
            } else {
                json!(f)
            },
        ),
        Value::Decimal(s) => tagged("decimal", Json::from(&**s)),
        Value::Str(s) => tagged("str", Json::from(&**s)),
        Value::Json(s) => tagged("json", Json::from(&**s)),
        Value::Bytes(b) => tagged("bytes", Json::from(B64.encode(b))),
        Value::Date(d) => tagged("date", Json::from(*d)),
        Value::Time { nanos } => tagged("time", Json::from(*nanos)),
        Value::Timestamp { micros, tz } => {
            let tz = match tz {
                TzSpec::Utc => "utc".to_string(),
                TzSpec::Naive => "naive".to_string(),
                TzSpec::Named(name) => name.to_string(),
                TzSpec::Offset(mins) => {
                    let sign = if *mins < 0 { '-' } else { '+' };
                    let m = mins.unsigned_abs();
                    format!("{sign}{:02}:{:02}", m / 60, m % 60)
                }
            };
            tagged("timestamp", json!({ "us": micros, "tz": tz }))
        }
        Value::Interval {
            months,
            days,
            nanos,
        } => tagged("interval", json!([months, days, nanos])),
        Value::Uuid(b) => tagged("uuid", Json::from(format_uuid(b))),
        Value::Array(items) => tagged(
            "array",
            Json::Array(items.iter().map(encode_tagged).collect()),
        ),
        Value::Document(doc) => tagged(
            "document",
            Json::Array(
                doc.iter()
                    .map(|(k, v)| json!([&**k, encode_tagged(v)]))
                    .collect(),
            ),
        ),
        Value::Ref { target, key } => json!({
            "$t": "ref",
            "target": path_json(target),
            "key": key.iter().map(encode_tagged).collect::<Vec<_>>(),
        }),
        Value::Geo(g) => match &**g {
            Geometry::Raw { wkb } => tagged("geo", Json::from(B64.encode(wkb))),
            other => json!({
                "$t": "unsupported",
                "type": "geometry",
                "display": format!("{other:?}"),
            }),
        },
        Value::Vector(v) => tagged("vector", json!(v)),
        Value::Unsupported {
            type_name,
            raw,
            display,
        } => json!({
            "$t": "unsupported",
            "type": &**type_name,
            "raw": B64.encode(raw),
            "display": &**display,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(v: Value) {
        let wire = encode_tagged(&v);
        let back = decode_cell(wire.clone(), LogicalType::Unknown)
            .or_else(|_| decode_nested(wire.clone()))
            .unwrap_or_else(|e| panic!("{v:?} -> {wire}: {e}"));
        assert_eq!(back, v, "{wire}");
    }

    #[test]
    fn every_value_variant_round_trips_through_the_tagged_form() {
        let doc = Document::from_fields(vec![
            (Arc::from("b"), Value::I64(2)),
            (Arc::from("a"), Value::Str(Arc::from("first key is b"))),
        ]);
        for v in [
            Value::Null,
            Value::Absent,
            Value::Bool(true),
            Value::I64(i64::MIN),
            Value::U64(u64::MAX),
            Value::F64(1.5),
            Value::F64(f64::INFINITY),
            Value::F64(f64::NEG_INFINITY),
            Value::Decimal(Arc::from("12345678901234567890.000")),
            Value::Str(Arc::from("ünïcode \"quoted\"")),
            Value::Json(Arc::from(r#"{"a":1}"#)),
            Value::Bytes(bytes::Bytes::from_static(&[0, 1, 255])),
            Value::Date(-719_162),
            Value::Time {
                nanos: 86_399_999_999_999,
            },
            Value::Timestamp {
                micros: 1,
                tz: TzSpec::Utc,
            },
            Value::Timestamp {
                micros: -1,
                tz: TzSpec::Naive,
            },
            Value::Timestamp {
                micros: 0,
                tz: TzSpec::Offset(-570),
            },
            Value::Timestamp {
                micros: 0,
                tz: TzSpec::Named(Arc::from("Asia/Jakarta")),
            },
            Value::Interval {
                months: -1,
                days: 2,
                nanos: 3,
            },
            Value::Uuid([0xab; 16]),
            Value::Array(Arc::from(vec![
                Value::I64(1),
                Value::Null,
                Value::Bool(false),
            ])),
            Value::Document(Arc::new(doc)),
            Value::Ref {
                target: ObjectPath::new(vec![Arc::from("hr"), Arc::from("emp")]),
                key: Arc::from(vec![Value::I64(7)]),
            },
            Value::Geo(Arc::new(Geometry::Raw {
                wkb: bytes::Bytes::from_static(b"\x01\x01"),
            })),
            Value::Vector(Arc::from(vec![0.5f32, -1.0])),
            Value::Unsupported {
                type_name: Arc::from("SDO_GEOMETRY"),
                raw: bytes::Bytes::from_static(b"\x00"),
                display: Arc::from("<geometry>"),
            },
        ] {
            round_trip(v);
        }
        // NaN is not equal to itself, so it is checked apart.
        let nan = decode_cell(encode_tagged(&Value::F64(f64::NAN)), LogicalType::F64).unwrap();
        assert!(matches!(nan, Value::F64(f) if f.is_nan()));
    }

    #[test]
    fn bytes_and_uuids_are_strings_on_the_wire_not_integer_arrays() {
        assert_eq!(
            encode_tagged(&Value::Bytes(bytes::Bytes::from_static(b"hi"))),
            json!({"$t": "bytes", "v": "aGk="})
        );
        let mut id = [0u8; 16];
        id[15] = 1;
        assert_eq!(
            encode_tagged(&Value::Uuid(id)),
            json!({"$t": "uuid", "v": "00000000-0000-0000-0000-000000000001"})
        );
    }

    #[test]
    fn untagged_cells_are_read_against_the_declared_type_and_mismatches_are_errors() {
        assert_eq!(
            decode_cell(json!("1.10"), LogicalType::Decimal).unwrap(),
            Value::Decimal(Arc::from("1.10"))
        );
        assert_eq!(
            decode_cell(json!(null), LogicalType::I64).unwrap(),
            Value::Null
        );
        assert!(decode_cell(json!("7"), LogicalType::I64).is_err());
        assert!(decode_cell(json!(1.5), LogicalType::I64).is_err());
        assert!(decode_cell(json!({"x": 1}), LogicalType::Str).is_err());
        assert!(decode_cell(json!(1), LogicalType::Unknown).is_err());
        // A tagged cell overrides the column type.
        assert_eq!(
            decode_cell(json!({"$t": "str", "v": "x"}), LogicalType::I64).unwrap(),
            Value::Str(Arc::from("x"))
        );
        assert!(decode_cell(json!({"$t": "nope", "v": 1}), LogicalType::I64).is_err());
    }

    #[test]
    fn error_kinds_map_onto_db_errors_and_a_sidecar_cannot_mint_safety() {
        let e = |kind: &str| WireError {
            kind: kind.into(),
            code: Some("ORA-00942".into()),
            message: "table or view does not exist".into(),
            position: Some(14),
        };
        assert!(matches!(
            e("query").into_db_error(),
            DbError::Query { code: Some(c), position: Some(14), .. } if c == "ORA-00942"
        ));
        assert!(matches!(e("auth").into_db_error(), DbError::Auth(_)));
        assert!(matches!(e("cancelled").into_db_error(), DbError::Cancelled));
        assert!(matches!(
            e("panic").into_db_error(),
            DbError::DriverPanic(_)
        ));
        let forged = e("safety").into_db_error();
        assert!(matches!(forged, DbError::Protocol(_)), "{forged:?}");
        assert!(!forged.is_recoverable());
    }
}
