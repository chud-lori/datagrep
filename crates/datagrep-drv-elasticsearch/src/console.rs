//! The connection's native language (`LanguageId::EsDsl`).
//!
//! `datagrep-lang` has no Elasticsearch/Kibana-console language — its
//! `Language` implementations are SQL, MongoShell and RedisCli — so this
//! module accepts the two forms an Elasticsearch user actually types, and the
//! crate report records the gap:
//!
//! ```text
//! GET /my-index/_search          <- Kibana console: verb, path, optional body
//! { "query": { "match_all": {} } }
//! ```
//!
//! ```text
//! { "query": { "match_all": {} } }   <- a bare search body against the
//!                                       connection's default index
//! ```
//!
//! # Parameters are bound into the parsed document, never spliced (§3.8)
//!
//! `Request::Native { params }` values are substituted **after** the body has
//! been parsed into JSON, by replacing any string that is exactly `"$1"`,
//! `"$2"`, … with the typed value. Because the substitution happens on the
//! tree rather than on the text, a parameter can never introduce a key, a
//! clause, or a nesting level — the design doc's NoSQL-injection rule (§3.8
//! risk 4) applied to this engine. A parameter is also never substituted into
//! the request *path*, so it can never retarget the request at another
//! endpoint.

use serde_json::Value as Json;

use datagrep_api::error::DbError;
use datagrep_api::value::Value;

use crate::http::Method;
use crate::value::value_to_json;

/// One parsed console request.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleRequest {
    pub method: Method,
    /// Always begins with `/`.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Json>,
}

impl ConsoleRequest {
    /// Whether this request is a search whose results should stream through a
    /// [`crate::cursor::SearchCursor`] rather than come back as a single
    /// reply document.
    pub fn is_search(&self) -> bool {
        let last = self.path.rsplit('/').next().unwrap_or_default();
        matches!(last, "_search") && !self.path.contains("/_search/scroll")
    }

    /// The index expression in a `/<index>/_search`-shaped path, if any.
    pub fn search_index(&self) -> Option<&str> {
        let trimmed = self.path.trim_start_matches('/');
        let (index, rest) = trimmed.rsplit_once('/')?;
        if rest != "_search" || index.is_empty() {
            return None;
        }
        Some(index)
    }
}

/// Parse console text into a request, binding `params` into the body.
pub fn parse(
    text: &str,
    default_index: Option<&str>,
    params: &[Value],
) -> Result<ConsoleRequest, DbError> {
    let cleaned = strip_comment_lines(text);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err(DbError::Query {
            code: None,
            message: "empty request".to_string(),
            position: Some(0),
        });
    }

    let (method, path, query, body_text) = if trimmed.starts_with('{') {
        // A bare search body against the default index.
        let index = default_index.unwrap_or("_all");
        (
            Method::Post,
            format!("/{index}/_search"),
            Vec::new(),
            Some(trimmed),
        )
    } else {
        let (first_line, rest) = match trimmed.split_once('\n') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (trimmed, ""),
        };
        let mut parts = first_line.split_whitespace();
        let verb = parts.next().unwrap_or_default();
        let method = Method::parse(verb).ok_or_else(|| DbError::Query {
            code: None,
            message: format!(
                "expected a Kibana-console request line (`GET /index/_search`) or a bare JSON \
                 search body; `{verb}` is not an HTTP method"
            ),
            position: Some(0),
        })?;
        let target = parts.next().ok_or_else(|| DbError::Query {
            code: None,
            message: "request line has a method but no path".to_string(),
            position: Some(verb.len() as u32),
        })?;
        if parts.next().is_some() {
            return Err(DbError::Query {
                code: None,
                message: "request line has trailing text after the path".to_string(),
                position: Some(first_line.len() as u32),
            });
        }
        let (raw_path, query) = split_query(target);
        let path = if raw_path.starts_with('/') {
            raw_path.to_string()
        } else {
            format!("/{raw_path}")
        };
        (
            method,
            path,
            query,
            if rest.is_empty() { None } else { Some(rest) },
        )
    };

    let mut body = match body_text {
        None => None,
        Some(text) => Some(serde_json::from_str::<Json>(text).map_err(|e| DbError::Query {
            code: None,
            message: format!("request body is not valid JSON: {e}"),
            // `serde_json` reports a 1-based line/column; the byte offset of
            // the body inside the original text is the honest anchor we have.
            position: Some(cleaned.len().saturating_sub(text.len()) as u32),
        })?),
    };

    if let Some(body) = body.as_mut() {
        bind_params(body, params)?;
    } else if !params.is_empty() {
        return Err(DbError::Query {
            code: None,
            message: "parameters were supplied but the request has no JSON body to bind them into"
                .to_string(),
            position: None,
        });
    }

    Ok(ConsoleRequest {
        method,
        path,
        query,
        body,
    })
}

