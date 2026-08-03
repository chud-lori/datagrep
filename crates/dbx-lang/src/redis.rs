//! Redis [`Language`] impl (design §3.6 registry / requirement 5): a line
//! splitter plus a redis-cli-compatible argument tokenizer, and
//! classification via a fixed read/write/admin command table.
//!
//! A "statement" is one command. Redis-cli itself keeps reading lines while
//! a quote is unterminated (so a quoted argument may legitimately contain a
//! literal newline), so the splitter tracks quote state *across* lines
//! rather than treating every `\n` as an unconditional boundary — that is
//! what makes multi-line pipelines and quoted args containing raw newlines
//! work the same way they would when pasted into the real `redis-cli`.

use std::ops::Range;

use dbx_api::LanguageId;

use crate::directives::{extract_directive_lines, parse_directives};
use crate::{EditContext, Language, StatementClass, StatementSpan, Token, TokenKind};

const DIRECTIVE_MARKER: &str = "#";

#[derive(Debug)]
pub struct RedisLanguage;

pub static REDIS: RedisLanguage = RedisLanguage;

impl Language for RedisLanguage {
    fn id(&self) -> LanguageId {
        LanguageId::RedisCli
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

/// One quoted-or-bare argument, as produced by [`tokenize_args`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    pub value: String,
    pub range: Range<usize>,
    pub quoted: bool,
}

/// Error tokenizing a redis-cli command line — an unterminated quote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unterminated {0} quote starting at byte {1}")]
pub struct RedisTokenError(&'static str, usize);

/// redis-cli-compatible argument tokenizer: bare (whitespace-separated)
/// words, `"double quoted"` args with backslash escapes (`\n \r \t \b \a
/// \\ \" \xHH`), and `'single quoted'` args where only `\\` and `\'` are
/// special (everything else inside single quotes is literal — this matches
/// real `redis-cli` behavior, not JSON/shell conventions).
pub fn tokenize_args(line: &str) -> Result<Vec<Arg>, RedisTokenError> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < len {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut value = String::new();
        let mut quoted = false;

        // A single argument may be built from adjacent quoted/bare runs
        // (e.g. `"a"'b'c`), same as a POSIX-ish shell; stop at whitespace.
        while i < len && !bytes[i].is_ascii_whitespace() {
            match bytes[i] {
                b'"' => {
                    quoted = true;
                    i += 1;
                    let arg_start = start;
                    loop {
                        if i >= len {
                            return Err(RedisTokenError("double", arg_start));
                        }
                        match bytes[i] {
                            b'"' => {
                                i += 1;
                                break;
                            }
                            b'\\' if i + 1 < len => {
                                let (ch, consumed) = decode_double_escape(&bytes[i + 1..]);
                                value.push(ch);
                                i += 1 + consumed;
                            }
                            _ => {
                                let ch_len = utf8_len(bytes[i]);
                                value.push_str(&line[i..i + ch_len]);
                                i += ch_len;
                            }
                        }
                    }
                }
                b'\'' => {
                    quoted = true;
                    i += 1;
                    let arg_start = start;
                    loop {
                        if i >= len {
                            return Err(RedisTokenError("single", arg_start));
                        }
                        match bytes[i] {
                            b'\'' => {
                                i += 1;
                                break;
                            }
                            b'\\' if matches!(bytes.get(i + 1), Some(b'\\') | Some(b'\'')) => {
                                value.push(bytes[i + 1] as char);
                                i += 2;
                            }
                            _ => {
                                let ch_len = utf8_len(bytes[i]);
                                value.push_str(&line[i..i + ch_len]);
                                i += ch_len;
                            }
                        }
                    }
                }
                _ => {
                    let ch_len = utf8_len(bytes[i]);
                    value.push_str(&line[i..i + ch_len]);
                    i += ch_len;
                }
            }
        }
        out.push(Arg {
            value,
            range: start..i,
            quoted,
        });
    }
    Ok(out)
}

fn utf8_len(b: u8) -> usize {
    if b & 0x80 == 0 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

/// Decode a `\X` escape after a backslash inside a double-quoted arg.
/// Returns the decoded char and how many bytes after the backslash were
/// consumed. `\xHH` consumes two hex digits when present; any unrecognized
/// escape falls back to the literal character (never a hard error — a
/// stray backslash in a real command shouldn't corrupt the whole line).
fn decode_double_escape(rest: &[u8]) -> (char, usize) {
    match rest.first() {
        Some(b'n') => ('\n', 1),
        Some(b'r') => ('\r', 1),
        Some(b't') => ('\t', 1),
        Some(b'b') => ('\u{8}', 1),
        Some(b'a') => ('\u{7}', 1),
        Some(b'\\') => ('\\', 1),
        Some(b'"') => ('"', 1),
        Some(b'x')
            if rest.len() >= 3 && rest[1].is_ascii_hexdigit() && rest[2].is_ascii_hexdigit() =>
        {
            let hi = hex_nibble(rest[1]);
            let lo = hex_nibble(rest[2]);
            (((hi << 4) | lo) as char, 3)
        }
        Some(&b) => (b as char, 1),
        None => ('\\', 0),
    }
}

/// The numeric value of an ASCII hex digit, or 0 for anything else. Callers
/// only reach this after checking `is_ascii_hexdigit`, so the fallback is
/// unreachable in practice — it exists so this never needs `.unwrap()`.
fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

/// Split `src` into one span per command. A command line that starts with
/// `#` (after leading whitespace) and is not a directive is treated as a
/// plain comment and produces no span.
fn split(src: &str) -> Vec<StatementSpan> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut spans = Vec::new();
    let mut stmt_start = 0usize;
    let mut i = 0usize;
    let mut quote: Option<u8> = None;

    while i < len {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' && i + 1 < len {
                    i += 2;
                } else if b == q {
                    quote = None;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            None => match b {
                b'"' | b'\'' => {
                    quote = Some(b);
                    i += 1;
                }
                b'\n' => {
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
    if start >= end {
        return;
    }
    let text = src[start..end].trim();
    if text.is_empty() || text.starts_with('#') {
        return;
    }
    // Directive lines directly above a command (contiguous `# @...` lines).
    let content_start = start + (src[start..end].len() - src[start..end].trim_start().len());
    let lines = extract_directive_lines(src, content_start, DIRECTIVE_MARKER);
    let directives = parse_directives(&lines);
    spans.push(StatementSpan {
        range: start..end,
        directives,
    });
}

const READ_COMMANDS: &[&str] = &[
    "GET",
    "MGET",
    "SCAN",
    "KEYS",
    "TTL",
    "PTTL",
    "EXISTS",
    "INFO",
    "HGET",
    "HGETALL",
    "HMGET",
    "HKEYS",
    "HVALS",
    "HLEN",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "SMEMBERS",
    "SISMEMBER",
    "SCARD",
    "ZRANGE",
    "ZSCORE",
    "ZCARD",
    "STRLEN",
    "TYPE",
    "DBSIZE",
    "PING",
    "RANDOMKEY",
    "HSCAN",
    "SSCAN",
    "ZSCAN",
    "GETRANGE",
    "OBJECT",
    "MEMORY",
    "DUMP",
];
const WRITE_COMMANDS: &[&str] = &[
    "SET", "SETEX", "PSETEX", "SETNX", "DEL", "UNLINK", "EXPIRE", "PEXPIRE", "EXPIREAT", "PERSIST",
    "RENAME", "APPEND", "INCR", "DECR", "INCRBY", "DECRBY", "HSET", "HDEL", "HINCRBY", "LPUSH",
    "RPUSH", "LPOP", "RPOP", "LSET", "LTRIM", "SADD", "SREM", "SPOP", "ZADD", "ZREM", "ZINCRBY",
    "MSET", "MSETNX", "GETSET", "COPY", "MOVE", "RESTORE",
];
const ADMIN_COMMANDS: &[&str] = &[
    "FLUSHDB",
    "FLUSHALL",
    "CONFIG",
    "SHUTDOWN",
    "DEBUG",
    "SAVE",
    "BGSAVE",
    "BGREWRITEAOF",
    "SLAVEOF",
    "REPLICAOF",
    "CLIENT",
    "CLUSTER",
    "ACL",
    "MONITOR",
    "LATENCY",
    "SLOWLOG",
];

fn classify(stmt: &str) -> StatementClass {
    let Ok(args) = tokenize_args(stmt) else {
        return StatementClass::Unknown;
    };
    let Some(cmd) = args.first() else {
        return StatementClass::Unknown;
    };
    let hit = |set: &[&str]| set.iter().any(|c| cmd.value.eq_ignore_ascii_case(c));
    if hit(READ_COMMANDS) {
        StatementClass::Read
    } else if hit(WRITE_COMMANDS) {
        StatementClass::Write
    } else if hit(ADMIN_COMMANDS) {
        StatementClass::Admin
    } else {
        StatementClass::Unknown
    }
}

fn context_at(src: &str, offset: usize) -> EditContext {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    let mut in_comment = false;

    while i < offset && i < bytes.len() {
        let b = bytes[i];
        if in_comment {
            if b == b'\n' {
                in_comment = false;
            }
            i += 1;
            continue;
        }
        match quote {
            Some(q) => {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else if b == q {
                    quote = None;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            None => match b {
                b'"' | b'\'' => {
                    quote = Some(b);
                    i += 1;
                }
                b'#' if is_line_start_ws(bytes, i) => {
                    in_comment = true;
                    i += 1;
                }
                _ => i += 1,
            },
        }
    }

    if in_comment {
        return EditContext::Comment;
    }
    if quote.is_some() {
        return EditContext::StringLiteral;
    }
    let is_ident_byte = |b: u8| !b.is_ascii_whitespace() && b != b'"' && b != b'\'' && b != b'#';
    let before = offset.checked_sub(1).and_then(|p| bytes.get(p)).copied();
    let at = bytes.get(offset).copied();
    if before.is_some_and(is_ident_byte) || at.is_some_and(is_ident_byte) {
        EditContext::Identifier
    } else {
        EditContext::Statement
    }
}

fn is_line_start_ws(bytes: &[u8], pos: usize) -> bool {
    let mut j = pos;
    while j > 0 {
        j -= 1;
        match bytes[j] {
            b'\n' => return true,
            b if b.is_ascii_whitespace() => continue,
            _ => return false,
        }
    }
    true
}

fn highlight(src: &str) -> Vec<Token> {
    let mut out = Vec::new();
    for line_range in line_ranges(src) {
        let line = &src[line_range.clone()];
        let trimmed = line.trim_start();
        let leading_ws = line.len() - trimmed.len();
        if trimmed.starts_with('#') {
            out.push(Token {
                range: line_range.start + leading_ws..line_range.end,
                kind: TokenKind::Comment,
            });
            continue;
        }
        let Ok(args) = tokenize_args(line) else {
            continue;
        };
        for (idx, arg) in args.iter().enumerate() {
            let kind = if arg.quoted {
                TokenKind::String
            } else if idx == 0 {
                TokenKind::Keyword
            } else {
                TokenKind::Ident
            };
            out.push(Token {
                range: line_range.start + arg.range.start..line_range.start + arg.range.end,
                kind,
            });
        }
    }
    out
}

fn line_ranges(src: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            out.push(start..i);
            start = i + 1;
        }
    }
    out.push(start..src.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_values(line: &str) -> Vec<String> {
        tokenize_args(line)
            .unwrap()
            .into_iter()
            .map(|a| a.value)
            .collect()
    }

    #[test]
    fn quoted_args_with_spaces() {
        assert_eq!(
            arg_values(r#"SET key "hello world""#),
            vec!["SET", "key", "hello world"]
        );
        assert_eq!(
            arg_values(r#"SET key 'hello world'"#),
            vec!["SET", "key", "hello world"]
        );
    }

    #[test]
    fn escapes_in_double_quotes() {
        assert_eq!(
            arg_values(r#"SET key "a\nb\tc\\d\"e""#),
            vec!["SET", "key", "a\nb\tc\\d\"e"]
        );
        assert_eq!(
            arg_values(r#"SET key "\x41\x42""#),
            vec!["SET", "key", "AB"]
        );
    }

    #[test]
    fn single_quotes_only_escape_backslash_and_quote() {
        assert_eq!(arg_values(r#"SET key 'a\nb'"#), vec!["SET", "key", "a\\nb"]);
        assert_eq!(arg_values(r#"SET key 'it\'s'"#), vec!["SET", "key", "it's"]);
    }

    #[test]
    fn bare_words_and_empty_line() {
        assert_eq!(arg_values("GET foo"), vec!["GET", "foo"]);
        assert!(arg_values("").is_empty());
        assert!(arg_values("   ").is_empty());
    }

    #[test]
    fn unterminated_quote_is_an_error_not_panic() {
        assert!(tokenize_args(r#"SET key "unterminated"#).is_err());
        assert!(tokenize_args("SET key 'unterminated").is_err());
    }

    #[test]
    fn split_multi_line_pipeline() {
        let src = "SET a 1\nGET a\n# a comment\n\nDEL a\n";
        let spans: Vec<&str> = split(src).iter().map(|s| s.text(src)).collect();
        assert_eq!(spans, vec!["SET a 1", "GET a", "DEL a"]);
    }

    #[test]
    fn classify_matches_command_table() {
        assert_eq!(classify("GET foo"), StatementClass::Read);
        assert_eq!(classify("MGET a b"), StatementClass::Read);
        assert_eq!(classify("SCAN 0"), StatementClass::Read);
        assert_eq!(classify("KEYS *"), StatementClass::Read);
        assert_eq!(classify("TTL a"), StatementClass::Read);
        assert_eq!(classify("EXISTS a"), StatementClass::Read);
        assert_eq!(classify("INFO"), StatementClass::Read);
        assert_eq!(classify("SET a 1"), StatementClass::Write);
        assert_eq!(classify("DEL a"), StatementClass::Write);
        assert_eq!(classify("EXPIRE a 10"), StatementClass::Write);
        assert_eq!(classify("FLUSHDB"), StatementClass::Admin);
        assert_eq!(classify("CONFIG GET maxmemory"), StatementClass::Admin);
        assert_eq!(
            classify("get foo"),
            StatementClass::Read,
            "case-insensitive command"
        );
        assert_eq!(classify(""), StatementClass::Unknown);
    }

    #[test]
    fn context_at_string_and_comment_and_statement() {
        let src = "SET a \"hello world\"\n# a note";
        let in_string = src.find("hello").unwrap();
        assert_eq!(context_at(src, in_string), EditContext::StringLiteral);
        let in_comment = src.find("note").unwrap();
        assert_eq!(context_at(src, in_comment), EditContext::Comment);
        let in_ident = src.find("SET").unwrap() + 1;
        assert_eq!(context_at(src, in_ident), EditContext::Identifier);
    }

    #[test]
    fn highlight_marks_command_args_strings_and_comments() {
        let src = "SET a \"hi\"\n# note";
        let toks = highlight(src);
        assert!(toks
            .iter()
            .any(|t| t.kind == TokenKind::Keyword && &src[t.range.clone()] == "SET"));
        assert!(toks
            .iter()
            .any(|t| t.kind == TokenKind::String && &src[t.range.clone()] == "\"hi\""));
        assert!(toks.iter().any(|t| t.kind == TokenKind::Comment));
    }

    #[test]
    fn unicode_args_round_trip() {
        assert_eq!(
            arg_values("SET name \"héllo wörld\""),
            vec!["SET", "name", "héllo wörld"]
        );
    }
}
