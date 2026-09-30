use datagrep_api::SqlDialect;

use super::lexer::{lex_chunks, Chunk, QuoteKind};

// Metadata every MySQL connection can read; `mysql.*` holds grants and hashes, so it is not here.
const SHARED_SCHEMAS: &[&str] = &["information_schema", "performance_schema", "sys"];

const TABLE_KEYWORDS: &[&str] = &["FROM", "JOIN", "STRAIGHT_JOIN", "INTO", "UPDATE", "TABLE"];
const LIST_ENDS: &[&str] = &[
    "WHERE",
    "ON",
    "USING",
    "SET",
    "GROUP",
    "ORDER",
    "HAVING",
    "SELECT",
    "RETURNING",
];

#[derive(Debug)]
enum Lexeme<'a> {
    Word(&'a str),
    Quoted(String),
    Dot,
    Comma,
    Open,
    Close,
    Other,
}

impl Lexeme<'_> {
    fn name(&self) -> Option<String> {
        match self {
            Lexeme::Word(w) => Some((*w).to_string()),
            Lexeme::Quoted(q) => Some(q.clone()),
            _ => None,
        }
    }

    fn is_word(&self, set: &[&str]) -> bool {
        matches!(self, Lexeme::Word(w) if set.iter().any(|k| w.eq_ignore_ascii_case(k)))
    }
}

// Only MySQL names another database inline; Postgres rejects it and SQLite has no configured name.
pub fn foreign_databases(stmt: &str, dialect: SqlDialect, current: &str) -> Vec<String> {
    if dialect != SqlDialect::Mysql {
        return Vec::new();
    }
    let mut out: Vec<String> = mysql_databases(stmt)
        .into_iter()
        .filter(|db| {
            !db.eq_ignore_ascii_case(current)
                && !SHARED_SCHEMAS.iter().any(|s| db.eq_ignore_ascii_case(s))
        })
        .collect();
    out.dedup();
    out
}

fn mysql_databases(stmt: &str) -> Vec<String> {
    let lexemes = lexemes(stmt);
    let mut out = Vec::new();

    if let [first, db, ..] = lexemes.as_slice() {
        if first.is_word(&["USE"]) {
            out.extend(db.name());
        }
    }

    let mut in_list = false;
    let mut expect_table = false;
    let mut outer = Vec::new();
    for (i, lexeme) in lexemes.iter().enumerate() {
        let at_table = std::mem::take(&mut expect_table);
        let after_key = i > 0 && lexemes[i - 1].is_word(&["KEY"]);
        match lexeme {
            l if l.is_word(TABLE_KEYWORDS) && !(after_key && l.is_word(&["UPDATE"])) => {
                in_list = true;
                expect_table = true;
            }
            l if l.is_word(LIST_ENDS) => in_list = false,
            Lexeme::Comma => expect_table = in_list,
            Lexeme::Open => outer.push(std::mem::replace(&mut in_list, false)),
            Lexeme::Close => in_list = outer.pop().unwrap_or(false),
            _ => {}
        }
        if at_table && matches!(lexemes.get(i + 1), Some(Lexeme::Dot)) {
            if let (Some(db), Some(_)) = (lexeme.name(), lexemes.get(i + 2).and_then(Lexeme::name))
            {
                out.push(db);
            }
        }
    }
    out
}

