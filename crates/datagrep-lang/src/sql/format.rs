use datagrep_api::SqlDialect;

use super::highlight::{highlight, KEYWORDS};
use super::splitter::split;
use crate::{Token, TokenKind};

const CLAUSES: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "VALUES",
    "SET",
    "RETURNING",
    "WINDOW",
    "INSERT",
    "UPDATE",
    "DELETE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "FULL",
    "CROSS",
];
const JOIN_LEADS: &[&str] = &["LEFT", "RIGHT", "INNER", "FULL", "CROSS", "OUTER"];
// The keywords MySQL reserves: any other word may be a case-sensitive table name or alias there.
const MYSQL_RESERVED: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "REPLACE",
    "CREATE",
    "ALTER",
    "DROP",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER",
    "OUTER",
    "LEFT",
    "RIGHT",
    "CROSS",
    "ON",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "AS",
    "DISTINCT",
    "UNION",
    "ALL",
    "INTO",
    "VALUES",
    "SET",
    "DEFAULT",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "INDEX",
    "TABLE",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "WITH",
    "ASC",
    "DESC",
    "TRUE",
    "FALSE",
    "USING",
    "DATABASE",
    "SCHEMA",
    "IF",
    "CASCADE",
    "RESTRICT",
    "CONSTRAINT",
    "COLUMN",
    "GRANT",
    "REVOKE",
    "WHILE",
    "FOR",
    "OF",
    "OVER",
    "PARTITION",
    "WINDOW",
    "RECURSIVE",
    "LATERAL",
    "EXPLAIN",
    "SHOW",
    "KILL",
    "RENAME",
    "CALL",
];
const SELECT_ALIGN: usize = "SELECT ".len();
const STEP: usize = 2;

#[derive(Debug, Default)]
struct Block {
    indent: usize,
    inline: usize,
    case_depth: usize,
    clause: Option<String>,
    between: bool,
}

/// Re-flows whitespace and upper-cases keywords; every other byte of every token is kept.
pub fn format(src: &str, dialect: SqlDialect) -> String {
    let spans = split(src, dialect);
    if spans.is_empty() {
        return src.to_string();
    }
    let mut out = String::new();
    let mut prev = 0;
    for span in &spans {
        out.push_str(src[prev..span.range.start].trim());
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&statement(&src[span.range.clone()], dialect));
        prev = span.range.end;
    }
    out.push_str(src[prev..].trim());
    if src.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn upcase_ok(word: &str, dialect: SqlDialect) -> bool {
    let list = if dialect == SqlDialect::Mysql {
        MYSQL_RESERVED
    } else {
        KEYWORDS
    };
    list.iter().any(|k| k.eq_ignore_ascii_case(word))
}

struct Writer<'a> {
    src: &'a str,
    dialect: SqlDialect,
    tokens: Vec<Token>,
    out: String,
    blocks: Vec<Block>,
    line_indent: usize,
    pending: Option<usize>,
    force_newline: bool,
    last_code: Option<usize>,
}

