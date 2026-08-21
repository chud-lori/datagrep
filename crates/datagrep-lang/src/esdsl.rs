use datagrep_api::LanguageId;

use crate::directives::{extract_directive_lines, parse_directives};
use crate::{EditContext, Language, StatementClass, StatementSpan, Token, TokenKind};

const DIRECTIVE_MARKER: &str = "#";

const SLASH_DIRECTIVE_MARKER: &str = "//";

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

#[derive(Debug)]
pub struct EsDslLanguage;

pub static ES_DSL: EsDslLanguage = EsDslLanguage;

impl Language for EsDslLanguage {
    fn id(&self) -> LanguageId {
        LanguageId::EsDsl
    }

    fn split(&self, src: &str) -> Vec<StatementSpan> {
        split(src)
    }

    fn classify(&self, stmt: &str) -> StatementClass {
        classify(stmt)
    }

    fn context_at(&self, src: &str, byte_offset: usize) -> EditContext {
        context_at(src, byte_offset)
    }

    fn highlight(&self, src: &str) -> Vec<Token> {
        highlight(src)
    }
}

fn split(src: &str) -> Vec<StatementSpan> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut spans = Vec::new();
    let mut i = 0usize;

    while i < len {
        i = skip_ws_and_comments(bytes, i);
        if i >= len {
            break;
        }
        let Some(method_end) = read_method(bytes, i) else {
            i = next_line(bytes, i);
            continue;
        };

        let content_start = i;
        // The request line runs to end of line; the path/query lives here.
        let mut end = method_end;
        while end < len && bytes[end] != b'\n' {
            end += 1;
        }

        loop {
            let peek = skip_ws_and_comments(bytes, end);
            if peek < len && bytes[peek] == b'{' {
                let obj_end = scan_json_object(bytes, peek);
                end = obj_end;
                if obj_end >= len {
                    break;
                }
            } else {
                break;
            }
        }

        emit_span(&mut spans, src, content_start, end);
        i = end;
    }

    spans
}

fn emit_span(spans: &mut Vec<StatementSpan>, src: &str, start: usize, end: usize) {
    if start >= end {
        return;
    }
    let floor = spans.last().map(|s| s.range.end).unwrap_or(0);
    let scoped = &src[floor..];
    let scoped_start = start - floor;
    let mut lines = extract_directive_lines(scoped, scoped_start, DIRECTIVE_MARKER);
    if lines.is_empty() {
        lines = extract_directive_lines(scoped, scoped_start, SLASH_DIRECTIVE_MARKER);
    }
    let directives = parse_directives(&lines);
    spans.push(StatementSpan {
        range: start..end,
        directives,
    });
}

fn read_method(bytes: &[u8], i: usize) -> Option<usize> {
    for m in METHODS {
        let end = i + m.len();
        if end <= bytes.len()
            && bytes[i..end].eq_ignore_ascii_case(m.as_bytes())
            && bytes.get(end).is_some_and(|b| b.is_ascii_whitespace())
        {
            return Some(end);
        }
    }
    None
}

fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> usize {
    let len = bytes.len();
    loop {
        // Whitespace.
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            return i;
        }
        match bytes[i] {
            b'#' => i = end_of_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = end_of_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(len);
            }
            _ => return i,
        }
    }
}

