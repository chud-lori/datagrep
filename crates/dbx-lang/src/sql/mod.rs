//! SQL [`Language`] impl, one per [`SqlDialect`]. Splitting, classification,
//! and highlighting are dialect-parameterized functions in the sibling
//! modules; this module is just the thin [`Language`] wrapper plus the
//! per-dialect static instances the [`crate::language_for`] registry hands
//! out.

pub mod classifier;
pub mod highlight;
pub mod lexer;
pub mod splitter;

use dbx_api::{LanguageId, SqlDialect};

use crate::{EditContext, Language, StatementClass, StatementSpan, Token};

#[derive(Debug)]
pub struct SqlLanguage {
    dialect: SqlDialect,
}

pub static POSTGRES: SqlLanguage = SqlLanguage {
    dialect: SqlDialect::Postgres,
};
pub static MYSQL: SqlLanguage = SqlLanguage {
    dialect: SqlDialect::Mysql,
};
pub static SQLITE: SqlLanguage = SqlLanguage {
    dialect: SqlDialect::Sqlite,
};
pub static MSSQL: SqlLanguage = SqlLanguage {
    dialect: SqlDialect::Mssql,
};

impl Language for SqlLanguage {
    fn id(&self) -> LanguageId {
        LanguageId::Sql(self.dialect)
    }

    fn split(&self, src: &str) -> Vec<StatementSpan> {
        splitter::split(src, self.dialect)
    }

    fn classify(&self, stmt: &str) -> StatementClass {
        classifier::classify(stmt)
    }

    fn context_at(&self, src: &str, byte_offset: usize) -> EditContext {
        context_at(src, self.dialect, byte_offset)
    }

    fn highlight(&self, src: &str) -> Vec<Token> {
        highlight::highlight(src, self.dialect)
    }
}

fn context_at(src: &str, dialect: SqlDialect, offset: usize) -> EditContext {
    use lexer::Chunk;

    let bytes = src.as_bytes();
    let chunks = lexer::lex_chunks(src, dialect);
    let containing = chunks.iter().find(|c| {
        let r = c.range();
        r.start <= offset && offset < r.end
    });
    match containing {
        Some(Chunk::Comment(_)) => return EditContext::Comment,
        Some(Chunk::Quoted(..)) => return EditContext::StringLiteral,
        _ => {}
    }

    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    let before = offset.checked_sub(1).and_then(|p| bytes.get(p)).copied();
    let at = bytes.get(offset).copied();
    if before.is_some_and(is_ident_byte) || at.is_some_and(is_ident_byte) {
        EditContext::Identifier
    } else {
        EditContext::Statement
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_at_distinguishes_string_comment_identifier_statement() {
        let src = "SELECT id FROM t WHERE x = 'hi' -- note";
        let offset_in_ident = src.find("id").unwrap() + 1;
        assert_eq!(
            context_at(src, SqlDialect::Postgres, offset_in_ident),
            EditContext::Identifier
        );

        let offset_in_string = src.find("'hi'").unwrap() + 2;
        assert_eq!(
            context_at(src, SqlDialect::Postgres, offset_in_string),
            EditContext::StringLiteral
        );

        let offset_in_comment = src.find("note").unwrap();
        assert_eq!(
            context_at(src, SqlDialect::Postgres, offset_in_comment),
            EditContext::Comment
        );

        // `=` sits between two spaces: neither neighboring byte is
        // identifier-like, so this is unambiguously plain statement text.
        let offset_at_operator = src.find(" = ").unwrap() + 1;
        assert_eq!(
            context_at(src, SqlDialect::Postgres, offset_at_operator),
            EditContext::Statement
        );
    }

    #[test]
    fn language_trait_end_to_end_smoke() {
        let lang = &POSTGRES;
        assert_eq!(lang.id(), LanguageId::Sql(SqlDialect::Postgres));
        let spans = lang.split("SELECT 1; INSERT INTO t VALUES (1);");
        assert_eq!(spans.len(), 2);
        assert_eq!(
            lang.classify(spans[0].text("SELECT 1; INSERT INTO t VALUES (1);")),
            StatementClass::Read
        );
        assert!(!lang.highlight("SELECT 1").is_empty());
    }
}
