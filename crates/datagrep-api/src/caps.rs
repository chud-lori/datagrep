//! Capabilities, not driver checks. The UI renders a control
//! disabled with a truthful tooltip instead of existing-and-erroring; any
//! `if driver_id == …` above this crate is a missing flag here.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Feature flags a connection reports post-handshake (version-aware).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Caps: u32 {
        const TRANSACTIONS      = 1 << 0;
        const DDL               = 1 << 1;
        const EXPLAIN           = 1 << 2;
        /// Off when rows have no usable identity — we never guess what to mutate.
        const EDITABLE_RESULTS  = 1 << 3;
        /// A cancel actually reaches the server (see `CancelKind`).
        const SERVER_CANCEL     = 1 << 4;
        /// False for Mongo/ES/DDB — the UI shows "≥ N" instead of lying.
        const EXACT_COUNT_CHEAP = 1 << 5;
        /// False → keyset-only pagination UI; no OFFSET tricks.
        const RANDOM_ACCESS_PAGE = 1 << 6;
        /// False → the catalog uses sample-based inference, and says so.
        const SCHEMA_DECLARED   = 1 << 7;
        /// False for prod Redis — no "list all keys", ever.
        const KEY_ENUMERATION   = 1 << 8;
        /// The engine can put the whole session in read-only mode server-side.
        const READ_ONLY_SESSION = 1 << 9;
        /// Savepoints — a transaction can nest.
        const NESTED_TRANSACTIONS = 1 << 10;
        /// `EXPLAIN` can actually run the statement and report real timings.
        const EXPLAIN_ANALYZE   = 1 << 11;
        /// More than one statement per round trip.
        const MULTI_STATEMENT   = 1 << 12;
        /// Placeholders are positional (`$1`, `?`).
        const POSITIONAL_PARAMS = 1 << 13;
        /// Placeholders are named (`:name`, `@name`).
        const NAMED_PARAMS      = 1 << 14;
        /// Results can be streamed straight to a file without going through
        /// the result store — "export all" must not mean "load all into memory".
        const EXPORT_STREAMING  = 1 << 15;
        /// The driver can compile a `Predicate` at all; false greys out the
        /// filter box rather than erroring at run time.
        const EXPRESSION_FILTER = 1 << 16;
        /// A `MutationBatch` applies all-or-nothing (a real transaction or an
        /// atomic pipeline). Off means a failing batch can leave a prefix
        /// applied — the commit UI must say so *before* the click. Distinct
        /// from `TRANSACTIONS`, which is about interactive `begin`.
        const ATOMIC_BATCH      = 1 << 17;
    }
}

/// Everything the core needs to know about a connection's dialect and limits,
/// so no layer above ever needs to know which engine it is talking to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub flags: Caps,
    /// Server-imposed statement size limit, when one exists.
    pub max_statement_bytes: Option<u64>,
    /// Starting fetch size before adaptive sizing kicks in; the sane per-engine
    /// starting points differ wildly (PG 500, ClickHouse 65536, Mongo 101).
    pub default_fetch_rows: u32,
    pub param_style: ParamStyle,
    /// The language the editor should speak for this connection — there is no
    /// cross-engine translation layer, by decision.
    pub language: LanguageId,
    /// Identifier quoting char for statements we generate (`"` PG, `` ` `` MySQL).
    pub identifier_quote: char,
    /// Depth of the catalog hierarchy (e.g. database→schema→table = 3).
    pub catalog_levels: u8,
}

/// How the engine's native protocol binds parameters — values are always bound,
/// never spliced into text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParamStyle {
    /// `$1, $2` (Postgres).
    DollarNumbered,
    /// `?` positional (SQLite, MySQL).
    QuestionMark,
    /// `:name` (SQLite named, Oracle).
    ColonNamed,
    /// `@p1` (MSSQL).
    AtNamed,
    /// The engine takes structured requests, not parameterized text.
    None,
}

/// Query language of a connection; the editor is language-agnostic and the
/// connection decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageId {
    Sql(SqlDialect),
    MongoShell,
    RedisCli,
    EsDsl,
    Cypher,
    PartiQl,
}

/// SQL dialect for highlighting, splitting, and identifier quoting — never for
/// translating user-authored text between engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SqlDialect {
    Postgres,
    Mysql,
    Sqlite,
    Mssql,
    Clickhouse,
    Duckdb,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_flags_compose_and_query() {
        let pg = Caps::TRANSACTIONS | Caps::DDL | Caps::EXPLAIN | Caps::SERVER_CANCEL;
        assert!(pg.contains(Caps::SERVER_CANCEL | Caps::TRANSACTIONS));
        let redis = Caps::KEY_ENUMERATION; // dev Redis: scan allowed, no transactions flag
        assert!(!redis.contains(Caps::TRANSACTIONS));
        assert!(Caps::empty().is_empty());
    }
}
