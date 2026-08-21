#![warn(rust_2018_idioms)]
#![warn(missing_debug_implementations)]

pub mod directives;
pub mod esdsl;
pub mod mongo;
pub mod redis;
pub mod sql;

use std::fmt;
use std::ops::Range;

use datagrep_api::LanguageId;

pub use directives::{DirectiveError, Directives};

#[derive(Clone, PartialEq, Eq)]
pub struct StatementSpan {
    pub range: Range<usize>,
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
    pub fn text<'s>(&self, src: &'s str) -> &'s str {
        &src[self.range.clone()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementClass {
    Read,
    Write,
    Ddl,
    Tcl,
    Admin,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditContext {
    Statement,
    StringLiteral,
    Comment,
    Identifier,
}

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

pub trait Language: fmt::Debug + Send + Sync {
    fn id(&self) -> LanguageId;

    fn split(&self, src: &str) -> Vec<StatementSpan>;

    fn classify(&self, stmt: &str) -> StatementClass;

    fn context_at(&self, src: &str, byte_offset: usize) -> EditContext;

    fn highlight(&self, src: &str) -> Vec<Token>;
}

pub fn language_for(id: LanguageId) -> &'static dyn Language {
    use datagrep_api::SqlDialect;

    match id {
        LanguageId::Sql(SqlDialect::Postgres) => &sql::POSTGRES,
        LanguageId::Sql(SqlDialect::Mysql) => &sql::MYSQL,
        LanguageId::Sql(SqlDialect::Sqlite) => &sql::SQLITE,
        LanguageId::Sql(SqlDialect::Mssql) => &sql::MSSQL,
        LanguageId::Sql(SqlDialect::Clickhouse) => &sql::POSTGRES,
        LanguageId::Sql(SqlDialect::Duckdb) => &sql::POSTGRES,
        LanguageId::MongoShell => &mongo::MONGO,
        LanguageId::RedisCli => &redis::REDIS,
        LanguageId::EsDsl => &esdsl::ES_DSL,
        LanguageId::Cypher | LanguageId::PartiQl => &fallback::FALLBACK,
    }
}

pub mod fallback {
    use super::*;

    #[derive(Debug)]
    pub struct FallbackLanguage;

    pub static FALLBACK: FallbackLanguage = FallbackLanguage;

    impl Language for FallbackLanguage {
        fn id(&self) -> LanguageId {
            LanguageId::Cypher
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
    use datagrep_api::SqlDialect;

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
