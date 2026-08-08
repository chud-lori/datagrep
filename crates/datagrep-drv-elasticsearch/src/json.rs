//! An order-preserving JSON value, used for everything that becomes a
//! [`datagrep_api::Value`].
//!
//! # Why this exists
//!
//! `datagrep-api`'s `Document` is explicit that **key order is data** — "for
//! BSON, ES `_source`, Redis hashes" — and this driver's whole reason for
//! existing is to be truthful about document shape. But `serde_json::Value`
//! stores objects in a `BTreeMap` unless the crate-wide `preserve_order`
//! feature is on, which would silently re-alphabetize every `_source` the grid
//! and the detail pane show.
//!
//! Enabling that feature is not an option here: Cargo unifies features across
//! the workspace, so switching it on for this driver switches it on for
//! `datagrep-ffi`, `datagrep-core`, `datagrep-profiles` and every other crate
//! that touches `serde_json` — changing the key order of JSON *they* emit.
//! A driver must not reach across the workspace like that.
//!
//! So responses whose ordering is user-visible are parsed into this type
//! instead: a `Vec<(String, OrderedJson)>` for objects, which also preserves
//! duplicate keys exactly as `Document` does. Control-plane responses (`_cat`,
//! `_stats`, `_tasks`) keep using `serde_json::Value`, because nothing about
//! their key order is ever shown to anyone.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Number, Value as Json};

/// A JSON value that remembers the order — and the duplicates — of its object
/// keys.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderedJson {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<OrderedJson>),
    /// Ordered, duplicate-preserving object fields.
    Object(Vec<(String, OrderedJson)>),
}

impl OrderedJson {
    /// Parse JSON text, preserving object key order.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// First occurrence of `key`, matching `serde_json::Value::get`'s
    /// behaviour so call sites read the same.
    pub fn get(&self, key: &str) -> Option<&OrderedJson> {
        match self {
            OrderedJson::Object(fields) => {
                fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            OrderedJson::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            OrderedJson::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            OrderedJson::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            OrderedJson::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[OrderedJson]> {
        match self {
            OrderedJson::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, OrderedJson)]> {
        match self {
            OrderedJson::Object(fields) => Some(fields),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, OrderedJson::Object(_))
    }

    /// Lower to a `serde_json::Value` — used where a value read out of a
    /// response has to go back into a request body (the `search_after` sort
    /// values, most importantly) or into a resume token. Object order is lost
    /// in the conversion, which is why it is never used for `_source`.
    pub fn to_serde(&self) -> Json {
        match self {
            OrderedJson::Null => Json::Null,
            OrderedJson::Bool(b) => Json::Bool(*b),
            OrderedJson::Number(n) => Json::Number(n.clone()),
            OrderedJson::String(s) => Json::String(s.clone()),
            OrderedJson::Array(items) => Json::Array(items.iter().map(Self::to_serde).collect()),
            OrderedJson::Object(fields) => Json::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_serde()))
                    .collect(),
            ),
        }
    }

    /// Lift a `serde_json::Value` — for the paths where the order was never
    /// available in the first place (a control-plane reply shown as one
    /// document), so a single conversion routine serves both.
    pub fn from_serde(json: &Json) -> Self {
        match json {
            Json::Null => OrderedJson::Null,
            Json::Bool(b) => OrderedJson::Bool(*b),
            Json::Number(n) => OrderedJson::Number(n.clone()),
            Json::String(s) => OrderedJson::String(s.clone()),
            Json::Array(items) => OrderedJson::Array(items.iter().map(Self::from_serde).collect()),
            Json::Object(map) => OrderedJson::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), Self::from_serde(v)))
                    .collect(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OrderedJsonVisitor)
    }
}

struct OrderedJsonVisitor;

