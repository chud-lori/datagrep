use datagrep_api::SqlDialect;

use super::lexer::{lex_chunks, Chunk};
use crate::directives::{extract_directive_lines, parse_directives};
use crate::StatementSpan;

const SQL_DIRECTIVE_MARKER: &str = "--";

pub fn split(src: &str, dialect: SqlDialect) -> Vec<StatementSpan> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let chunks = lex_chunks(src, dialect);

    let mut spans = Vec::new();
    let mut stmt_start = 0usize;
    let mut terminator: Vec<u8> = vec![b';'];

    for chunk in &chunks {
        let Chunk::Code(range) = chunk else { continue };
        let mut i = range.start;
        let chunk_end = range.end;

        while i < chunk_end {
            let at_line_start = i == 0 || bytes[i - 1] == b'\n';

            if at_line_start && dialect == SqlDialect::Mssql {
                if let Some(line_end) = line_end_at(bytes, i) {
                    if is_go_line(src[i..line_end].trim()) {
                        emit_span(&mut spans, src, &chunks, stmt_start, i);
                        let next = skip_newline(bytes, line_end);
                        stmt_start = next;
                        i = next;
                        continue;
                    }
                }
            }

            if at_line_start
                && dialect == SqlDialect::Mysql
                && bytes[stmt_start..i].iter().all(u8::is_ascii_whitespace)
            {
                if let Some(line_end) = line_end_at(bytes, i) {
                    if let Some(new_delim) = parse_delimiter_line(src[i..line_end].trim()) {
                        terminator = new_delim.as_bytes().to_vec();
                        let next = skip_newline(bytes, line_end);
                        stmt_start = next;
                        i = next;
                        continue;
                    }
                }
            }

            if !terminator.is_empty() && bytes[i..chunk_end].starts_with(terminator.as_slice()) {
                emit_span(&mut spans, src, &chunks, stmt_start, i);
                i += terminator.len();
                stmt_start = i;
                continue;
            }

            i += 1;
        }
    }

    emit_span(&mut spans, src, &chunks, stmt_start, len);
    spans
}

fn skip_newline(bytes: &[u8], pos: usize) -> usize {
    if bytes.get(pos) == Some(&b'\n') {
        pos + 1
    } else {
        pos
    }
}

fn line_end_at(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() {
        return None;
    }
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    Some(i)
}

fn is_go_line(trimmed: &str) -> bool {
    let mut words = trimmed.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some(go), None, None) => go.eq_ignore_ascii_case("go"),
        (Some(go), Some(n), None) => {
            go.eq_ignore_ascii_case("go") && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    }
}

fn parse_delimiter_line(trimmed: &str) -> Option<&str> {
    const KW: &str = "delimiter";
    if trimmed.len() <= KW.len() {
        return None;
    }
    let (head, tail) = trimmed.split_at(KW.len());
    if !head.eq_ignore_ascii_case(KW) {
        return None;
    }
    if !tail.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let rest = tail.trim();
    (!rest.is_empty()).then_some(rest)
}

fn emit_span(
    spans: &mut Vec<StatementSpan>,
    src: &str,
    chunks: &[Chunk],
    start: usize,
    end: usize,
) {
    if start >= end {
        return;
    }
    if src[start..end].trim().is_empty() {
        return;
    }
    let content_start = skip_leading_trivia(chunks, src.as_bytes(), start, end);
    let lines = extract_directive_lines(src, content_start, SQL_DIRECTIVE_MARKER);
    let directives = parse_directives(&lines);
    spans.push(StatementSpan {
        range: start..end,
        directives,
    });
}

