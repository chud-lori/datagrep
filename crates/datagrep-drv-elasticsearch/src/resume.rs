//! The [`ResumeToken`] payload for an Elasticsearch scan: `search_after` plus
//! a point-in-time is the textbook case a resume token exists for, and it is
//! what makes idle auto-disconnect safe — the core can close the cursor, drop
//! the connection to zero, and pick the scan up later from the token alone.
//!
//! The token is driver-private, so the encoding is simply JSON — versioned, so
//! a token minted by a future build is rejected honestly instead of being
//! misread.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use datagrep_api::driver::ResumeToken;
use datagrep_api::error::DbError;

/// Bump when the payload's meaning changes; never reuse a version number.
pub const TOKEN_VERSION: u32 = 1;

/// What has to survive a disconnect for a scan to continue exactly where it
/// stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EsResume {
    /// Token format version.
    pub v: u32,
    /// Which pagination mechanism minted this token. A `pit` token is
    /// meaningless to a `scroll` cursor and vice versa, so it is checked on
    /// the way back in.
    pub mode: ResumeMode,
    /// Index/alias/data-stream expression the scan is reading. Carried so a
    /// resumed scan can rebuild its request without the caller re-supplying it
    /// (and so a scroll resume, which has no index in its continuation call,
    /// can still report what it is reading).
    pub index: String,
    /// PIT id (`mode = Pit`) or scroll id (`mode = Scroll`).
    pub id: String,
    /// How long the server was asked to keep the context alive, e.g. `"5m"`.
    pub keep_alive: String,
    /// The last hit's `sort` values — the `search_after` cursor itself. Empty
    /// for scroll, which carries its position inside the scroll id.
    pub sort: Vec<Json>,
    /// The search body the scan was issued with (query, `_source` filtering,
    /// user sort keys), so a resume reproduces the same scan rather than a
    /// fresh unfiltered one.
    pub body: Json,
    /// Rows already delivered, so a `limit` survives the round trip.
    pub delivered: u64,
    /// Remaining row budget from the original `Op::Scan { limit }`, if any.
    pub remaining: Option<u64>,
}

/// Which mechanism minted a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeMode {
    Pit,
    Scroll,
}

impl EsResume {
    /// A token for the current format version. Fields are public and set by
    /// the caller; only `v` is fixed here so no construction site can forget
    /// to stamp the version.
    pub fn current(mode: ResumeMode, index: String, id: String, keep_alive: String) -> Self {
        Self {
            v: TOKEN_VERSION,
            mode,
            index,
            id,
            keep_alive,
            sort: Vec::new(),
            body: Json::Null,
            delivered: 0,
            remaining: None,
        }
    }

    /// The `search_after` position and the body needed to reproduce the scan.
    pub fn at(mut self, sort: Vec<Json>, body: Json) -> Self {
        self.sort = sort;
        self.body = body;
        self
    }

    /// Row-budget bookkeeping, so an `Op::Scan { limit }` survives a resume.
    pub fn counted(mut self, delivered: u64, remaining: Option<u64>) -> Self {
        self.delivered = delivered;
        self.remaining = remaining;
        self
    }

    pub fn encode(&self) -> Option<ResumeToken> {
        let bytes = serde_json::to_vec(self).ok()?;
        Some(ResumeToken(bytes.into()))
    }

    pub fn decode(token: &ResumeToken) -> Result<Self, DbError> {
        let mut parsed: Self = serde_json::from_slice(&token.0).map_err(|e| {
            DbError::Protocol(format!(
                "resume token is not a valid elasticsearch token: {e}"
            ))
        })?;
        if parsed.v != TOKEN_VERSION {
            return Err(DbError::Protocol(format!(
                "resume token version {} is not understood by this driver (expected {TOKEN_VERSION})",
                parsed.v
            )));
        }
        if parsed.id.is_empty() {
            return Err(DbError::Protocol(
                "resume token carries no point-in-time / scroll id".to_string(),
            ));
        }
        // A resume token is untrusted input — it round-trips through whatever
        // held it — and `body` is fed straight to `EsCursor::page_body`, which
        // needs a JSON object to insert `size`/`sort` into. `current()` mints
        // `Json::Null` for "no body", which means the same thing to
        // Elasticsearch as `{}`, so normalise that one case and refuse the
        // rest. Refusing rather than substituting `{}` matters: a token whose
        // body is a string or an array has lost the original scan's query, and
        // resuming without it would quietly return *more* rows than the scan
        // the user actually ran.
        match &parsed.body {
            Json::Null => parsed.body = Json::Object(serde_json::Map::new()),
            Json::Object(_) => {}
            other => {
                return Err(DbError::Protocol(format!(
                    "resume token's search body must be a JSON object, got {}",
                    kind_of(other)
                )))
            }
        }
        Ok(parsed)
    }
}

