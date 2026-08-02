//! Capabilities, not driver checks (design §3.1). The UI renders a control
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
    }
}

/// Everything the core needs to know about a connection's dialect and limits,
/// so no layer above ever needs to know which engine it is talking to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub flags: Caps,
    /// Server-imposed statement size limit, when one exists.
    pub max_statement_bytes: Option<u64>,
    /// Starting fetch size before adaptive sizing kicks in (design §3.2:
    /// PG 500, ClickHouse 65536, Mongo 101).
    pub default_fetch_rows: u32,
    pub param_style: ParamStyle,
    /// The language the editor should speak for this connection — there is no
    /// cross-engine translation layer, by decision (design §3.6).
    pub language: LanguageId,
    /// Identifier quoting char for statements we generate (`"` PG, `` ` `` MySQL).
    pub identifier_quote: char,
    /// Depth of the catalog hierarchy (e.g. database→schema→table = 3).
    pub catalog_levels: u8,
}

/// How the engine's native protocol binds parameters — values are always bound,
/// never spliced into text (design §3.8).
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
/// connection decides (design §3.6).
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
