//! MongoShell [`Language`] impl: the editor surface is
//! `db.<collection>.<method>(...)` chains and raw command documents,
//! hand-parsed by [`parser`] — explicitly **not** an embedded JS engine.

pub mod date;
pub mod error;
pub mod parser;

use datagrep_api::LanguageId;

pub use error::MongoError;
pub use parser::{parse, MongoStatement, ParsedMongo};

use crate::directives::{extract_directive_lines, parse_directives};
use crate::{EditContext, Language, StatementClass, StatementSpan, Token, TokenKind};

const DIRECTIVE_MARKER: &str = "#";

#[derive(Debug)]
pub struct MongoLanguage;

pub static MONGO: MongoLanguage = MongoLanguage;

impl Language for MongoLanguage {
    fn id(&self) -> LanguageId {
        LanguageId::MongoShell
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

/// Lexical state used by [`split`], [`context_at`], and [`highlight`] to
/// stay out of strings/comments/nested-bracket regions. Not shared as a
/// single chunk pass like [`crate::sql::lexer`] — this language's three
/// consumers need different enough output shapes (depth-aware terminator
/// search vs. a flat token stream) that a shared struct would cost more
/// than it saves for ~150 lines of scanning logic each.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    SingleString,
    DoubleString,
    LineComment,
    BlockComment,
}

/// Split on top-level `;` (bracket/paren/brace-depth and quote/comment
/// aware) — the same terminator convention as the SQL splitter, so a script
/// containing several `db....` statements behaves predictably. A whole
/// buffer with no top-level `;` is one statement (this also covers a
/// `.find({...})\n  .limit(5)\n  .sort({...})` chain formatted across
/// multiple lines with no semicolon — the common `mongosh` style).
fn split(src: &str) -> Vec<StatementSpan> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut spans = Vec::new();
    let mut stmt_start = 0usize;
    let mut i = 0usize;
    let mut mode = Mode::Code;
    let mut depth = 0i32;