impl<'de> Visitor<'de> for OrderedJsonVisitor {
    type Value = OrderedJson;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(OrderedJson::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(OrderedJson::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(OrderedJson::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(Number::from_f64(v)
            .map(OrderedJson::Number)
            // Non-finite floats are not JSON; `serde_json` never produces one,
            // and turning it into null loses nothing that was ever there.
            .unwrap_or(OrderedJson::Null))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(OrderedJson::String(v.to_string()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(OrderedJson::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(OrderedJson::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        // Push, never insert: duplicate keys are preserved in order, exactly
        // as `datagrep_api::Document` does.
        let mut fields = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some((k, v)) = map.next_entry::<String, OrderedJson>()? {
            fields.push((k, v));
        }
        Ok(OrderedJson::Object(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_order_survives_parsing() {
        // Deliberately reverse-alphabetical: a BTreeMap-backed parser would
        // hand these back as a, m, z.
        let parsed = OrderedJson::parse(r#"{"z":1,"m":2,"a":3}"#).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, vec!["z", "m", "a"], "key order is data");

        // …and `serde_json` really would have re-sorted them, which is the
        // whole reason this type exists.
        let via_serde: Json = serde_json::from_str(r#"{"z":1,"m":2,"a":3}"#).unwrap();
        let serde_keys: Vec<&str> = via_serde
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(serde_keys, vec!["a", "m", "z"]);
    }

    #[test]
    fn nested_objects_keep_their_own_order() {
        let parsed =
            OrderedJson::parse(r#"{"outer":{"zulu":1,"alpha":2},"first":true}"#).unwrap();
        let outer_keys: Vec<&str> = parsed
            .get("outer")
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(outer_keys, vec!["zulu", "alpha"]);
    }

    #[test]
    fn duplicate_keys_are_preserved_and_lookup_takes_the_first() {
        let parsed = OrderedJson::parse(r#"{"k":1,"k":2}"#).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 2);
        assert_eq!(parsed.get("k").unwrap().as_i64(), Some(1));
    }

    #[test]
    fn scalars_and_arrays_round_trip() {
        let parsed = OrderedJson::parse(
            r#"{"n":-3,"u":18446744073709551615,"f":1.5,"s":"x","b":false,"nil":null,"a":[1,"2",null]}"#,
        )
        .unwrap();
        assert_eq!(parsed.get("n").unwrap().as_i64(), Some(-3));
        assert_eq!(parsed.get("u").unwrap().as_u64(), Some(u64::MAX));
        assert_eq!(parsed.get("s").unwrap().as_str(), Some("x"));
        assert_eq!(parsed.get("b").unwrap().as_bool(), Some(false));
        assert_eq!(parsed.get("nil"), Some(&OrderedJson::Null));
        assert_eq!(parsed.get("a").unwrap().as_array().unwrap().len(), 3);
        assert_eq!(parsed.get("missing"), None);
    }

    #[test]
    fn accessors_return_none_for_the_wrong_shape_rather_than_coercing() {
        let parsed = OrderedJson::parse(r#"{"s":"7"}"#).unwrap();
        let s = parsed.get("s").unwrap();
        assert_eq!(s.as_i64(), None, "a string is not silently parsed");
        assert!(s.as_array().is_none());
        assert!(s.as_object().is_none());
        assert!(s.get("anything").is_none());
    }

    #[test]
    fn lowering_and_lifting_are_inverse_up_to_key_order() {
        let parsed = OrderedJson::parse(r#"{"z":[1,{"b":2}],"a":null}"#).unwrap();
        let lowered = parsed.to_serde();
        assert_eq!(lowered["z"][1]["b"], serde_json::json!(2));
        // Lifting back gives an equal tree (serde's own order, which is all
        // that was left after lowering).
        let lifted = OrderedJson::from_serde(&lowered);
        assert_eq!(lifted.get("z").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(lifted.get("a"), Some(&OrderedJson::Null));
    }

    #[test]
    fn malformed_json_is_an_error_not_a_partial_value() {
        assert!(OrderedJson::parse("{not json}").is_err());
        assert!(OrderedJson::parse("").is_err());
    }
}
