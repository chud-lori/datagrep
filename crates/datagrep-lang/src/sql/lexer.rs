//! Shared dialect-aware SQL lexical scanner (design §3.6: "dialect-aware
//! lexer (hand-rolled)"). One state machine walks the byte stream and
//! classifies every byte into a [`Chunk`] — code, a quoted region (string or
//! quoted identifier, in any of the dialects' quoting styles), or a comment
//! (with nested `/* */` for Postgres). [`crate::sql::splitter`],
//! [`crate::sql::classifier`], and [`crate::sql::highlight`] all build on
//! this single pass so the quoting/comment/dollar-quote escaping rules are
//! implemented exactly once.
//!
//! Scanning is byte-wise, which is safe for UTF-8 text here because every
//! delimiter this scanner looks for (`'`, `"`, `` ` ``, `[`, `]`, `-`, `#`,
//! `/`, `*`, `$`, `;`, `\n`) is ASCII, and no UTF-8 continuation or
//! multi-byte lead byte ever equals an ASCII byte value. Unicode content
//! inside strings/identifiers/comments rides through untouched, and every
//! chunk boundary this scanner produces sits on a UTF-8 char boundary.

use datagrep_api::SqlDialect;
use std::ops::Range;

/// One lexical region of a SQL buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    /// Ordinary SQL text: keywords, identifiers, operators, punctuation,
    /// literals other than strings.
    Code(Range<usize>),
    /// A quoted region, delimiters included: `'...'`, `"..."`, `` `...` ``,
    /// `[...]`, or Postgres `$tag$...$tag$`.
    Quoted(Range<usize>, QuoteKind),
    /// A comment, delimiters included: `-- ...`, `# ...` (MySQL), or
    /// `/* ... */` (nesting only for Postgres, per the design doc).
    Comment(Range<usize>),
}

