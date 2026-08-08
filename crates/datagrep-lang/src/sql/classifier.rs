//! Statement classification, feeding the client-side write guardrail: the
//! first significant keyword after comments/whitespace decides
//! [`StatementClass`], with one deliberate exception — `WITH` requires
//! looking past the CTE list (skipping nested parens correctly, and
//! visiting every CTE in a comma-separated list) to the real keyword, so
//! `WITH ... INSERT` classifies as `Write`, not `Read`.

use datagrep_api::SqlDialect;

use super::lexer::{lex_chunks, Chunk};
use crate::StatementClass;

/// One significant lexeme for classification purposes: either a run of
/// identifier/keyword characters, or a single punctuation byte we care about
/// (`(`, `)`, `,`). Everything else in `Code` chunks (operators, other
/// punctuation, numbers) is skipped — classification never needs it.
enum Lexeme<'a> {
    Word(&'a str),
    Open,
    Close,
    Comma,
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_word_continue(b: u8) -> bool {
    is_word_start(b) || b.is_ascii_digit()
}

/// Tokenize every `Code` chunk of `stmt` into [`Lexeme`]s, skipping
/// `Quoted`/`Comment` chunks entirely (a `(` inside a string or comment must
/// never affect paren depth, and a keyword-looking string literal must never
/// be treated as a keyword).
fn significant_lexemes(stmt: &str) -> Vec<Lexeme<'_>> {
    // Classification doesn't depend on dialect-specific quoting rules in any
    // way that changes the *keyword skeleton* of a statement, so a single
    // dialect-agnostic-enough choice (Postgres profile: recognizes `'`, `"`,
    // and nested `/* */`) is used purely to avoid miscounting parens inside
    // strings/comments. Backtick/bracket idents and dollar-quotes only ever
    // appear where an identifier or string is legal, never around the
    // keywords or parens this scan is looking for, so this doesn't change
    // classification correctness for any dialect.
    let chunks = lex_chunks(stmt, SqlDialect::Postgres);
    let bytes = stmt.as_bytes();
    let mut out = Vec::new();
    for chunk in chunks {
        let Chunk::Code(range) = chunk else { continue };
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
                out.push(Lexeme::Word(&stmt[start..i]));
            } else {
                match b {
                    b'(' => out.push(Lexeme::Open),
                    b')' => out.push(Lexeme::Close),
                    b',' => out.push(Lexeme::Comma),
                    _ => {}
                }
                i += 1;
            }
        }
    }
    out
}

pub fn classify(stmt: &str) -> StatementClass {
    let toks = significant_lexemes(stmt);
    classify_from(&toks, 0).0
}

/// Map a leading keyword directly (everything except `WITH`, which needs
/// lookahead). Returns `None` for anything not in the fixed keyword table.
fn classify_keyword(word: &str) -> Option<StatementClass> {
    use StatementClass::*;
    // Case-insensitive match against the fixed vocabulary from design intent
    // (requirement 3): SQL keywords are ASCII, so `eq_ignore_ascii_case` is
    // exact and avoids allocating an uppercased copy per token.
    const READ: &[&str] = &["SELECT", "VALUES", "SHOW", "EXPLAIN"];
    const WRITE: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "REPLACE", "COPY",
    ];
    const DDL: &[&str] = &["CREATE", "ALTER", "DROP", "TRUNCATE", "COMMENT", "RENAME"];
    const TCL: &[&str] = &["BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT", "START"];
    const ADMIN: &[&str] = &["GRANT", "REVOKE", "VACUUM", "ANALYZE", "SET", "KILL"];

    let hit = |set: &[&str]| set.iter().any(|k| word.eq_ignore_ascii_case(k));
    if hit(READ) {
        Some(Read)
    } else if hit(WRITE) {
        Some(Write)
    } else if hit(DDL) {
        Some(Ddl)
    } else if hit(TCL) {
        Some(Tcl)
    } else if hit(ADMIN) {
        Some(Admin)
    } else {
        None
    }
}