fn lexemes(stmt: &str) -> Vec<Lexeme<'_>> {
    let bytes = stmt.as_bytes();
    let mut out = Vec::new();
    for chunk in lex_chunks(stmt, SqlDialect::Mysql) {
        match chunk {
            Chunk::Quoted(range, QuoteKind::Backtick) => {
                let raw = &stmt[range.start + 1..range.end];
                let inner = raw.strip_suffix('`').unwrap_or(raw);
                out.push(Lexeme::Quoted(inner.replace("``", "`")));
            }
            Chunk::Quoted(..) => out.push(Lexeme::Other),
            Chunk::Comment(_) => {}
            Chunk::Code(range) => {
                let mut i = range.start;
                while i < range.end {
                    let b = bytes[i];
                    let start = i;
                    i += 1;
                    match b {
                        b'.' => out.push(Lexeme::Dot),
                        b',' => out.push(Lexeme::Comma),
                        b'(' => out.push(Lexeme::Open),
                        b')' => out.push(Lexeme::Close),
                        _ if is_ident_byte(b) => {
                            while i < range.end && is_ident_byte(bytes[i]) {
                                i += 1;
                            }
                            out.push(Lexeme::Word(&stmt[start..i]));
                        }
                        _ if b.is_ascii_whitespace() => {}
                        _ => out.push(Lexeme::Other),
                    }
                }
            }
        }
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn foreign(stmt: &str) -> Vec<String> {
        foreign_databases(stmt, SqlDialect::Mysql, "staging")
    }

    #[test]
    fn qualified_tables_name_their_database() {
        assert_eq!(foreign("SELECT * FROM prod.users"), ["prod"]);
        assert_eq!(foreign("select * from `prod`.`users`"), ["prod"]);
        assert_eq!(foreign("SELECT * FROM `we``ird`.t"), ["we`ird"]);
        assert_eq!(foreign("SELECT * FROM prod . users"), ["prod"]);
        assert_eq!(
            foreign("SELECT * FROM a JOIN prod.b ON a.id = b.id"),
            ["prod"]
        );
        assert_eq!(
            foreign("SELECT * FROM t, prod.u AS x, other.v"),
            ["prod", "other"]
        );
        assert_eq!(foreign("SELECT * FROM (SELECT 1) s, prod.u"), ["prod"]);
        assert_eq!(
            foreign("SELECT * FROM t WHERE id IN (SELECT id FROM prod.u)"),
            ["prod"]
        );
        assert_eq!(foreign("INSERT INTO prod.t (a, b) VALUES (1, 2)"), ["prod"]);
        assert_eq!(foreign("UPDATE prod.t SET a = 1"), ["prod"]);
    }

    #[test]
    fn use_switches_database() {
        assert_eq!(foreign("USE prod"), ["prod"]);
        assert_eq!(foreign("use `prod`"), ["prod"]);
        assert!(foreign("USE staging").is_empty());
        assert!(foreign("SELECT * FROM t USE INDEX (i)").is_empty());
    }

    #[test]
    fn columns_and_aliases_are_not_databases() {
        assert!(
            foreign("SELECT u.id, o.total FROM users u JOIN orders o ON u.id = o.uid").is_empty()
        );
        assert!(foreign("SELECT * FROM t GROUP BY t.a, t.b ORDER BY t.a, t.b").is_empty());
        assert!(foreign("UPDATE t SET t.a = 1, t.b = 2").is_empty());
        assert!(foreign("INSERT INTO t SELECT u.a, u.b FROM u").is_empty());
        assert!(
            foreign("INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE t.a = 1, t.b = 2")
                .is_empty()
        );
        assert!(foreign("SELECT 1.5, x.y FROM t").is_empty());
    }

    #[test]
    fn strings_and_comments_are_ignored() {
        assert!(foreign("SELECT 'FROM prod.users' FROM t").is_empty());
        assert!(foreign("SELECT \"FROM prod.users\" FROM t").is_empty());
        assert!(foreign("SELECT * FROM t -- FROM prod.users").is_empty());
        assert!(foreign("SELECT * FROM t # FROM prod.users").is_empty());
        assert!(foreign("SELECT * FROM t /* FROM prod.users */").is_empty());
    }

    #[test]
    fn same_database_and_shared_schemas_are_not_foreign() {
        assert!(foreign("SELECT * FROM staging.users").is_empty());
        assert!(foreign("SELECT * FROM `Staging`.users").is_empty());
        assert!(foreign("SELECT * FROM information_schema.tables").is_empty());
        assert!(foreign("SELECT * FROM performance_schema.threads").is_empty());
        assert!(foreign("SELECT * FROM sys.version").is_empty());
        assert_eq!(foreign("SELECT * FROM mysql.user"), ["mysql"]);
    }

    #[test]
    fn other_dialects_never_leave_their_database() {
        let pg = foreign_databases("SELECT * FROM prod.users", SqlDialect::Postgres, "staging");
        assert!(pg.is_empty(), "in Postgres `prod` is a schema");
        let lite = foreign_databases("SELECT * FROM aux.t", SqlDialect::Sqlite, "main");
        assert!(lite.is_empty());
    }
}