impl<'a> Writer<'a> {
    fn text(&self, i: usize) -> &'a str {
        &self.src[self.tokens[i].range.clone()]
    }

    fn is(&self, i: usize, words: &[&str]) -> bool {
        self.tokens[i].kind == TokenKind::Keyword
            && words.iter().any(|w| self.text(i).eq_ignore_ascii_case(w))
    }

    fn is_punct(&self, i: usize, p: &str) -> bool {
        self.tokens[i].kind == TokenKind::Punct && self.text(i) == p
    }

    fn next_code(&self, i: usize) -> Option<usize> {
        (i + 1..self.tokens.len()).find(|&j| self.tokens[j].kind != TokenKind::Comment)
    }

    fn block(&mut self) -> &mut Block {
        self.blocks
            .last_mut()
            .expect("the statement block is never popped")
    }

    fn quiet(&self) -> bool {
        let b = self
            .blocks
            .last()
            .expect("the statement block is never popped");
        b.inline > 0 || b.case_depth > 0
    }

    fn starts_clause(&self, i: usize, prev: Option<usize>) -> bool {
        if self.quiet() || !self.is(i, CLAUSES) {
            return false;
        }
        let Some(p) = prev else {
            return true;
        };
        let next = self.next_code(i);
        let after = |words: &[&str]| self.is(p, words);
        let followed_by = |words: &[&str]| next.is_some_and(|n| self.is(n, words));
        let opens_call = next.is_some_and(|n| {
            self.is_punct(n, "(") && self.tokens[n].range.start == self.tokens[i].range.end
        });
        let word = self.text(i).to_ascii_uppercase();
        let glued = match self.tokens[p].kind {
            TokenKind::Operator => self.text(p) != "*",
            TokenKind::Punct => self.text(p) != ")",
            _ => false,
        };
        !(glued
            || (matches!(word.as_str(), "GROUP" | "ORDER") && !followed_by(&["BY"]))
            || (word == "JOIN" && after(JOIN_LEADS))
            || (matches!(word.as_str(), "LEFT" | "RIGHT") && opens_call)
            || (word == "UPDATE" && after(&["FOR", "DO", "ON", "KEY"]))
            || (word == "DELETE" && after(&["ON"]))
            || (word == "FROM" && after(&["DELETE", "DISTINCT"])))
    }

    fn newline(&mut self, indent: usize) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out.extend(std::iter::repeat(' ').take(indent));
        self.line_indent = indent;
    }

    fn gap_before(&self, i: usize) -> &'a str {
        let start = if i == 0 {
            0
        } else {
            self.tokens[i - 1].range.end
        };
        &self.src[start..self.tokens[i].range.start]
    }

    fn space(&mut self, i: usize) {
        if self.out.is_empty() || self.out.ends_with('\n') || self.out.ends_with(' ') {
            return;
        }
        let after_comma = self.last_code.is_some_and(|p| self.is_punct(p, ","));
        if !self.gap_before(i).is_empty() || after_comma {
            self.out.push(' ');
        }
    }

    fn push_token(&mut self, i: usize) {
        let text = self.text(i);
        if self.tokens[i].kind == TokenKind::Keyword && upcase_ok(text, self.dialect) {
            self.out.push_str(&text.to_ascii_uppercase());
        } else {
            self.out.push_str(text);
        }
    }

    fn comment(&mut self, i: usize) {
        let own_line = self.out.is_empty() || self.gap_before(i).contains('\n');
        if own_line {
            let indent = match self.next_code(i) {
                Some(n) if self.starts_clause(n, self.last_code) => {
                    self.blocks.last().map_or(0, |b| b.indent)
                }
                _ => self.pending.take().unwrap_or(self.line_indent),
            };
            self.newline(indent);
        } else {
            self.space(i);
        }
        self.out.push_str(self.text(i));
        let after = match self.tokens.get(i + 1) {
            Some(next) => &self.src[self.tokens[i].range.end..next.range.start],
            None => "",
        };
        if self.text(i).starts_with("--") || self.text(i).starts_with('#') || after.contains('\n') {
            self.force_newline = true;
        }
    }

    fn code(&mut self, i: usize) {
        let prev = self.last_code;
        let indent = self.blocks.last().map_or(0, |b| b.indent);
        let mut brk = self.pending.take();

        if self.is_punct(i, ")") {
            if self.block().inline > 0 {
                self.block().inline -= 1;
            } else if self.blocks.len() > 1 {
                self.blocks.pop();
                brk = Some(self.blocks.last().map_or(0, |b| b.indent));
            }
        } else if self.starts_clause(i, prev) {
            if prev.is_some() {
                brk = Some(indent);
            }
            let word = self.text(i).to_ascii_uppercase();
            let b = self.block();
            b.clause = Some(word);
            b.between = false;
        } else if self.is(i, &["AND", "OR"]) && !self.quiet() {
            if self.block().between && self.is(i, &["AND"]) {
                self.block().between = false;
            } else if self.blocks.last().is_some_and(|b| b.clause.is_some()) {
                brk = Some(indent + STEP);
            }
        }

        match brk {
            Some(n) => self.newline(n),
            None if self.force_newline => {
                let n = self.line_indent;
                self.newline(n);
            }
            None => self.space(i),
        }
        self.force_newline = false;
        self.push_token(i);
        self.last_code = Some(i);

        if self.is(i, &["BETWEEN"]) {
            self.block().between = true;
        } else if self.is(i, &["CASE"]) {
            self.block().case_depth += 1;
        } else if self.is(i, &["END"]) && self.block().case_depth > 0 {
            self.block().case_depth -= 1;
        } else if self.is_punct(i, "(") {
            let subquery = self
                .next_code(i)
                .is_some_and(|n| self.is(n, &["SELECT", "WITH"]));
            if subquery && !self.quiet() {
                let inner = indent + STEP;
                self.blocks.push(Block {
                    indent: inner,
                    ..Block::default()
                });
                self.pending = Some(inner);
            } else {
                self.block().inline += 1;
            }
        } else if self.is_punct(i, ",")
            && !self.quiet()
            && self
                .blocks
                .last()
                .is_some_and(|b| b.clause.as_deref() == Some("SELECT"))
        {
            self.pending = Some(indent + SELECT_ALIGN);
        }
    }
}

