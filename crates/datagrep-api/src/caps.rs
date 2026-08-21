use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Caps: u32 {
        const TRANSACTIONS      = 1 << 0;
        const DDL               = 1 << 1;
        const EXPLAIN           = 1 << 2;
        const EDITABLE_RESULTS  = 1 << 3;
        const SERVER_CANCEL     = 1 << 4;
        const EXACT_COUNT_CHEAP = 1 << 5;
        const RANDOM_ACCESS_PAGE = 1 << 6;
        const SCHEMA_DECLARED   = 1 << 7;
        const KEY_ENUMERATION   = 1 << 8;
        const READ_ONLY_SESSION = 1 << 9;
        const NESTED_TRANSACTIONS = 1 << 10;
        const EXPLAIN_ANALYZE   = 1 << 11;
        const MULTI_STATEMENT   = 1 << 12;
        const POSITIONAL_PARAMS = 1 << 13;
        const NAMED_PARAMS      = 1 << 14;
        const EXPORT_STREAMING  = 1 << 15;
        const EXPRESSION_FILTER = 1 << 16;
        const ATOMIC_BATCH      = 1 << 17;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub flags: Caps,
    pub max_statement_bytes: Option<u64>,
    pub default_fetch_rows: u32,
    pub param_style: ParamStyle,
    pub language: LanguageId,
    pub identifier_quote: char,
    pub catalog_levels: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParamStyle {
    DollarNumbered,
    QuestionMark,
    ColonNamed,
    AtNamed,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageId {
    Sql(SqlDialect),
    MongoShell,
    RedisCli,
    EsDsl,
    Cypher,
    PartiQl,
}

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
