//! # dbx-lang — per-connection editor language support
//!
//! Implements the `Language` trait sketched in `dbx-design.md` §3.6: the
//! editor is language-agnostic, and the **connection** picks the language via
//! `Capabilities::language` (`dbx_api::LanguageId`). There is no cross-engine
//! query language and no translation of user-authored text between engines
//! — see §3.6 point 1–4 — so every [`Language`] impl here speaks exactly one
//! engine's dialect, natively.
//!
//! Four responsibilities per language, matched 1:1 to trait methods:
//! - [`Language::split`] — cut a buffer into statements (byte ranges). This
//!   is, per the design doc, "where every client has bugs": see
//!   [`sql::splitter`] for the gory dialect-specific details.
//! - [`Language::classify`] — is a statement Read/Write/Ddl/Tcl/Admin, for
//!   the client-side guardrail described in design §3.8 layer 2.
//! - [`Language::context_at`] — a *minimal* cursor-context classification
//!   (full semantic context, e.g. "inside a WHERE clause", is a later
//!   milestone per the design doc).
//! - [`Language::highlight`] — a token stream for syntax highlighting.
//!
//! Hand-rolled lexers only (design §3.6 / §5.1 tree-sitter sizing note): no
//! parser-generator, no tree-sitter, no `sqlparser-rs`. `sqlparser-rs` is
//! reserved for optional *deep analysis*, out of scope for this crate.
//!
//! No embedded JS engine for Mongo (design §3.6, explicit decision): the
//! `mongo` module hand-rolls a ~700-line parser for the `db.coll.method(...)`
//! surface plus extended-JSON constructors, and explicitly rejects anything
//! that is actually arbitrary JavaScript.

#![warn(rust_2018_idioms)]
#![warn(missing_debug_implementations)]

pub mod directives;
pub mod mongo;
pub mod redis;
pub mod sql;

use std::fmt;
use std::ops::Range;

use dbx_api::LanguageId;

pub use directives::{DirectiveError, Directives};

/// One statement (or batch, for T-SQL `GO`) inside a buffer, as a byte range
/// into the original source text — never a copy, so callers can slice the
/// original `&str` (or, for a rope-backed editor, translate the range
/// themselves).
///
/// `directives` is a `Result` rather than a bare [`Directives`] so that a
/// malformed directive comment (design §3.6 block directives) is a value the
/// caller can react to per-statement — never a panic. See
/// [`directives::parse_directives`].
#[derive(Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// Byte range into the source buffer passed to [`Language::split`].
    pub range: Range<usize>,
    /// Directives parsed from the run of directive-comment lines immediately
    /// above this statement (design §3.6: `@limit`, `@timeout`,
    /// `@connection`, `@readonly`). `Ok(Directives::default())` when none are
    /// present.
    pub directives: Result<Directives, DirectiveError>,
}

impl fmt::Debug for StatementSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatementSpan")
            .field("range", &self.range)
            .field("directives", &self.directives)
            .finish()
    }
}

impl StatementSpan {
    /// Slice the original source with this span's range.
    pub fn text<'s>(&self, src: &'s str) -> &'s str {
        &src[self.range.clone()]
    }
}

/// What a statement *does*, for the client-side write guardrail (design
/// §3.8, layer 2 of three — "a guardrail against fat fingers, not an
/// adversary").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementClass {
    Read,
    Write,
    Ddl,
    Tcl,
    Admin,
    Unknown,
}

/// Minimal cursor-context classification. Full semantic context (e.g. "past
/// the `WHERE` keyword, before a column name") is an explicit later
/// milestone — this is deliberately coarse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditContext {
    /// Ordinary statement text — not inside a string, comment, or the
    /// identifier the cursor happens to sit in.
    Statement,
    StringLiteral,
    Comment,
    /// Cursor is inside (or immediately after) an identifier-like run of
    /// characters.
    Identifier,
}

/// One highlighted token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub range: Range<usize>,
    pub kind: TokenKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Keyword,
    Ident,
    String,
    Number,
    Comment,
    Operator,
    Punct,
}

/// Everything an editor needs from a query language, for one
/// `dbx_api::LanguageId`. See the module docs for what each method is for.
pub trait Language: fmt::Debug + Send + Sync {
    fn id(&self) -> LanguageId;

    /// Cut `src` into statements. Byte ranges only — no allocation of the
    /// statement text itself. A trailing statement with no terminator is
    /// still returned as a span (design §3.6 splitter requirements).
    fn split(&self, src: &str) -> Vec<StatementSpan>;

    /// Classify a single statement's text (typically `span.text(src)` from a
    /// [`StatementSpan`] produced by [`Language::split`]).
    fn classify(&self, stmt: &str) -> StatementClass;