fn statement(stmt: &str, dialect: SqlDialect) -> String {
    let mut w = Writer {
        src: stmt,
        dialect,
        tokens: highlight(stmt, dialect),
        out: String::new(),
        blocks: vec![Block::default()],
        line_indent: 0,
        pending: None,
        force_newline: false,
        last_code: None,
    };
    for i in 0..w.tokens.len() {
        if w.tokens[i].kind == TokenKind::Comment {
            w.comment(i);
        } else {
            w.code(i);
        }
    }
    w.out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: SqlDialect = SqlDialect::Postgres;

    fn fmt(src: &str) -> String {
        format(src, PG)
    }

    // Everything but whitespace and keyword case must survive, in order.
    fn same_tokens(a: &str, b: &str, dialect: SqlDialect) {
        let norm = |s: &str| -> Vec<(TokenKind, String)> {
            highlight(s, dialect)
                .into_iter()
                .map(|t| {
                    let text = &s[t.range];
                    let text = if t.kind == TokenKind::Keyword {
                        text.to_ascii_uppercase()
                    } else {
                        text.to_string()
                    };
                    (t.kind, text)
                })
                .collect()
        };
        assert_eq!(norm(a), norm(b), "tokens changed:\n{a}\n---\n{b}");
    }

    const CORPUS: &[&str] = &[
        "select id, name from users where active = true and age between 18 and 65 order by name",
        "select u.id,o.total from users u left join orders o on o.user_id=u.id where o.total>10 or u.vip",
        "SELECT * FROM t WHERE id IN (SELECT user_id FROM orders WHERE total > 100) LIMIT 5",
        "insert into t (a, b) values (1, 'x;y'), (2, 'it''s')",
        "update t set a = 1, b = $1 where id = :id and tag = @tag",
        "select x::int, data->>'k', E'a\\nb' from t -- trailing note\nwhere y = 1",
        "/* lead */ select case when a = 1 then 'one' else 'many' end as n from t",
        "with recent as (select * from events where ts > now() - interval '1 day') select count(*) from recent",
        "select count(*) filter (where x) over (partition by g order by ts) from t group by g having count(*) > 1",
        "delete from t where id = 1 returning id",
        "-- @connection prod\nselect 1",
        "select a from t union all select a from u",
        "select left(name, 3) from t",
        "create table t (id int primary key, ref int references u(id) on delete cascade)",
        "select $tag$ a; b $tag$, \"Mixed Case\" from \"Weird\"\"Table\"",
    ];

    #[test]
    fn formatting_keeps_every_token() {
        for src in CORPUS {
            same_tokens(src, &fmt(src), PG);
        }
    }

    #[test]
    fn formatting_is_idempotent() {
        for src in CORPUS {
            let once = fmt(src);
            assert_eq!(fmt(&once), once, "second pass moved something:\n{once}");
        }
    }

    #[test]
    fn clauses_start_lines_and_select_items_align() {
        assert_eq!(
            fmt("select id, name from users where a = 1 and b = 2 order by name"),
            "SELECT id,\n       name\nFROM users\nWHERE a = 1\n  AND b = 2\nORDER BY name"
        );
    }

    #[test]
    fn subqueries_indent_and_calls_stay_inline() {
        assert_eq!(
            fmt("select * from t where id in (select uid from o where n > count(x))"),
            "SELECT *\nFROM t\nWHERE id IN (\n  SELECT uid\n  FROM o\n  WHERE n > count(x)\n)"
        );
    }

    #[test]
    fn joins_break_once_per_join() {
        assert_eq!(
            fmt("select * from a left outer join b on a.id = b.id inner join c using (id)"),
            "SELECT *\nFROM a\nLEFT OUTER JOIN b ON a.id = b.id\nINNER JOIN c USING (id)"
        );
    }

    #[test]
    fn between_keeps_its_and() {
        assert_eq!(
            fmt("select 1 from t where a between 1 and 2 and b = 3"),
            "SELECT 1\nFROM t\nWHERE a BETWEEN 1 AND 2\n  AND b = 3"
        );
    }

    #[test]
    fn a_line_comment_never_swallows_the_next_token() {
        let out = fmt("select a, -- why\nb from t");
        assert_eq!(out, "SELECT a, -- why\n       b\nFROM t");
    }

    #[test]
    fn statements_are_separated_and_terminators_kept() {
        assert_eq!(fmt("select 1;select 2;\n"), "SELECT 1;\n\nSELECT 2;\n");
    }

    #[test]
    fn strings_and_quoted_names_are_untouched() {
        let out = fmt("select 'from  where', \"select\" from t");
        assert!(out.contains("'from  where'"), "{out}");
        assert!(out.contains("\"select\""), "{out}");
    }

    #[test]
    fn mysql_keeps_the_case_of_words_it_does_not_reserve() {
        let src = "select comment from first join `key` on first.id = `key`.id";
        let out = format(src, SqlDialect::Mysql);
        assert!(out.contains("SELECT comment"), "{out}");
        assert!(out.contains("FROM first"), "{out}");
        same_tokens(src, &out, SqlDialect::Mysql);
    }

    #[test]
    fn mysql_delimiter_blocks_survive() {
        let src =
            "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END//\nDELIMITER ;\nselect 2;";
        let out = format(src, SqlDialect::Mysql);
        assert!(
            out.starts_with("DELIMITER //\n\nCREATE PROCEDURE p()"),
            "{out}"
        );
        assert!(out.contains("END//\nDELIMITER ;"), "{out}");
        assert!(out.ends_with("SELECT 2;"), "{out}");
    }

    #[test]
    fn blank_input_is_returned_as_is() {
        assert_eq!(fmt("  \n"), "  \n");
    }
}