fn end_of_line(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn next_line(bytes: &[u8], i: usize) -> usize {
    let eol = end_of_line(bytes, i);
    (eol + 1).min(bytes.len())
}

fn scan_json_object(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start;
    let mut depth = 0usize;
    while i < len {
        match bytes[i] {
            b'"' => {
                i = scan_string(bytes, i);
            }
            b'#' => i = end_of_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = end_of_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(len);
            }
            b'\n' => {
                i += 1;
                let line_start = i;
                let mut j = i;
                while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if read_method(bytes, j).is_some() {
                    return line_start;
                }
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    len
}

fn scan_string(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    if bytes.get(start + 1) == Some(&b'"') && bytes.get(start + 2) == Some(&b'"') {
        // Triple-quoted: scan to the next `"""`.
        let mut i = start + 3;
        while i + 2 < len {
            if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                return i + 3;
            }
            i += 1;
        }
        return len;
    }
    let mut i = start + 1;
    while i < len {
        match bytes[i] {
            b'\\' if i + 1 < len => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    len
}

// Mirror of READ_ONLY_POST_ENDPOINTS in datagrep-drv-elasticsearch — keep the two tables in sync.
const READ_ACTIONS: &[&str] = &[
    "_search",
    "_msearch",
    "_count",
    "_explain",
    "_validate",
    "_field_caps",
    "_analyze",
    "_pit",
    "_async_search",
    "_terms_enum",
    "_rank_eval",
    "_search_shards",
    "_resolve",
    "_mget",
    "_termvectors",
    "_mtermvectors",
];

const ADMIN_ACTIONS: &[&str] = &[
    "_cluster",
    "_snapshot",
    "_slm",
    "_nodes",
    "_tasks",
    "_ilm",
    "_ingest",
    "_security",
    "_reroute",
    "_ccr",
    "_watcher",
];

const DDL_ACTIONS: &[&str] = &[
    "_mapping",
    "_mappings",
    "_settings",
    "_alias",
    "_aliases",
    "_template",
    "_index_template",
    "_component_template",
    "_open",
    "_close",
    "_clone",
    "_shrink",
    "_split",
    "_rollover",
    "_refresh",
    "_flush",
    "_forcemerge",
    "_cache",
];

const WRITE_ACTIONS: &[&str] = &[
    "_doc",
    "_create",
    "_update",
    "_bulk",
    "_delete_by_query",
    "_update_by_query",
    "_reindex",
];

fn classify(stmt: &str) -> StatementClass {
    let Some((method, path)) = parse_request_line(stmt) else {
        return StatementClass::Unknown;
    };

    // GET/HEAD are always reads.
    if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
        return StatementClass::Read;
    }

    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let has = |set: &[&str]| segments.iter().any(|s| set.contains(s));

    // A POST to a read endpoint (`_search`, `_count`, …) is a read.
    if method.eq_ignore_ascii_case("POST") && has(READ_ACTIONS) {
        return StatementClass::Read;
    }
    if has(ADMIN_ACTIONS) {
        return StatementClass::Admin;
    }
    if has(DDL_ACTIONS) {
        return StatementClass::Ddl;
    }
    if has(WRITE_ACTIONS) {
        return StatementClass::Write;
    }

    if segments.is_empty() {
        return StatementClass::Unknown;
    }
    if method.eq_ignore_ascii_case("PUT") || method.eq_ignore_ascii_case("DELETE") {
        StatementClass::Ddl
    } else {
        StatementClass::Write
    }
}

fn parse_request_line(stmt: &str) -> Option<(&str, &str)> {
    for line in stmt.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("//")
            || line.starts_with("/*")
        {
            continue;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next()?;
        if !METHODS.iter().any(|m| method.eq_ignore_ascii_case(m)) {
            return None;
        }
        let target = parts.next().unwrap_or("");
        // Drop any `?query=string` — only the path decides the class.
        let path = target.split('?').next().unwrap_or(target);
        return Some((method, path));
    }
    None
}

fn context_at(src: &str, offset: usize) -> EditContext {
    let bytes = src.as_bytes();
    let end = offset.min(bytes.len());
    let mut i = 0usize;

    while i < end {
        match bytes[i] {
            b'#' => {
                let eol = end_of_line(bytes, i);
                if end <= eol {
                    return EditContext::Comment;
                }
                i = eol;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let eol = end_of_line(bytes, i);
                if end <= eol {
                    return EditContext::Comment;
                }
                i = eol;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut j = i + 2;
                while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                    j += 1;
                }
                let close = (j + 2).min(bytes.len());
                if end < close {
                    return EditContext::Comment;
                }
                i = close;
            }
            b'"' => {
                let close = scan_string(bytes, i);
                if end < close {
                    return EditContext::StringLiteral;
                }
                i = close;
            }
            _ => i += 1,
        }
    }

    let is_ident_byte =
        |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' || b == b'*';
    let before = offset.checked_sub(1).and_then(|p| bytes.get(p)).copied();
    let at = bytes.get(offset).copied();
    if before.is_some_and(is_ident_byte) || at.is_some_and(is_ident_byte) {
        EditContext::Identifier
    } else {
        EditContext::Statement
    }
}

fn highlight(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut at_line_start = true;

    while i < len {
        let b = bytes[i];
        match b {
            b'\n' => {
                at_line_start = true;
                i += 1;
            }
            b if b.is_ascii_whitespace() => i += 1,
            b'#' => {
                let eol = end_of_line(bytes, i);
                out.push(tok(i, eol, TokenKind::Comment));
                i = eol;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let eol = end_of_line(bytes, i);
                out.push(tok(i, eol, TokenKind::Comment));
                i = eol;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut j = i + 2;
                while j + 1 < len && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                    j += 1;
                }
                let close = (j + 2).min(len);
                out.push(tok(i, close, TokenKind::Comment));
                i = close;
            }
            b'/' => {
                // A path separator on a request line.
                out.push(tok(i, i + 1, TokenKind::Punct));
                i += 1;
                at_line_start = false;
            }
            b'"' => {
                let close = scan_string(bytes, i);
                out.push(tok(i, close, TokenKind::String));
                i = close;
                at_line_start = false;
            }
            b'?' | b'&' | b'=' => {
                out.push(tok(i, i + 1, TokenKind::Operator));
                i += 1;
                at_line_start = false;
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                out.push(tok(i, i + 1, TokenKind::Punct));
                i += 1;
                at_line_start = false;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < len
                    && (bytes[i].is_ascii_digit()
                        || matches!(bytes[i], b'.' | b'e' | b'E' | b'+' | b'-'))
                {
                    i += 1;
                }
                out.push(tok(start, i, TokenKind::Number));
                at_line_start = false;
            }
            _ if is_word_byte(b) => {
                let start = i;
                while i < len && is_word_byte(bytes[i]) {
                    i += 1;
                }
                let word = &src[start..i];
                let is_method =
                    at_line_start && METHODS.iter().any(|m| word.eq_ignore_ascii_case(m));
                let kind = if is_method || matches!(word, "true" | "false" | "null") {
                    TokenKind::Keyword
                } else {
                    TokenKind::Ident
                };
                out.push(tok(start, i, kind));
                at_line_start = false;
            }
            _ => {
                i += 1;
                at_line_start = false;
            }
        }
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'*'
}

fn tok(start: usize, end: usize, kind: TokenKind) -> Token {
    Token {
        range: start..end,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn texts(src: &str) -> Vec<String> {
        split(src)
            .iter()
            .map(|s| s.text(src).trim().to_string())
            .collect()
    }

    #[test]
    fn two_requests_split_on_the_method_not_a_blank_line() {
        // No blank line between them — the boundary is the next method token.
        let src = "GET _search\nPOST _test_index/_doc\n{\"a\":1}";
        assert_eq!(
            texts(src),
            vec!["GET _search", "POST _test_index/_doc\n{\"a\":1}"]
        );
    }

    #[test]
    fn a_request_line_plus_body_is_one_statement() {
        let src = "GET /my-index/_search\n{ \"query\": { \"match_all\": {} } }";
        let spans = split(src);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text(src), src);
    }

    #[test]
    fn getter_is_not_a_method() {
        // `GETTER` must not be read as `GET` — the char after must be space.
        let src = "GETTER /x";
        // No method line at all -> nothing splits out.
        assert!(texts(src).is_empty());
    }

    #[test]
    fn bulk_ndjson_stays_one_request() {
        // Several JSON objects in a row belong to one `_bulk` request.
        let src = "POST _bulk\n{\"index\":{\"_id\":1}}\n{\"field\":\"a\"}\n{\"index\":{\"_id\":2}}\n{\"field\":\"b\"}";
        let spans = split(src);
        assert_eq!(spans.len(), 1, "bulk is one request, not four");
        assert_eq!(spans[0].text(src), src);
    }

    #[test]
    fn a_body_with_two_objects_then_a_second_request() {
        let src = "POST _bulk\n{\"index\":{}}\n{\"a\":1}\nGET /i/_search\n{}";
        assert_eq!(
            texts(src),
            vec![
                "POST _bulk\n{\"index\":{}}\n{\"a\":1}",
                "GET /i/_search\n{}"
            ]
        );
    }

    #[test]
    fn triple_quoted_string_with_a_brace_does_not_end_the_body() {
        // The embedded `}` inside `"""…"""` must not close the object early.
        let src = "POST /i/_doc\n{\"script\":\"\"\"ctx._source.x = {}\"\"\"}\nGET /i/_search";
        let spans = split(src);
        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans[0].text(src),
            "POST /i/_doc\n{\"script\":\"\"\"ctx._source.x = {}\"\"\"}"
        );
        assert_eq!(spans[1].text(src), "GET /i/_search");
    }

    #[test]
    fn string_with_escaped_quote_and_brace() {
        let src = "PUT /i/_doc/1\n{\"note\":\"a \\\" and a }\"}\nGET /i";
        let spans = split(src);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].text(src), "GET /i");
    }

    #[test]
    fn comment_forms_between_requests_are_skipped() {
        let src =
            "# a pound comment\nGET /i/_search\n// a slash comment\n/* block */\nDELETE /i/_doc/1";
        assert_eq!(texts(src), vec!["GET /i/_search", "DELETE /i/_doc/1"]);
    }

    #[test]
    fn a_brace_in_a_line_comment_does_not_swallow_the_next_request() {
        let src = "POST /i/_search\n{\n // { note\n \"q\":1\n}\nGET /o/_search";
        assert_eq!(
            texts(src),
            vec![
                "POST /i/_search\n{\n // { note\n \"q\":1\n}",
                "GET /o/_search"
            ]
        );
    }

    #[test]
    fn a_brace_in_a_block_comment_does_not_truncate_the_body() {
        let src = "GET /i/_search\n{ /* } */ \"q\":1 }\nGET /b/_search";
        assert_eq!(
            texts(src),
            vec!["GET /i/_search\n{ /* } */ \"q\":1 }", "GET /b/_search"]
        );
    }

    #[test]
    fn an_unterminated_body_reanchors_on_the_next_method_line() {
        let src = "GET /a/_search\n{ \"q\":1\nGET /b/_search\n{}";
        assert_eq!(
            texts(src),
            vec!["GET /a/_search\n{ \"q\":1", "GET /b/_search\n{}"]
        );
    }

    #[test]
    fn garbage_line_reanchors_on_the_next_method() {
        let src = "not a request at all\nGET /i/_search";
        assert_eq!(texts(src), vec!["GET /i/_search"]);
    }

    #[test]
    fn empty_and_whitespace_only_yield_no_spans() {
        assert!(split("").is_empty());
        assert!(split("   \n  \t\n").is_empty());
        assert!(split("# just a comment\n").is_empty());
    }

    #[test]
    fn directives_above_a_request_are_parsed() {
        let src = "# @limit 50\n# @readonly\nGET /i/_search\n{}";
        let spans = split(src);
        assert_eq!(spans.len(), 1);
        let d = spans[0].directives.clone().unwrap();
        assert_eq!(d.limit, Some(50));
        assert!(d.readonly);
    }

    #[test]
    fn directives_carry_timeout_and_connection() {
        let src = "# @timeout 30s\n# @connection staging\nGET /i/_search";
        let d = split(src)[0].directives.clone().unwrap();
        assert_eq!(d.timeout, Some(Duration::from_secs(30)));
        assert_eq!(d.connection.as_deref(), Some("staging"));
    }

    #[test]
    fn a_malformed_directive_is_reported_per_statement_not_panicked() {
        let src = "# @bogus 1\nGET /i/_search";
        assert!(split(src)[0].directives.is_err());
    }

    #[test]
    fn slash_slash_directives_are_accepted() {
        let src = "// @limit 25\n// @readonly\nGET /i/_search";
        let spans = split(src);
        assert_eq!(spans.len(), 1);
        let d = spans[0].directives.clone().unwrap();
        assert_eq!(d.limit, Some(25));
        assert!(d.readonly);
    }

    #[test]
    fn a_directive_shaped_line_inside_the_previous_span_is_not_extracted() {
        let src = "POST /i/_doc\n{\"s\":\"\"\"\n# @limit 5\"\"\"}\nGET /b/_search";
        let spans = split(src);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].text(src).trim(), "GET /b/_search");
        assert_eq!(
            spans[1].directives.clone().unwrap(),
            crate::directives::Directives::default()
        );
    }

    #[test]
    fn classify_reads() {
        assert_eq!(classify("GET /i/_search\n{}"), StatementClass::Read);
        assert_eq!(classify("GET /_cluster/health"), StatementClass::Read);
        assert_eq!(classify("HEAD /i"), StatementClass::Read);
        assert_eq!(classify("POST /i/_search\n{}"), StatementClass::Read);
        assert_eq!(classify("POST /i/_count"), StatementClass::Read);
        assert_eq!(classify("POST /_field_caps"), StatementClass::Read);
        assert_eq!(classify("POST /i/_pit"), StatementClass::Read);
        assert_eq!(classify("POST /i/_mget\n{}"), StatementClass::Read);
        assert_eq!(classify("POST /i/_termvectors/1"), StatementClass::Read);
        assert_eq!(classify("POST /i/_mtermvectors\n{}"), StatementClass::Read);
    }

    #[test]
    fn classify_writes() {
        assert_eq!(classify("POST /i/_doc\n{}"), StatementClass::Write);
        assert_eq!(classify("PUT /i/_doc/1\n{}"), StatementClass::Write);
        assert_eq!(classify("DELETE /i/_doc/1"), StatementClass::Write);
        assert_eq!(classify("POST /i/_update/1\n{}"), StatementClass::Write);
        assert_eq!(classify("POST /_bulk\n{}"), StatementClass::Write);
        assert_eq!(classify("POST /_reindex\n{}"), StatementClass::Write);
        assert_eq!(
            classify("POST /i/_delete_by_query\n{}"),
            StatementClass::Write
        );
    }

    #[test]
    fn classify_ddl() {
        assert_eq!(classify("PUT /my-index"), StatementClass::Ddl);
        assert_eq!(classify("DELETE /my-index"), StatementClass::Ddl);
        assert_eq!(classify("PUT /i/_mapping\n{}"), StatementClass::Ddl);
        assert_eq!(classify("PUT /i/_settings\n{}"), StatementClass::Ddl);
        assert_eq!(classify("POST /i/_close"), StatementClass::Ddl);
        assert_eq!(classify("POST /_aliases\n{}"), StatementClass::Ddl);
    }

    #[test]
    fn classify_admin() {
        assert_eq!(
            classify("PUT /_cluster/settings\n{}"),
            StatementClass::Admin
        );
        assert_eq!(classify("PUT /_snapshot/repo\n{}"), StatementClass::Admin);
        assert_eq!(classify("POST /_tasks/abc/_cancel"), StatementClass::Admin);
    }

    #[test]
    fn classify_case_insensitive_and_unknown() {
        assert_eq!(classify("get /i/_search"), StatementClass::Read);
        assert_eq!(classify("post /i/_doc\n{}"), StatementClass::Write);
        assert_eq!(classify(""), StatementClass::Unknown);
        assert_eq!(classify("SELECT 1"), StatementClass::Unknown);
    }

    #[test]
    fn context_at_string_comment_and_identifier() {
        let src = "GET /i/_search\n{ \"query\": \"hello\" }\n# note";
        let in_string = src.find("hello").unwrap();
        assert_eq!(context_at(src, in_string), EditContext::StringLiteral);
        let in_comment = src.find("note").unwrap();
        assert_eq!(context_at(src, in_comment), EditContext::Comment);
        let in_ident = src.find("_search").unwrap() + 1;
        assert_eq!(context_at(src, in_ident), EditContext::Identifier);
    }

    #[test]
    fn context_at_inside_a_triple_quoted_string() {
        let src = "POST /i/_doc\n{\"s\":\"\"\"body here\"\"\"}";
        let inside = src.find("body").unwrap();
        assert_eq!(context_at(src, inside), EditContext::StringLiteral);
    }

    #[test]
    fn highlight_marks_method_path_strings_numbers_and_comments() {
        let src = "GET /my-index/_search?size=5\n{ \"n\": 7, \"ok\": true }\n# note";
        let toks = highlight(src);
        let has = |kind: TokenKind, text: &str| {
            toks.iter()
                .any(|t| t.kind == kind && &src[t.range.clone()] == text)
        };
        assert!(has(TokenKind::Keyword, "GET"), "method is a keyword");
        assert!(has(TokenKind::String, "\"n\""), "json key is a string");
        assert!(has(TokenKind::Number, "7"), "number highlighted");
        assert!(has(TokenKind::Keyword, "true"), "json literal is a keyword");
        assert!(
            toks.iter().any(|t| t.kind == TokenKind::Comment),
            "trailing comment highlighted"
        );
    }

    #[test]
    fn highlight_keeps_a_triple_quoted_string_as_one_token() {
        let src = "POST /i/_doc\n{\"s\":\"\"\"a } b\"\"\"}";
        let toks = highlight(src);
        assert!(toks
            .iter()
            .any(|t| t.kind == TokenKind::String && &src[t.range.clone()] == "\"\"\"a } b\"\"\""));
    }
}