    /// Minimal context of the cursor at `byte_offset` within `src`.
    ///
    /// # Panics
    /// May panic if `byte_offset` is not a char boundary of `src` or is out
    /// of range — callers are expected to pass an offset from the same
    /// buffer's own editor cursor, which is always valid UTF-8-aligned.
    fn context_at(&self, src: &str, byte_offset: usize) -> EditContext;

    /// Tokenize `src` for syntax highlighting.
    fn highlight(&self, src: &str) -> Vec<Token>;
}

/// Registry: the one language impl for a given connection's
/// `Capabilities::language` (design §3.6).
///
/// `EsDsl`, `Cypher`, and `PartiQl` are members of [`LanguageId`] that this
/// crate does not yet implement (out of scope for this milestone — see the
/// crate-level deviations note); they resolve to a minimal
/// [`fallback::FallbackLanguage`] rather than making this function partial,
/// so the registry stays total and no caller has to handle a missing
/// language.
pub fn language_for(id: LanguageId) -> &'static dyn Language {
    use dbx_api::SqlDialect;

    match id {
        LanguageId::Sql(SqlDialect::Postgres) => &sql::POSTGRES,
        LanguageId::Sql(SqlDialect::Mysql) => &sql::MYSQL,
        LanguageId::Sql(SqlDialect::Sqlite) => &sql::SQLITE,
        LanguageId::Sql(SqlDialect::Mssql) => &sql::MSSQL,
        // ClickHouse and DuckDB are SQL dialects without a dedicated splitter
        // profile in this milestone (design §3.6 lists Postgres/MySQL/SQLite/
        // MSSQL splitter quirks explicitly; ClickHouse/DuckDB are close
        // enough to Postgres-family syntax generally that the Postgres
        // profile — `--`/`/* */` comments, `'`/`"` quoting, `$$`
        // dollar-quoting — is a reasonable default rather than a dedicated
        // one; deviation noted in the final report).
        LanguageId::Sql(SqlDialect::Clickhouse) => &sql::POSTGRES,
        LanguageId::Sql(SqlDialect::Duckdb) => &sql::POSTGRES,
        LanguageId::MongoShell => &mongo::MONGO,
        LanguageId::RedisCli => &redis::REDIS,
        LanguageId::EsDsl | LanguageId::Cypher | LanguageId::PartiQl => &fallback::FALLBACK,
    }
}

/// Minimal fallback for `LanguageId` variants this milestone does not
/// implement, so [`language_for`] stays a total function. See its doc.
pub mod fallback {
    use super::*;

    #[derive(Debug)]
    pub struct FallbackLanguage;

    pub static FALLBACK: FallbackLanguage = FallbackLanguage;

    impl Language for FallbackLanguage {
        fn id(&self) -> LanguageId {
            // Arbitrary: this impl is never selected for `Sql`/`MongoShell`/
            // `RedisCli`, only for the not-yet-implemented variants, and
            // `id()` is not used to distinguish among those.
            LanguageId::EsDsl
        }

        fn split(&self, src: &str) -> Vec<StatementSpan> {
            if src.trim().is_empty() {
                return Vec::new();
            }
            vec![StatementSpan {
                range: 0..src.len(),
                directives: Ok(Directives::default()),
            }]
        }

        fn classify(&self, _stmt: &str) -> StatementClass {
            StatementClass::Unknown
        }

        fn context_at(&self, _src: &str, _byte_offset: usize) -> EditContext {
            EditContext::Statement
        }

        fn highlight(&self, _src: &str) -> Vec<Token> {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbx_api::SqlDialect;

    #[test]
    fn registry_covers_every_language_id_without_panicking() {
        let ids = [
            LanguageId::Sql(SqlDialect::Postgres),
            LanguageId::Sql(SqlDialect::Mysql),
            LanguageId::Sql(SqlDialect::Sqlite),
            LanguageId::Sql(SqlDialect::Mssql),
            LanguageId::Sql(SqlDialect::Clickhouse),
            LanguageId::Sql(SqlDialect::Duckdb),
            LanguageId::MongoShell,
            LanguageId::RedisCli,
            LanguageId::EsDsl,
            LanguageId::Cypher,
            LanguageId::PartiQl,
        ];
        for id in ids {
            let lang = language_for(id);
            let _ = lang.split("select 1");
        }
    }

    #[test]
    fn fallback_language_is_inert() {
        let lang = &fallback::FALLBACK;
        assert_eq!(lang.classify("anything"), StatementClass::Unknown);
        assert_eq!(lang.context_at("x", 0), EditContext::Statement);
        assert!(lang.highlight("x").is_empty());
        assert!(lang.split("   \n  ").is_empty());
    }
}