fn split_query(target: &str) -> (&str, Vec<(String, String)>) {
    let Some((path, query)) = target.split_once('?') else {
        return (target, Vec::new());
    };
    let pairs = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect();
    (path, pairs)
}

/// Drop whole-line `#` comments (Kibana console's own convention). A line
/// whose first non-space character is `#` can never be part of a valid JSON
/// body, so this cannot corrupt one.
fn strip_comment_lines(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace `"$1"`, `"$2"`, … in the *parsed* document with typed parameter
/// values. Returns an error for an out-of-range index rather than leaving a
/// literal `"$3"` in the query, which would silently search for that string.
pub fn bind_params(body: &mut Json, params: &[Value]) -> Result<(), DbError> {
    match body {
        Json::String(s) => {
            if let Some(index) = placeholder_index(s) {
                let value = params.get(index - 1).ok_or_else(|| DbError::Query {
                    code: None,
                    message: format!(
                        "query references ${index} but only {} parameter(s) were supplied",
                        params.len()
                    ),
                    position: None,
                })?;
                *body = value_to_json(value);
            }
            Ok(())
        }
        Json::Array(items) => {
            for item in items {
                bind_params(item, params)?;
            }
            Ok(())
        }
        Json::Object(map) => {
            for value in map.values_mut() {
                bind_params(value, params)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `"$12"` -> `Some(12)`; anything else -> `None`. Deliberately strict: only a
/// string that is *entirely* a placeholder is substituted, so a document
/// legitimately containing `"cost: $5"` is left alone.
fn placeholder_index(s: &str) -> Option<usize> {
    let digits = s.strip_prefix('$')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = digits.parse().ok()?;
    (n > 0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn a_kibana_console_request_line_plus_body_parses() {
        let req = parse(
            "GET /my-index/_search\n{ \"query\": { \"match_all\": {} } }",
            None,
            &[],
        )
        .unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/my-index/_search");
        assert_eq!(req.body.unwrap()["query"]["match_all"], json!({}));
    }

    #[test]
    fn a_bare_json_body_targets_the_default_index() {
        let req = parse("{\"query\":{\"match_all\":{}}}", Some("events"), &[]).unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/events/_search");
        assert!(req.is_search());
        assert_eq!(req.search_index(), Some("events"));

        // With no default index configured it is cluster-wide, explicitly.
        let req = parse("{}", None, &[]).unwrap();
        assert_eq!(req.path, "/_all/_search");
    }

    #[test]
    fn query_parameters_on_the_request_line_are_kept() {
        let req = parse("POST /i/_search?size=5&pretty", None, &[]).unwrap();
        assert_eq!(
            req.query,
            vec![
                ("size".to_string(), "5".to_string()),
                ("pretty".to_string(), String::new())
            ]
        );
        assert_eq!(req.path, "/i/_search");
    }

    #[test]
    fn non_search_requests_are_recognised_as_such() {
        let req = parse("GET /_cluster/health", None, &[]).unwrap();
        assert!(!req.is_search());
        assert!(req.search_index().is_none());
        assert!(req.body.is_none());

        let scroll = parse("POST /_search/scroll\n{\"scroll_id\":\"x\"}", None, &[]).unwrap();
        assert!(
            !scroll.is_search(),
            "a scroll continuation is not a new search"
        );
    }

    #[test]
    fn comment_lines_are_ignored() {
        let req = parse(
            "# find the errors\nGET /logs/_search\n{\"size\":1}",
            None,
            &[],
        )
        .unwrap();
        assert_eq!(req.path, "/logs/_search");
        assert_eq!(req.body.unwrap()["size"], json!(1));
    }

    #[test]
    fn garbage_is_rejected_with_a_position_the_editor_can_point_at() {
        for text in ["", "   ", "SELECT * FROM t", "GET", "GET /a /b"] {
            let err = parse(text, None, &[]).unwrap_err();
            assert!(matches!(err, DbError::Query { .. }), "{text:?} -> {err:?}");
        }
        let err = parse("GET /i/_search\n{not json}", None, &[]).unwrap_err();
        match err {
            DbError::Query { message, position, .. } => {
                assert!(message.contains("not valid JSON"));
                assert!(position.is_some());
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    /// §3.8 risk 4, this engine's version: a parameter is substituted into the
    /// parsed tree as a typed value, so it can never become a clause.
    #[test]
    fn a_parameter_can_never_inject_a_query_clause() {
        // The attacker supplies a value that, if string-spliced, would close
        // the term and open a `match_all` — and, as a document, would be a
        // whole query clause.
        let hostile_text = Value::Str(Arc::from(
            r#"x"}},{"match_all":{}},{"term":{"y":{"value":"#,
        ));
        let mut req = parse(
            r#"GET /i/_search
{"query":{"bool":{"filter":[{"term":{"user":{"value":"$1"}}}]}}}"#,
            None,
            &[hostile_text],
        )
        .unwrap();

        let body = req.body.take().unwrap();
        let filters = body["query"]["bool"]["filter"].as_array().unwrap();
        assert_eq!(filters.len(), 1, "no extra clause was created");
        // The hostile text is a single JSON string operand, quotes and all.
        assert_eq!(
            filters[0]["term"]["user"]["value"],
            json!(r#"x"}},{"match_all":{}},{"term":{"y":{"value":"#)
        );
        // And the structure around it is byte-identical to what was typed.
        assert_eq!(
            filters[0]["term"]["user"].as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["value"]
        );
    }

    #[test]
    fn an_object_parameter_lands_as_an_operand_not_as_structure() {
        let hostile = Value::Document(Arc::new(datagrep_api::value::Document::from_fields(
            vec![(Arc::from("match_all"), Value::Document(Arc::new(datagrep_api::value::Document::new())))],
        )));
        let req = parse(
            r#"{"query":{"term":{"f":{"value":"$1"}}}}"#,
            Some("i"),
            &[hostile],
        )
        .unwrap();
        let body = req.body.unwrap();
        // `f`'s options object still has exactly one key.
        assert_eq!(
            body["query"]["term"]["f"].as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["value"]
        );
        // The attacker's document is the compared *value*, one level deeper.
        assert_eq!(body["query"]["term"]["f"]["value"], json!({"match_all": {}}));
        // And the query has not sprouted a second clause.
        assert_eq!(
            body["query"].as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["term"]
        );
    }

    #[test]
    fn parameters_bind_positionally_and_keep_their_types() {
        let req = parse(
            r#"{"query":{"bool":{"filter":[
                 {"term":{"a":{"value":"$1"}}},
                 {"range":{"b":{"gte":"$2"}}},
                 {"terms":{"c":["$3","$1"]}}
               ]}}}"#,
            Some("i"),
            &[
                Value::Str(Arc::from("s")),
                Value::I64(42),
                Value::Bool(true),
            ],
        )
        .unwrap();
        let f = &req.body.unwrap()["query"]["bool"]["filter"];
        assert_eq!(f[0]["term"]["a"]["value"], json!("s"));
        assert_eq!(f[1]["range"]["b"]["gte"], json!(42));
        assert!(f[1]["range"]["b"]["gte"].is_number(), "typed, not stringified");
        assert_eq!(f[2]["terms"]["c"], json!([true, "s"]));
    }

    #[test]
    fn a_missing_parameter_is_an_error_not_a_literal_dollar_string() {
        let err = parse(r#"{"query":{"term":{"a":{"value":"$3"}}}}"#, Some("i"), &[])
            .unwrap_err();
        assert!(err.to_string().contains("$3"));
        // …and supplying params with no body at all is also refused.
        assert!(parse("GET /_cluster/health", None, &[Value::I64(1)]).is_err());
    }

    #[test]
    fn only_a_string_that_is_entirely_a_placeholder_is_substituted() {
        assert_eq!(placeholder_index("$1"), Some(1));
        assert_eq!(placeholder_index("$12"), Some(12));
        assert_eq!(placeholder_index("$0"), None);
        assert_eq!(placeholder_index("$"), None);
        assert_eq!(placeholder_index("$a"), None);
        assert_eq!(placeholder_index("cost: $5"), None);
        assert_eq!(placeholder_index("price$1"), None);

        // A document that legitimately mentions a dollar amount is untouched.
        let req = parse(
            r#"{"query":{"match":{"note":{"query":"refund $5 please"}}}}"#,
            Some("i"),
            &[Value::I64(9)],
        )
        .unwrap();
        assert_eq!(
            req.body.unwrap()["query"]["match"]["note"]["query"],
            json!("refund $5 please")
        );
    }

    #[test]
    fn placeholders_are_never_taken_from_the_path() {
        // `$1` in a path is not a placeholder — it stays literal, so a
        // parameter can never retarget the request at another endpoint.
        let req = parse("GET /$1/_search\n{}", None, &[Value::Str(Arc::from("other"))]).unwrap();
        assert_eq!(req.path, "/$1/_search");
    }
}