/// Classify starting at token index `idx`. Returns the class and the index
/// just past whatever it consumed (the latter is only meaningful for the
/// `WITH` recursion).
fn classify_from(toks: &[Lexeme<'_>], idx: usize) -> (StatementClass, usize) {
    let Some(Lexeme::Word(first)) = toks.get(idx) else {
        return (StatementClass::Unknown, idx);
    };
    if first.eq_ignore_ascii_case("WITH") {
        return (classify_with(toks, idx + 1), toks.len());
    }
    match classify_keyword(first) {
        Some(class) => (class, idx + 1),
        None => (StatementClass::Unknown, idx + 1),
    }
}

/// `WITH [RECURSIVE] name [(cols)] AS [[NOT] MATERIALIZED] (query) [, ...]
/// <final statement>` — walk past every CTE (skipping nested parens
/// correctly) to classify on the real final keyword.
fn classify_with(toks: &[Lexeme<'_>], mut idx: usize) -> StatementClass {
    if matches!(toks.get(idx), Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("RECURSIVE")) {
        idx += 1;
    }
    loop {
        // CTE name.
        match toks.get(idx) {
            Some(Lexeme::Word(_)) => idx += 1,
            _ => return StatementClass::Unknown,
        }
        // Optional column list.
        if matches!(toks.get(idx), Some(Lexeme::Open)) {
            idx = match skip_balanced(toks, idx) {
                Some(i) => i,
                None => return StatementClass::Unknown,
            };
        }
        // AS
        match toks.get(idx) {
            Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("AS") => idx += 1,
            _ => return StatementClass::Unknown,
        }
        // Optional [NOT] MATERIALIZED.
        if matches!(toks.get(idx), Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("NOT")) {
            idx += 1;
        }
        if matches!(toks.get(idx), Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("MATERIALIZED"))
        {
            idx += 1;
        }
        // CTE body.
        match toks.get(idx) {
            Some(Lexeme::Open) => {
                idx = match skip_balanced(toks, idx) {
                    Some(i) => i,
                    None => return StatementClass::Unknown,
                };
            }
            _ => return StatementClass::Unknown,
        }
        match toks.get(idx) {
            Some(Lexeme::Comma) => {
                idx += 1;
                continue;
            }
            _ => break,
        }
    }
    classify_from(toks, idx).0
}

/// `toks[idx]` must be `Open`; returns the index just past the matching
/// `Close`, or `None` if the parens never balance (malformed input).
fn skip_balanced(toks: &[Lexeme<'_>], idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = idx;
    loop {
        match toks.get(i)? {
            Lexeme::Open => depth += 1,
            Lexeme::Close => depth -= 1,
            _ => {}
        }
        i += 1;
        if depth == 0 {
            return Some(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementClass::*;

    #[test]
    fn basic_dml_and_ddl_and_tcl_and_admin() {
        assert_eq!(classify("SELECT * FROM t"), Read);
        assert_eq!(classify("VALUES (1)"), Read);
        assert_eq!(classify("SHOW TABLES"), Read);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), Write);
        assert_eq!(classify("UPDATE t SET x=1"), Write);
        assert_eq!(classify("DELETE FROM t"), Write);
        assert_eq!(
            classify("MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE"),
            Write
        );
        assert_eq!(classify("COPY t FROM stdin"), Write);
        assert_eq!(classify("CREATE TABLE t (x int)"), Ddl);
        assert_eq!(classify("ALTER TABLE t ADD COLUMN y int"), Ddl);
        assert_eq!(classify("DROP TABLE t"), Ddl);
        assert_eq!(classify("TRUNCATE t"), Ddl);
        assert_eq!(classify("BEGIN"), Tcl);
        assert_eq!(classify("COMMIT"), Tcl);
        assert_eq!(classify("ROLLBACK"), Tcl);
        assert_eq!(classify("GRANT SELECT ON t TO u"), Admin);
        assert_eq!(classify("VACUUM t"), Admin);
        assert_eq!(classify("SET search_path = public"), Admin);
        assert_eq!(classify("frobnicate everything"), Unknown);
        assert_eq!(classify(""), Unknown);
    }

    /// DML with a `RETURNING` clause is still a **Write** — it produces rows,
    /// but the leading keyword decides the class. Drivers that wrap
    /// row-producing statements in a READ ONLY transaction must consult this
    /// classification, not just "does it have columns" (TEST-REPORT F5).
    #[test]
    fn dml_with_returning_is_still_write() {
        assert_eq!(
            classify("INSERT INTO t (v) VALUES ('a') RETURNING id"),
            Write
        );
        assert_eq!(classify("UPDATE t SET v = 'b' RETURNING id, v"), Write);
        assert_eq!(classify("DELETE FROM t WHERE id = 1 RETURNING *"), Write);
        assert_eq!(
            classify("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x RETURNING id"),
            Write
        );
    }

    #[test]
    fn explain_variants_are_always_read() {
        assert_eq!(classify("EXPLAIN SELECT 1"), Read);
        assert_eq!(classify("EXPLAIN ANALYZE SELECT 1"), Read);
        assert_eq!(classify("EXPLAIN (FORMAT JSON) SELECT 1"), Read);
        assert_eq!(classify("explain select 1"), Read);
    }

    #[test]
    fn leading_comments_and_whitespace_are_skipped() {
        assert_eq!(classify("  \n-- a comment\n/* another */  SELECT 1"), Read);
        assert_eq!(classify("-- @limit 10\nINSERT INTO t VALUES (1)"), Write);
    }

    #[test]
    fn with_select_is_read() {
        assert_eq!(classify("WITH cte AS (SELECT 1) SELECT * FROM cte"), Read);
        assert_eq!(
            classify("WITH RECURSIVE cte AS (SELECT 1) SELECT * FROM cte"),
            Read
        );
    }

    #[test]
    fn with_insert_is_write_not_read() {
        assert_eq!(
            classify("WITH cte AS (SELECT 1) INSERT INTO t SELECT * FROM cte"),
            Write
        );
    }

    #[test]
    fn with_multiple_ctes_including_nested_parens() {
        let stmt = "WITH a AS (SELECT (1 + (2 * 3)) AS x), b AS (SELECT * FROM a) \
                    DELETE FROM t USING b WHERE t.id = b.x";
        assert_eq!(classify(stmt), Write);
    }

    #[test]
    fn with_cte_column_list_and_materialized() {
        let stmt = "WITH a (x, y) AS MATERIALIZED (SELECT 1, 2) UPDATE t SET x = 1";
        assert_eq!(classify(stmt), Write);
    }

    #[test]
    fn with_malformed_falls_back_to_unknown_not_panic() {
        assert_eq!(classify("WITH"), Unknown);
        assert_eq!(classify("WITH a AS"), Unknown);
        assert_eq!(classify("WITH a AS ("), Unknown);
    }

    #[test]
    fn semicolons_and_parens_inside_strings_do_not_confuse_classification() {
        assert_eq!(classify("SELECT '(WITH INSERT'"), Read);
    }
}