impl Chunk {
    pub fn range(&self) -> Range<usize> {
        match self {
            Chunk::Code(r) | Chunk::Quoted(r, _) | Chunk::Comment(r) => r.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    /// `'...'` — a string literal, `''` escapes a literal quote.
    SingleString,
    /// `"..."` — a quoted identifier, `""` escapes a literal quote.
    DoubleIdent,
    /// `` `...` `` — MySQL quoted identifier, `` `` `` escapes.
    Backtick,
    /// `[...]` — SQLite/MSSQL bracketed identifier, `]]` escapes a literal
    /// `]`.
    Bracket,
    /// Postgres dollar-quoting, `$tag$...$tag$` (tag may be empty: `$$...$$`).
    DollarQuote,
}

/// Scan all of `src` into a flat sequence of [`Chunk`]s, dialect-aware.
/// Adjacent `Code` chunks are never emitted (the scanner coalesces them),
/// so callers can rely on chunk kinds alternating meaningfully.
pub fn lex_chunks(src: &str, dialect: SqlDialect) -> Vec<Chunk> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut chunks = Vec::new();
    let mut code_start = 0usize;
    let mut i = 0usize;

    macro_rules! flush_code {
        ($end:expr) => {
            if code_start < $end {
                chunks.push(Chunk::Code(code_start..$end));
            }
        };
    }

    while i < len {
        let b = bytes[i];
        match b {
            b'\'' => {
                flush_code!(i);
                let end = scan_simple_quoted(bytes, i, b'\'');
                chunks.push(Chunk::Quoted(i..end, QuoteKind::SingleString));
                i = end;
                code_start = i;
            }
            b'"' => {
                flush_code!(i);
                let end = scan_simple_quoted(bytes, i, b'"');
                chunks.push(Chunk::Quoted(i..end, QuoteKind::DoubleIdent));
                i = end;
                code_start = i;
            }
            b'`' if dialect == SqlDialect::Mysql => {
                flush_code!(i);
                let end = scan_simple_quoted(bytes, i, b'`');
                chunks.push(Chunk::Quoted(i..end, QuoteKind::Backtick));
                i = end;
                code_start = i;
            }
            b'[' if matches!(dialect, SqlDialect::Sqlite | SqlDialect::Mssql) => {
                flush_code!(i);
                let end = scan_bracket_quoted(bytes, i);
                chunks.push(Chunk::Quoted(i..end, QuoteKind::Bracket));
                i = end;
                code_start = i;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                flush_code!(i);
                let end = scan_line_comment(bytes, i);
                chunks.push(Chunk::Comment(i..end));
                i = end;
                code_start = i;
            }
            b'#' if dialect == SqlDialect::Mysql => {
                flush_code!(i);
                let end = scan_line_comment(bytes, i);
                chunks.push(Chunk::Comment(i..end));
                i = end;
                code_start = i;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                flush_code!(i);
                let nest = dialect == SqlDialect::Postgres;
                let end = scan_block_comment(bytes, i, nest);
                chunks.push(Chunk::Comment(i..end));
                i = end;
                code_start = i;
            }
            b'$' if dialect == SqlDialect::Postgres => {
                if let Some((tag_end, close_end)) = try_scan_dollar_quote(bytes, i) {
                    flush_code!(i);
                    let _ = tag_end;
                    chunks.push(Chunk::Quoted(i..close_end, QuoteKind::DollarQuote));
                    i = close_end;
                    code_start = i;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    flush_code!(len);
    chunks
}

/// Scan a `quote`-delimited region starting at `start` (`bytes[start] ==
/// quote`), where the escape convention is doubling the delimiter
/// (`''`, `""`, `` `` ``). Returns the exclusive end byte offset. An
/// unterminated quote runs to end of input — still a well-formed chunk, just
/// one that never closes (the splitter treats this as "still inside a
/// statement," which is the honest behavior for a truncated buffer).
fn scan_simple_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

/// `[...]` with `]]` escaping a literal `]`.
fn scan_bracket_quoted(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b']' {
            if bytes.get(i + 1) == Some(&b']') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

fn scan_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

/// `/* ... */`, nesting only when `nest` is true (Postgres). Non-nesting
/// dialects close on the first `*/` regardless of interior `/*`.
fn scan_block_comment(bytes: &[u8], start: usize, nest: bool) -> usize {
    let mut i = start + 2;
    let mut depth = 1u32;
    while i < bytes.len() {
        if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else if nest && bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

fn is_dollar_tag_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_dollar_tag_continue(b: u8) -> bool {
    is_dollar_tag_start(b) || b.is_ascii_digit()
}

/// Try to scan a Postgres dollar-quoted region starting at `bytes[start] ==
/// '$'`. Returns `(tag_end, close_end)` on success — `close_end` is the
/// exclusive end of the whole `$tag$...$tag$` region. Returns `None` when
/// this isn't a valid opening tag (e.g. a `$1` bind parameter), in which
/// case the caller should treat the `$` as ordinary code.
fn try_scan_dollar_quote(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    debug_assert_eq!(bytes[start], b'$');
    let mut j = start + 1;
    if bytes.get(j) != Some(&b'$') {
        if !bytes.get(j).copied().is_some_and(is_dollar_tag_start) {
            return None;
        }
        j += 1;
        while bytes.get(j).copied().is_some_and(is_dollar_tag_continue) {
            j += 1;
        }
        if bytes.get(j) != Some(&b'$') {
            return None;
        }
    }
    let tag_end = j + 1; // just past the opening tag's closing '$'
    let tag = &bytes[start + 1..j];
    // Search forward for the matching close: '$' + tag + '$'.
    let mut k = tag_end;
    while k < bytes.len() {
        if bytes[k] == b'$' {
            let candidate_end = k + 1 + tag.len();
            if bytes.len() > candidate_end
                && &bytes[k + 1..k + 1 + tag.len()] == tag
                && bytes[candidate_end] == b'$'
            {
                return Some((tag_end, candidate_end + 1));
            }
        }
        k += 1;
    }
    // Unterminated dollar-quote: run to end of input, same honesty policy as
    // `scan_simple_quoted`.
    Some((tag_end, bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(src: &str, dialect: SqlDialect) -> Vec<Range<usize>> {
        lex_chunks(src, dialect)
            .into_iter()
            .filter_map(|c| match c {
                Chunk::Code(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn single_quote_escaping() {
        let src = "select 'it''s fine'";
        let chunks = lex_chunks(src, SqlDialect::Postgres);
        assert_eq!(
            chunks,
            vec![
                Chunk::Code(0..7),
                Chunk::Quoted(7..19, QuoteKind::SingleString),
            ]
        );
        assert_eq!(&src[7..19], "'it''s fine'");
    }

    #[test]
    fn nested_block_comment_postgres_only() {
        let src = "/* a /* b */ c */ select 1";
        let chunks = lex_chunks(src, SqlDialect::Postgres);
        // The whole "/* a /* b */ c */" must be ONE comment chunk.
        assert_eq!(chunks[0], Chunk::Comment(0..17));

        let mysql_chunks = lex_chunks(src, SqlDialect::Mysql);
        // MySQL doesn't nest: closes at the first `*/`.
        assert_eq!(mysql_chunks[0], Chunk::Comment(0..12));
    }

    #[test]
    fn dollar_quote_anonymous_and_tagged() {
        let src = "select $$a;b$$, $tag$c;d$tag$";
        let chunks = lex_chunks(src, SqlDialect::Postgres);
        let quoted: Vec<_> = chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Quoted(r, QuoteKind::DollarQuote) => Some(&src[r.clone()]),
                _ => None,
            })
            .collect();
        assert_eq!(quoted, vec!["$$a;b$$", "$tag$c;d$tag$"]);
    }

    #[test]
    fn dollar_bind_param_is_not_a_dollar_quote() {
        let src = "select * from t where id = $1";
        let chunks = lex_chunks(src, SqlDialect::Postgres);
        assert!(chunks
            .iter()
            .all(|c| !matches!(c, Chunk::Quoted(_, QuoteKind::DollarQuote))));
        assert_eq!(code(src, SqlDialect::Postgres), vec![0..src.len()]);
    }

    #[test]
    fn mysql_backtick_and_hash_comment() {
        let src = "select `col#name` # trailing comment\nfrom t";
        let chunks = lex_chunks(src, SqlDialect::Mysql);
        assert!(chunks
            .iter()
            .any(|c| matches!(c, Chunk::Quoted(_, QuoteKind::Backtick))));
        assert!(chunks.iter().any(|c| matches!(c, Chunk::Comment(_))));
    }

    #[test]
    fn sqlite_bracket_ident_with_escaped_bracket() {
        let src = "select [a]]b] from t";
        let chunks = lex_chunks(src, SqlDialect::Sqlite);
        let bracket = chunks
            .iter()
            .find_map(|c| match c {
                Chunk::Quoted(r, QuoteKind::Bracket) => Some(&src[r.clone()]),
                _ => None,
            })
            .unwrap();
        assert_eq!(bracket, "[a]]b]");
    }

    #[test]
    fn unicode_in_strings_and_idents_is_transparent() {
        let src = "select 'héllo wörld', \"名前\" from t";
        let chunks = lex_chunks(src, SqlDialect::Postgres);
        let s = chunks
            .iter()
            .find_map(|c| match c {
                Chunk::Quoted(r, QuoteKind::SingleString) => Some(&src[r.clone()]),
                _ => None,
            })
            .unwrap();
        assert_eq!(s, "'héllo wörld'");
        let ident = chunks
            .iter()
            .find_map(|c| match c {
                Chunk::Quoted(r, QuoteKind::DoubleIdent) => Some(&src[r.clone()]),
                _ => None,
            })
            .unwrap();
        assert_eq!(ident, "\"名前\"");
    }

    #[test]
    fn unterminated_string_runs_to_end_without_panicking() {
        let src = "select 'unterminated";
        let chunks = lex_chunks(src, SqlDialect::Postgres);
        assert_eq!(
            chunks.last().unwrap(),
            &Chunk::Quoted(7..src.len(), QuoteKind::SingleString)
        );
    }
}
