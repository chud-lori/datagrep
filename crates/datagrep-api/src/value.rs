use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::shape::{LogicalType, ObjectPath};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TzSpec {
    Naive,
    Utc,
    Named(Arc<str>),
    Offset(i16),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Absent,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Decimal(Arc<str>),
    Str(Arc<str>),
    Bytes(Bytes),
    Date(i32),
    Time {
        nanos: i64,
    },
    Timestamp {
        micros: i64,
        tz: TzSpec,
    },
    Interval {
        months: i32,
        days: i32,
        nanos: i64,
    },
    Uuid([u8; 16]),
    Json(Arc<str>),
    Array(Arc<[Value]>),
    Document(Arc<Document>),
    Ref {
        target: ObjectPath,
        key: Arc<[Value]>,
    },
    Geo(Arc<Geometry>),
    Vector(Arc<[f32]>),
    Unsupported {
        type_name: Arc<str>,
        raw: Bytes,
        display: Arc<str>,
    },
}

impl Value {
    pub fn logical_type(&self) -> Option<LogicalType> {
        Some(match self {
            Value::Null => LogicalType::Null,
            Value::Absent => return None,
            Value::Bool(_) => LogicalType::Bool,
            Value::I64(_) => LogicalType::I64,
            Value::U64(_) => LogicalType::U64,
            Value::F64(_) => LogicalType::F64,
            Value::Decimal(_) => LogicalType::Decimal,
            Value::Str(_) => LogicalType::Str,
            Value::Bytes(_) => LogicalType::Bytes,
            Value::Date(_) => LogicalType::Date,
            Value::Time { .. } => LogicalType::Time,
            Value::Timestamp { .. } => LogicalType::Timestamp,
            Value::Interval { .. } => LogicalType::Interval,
            Value::Uuid(_) => LogicalType::Uuid,
            Value::Json(_) => LogicalType::Json,
            Value::Array(_) => LogicalType::Array,
            Value::Document(_) => LogicalType::Document,
            Value::Ref { .. } => LogicalType::Ref,
            Value::Geo(_) => LogicalType::Geo,
            Value::Vector(_) => LogicalType::Vector,
            Value::Unsupported { .. } => LogicalType::Unknown,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Geometry {
    Point { x: f64, y: f64 },
    LineString(Vec<(f64, f64)>),
    Polygon(Vec<Vec<(f64, f64)>>),
    Raw { wkb: Bytes },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "Vec<(Arc<str>, Value)>", into = "Vec<(Arc<str>, Value)>")]
pub struct Document {
    fields: Vec<(Arc<str>, Value)>,
    index: HashMap<Arc<str>, usize>,
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_fields(fields: Vec<(Arc<str>, Value)>) -> Self {
        let mut index = HashMap::with_capacity(fields.len());
        for (i, (k, _)) in fields.iter().enumerate() {
            index.entry(k.clone()).or_insert(i);
        }
        Self { fields, index }
    }

    pub fn push(&mut self, key: impl Into<Arc<str>>, value: Value) {
        let key = key.into();
        self.index.entry(key.clone()).or_insert(self.fields.len());
        self.fields.push((key, value));
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.index.get(key).map(|&i| &self.fields[i].1)
    }

    pub fn get_path(&self, path: &FieldPath) -> Option<&Value> {
        let mut cur: Option<&Value> = None;
        for seg in path.segments() {
            match seg {
                PathSeg::Field(name) => {
                    let doc = match cur {
                        None => self,
                        Some(Value::Document(d)) => d,
                        Some(_) => return None,
                    };
                    cur = Some(doc.get(name)?);
                }
                PathSeg::Index(n) => match cur? {
                    Value::Array(items) => cur = Some(items.get(*n as usize)?),
                    _ => return None,
                },
            }
        }
        cur
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Arc<str>, &Value)> {
        self.fields.iter().map(|(k, v)| (k, v))
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
    }
}

impl From<Vec<(Arc<str>, Value)>> for Document {
    fn from(fields: Vec<(Arc<str>, Value)>) -> Self {
        Self::from_fields(fields)
    }
}

impl From<Document> for Vec<(Arc<str>, Value)> {
    fn from(doc: Document) -> Self {
        doc.fields
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathSeg {
    Field(Arc<str>),
    Index(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FieldPath(Vec<PathSeg>);

impl FieldPath {
    pub fn new(segments: Vec<PathSeg>) -> Self {
        Self(segments)
    }

    pub fn field(name: impl Into<Arc<str>>) -> Self {
        Self(vec![PathSeg::Field(name.into())])
    }

    pub fn segments(&self) -> &[PathSeg] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid field path at byte {at}: {reason}")]
pub struct PathParseError {
    pub at: usize,
    pub reason: &'static str,
}

impl FromStr for FieldPath {
    type Err = PathParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |at, reason| PathParseError { at, reason };
        if s.is_empty() {
            return Err(err(0, "empty path"));
        }
        let bytes = s.as_bytes();
        let mut segs = Vec::new();
        let mut i = 0;
        // At loop entry `i` is at the start of a segment: a field name, or `[`.
        loop {
            match bytes.get(i) {
                Some(b'[') => {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j == start {
                        return Err(err(start, "expected digits inside []"));
                    }
                    if bytes.get(j) != Some(&b']') {
                        return Err(err(j, "unterminated index, expected ]"));
                    }
                    let n: u64 = s[start..j]
                        .parse()
                        .map_err(|_| err(start, "index out of range"))?;
                    segs.push(PathSeg::Index(n));
                    i = j + 1;
                }
                Some(b'.') => return Err(err(i, "empty field name")),
                Some(b']') => return Err(err(i, "unexpected ]")),
                Some(_) => {
                    let start = i;
                    while i < bytes.len() && !matches!(bytes[i], b'.' | b'[' | b']') {
                        i += 1;
                    }
                    segs.push(PathSeg::Field(Arc::from(&s[start..i])));
                }
                None => return Err(err(i, "expected a segment")),
            }
            // After a segment: end, `.` (a field must follow), or `[` (index).
            match bytes.get(i) {
                None => return Ok(Self(segs)),
                Some(b'.') => {
                    i += 1;
                    if i == bytes.len() {
                        return Err(err(i, "trailing dot"));
                    }
                    if bytes[i] == b'[' {
                        return Err(err(i, "index must not follow a dot"));
                    }
                }
                Some(b'[') => {}
                Some(_) => return Err(err(i, "expected . or [ after index")),
            }
        }
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            match seg {
                PathSeg::Field(name) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(name)?;
                }
                PathSeg::Index(n) => write!(f, "[{n}]")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> FieldPath {
        s.parse().expect(s)
    }

    #[test]
    fn field_path_round_trip() {
        for s in [
            "address.city",
            "tags[3]",
            "a.b[3].c",
            "[0].a",
            "m[1][2]",
            "x",
        ] {
            assert_eq!(parse(s).to_string(), s, "round-trip of {s:?}");
        }
    }

    #[test]
    fn field_path_structure() {
        assert_eq!(
            parse("a.b[3].c").segments(),
            &[
                PathSeg::Field(Arc::from("a")),
                PathSeg::Field(Arc::from("b")),
                PathSeg::Index(3),
                PathSeg::Field(Arc::from("c")),
            ]
        );
    }

    #[test]
    fn field_path_rejects_garbage() {
        for s in [
            "", ".", "a..b", "a.", "a[", "a[x]", "a[1", "a[1]b", "]", "a.[1]",
        ] {
            assert!(s.parse::<FieldPath>().is_err(), "{s:?} should not parse");
        }
    }

    fn sample_doc() -> Document {
        // { name: "amy", name: "dup", address: { city: "sg", tags: ["a","b"] }, nil: null }
        let address = Document::from_fields(vec![
            (Arc::from("city"), Value::Str(Arc::from("sg"))),
            (
                Arc::from("tags"),
                Value::Array(Arc::from(vec![
                    Value::Str(Arc::from("a")),
                    Value::Str(Arc::from("b")),
                ])),
            ),
        ]);
        Document::from_fields(vec![
            (Arc::from("name"), Value::Str(Arc::from("amy"))),
            (Arc::from("name"), Value::Str(Arc::from("dup"))),
            (Arc::from("address"), Value::Document(Arc::new(address))),
            (Arc::from("nil"), Value::Null),
        ])
    }

    #[test]
    fn document_duplicate_keys_first_wins_lookup_all_preserved_iteration() {
        let doc = sample_doc();
        assert_eq!(doc.get("name"), Some(&Value::Str(Arc::from("amy"))));
        let names: Vec<_> = doc.iter().filter(|(k, _)| &***k == "name").collect();
        assert_eq!(names.len(), 2, "duplicates preserved in iteration");
        assert_eq!(doc.len(), 4);
    }

    #[test]
    fn document_get_path() {
        let doc = sample_doc();
        assert_eq!(
            doc.get_path(&parse("address.city")),
            Some(&Value::Str(Arc::from("sg")))
        );
        assert_eq!(
            doc.get_path(&parse("address.tags[1]")),
            Some(&Value::Str(Arc::from("b")))
        );
        // Absent path: None — the caller maps this to Value::Absent.
        assert_eq!(doc.get_path(&parse("address.zip")), None);
        assert_eq!(doc.get_path(&parse("address.tags[9]")), None);
        // Traversing through a non-document is absence, not an error.
        assert_eq!(doc.get_path(&parse("name.x")), None);
        // A stored NULL is present — the distinction the grid renders.
        assert_eq!(doc.get_path(&parse("nil")), Some(&Value::Null));
    }

    #[test]
    fn value_equality_edge_cases() {
        assert_ne!(Value::Null, Value::Absent, "Null and Absent must differ");
        assert_eq!(Value::Null, Value::Null);
        assert_ne!(Value::I64(1), Value::U64(1), "signedness is meaningful");
        assert_ne!(
            Value::Decimal(Arc::from("1.10")),
            Value::Decimal(Arc::from("1.1")),
            "decimals compare textually; trailing zeros are data"
        );
        assert_eq!(
            Value::Timestamp {
                micros: 0,
                tz: TzSpec::Utc
            },
            Value::Timestamp {
                micros: 0,
                tz: TzSpec::Utc
            }
        );
        assert_ne!(
            Value::Timestamp {
                micros: 0,
                tz: TzSpec::Utc
            },
            Value::Timestamp {
                micros: 0,
                tz: TzSpec::Naive
            },
            "tz qualifier is part of the value"
        );
    }

    #[test]
    fn absent_has_no_logical_type() {
        assert_eq!(Value::Absent.logical_type(), None);
        assert_eq!(Value::Null.logical_type(), Some(LogicalType::Null));
        assert_eq!(Value::Bool(true).logical_type(), Some(LogicalType::Bool));
    }
}