fn skip_leading_trivia(chunks: &[Chunk], bytes: &[u8], start: usize, end: usize) -> usize {
    let mut pos = start;
    for chunk in chunks {
        let r = chunk.range();
        if r.end <= pos {
            continue;
        }
        if r.start >= end {
            break;
        }
        match chunk {
            Chunk::Comment(_) => pos = r.end.min(end),
            Chunk::Quoted(..) => return r.start.max(pos),
            Chunk::Code(cr) => {
                let s = cr.start.max(pos);
                let e = cr.end.min(end);
                if let Some(rel) = bytes[s..e].iter().position(|b| !b.is_ascii_whitespace()) {
                    return s + rel;
                }
                pos = e;
            }
        }
        if pos >= end {
            break;
        }
    }
    pos.min(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn texts(src: &str, dialect: SqlDialect) -> Vec<&str> {
        split(src, dialect).iter().map(|s| s.text(src)).collect()
    }

    #[test]
    fn semicolon_inside_string_is_not_a_terminator() {
        assert_eq!(
            texts(
                "select ';not a terminator'; select 2;",
                SqlDialect::Postgres
            ),
            vec!["select ';not a terminator'", " select 2"]
        );
    }

    #[test]
    fn semicolon_inside_ident_and_comment_is_not_a_terminator() {
        assert_eq!(
            texts(
                "select \"weird;name\"; -- trailing ; in comment\nselect 2;",
                SqlDialect::Postgres
            )
            .len(),
            2
        );
    }

    #[test]
    fn semicolon_inside_dollar_quote_is_not_a_terminator() {
        let src = "create function f() returns void as $$ begin; end; $$ language sql; select 1;";
        assert_eq!(split(src, SqlDialect::Postgres).len(), 2);
    }

    #[test]
    fn nested_block_comment_semicolon_ignored_postgres() {
        let src = "/* a; /* nested; */ still comment; */ select 1;";
        let spans = texts(src, SqlDialect::Postgres);
        assert_eq!(spans, vec![&src[..src.len() - 1]]);
    }

    #[test]
    fn dollar_tag_variants() {
        let src = "select $tag$ ; $$ inner $$ ; $tag$; select 2;";
        assert_eq!(split(src, SqlDialect::Postgres).len(), 2);
    }

    #[test]
    fn mysql_delimiter_switch_and_back() {
        let src = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\nDELIMITER ;\nSELECT 3;";
        let spans = texts(src, SqlDialect::Mysql);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].contains("BEGIN SELECT 1; SELECT 2; END"));
        assert_eq!(spans[1].trim(), "SELECT 3");
    }

    #[test]
    fn mssql_go_alone_on_line_splits_batch() {
        let src = "SELECT 1\nGO\nSELECT 2\nGO 3\nSELECT 3";
        let spans = texts(src, SqlDialect::Mssql);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].trim(), "SELECT 1");
        assert_eq!(spans[1].trim(), "SELECT 2");
        assert_eq!(spans[2].trim(), "SELECT 3");
    }

    #[test]
    fn mssql_go_mid_line_does_not_split() {
        let src = "SELECT 'GO' AS x\nSELECT GO_FAST\nGO\nSELECT 1";
        let spans = texts(src, SqlDialect::Mssql);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].contains("SELECT 'GO' AS x"));
        assert!(spans[0].contains("SELECT GO_FAST"));
    }

    #[test]
    fn empty_statements_are_skipped_without_panicking() {
        let spans = texts("SELECT 1;;   ;\nSELECT 2;", SqlDialect::Postgres);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn missing_trailing_semicolon_still_a_span() {
        let spans = texts("SELECT 1; SELECT 2", SqlDialect::Postgres);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].trim(), "SELECT 2");
    }

    #[test]
    fn unicode_in_idents_and_strings_does_not_break_splitting() {
        let src = "select \"名前\" from t where x = 'héllo;wörld'; select 2;";
        assert_eq!(split(src, SqlDialect::Postgres).len(), 2);
    }

    #[test]
    fn perf_smoke_one_megabyte_statement_under_50ms() {
        // One giant statement: a huge IN (...) list, no terminators inside.
        let mut src = String::from("SELECT * FROM t WHERE x IN (");
        while src.len() < 1_000_000 {
            src.push_str("123,");
        }
        src.push_str("999);");
        let start = Instant::now();
        let spans = split(&src, SqlDialect::Postgres);
        let elapsed = start.elapsed();
        assert_eq!(spans.len(), 1);
        assert!(
            elapsed.as_millis() < 50,
            "splitting took {elapsed:?}, budget is 50ms"
        );
    }

    #[test]
    fn directives_attach_to_the_following_statement_only() {
        let src = "-- @limit 10\n-- @readonly\nSELECT 1;\nSELECT 2;";
        let spans = split(src, SqlDialect::Postgres);
        assert_eq!(spans.len(), 2);
        let d0 = spans[0].directives.as_ref().unwrap();
        assert_eq!(d0.limit, Some(10));
        assert!(d0.readonly);
        let d1 = spans[1].directives.as_ref().unwrap();
        assert_eq!(*d1, crate::Directives::default());
    }

    #[test]
    fn bad_directive_value_is_an_error_on_the_span_not_a_panic() {
        let src = "-- @limit notanumber\nSELECT 1;";
        let spans = split(src, SqlDialect::Postgres);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].directives.is_err());
    }
}