/// The JSON type name, for an error message that tells the user what they
/// actually handed over.
fn kind_of(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> EsResume {
        EsResume::current(
            ResumeMode::Pit,
            "events-2026.08".to_string(),
            "u961LAEwaW5kZXgtMDAx".to_string(),
            "5m".to_string(),
        )
        .at(
            vec![json!(1723075200000_i64), json!(4294967298_i64)],
            json!({ "query": { "term": { "level": { "value": "error" } } } }),
        )
        .counted(1500, Some(98_500))
    }

    #[test]
    fn pit_and_search_after_round_trip_exactly() {
        let original = sample();
        let token = original.encode().expect("encode");
        let back = EsResume::decode(&token).expect("decode");
        assert_eq!(back, original);
        // The two things that actually make a resume work.
        assert_eq!(back.id, "u961LAEwaW5kZXgtMDAx", "the PIT id survives");
        assert_eq!(
            back.sort,
            vec![json!(1723075200000_i64), json!(4294967298_i64)],
            "the search_after cursor survives, with its 64-bit values intact"
        );
        assert_eq!(back.mode, ResumeMode::Pit);
        assert_eq!(back.remaining, Some(98_500));
        assert_eq!(back.delivered, 1500);
    }

    #[test]
    fn scroll_tokens_round_trip_too_and_stay_distinguishable_from_pit() {
        let scroll = EsResume::current(
            ResumeMode::Scroll,
            "logs".into(),
            "DXF1ZXJ5QW5kRmV0Y2gBAAAAAAAAAD4WYm9laVYtZndUQlNsdDcwakFMNjU1QQ==".into(),
            "1m".into(),
        )
        .at(Vec::new(), json!({}));
        let back = EsResume::decode(&scroll.encode().unwrap()).unwrap();
        assert_eq!(back.mode, ResumeMode::Scroll);
        assert!(back.sort.is_empty(), "scroll carries position in its id");
        assert_ne!(back.mode, ResumeMode::Pit);
    }

    #[test]
    fn heterogeneous_sort_values_survive_including_strings_and_nulls() {
        let mut r = sample();
        r.sort = vec![json!("2026-08-08T00:00:00Z"), json!(null), json!(3.5)];
        let back = EsResume::decode(&r.encode().unwrap()).unwrap();
        assert_eq!(back.sort, r.sort);
    }

    #[test]
    fn the_original_search_body_survives_so_a_resume_is_the_same_scan() {
        let back = EsResume::decode(&sample().encode().unwrap()).unwrap();
        assert_eq!(back.body["query"]["term"]["level"]["value"], json!("error"));
    }

    #[test]
    fn garbage_and_wrong_version_tokens_are_rejected_not_misread() {
        assert!(EsResume::decode(&ResumeToken(bytes::Bytes::from_static(b"not json"))).is_err());
        // A well-formed token from a future build.
        let mut future = sample();
        future.v = 99;
        let token = ResumeToken(serde_json::to_vec(&future).unwrap().into());
        let err = EsResume::decode(&token).unwrap_err();
        assert!(matches!(err, DbError::Protocol(_)));
        assert!(err.to_string().contains("version 99"));
        // A token with no context id cannot be resumed.
        let mut empty = sample();
        empty.id = String::new();
        let token = ResumeToken(serde_json::to_vec(&empty).unwrap().into());
        assert!(EsResume::decode(&token).is_err());
    }

    /// `EsCursor::page_body` inserts `size`/`sort` into the token's body and so
    /// requires an object. The fresh-scan path validates that; the resume path
    /// did not, so a token carrying `"body": null` — which `current()` itself
    /// mints before `at()` fills it in — reached an `expect` and panicked the
    /// cursor. Null now normalises to `{}`; anything else is refused, because a
    /// body that is not an object has lost the scan's query and resuming
    /// without it would return more rows than the user asked for.
    #[test]
    fn a_token_body_that_is_not_an_object_is_normalised_or_refused() {
        let mut null_body = sample();
        null_body.body = Json::Null;
        let token = ResumeToken(serde_json::to_vec(&null_body).unwrap().into());
        let back = EsResume::decode(&token).expect("a null body means `no body`");
        assert_eq!(back.body, json!({}));

        for bad in [json!("query"), json!([1, 2]), json!(7), json!(true)] {
            let mut r = sample();
            r.body = bad.clone();
            let token = ResumeToken(serde_json::to_vec(&r).unwrap().into());
            let err = EsResume::decode(&token)
                .expect_err(&format!("body {bad} must be refused, not panicked on"));
            assert!(err.to_string().contains("must be a JSON object"), "{err}");
        }
    }

    #[test]
    fn a_token_survives_the_api_level_serde_round_trip_the_core_performs() {
        // The core persists a ResumeToken through serde; prove the whole
        // path (EsResume -> ResumeToken -> JSON -> ResumeToken -> EsResume).
        let token = sample().encode().unwrap();
        let wire = serde_json::to_string(&token).unwrap();
        let back: ResumeToken = serde_json::from_str(&wire).unwrap();
        assert_eq!(EsResume::decode(&back).unwrap(), sample());
    }
}
