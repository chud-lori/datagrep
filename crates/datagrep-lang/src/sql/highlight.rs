//! Syntax highlighting tokens, built on the same [`super::lexer::lex_chunks`]
//! pass as the splitter and classifier. `Quoted` chunks map straight to
//! [`TokenKind::String`] (string literals, including dollar-quoted) or
//! [`TokenKind::Ident`] (quoted identifiers); `Comment` chunks map to
//! [`TokenKind::Comment`]; `Code` chunks get a second, cheap pass that only
//! needs to tell keywords from identifiers from numbers from punctuation —
//! it never needs to resolve grammar, so it doesn't try to.

use datagrep_api::SqlDialect;

use super::lexer::{lex_chunks, Chunk, QuoteKind};
use crate::{Token, TokenKind};

/// Not an exhaustive SQL grammar — a highlighter word list, deliberately
/// generous across dialects (a MySQL-only keyword lighting up in a Postgres
/// buffer is a cosmetic non-issue; failing to highlight a common keyword is
/// the annoying failure mode, so this errs toward inclusion).
const KEYWORDS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "UPSERT",
    "REPLACE",
    "COPY",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "COMMENT",
    "RENAME",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "START",
    "TRANSACTION",
    "GRANT",
    "REVOKE",
    "VACUUM",
    "ANALYZE",
    "SET",
    "KILL",
    "SHOW",
    "EXPLAIN",
    "VALUES",
    "WITH",
    "RECURSIVE",
    "MATERIALIZED",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER",
    "OUTER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "ON",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "ILIKE",
    "AS",
    "DISTINCT",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "ALL",
    "ANY",
    "SOME",
    "INTO",
    "DEFAULT",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "INDEX",
    "VIEW",
    "FUNCTION",
    "PROCEDURE",
    "TRIGGER",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "CAST",
    "RETURNING",
    "USING",
    "CONFLICT",
    "DO",
    "NOTHING",
    "TABLE",
    "COLUMN",
    "CONSTRAINT",
    "CASCADE",
    "RESTRICT",
    "IF",
    "TEMP",
    "TEMPORARY",
    "SCHEMA",
    "DATABASE",
    "SEQUENCE",
    "TRUE",
    "FALSE",
    "ASC",
    "DESC",
    "NULLS",
    "FIRST",
    "LAST",
    "OVER",
    "PARTITION",
    "WINDOW",
    "FILTER",
    "LATERAL",
    "FOR",
    "OF",
    "NOWAIT",
    "SKIP",
    "LOCKED",
    "RETURN",
    "DECLARE",
    "LOOP",
    "WHILE",
    "DELIMITER",
    "GO",
];

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_word_continue(b: u8) -> bool {
    is_word_start(b) || b.is_ascii_digit() || b == b'$'
}

fn is_operator_byte(b: u8) -> bool {
    matches!(
        b,
        b'=' | b'<' | b'>' | b'!' | b'+' | b'-' | b'*' | b'/' | b'%' | b'|' | b'&' | b'^' | b'~'
    )
}

fn is_number_start(bytes: &[u8], i: usize) -> bool {
    bytes[i].is_ascii_digit()
        || (bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
}

pub fn highlight(src: &str, dialect: SqlDialect) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    for chunk in lex_chunks(src, dialect) {
        match chunk {
            Chunk::Comment(r) => out.push(Token {
                range: r,
                kind: TokenKind::Comment,
            }),
            Chunk::Quoted(r, kind) => {
                let tk = match kind {
                    QuoteKind::SingleString | QuoteKind::DollarQuote => TokenKind::String,
                    QuoteKind::DoubleIdent | QuoteKind::Backtick | QuoteKind::Bracket => {
                        TokenKind::Ident
                    }
                };
                out.push(Token { range: r, kind: tk });
            }
            Chunk::Code(range) => tokenize_code(src, bytes, range, &mut out),
        }
    }
    out
}

fn tokenize_code(src: &str, bytes: &[u8], range: std::ops::Range<usize>, out: &mut Vec<Token>) {
    let mut i = range.start;
    let end = range.end;
    while i < end {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if is_word_start(b) {
            let start = i;
            i += 1;
            while i < end && is_word_continue(bytes[i]) {
                i += 1;
            }
            let word = &src[start..i];
            let kind = if KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k)) {
                TokenKind::Keyword
            } else {
                TokenKind::Ident
            };
            out.push(Token {
                range: start..i,
                kind,
            });
        } else if is_number_start(bytes, i) {
            let start = i;
            i += 1;
            while i < end
                && (bytes[i].is_ascii_digit()
                    || bytes[i] == b'.'
                    || bytes[i] == b'x'
                    || bytes[i] == b'X'
                    || bytes[i].is_ascii_hexdigit()
                    || ((bytes[i] == b'e' || bytes[i] == b'E')
                        && matches!(bytes.get(i + 1), Some(b'+' | b'-') | Some(b'0'..=b'9'))))
            {
                i += 1;
            }
            out.push(Token {
                range: start..i,
                kind: TokenKind::Number,
            });
        } else if is_operator_byte(b) {
            let start = i;
            i += 1;
            while i < end && is_operator_byte(bytes[i]) {
                i += 1;
            }
            out.push(Token {
                range: start..i,
                kind: TokenKind::Operator,
            });
        } else {
            out.push(Token {
                range: i..i + 1,
                kind: TokenKind::Punct,
            });
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, dialect: SqlDialect) -> Vec<(TokenKind, &str)> {
        highlight(src, dialect)
            .into_iter()
            .map(|t| (t.kind, &src[t.range]))
            .collect()
    }

    #[test]
    fn keyword_ident_string_number() {
        let toks = kinds("SELECT id FROM t WHERE x = 1", SqlDialect::Postgres);
        assert_eq!(toks[0], (TokenKind::Keyword, "SELECT"));
        assert_eq!(toks[1], (TokenKind::Ident, "id"));
        assert_eq!(toks[2], (TokenKind::Keyword, "FROM"));
        assert!(toks.contains(&(TokenKind::Number, "1")));
        assert!(toks.contains(&(TokenKind::Operator, "=")));
    }

    #[test]
    fn string_and_comment_tokens() {
        let toks = kinds("SELECT 'hi' -- comment\n", SqlDialect::Postgres);
        assert!(toks.contains(&(TokenKind::String, "'hi'")));
        assert!(toks.contains(&(TokenKind::Comment, "-- comment")));
    }

    #[test]
    fn quoted_identifier_is_ident_kind() {
        let toks = kinds("SELECT \"col\" FROM t", SqlDialect::Postgres);
        assert!(toks.contains(&(TokenKind::Ident, "\"col\"")));
    }

    #[test]
    fn punct_tokens_for_parens_and_comma() {
        let toks = kinds("f(a, b)", SqlDialect::Postgres);
        assert!(toks.contains(&(TokenKind::Punct, "(")));
        assert!(toks.contains(&(TokenKind::Punct, ",")));
        assert!(toks.contains(&(TokenKind::Punct, ")")));
    }

    #[test]
    fn tokens_cover_contiguous_ranges_without_overlap() {
        let src = "SELECT * FROM t WHERE id IN (1, 2, 3);";
        let toks = highlight(src, SqlDialect::Postgres);
        for w in toks.windows(2) {
            assert!(w[0].range.end <= w[1].range.start);
        }
    }
}