    while i < len {
        let b = bytes[i];
        match mode {
            Mode::SingleString | Mode::DoubleString => {
                let quote = if mode == Mode::SingleString {
                    b'\''
                } else {
                    b'"'
                };
                if b == b'\\' && i + 1 < len {
                    i += 2;
                    continue;
                }
                if b == quote {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::LineComment => {
                if b == b'\n' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::BlockComment => {
                if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    mode = Mode::Code;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Code => match b {
                b'\'' => {
                    mode = Mode::SingleString;
                    i += 1;
                }
                b'"' => {
                    mode = Mode::DoubleString;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    mode = Mode::LineComment;
                    i += 2;
                }
                b'#' => {
                    mode = Mode::LineComment;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    mode = Mode::BlockComment;
                    i += 2;
                }
                b'{' | b'[' | b'(' => {
                    depth += 1;
                    i += 1;
                }
                b'}' | b']' | b')' => {
                    depth = (depth - 1).max(0);
                    i += 1;
                }
                b';' if depth == 0 => {
                    emit_span(&mut spans, src, stmt_start, i);
                    i += 1;
                    stmt_start = i;
                }
                _ => i += 1,
            },
        }
    }
    emit_span(&mut spans, src, stmt_start, len);
    spans
}

fn emit_span(spans: &mut Vec<StatementSpan>, src: &str, start: usize, end: usize) {
    if start >= end || src[start..end].trim().is_empty() {
        return;
    }
    let trimmed_start = start + (src[start..end].len() - src[start..end].trim_start().len());
    let lines = extract_directive_lines(src, trimmed_start, DIRECTIVE_MARKER);
    let directives = parse_directives(&lines);
    spans.push(StatementSpan {
        range: start..end,
        directives,
    });
}

fn classify(stmt: &str) -> StatementClass {
    match parse(stmt) {
        Ok(ParsedMongo::Chain(c)) => classify_method(&c.method),
        Ok(ParsedMongo::RawCommand(datagrep_api::Value::Document(doc))) => {
            match doc.iter().next() {
                Some((key, _)) => classify_method(key),
                None => StatementClass::Unknown,
            }
        }
        _ => StatementClass::Unknown,
    }
}

/// find/aggregate/count/distinct → Read; insert*/update*/delete* → Write;
/// drop* → Ddl (design requirement 6, verbatim). Checked in this order
/// (write/ddl prefixes first) so e.g. a hypothetical `dropIndex`-style name
/// starting with a Read-ish prefix can't shadow a destructive one — though
/// with the fixed prefixes above none currently collide.
fn classify_method(method: &str) -> StatementClass {
    let starts = |p: &str| {
        method
            .get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    };
    if starts("insert") || starts("update") || starts("delete") {
        StatementClass::Write
    } else if starts("drop") {
        StatementClass::Ddl
    } else if starts("find") || starts("aggregate") || starts("count") || starts("distinct") {
        StatementClass::Read
    } else {
        StatementClass::Unknown
    }
}

fn context_at(src: &str, offset: usize) -> EditContext {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut mode = Mode::Code;

    while i < offset && i < bytes.len() {
        let b = bytes[i];
        match mode {
            Mode::SingleString | Mode::DoubleString => {
                let quote = if mode == Mode::SingleString {
                    b'\''
                } else {
                    b'"'
                };
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == quote {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::LineComment => {
                if b == b'\n' {
                    mode = Mode::Code;
                }
                i += 1;
            }
            Mode::BlockComment => {
                if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    mode = Mode::Code;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Code => match b {
                b'\'' => {
                    mode = Mode::SingleString;
                    i += 1;
                }
                b'"' => {
                    mode = Mode::DoubleString;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    mode = Mode::LineComment;
                    i += 2;
                }
                b'#' => {
                    mode = Mode::LineComment;
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    mode = Mode::BlockComment;
                    i += 2;
                }
                _ => i += 1,
            },
        }
    }

    match mode {
        Mode::SingleString | Mode::DoubleString => EditContext::StringLiteral,
        Mode::LineComment | Mode::BlockComment => EditContext::Comment,
        Mode::Code => {
            let is_ident_byte =
                |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80;
            let before = offset.checked_sub(1).and_then(|p| bytes.get(p)).copied();
            let at = bytes.get(offset).copied();
            if before.is_some_and(is_ident_byte) || at.is_some_and(is_ident_byte) {
                EditContext::Identifier
            } else {
                EditContext::Statement
            }
        }
    }
}

const KEYWORDS: &[&str] = &[
    "db",
    "true",
    "false",
    "null",
    "undefined",
    "ObjectId",
    "ISODate",
    "NumberLong",
    "NumberDecimal",
    "NumberInt",
    "getCollection",
];

fn highlight(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(Token {
                range: start..i,
                kind: TokenKind::Comment,
            });
        } else if b == b'#' {
            let start = i;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(Token {
                range: start..i,
                kind: TokenKind::Comment,
            });
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i < len && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(len);
            out.push(Token {
                range: start..i,
                kind: TokenKind::Comment,
            });
        } else if b == b'\'' || b == b'"' {
            let quote = b;
            let start = i;
            i += 1;
            while i < len && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i = (i + 1).min(len);
            out.push(Token {
                range: start..i,
                kind: TokenKind::String,
            });
        } else if b.is_ascii_digit()
            || (b == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_digit()
                    || matches!(bytes[i], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                i += 1;
            }
            out.push(Token {
                range: start..i,
                kind: TokenKind::Number,
            });
        } else if b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80 {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || bytes[i] == b'$'
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            let word = &src[start..i];
            let kind = if KEYWORDS.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Ident
            };
            out.push(Token {
                range: start..i,
                kind,
            });
        } else if matches!(
            b,
            b'{' | b'}' | b'[' | b']' | b'(' | b')' | b',' | b':' | b'.' | b';'
        ) {
            out.push(Token {
                range: i..i + 1,
                kind: TokenKind::Punct,
            });
            i += 1;
        } else if matches!(
            b,
            b'=' | b'<'
                | b'>'
                | b'!'
                | b'+'
                | b'-'
                | b'*'
                | b'/'
                | b'%'
                | b'&'
                | b'|'
                | b'^'
                | b'~'
                | b'?'
        ) {
            let start = i;
            i += 1;
            out.push(Token {
                range: start..i,
                kind: TokenKind::Operator,
            });
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str) -> Vec<&str> {
        split(src).iter().map(|s| s.text(src)).collect()
    }

    #[test]
    fn split_semicolon_at_depth_zero() {
        let src = r#"db.a.find({x: "a;b"}); db.b.find({});"#;
        assert_eq!(split(src).len(), 2);
    }

    #[test]
    fn split_multiline_chain_without_semicolon_is_one_statement() {
        let src = "db.users.find({a:1})\n  .limit(5)\n  .sort({b:-1})";
        let spans = texts(src);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0], src);
    }

    #[test]
    fn classify_read_write_ddl() {
        assert_eq!(classify(r#"db.a.find({})"#), StatementClass::Read);
        assert_eq!(classify(r#"db.a.aggregate([])"#), StatementClass::Read);
        assert_eq!(classify(r#"db.a.count({})"#), StatementClass::Read);
        assert_eq!(classify(r#"db.a.distinct("x")"#), StatementClass::Read);
        assert_eq!(classify(r#"db.a.insertOne({})"#), StatementClass::Write);
        assert_eq!(
            classify(r#"db.a.updateMany({}, {})"#),
            StatementClass::Write
        );
        assert_eq!(classify(r#"db.a.deleteOne({})"#), StatementClass::Write);
        assert_eq!(classify(r#"db.a.drop()"#), StatementClass::Ddl);
        assert_eq!(classify(r#"{ find: "a" }"#), StatementClass::Read);
        assert_eq!(
            classify(r#"{ insert: "a", documents: [] }"#),
            StatementClass::Write
        );
        assert_eq!(classify("for (;;) {}"), StatementClass::Unknown);
    }

    #[test]
    fn context_at_string_comment_identifier() {
        let src = r#"db.a.find({name: "hi"}) // note"#;
        let in_string = src.find("hi").unwrap();
        assert_eq!(context_at(src, in_string), EditContext::StringLiteral);
        let in_comment = src.find("note").unwrap();
        assert_eq!(context_at(src, in_comment), EditContext::Comment);
        let in_ident = src.find("name").unwrap() + 1;
        assert_eq!(context_at(src, in_ident), EditContext::Identifier);
    }

    #[test]
    fn highlight_smoke() {
        let toks = highlight(r#"db.a.find({x: 1}) // c"#);
        assert!(toks.iter().any(|t| t.kind == TokenKind::Keyword));
        assert!(toks.iter().any(|t| t.kind == TokenKind::Number));
        assert!(toks.iter().any(|t| t.kind == TokenKind::Comment));
        assert!(toks.iter().any(|t| t.kind == TokenKind::Punct));
    }
}
