//! Mapping the two error surfaces of a hand-rolled REST driver onto
//! `datagrep-api`'s single [`DbError`]: transport failures from `reqwest`, and
//! the structured JSON error envelope Elasticsearch/OpenSearch return with a
//! non-2xx status.
//!
//! **Nothing here ever logs or embeds credentials.** `map_reqwest_error`
//! deliberately formats the error without its URL (a `reqwest::Error`'s
//! `Display` includes the request URL, and a URL can carry `user:pass@`), so a
//! connect failure can never leak a password into a log line or a UI toast.

use serde_json::Value as Json;

use datagrep_api::error::DbError;

/// Map a transport-level `reqwest` failure. Recoverability follows
/// `DbError`'s own split: a timeout is recoverable, a broken connection is not.
pub fn map_reqwest_error(err: reqwest::Error) -> DbError {
    // `err.to_string()` on a reqwest error includes the URL, which may contain
    // userinfo. Strip it by rebuilding the message from the error's own
    // classification plus the innermost source, never the URL.
    let detail = innermost(&err);
    if err.is_timeout() {
        return DbError::Timeout;
    }
    if err.is_connect() {
        return DbError::Connect(detail);
    }
    if err.is_decode() || err.is_body() {
        return DbError::Protocol(detail);
    }
    if err.is_builder() {
        return DbError::Config(datagrep_api::config::ConfigError::InvalidUrl { reason: detail });
    }
    DbError::Io(std::io::Error::other(detail))
}

/// The deepest `source()` message, which is the useful part (`connection
/// refused`, `certificate verify failed`) and, unlike the top-level
/// `Display`, never contains the request URL.
fn innermost(err: &(dyn std::error::Error + 'static)) -> String {
    let mut cur: &(dyn std::error::Error + 'static) = err;
    let mut last = String::new();
    let mut depth = 0;
    while let Some(src) = cur.source() {
        last = src.to_string();
        cur = src;
        depth += 1;
        if depth > 8 {
            break;
        }
    }
    if last.is_empty() {
        // No source chain: fall back to a class label rather than the
        // URL-bearing Display string.
        "http request failed".to_string()
    } else {
        last
    }
}

/// Turn a non-2xx Elasticsearch/OpenSearch response into a [`DbError`].
///
/// The engine's JSON envelope is
/// `{"error": {"type": "...", "reason": "...", "root_cause": [...]}, "status": N}`;
/// `error` is occasionally a bare string (older OpenSearch, some plugins), and
/// on a proxy failure the body is not JSON at all. All three are handled, and
/// the engine's own `type` is preserved verbatim as `DbError::Query::code`,
/// so an error stays searchable against the engine's own documentation.
pub fn map_status_error(status: u16, body: &str) -> DbError {
    let parsed: Option<Json> = serde_json::from_str(body).ok();
    let (code, message) = match parsed.as_ref().and_then(|j| j.get("error")) {
        Some(Json::Object(err)) => {
            let ty = err.get("type").and_then(Json::as_str).map(str::to_string);
            let reason = err
                .get("reason")
                .and_then(Json::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| truncate(body));
            // A root cause is usually more specific than the outer reason
            // (`index_not_found_exception` vs `search_phase_execution_exception`).
            let root = err
                .get("root_cause")
                .and_then(Json::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("reason"))
                .and_then(Json::as_str);
            let message = match root {
                Some(r) if r != reason => format!("{reason}: {r}"),
                _ => reason,
            };
            (ty, message)
        }
        Some(Json::String(s)) => (None, s.clone()),
        _ => (None, truncate(body)),
    };

    match status {
        401 | 403 => DbError::Auth(message),
        408 | 504 => DbError::Timeout,
        429 => DbError::ResourceExhausted(message),
        _ => DbError::Query {
            code,
            message,
            // Elasticsearch reports no byte offset into the submitted body, so
            // there is nothing honest to put here for an editor squiggle.
            position: None,
        },
    }
}

/// Bound a non-JSON error body (an HTML 502 from a load balancer, say) so a
/// megabyte of proxy markup never ends up in an error string.
fn truncate(body: &str) -> String {
    const MAX: usize = 512;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty error body".to_string();
    }
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_error_keeps_engine_type_as_code() {
        let body = r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [nope]"}],"type":"index_not_found_exception","reason":"no such index [nope]"},"status":404}"#;
        match map_status_error(404, body) {
            DbError::Query { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("index_not_found_exception"));
                assert!(message.contains("no such index"));
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn root_cause_is_appended_when_more_specific() {
        let body = r#"{"error":{"root_cause":[{"type":"query_shard_exception","reason":"failed to create query"}],"type":"search_phase_execution_exception","reason":"all shards failed"},"status":400}"#;
        match map_status_error(400, body) {
            DbError::Query { message, .. } => {
                assert_eq!(message, "all shards failed: failed to create query");
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn auth_and_throttle_statuses_map_to_their_own_variants() {
        assert!(matches!(
            map_status_error(
                401,
                r#"{"error":{"type":"security_exception","reason":"missing authentication credentials"}}"#
            ),
            DbError::Auth(_)
        ));
        assert!(matches!(
            map_status_error(
                429,
                r#"{"error":{"type":"circuit_breaking_exception","reason":"too much"}}"#
            ),
            DbError::ResourceExhausted(_)
        ));
        assert!(matches!(
            map_status_error(504, "gateway timeout"),
            DbError::Timeout
        ));
    }

    #[test]
    fn non_json_body_is_bounded_not_dumped() {
        let html = "<html>".to_string() + &"x".repeat(10_000) + "</html>";
        match map_status_error(502, &html) {
            DbError::Query { message, .. } => {
                assert!(
                    message.len() < 600,
                    "body must be truncated, got {}",
                    message.len()
                );
                assert!(message.ends_with('…'));
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn empty_body_is_named_not_blank() {
        match map_status_error(500, "   ") {
            DbError::Query { message, .. } => assert_eq!(message, "empty error body"),
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
