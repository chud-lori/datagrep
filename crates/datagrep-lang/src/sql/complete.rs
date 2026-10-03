use datagrep_api::SqlDialect;

use super::highlight::{highlight, KEYWORDS};
use super::lexer::{lex_chunks, Chunk};
use super::splitter::split;
use crate::{Token, TokenKind};

const TABLE_KEYWORDS: &[&str] = &["FROM", "JOIN", "INTO", "UPDATE", "TABLE", "DESCRIBE"];
const COLUMN_KEYWORDS: &[&str] = &[
    "SELECT",
    "WHERE",
    "AND",
    "OR",
    "ON",
    "BY",
    "SET",
    "HAVING",
    "NOT",
    "DISTINCT",
    "WHEN",
    "THEN",
    "ELSE",
    "RETURNING",
    "USING",
];
const LIST_ENDS: &[&str] = &[
    "WHERE",
    "ON",
    "USING",
    "SET",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "SELECT",
    "RETURNING",
    "VALUES",
];
const MAX_CANDIDATES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Keyword,
    Table,
    Column,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

/// What the caret is in the middle of typing, and what the statement around it reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub prefix: String,
    pub qualifier: Option<String>,
    pub slot: Slot,
    pub tables: Vec<TableRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Schema,
    Table,
    View,
    Column,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Keyword => "keyword",
            Kind::Schema => "schema",
            Kind::Table => "table",
            Kind::View => "view",
            Kind::Column => "column",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub schema: Option<String>,
    pub name: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub table: String,
    pub name: String,
    pub type_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub insert: String,
    pub kind: Kind,
    pub detail: Option<String>,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// The identifier run ending at `caret`, as typed.
pub fn prefix_at(src: &str, caret: usize) -> &str {
    let bytes = src.as_bytes();
    let end = caret.min(bytes.len());
    let mut start = end;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    src.get(start..end).unwrap_or("")
}

/// None inside a string or comment, where nothing should pop up.
pub fn site(src: &str, caret: usize, dialect: SqlDialect) -> Option<Site> {
    if caret > src.len() || !src.is_char_boundary(caret) {
        return None;
    }
    // A sentinel past the caret is swallowed only by a string or comment still open there.
    let probe = format!("{} ", &src[..caret]);
    let open = lex_chunks(&probe, dialect)
        .iter()
        .any(|c| !matches!(c, Chunk::Code(_)) && c.range().contains(&caret));
    if open {
        return None;
    }

    let prefix = prefix_at(src, caret);
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut lead = caret - prefix.len();
    let mut qualifier = None;
    if lead > 0 && src.as_bytes()[lead - 1] == b'.' {
        let before = &src[..lead - 1];
        let (name, start) = trailing_name(before, dialect)?;
        qualifier = Some(name);
        lead = start;
    }

    let span = split(src, dialect)
        .into_iter()
        .find(|s| s.range.start <= caret && caret <= s.range.end);
    let (base, stmt) = match &span {
        Some(s) => (s.range.start, &src[s.range.clone()]),
        None => (caret, ""),
    };
    let tokens: Vec<Token> = highlight(stmt, dialect)
        .into_iter()
        .filter(|t| t.kind != TokenKind::Comment)
        .map(|t| Token {
            range: t.range.start + base..t.range.end + base,
            kind: t.kind,
        })
        .collect();

    let before: Vec<&Token> = tokens.iter().filter(|t| t.range.end <= lead).collect();
    let slot = slot_after(src, &before);
    let typed = caret - prefix.len()..caret;
    let others: Vec<&Token> = tokens
        .iter()
        .filter(|t| t.range != typed || prefix.is_empty())
        .collect();
    Some(Site {
        prefix: prefix.to_string(),
        qualifier,
        slot,
        tables: table_refs(src, &others),
    })
}

fn trailing_name(before: &str, dialect: SqlDialect) -> Option<(String, usize)> {
    let tok = highlight(before, dialect).pop()?;
    if tok.range.end != before.len() || !matches!(tok.kind, TokenKind::Ident | TokenKind::Keyword) {
        return None;
    }
    Some((unquote(&before[tok.range.clone()]), tok.range.start))
}

fn word<'a>(src: &'a str, t: &Token) -> &'a str {
    &src[t.range.clone()]
}

fn is_kw(src: &str, t: &Token, set: &[&str]) -> bool {
    t.kind == TokenKind::Keyword && set.iter().any(|k| word(src, t).eq_ignore_ascii_case(k))
}

fn is_punct(src: &str, t: &Token, p: &str) -> bool {
    t.kind == TokenKind::Punct && word(src, t) == p
}

fn slot_after(src: &str, before: &[&Token]) -> Slot {
    let Some(last) = before.last() else {
        return Slot::Keyword;
    };
    if is_kw(src, last, TABLE_KEYWORDS) {
        return Slot::Table;
    }
    if is_kw(src, last, COLUMN_KEYWORDS) || last.kind == TokenKind::Operator {
        return Slot::Column;
    }
    if is_punct(src, last, "(") {
        return Slot::Column;
    }
    if is_punct(src, last, ",") {
        let mut depth = 0i32;
        for t in before.iter().rev().skip(1) {
            if is_punct(src, t, ")") {
                depth += 1;
            } else if is_punct(src, t, "(") {
                if depth == 0 {
                    return Slot::Column;
                }
                depth -= 1;
            } else if depth == 0 && t.kind == TokenKind::Keyword {
                if is_kw(src, t, &["FROM", "JOIN"]) {
                    return Slot::Table;
                }
                if is_kw(src, t, COLUMN_KEYWORDS) || is_kw(src, t, LIST_ENDS) {
                    return Slot::Column;
                }
            }
        }
        return Slot::Column;
    }
    Slot::Keyword
}

fn unquote(raw: &str) -> String {
    let b = raw.as_bytes();
    match (b.first(), b.last()) {
        (Some(b'"'), Some(b'"')) | (Some(b'`'), Some(b'`')) if raw.len() >= 2 => {
            let q = &raw[..1];
            raw[1..raw.len() - 1].replace(&format!("{q}{q}"), q)
        }
        (Some(b'['), Some(b']')) if raw.len() >= 2 => raw[1..raw.len() - 1].to_string(),
        _ => raw.to_string(),
    }
}

fn table_refs(src: &str, tokens: &[&Token]) -> Vec<TableRef> {
    let name_at = |i: usize| {
        tokens
            .get(i)
            .filter(|t| t.kind == TokenKind::Ident)
            .map(|t| unquote(word(src, t)))
    };
    let mut out: Vec<TableRef> = Vec::new();
    let mut in_list = false;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let expect = if is_kw(src, t, TABLE_KEYWORDS) {
            in_list = is_kw(src, t, &["FROM", "JOIN"]);
            true
        } else if is_punct(src, t, ",") {
            in_list
        } else {
            if is_kw(src, t, LIST_ENDS) {
                in_list = false;
            }
            false
        };
        i += 1;
        if !expect {
            continue;
        }
        let Some(mut name) = name_at(i) else {
            continue;
        };
        let mut schema = None;
        i += 1;
        while tokens.get(i).is_some_and(|t| is_punct(src, t, ".")) {
            let Some(next) = name_at(i + 1) else { break };
            schema = Some(std::mem::replace(&mut name, next));
            i += 2;
        }
        if tokens.get(i).is_some_and(|t| is_kw(src, t, &["AS"])) {
            i += 1;
        }
        let alias = name_at(i);
        if alias.is_some() {
            i += 1;
        }
        if !out
            .iter()
            .any(|r| r.name == name && r.schema == schema && r.alias == alias)
        {
            out.push(TableRef {
                schema,
                name,
                alias,
            });
        }
    }
    out
}

impl Site {
    /// The references whose columns belong in the list: the qualified one, else all of them.
    pub fn column_sources(&self) -> Vec<&TableRef> {
        if self.slot != Slot::Column {
            return Vec::new();
        }
        match &self.qualifier {
            Some(q) => self
                .tables
                .iter()
                .filter(|r| {
                    r.alias
                        .as_deref()
                        .is_some_and(|a| a.eq_ignore_ascii_case(q))
                        || (r.alias.is_none() && r.name.eq_ignore_ascii_case(q))
                })
                .collect(),
            None => self.tables.iter().collect(),
        }
    }
}

// 0 prefix, 1 word-part prefix, 2 substring, 3 in-order letters; None is no match.
fn score(label: &str, typed: &str) -> Option<u8> {
    if typed.is_empty() {
        return Some(0);
    }
    let label = label.to_lowercase();
    let typed = typed.to_lowercase();
    if label.starts_with(&typed) {
        return Some(0);
    }
    if label
        .split('_')
        .skip(1)
        .any(|part| part.starts_with(&typed))
    {
        return Some(1);
    }
    if typed.chars().count() < 2 {
        return None;
    }
    if label.contains(&typed) {
        return Some(2);
    }
    let mut rest = label.chars();
    typed
        .chars()
        .all(|c| rest.by_ref().any(|l| l == c))
        .then_some(3)
}

fn needs_quotes(name: &str, dialect: SqlDialect) -> bool {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    let folds = dialect == SqlDialect::Postgres && name.chars().any(|c| c.is_ascii_uppercase());
    !plain || folds || KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(name))
}

pub fn quote_ident(name: &str, dialect: SqlDialect) -> String {
    if !needs_quotes(name, dialect) {
        return name.to_string();
    }
    let q = if dialect == SqlDialect::Mysql {
        '`'
    } else {
        '"'
    };
    format!("{q}{}{q}", name.replace(q, &format!("{q}{q}")))
}

fn keyword_case(keyword: &str, typed: &str) -> String {
    if !typed.is_empty() && !typed.chars().any(|c| c.is_ascii_uppercase()) {
        keyword.to_ascii_lowercase()
    } else {
        keyword.to_string()
    }
}

/// Ranks what fits the slot; `columns` should already be narrowed to `site.column_sources()`.
pub fn candidates(
    site: &Site,
    dialect: SqlDialect,
    objects: &[Object],
    columns: &[Column],
) -> Vec<Candidate> {
    let mut pool: Vec<Candidate> = Vec::new();
    let object = |o: &Object| Candidate {
        label: o.name.clone(),
        insert: quote_ident(&o.name, dialect),
        kind: o.kind,
        detail: o.schema.clone(),
    };
    let in_schema = |o: &&Object| match &site.qualifier {
        Some(q) => o
            .schema
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(q)),
        None => true,
    };
    match site.slot {
        Slot::Keyword if site.qualifier.is_none() => {}
        Slot::Table | Slot::Keyword => {
            pool.extend(objects.iter().filter(in_schema).map(object));
            if site.qualifier.is_none() {
                let mut schemas: Vec<&str> =
                    objects.iter().filter_map(|o| o.schema.as_deref()).collect();
                schemas.sort_unstable();
                schemas.dedup();
                if schemas.len() > 1 {
                    pool.extend(schemas.into_iter().map(|s| Candidate {
                        label: s.to_string(),
                        insert: quote_ident(s, dialect),
                        kind: Kind::Schema,
                        detail: None,
                    }));
                }
            }
        }
        Slot::Column => {
            pool.extend(columns.iter().map(|c| Candidate {
                label: c.name.clone(),
                insert: quote_ident(&c.name, dialect),
                kind: Kind::Column,
                detail: Some(match &c.type_name {
                    Some(ty) => format!("{} · {ty}", c.table),
                    None => c.table.clone(),
                }),
            }));
            if site.qualifier.is_none() {
                pool.extend(objects.iter().map(object));
            } else if columns.is_empty() {
                pool.extend(objects.iter().filter(in_schema).map(object));
            }
        }
    }
    if site.qualifier.is_none() && site.slot != Slot::Table {
        pool.extend(KEYWORDS.iter().map(|k| Candidate {
            label: k.to_string(),
            insert: keyword_case(k, &site.prefix),
            kind: Kind::Keyword,
            detail: None,
        }));
    }

    let rank = |k: Kind| match k {
        Kind::Column => 0,
        Kind::Table | Kind::View => 1,
        Kind::Schema => 2,
        Kind::Keyword => 3,
    };
    let mut scored: Vec<(u8, Candidate)> = pool
        .into_iter()
        .filter_map(|c| score(&c.label, &site.prefix).map(|s| (s, c)))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        (sa, rank(a.kind), a.label.len(), a.label.to_lowercase()).cmp(&(
            sb,
            rank(b.kind),
            b.label.len(),
            b.label.to_lowercase(),
        ))
    });
    let mut out: Vec<Candidate> = Vec::new();
    for (_, c) in scored {
        if out.len() == MAX_CANDIDATES {
            break;
        }
        if c.insert != site.prefix && !out.iter().any(|o| o.insert == c.insert && o.kind == c.kind)
        {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: SqlDialect = SqlDialect::Postgres;

    fn at(marked: &str) -> Option<Site> {
        let caret = marked.find('|').expect("test text marks the caret with |");
        site(&marked.replace('|', ""), caret, PG)
    }

    fn table(name: &str, alias: Option<&str>) -> TableRef {
        TableRef {
            schema: None,
            name: name.to_string(),
            alias: alias.map(str::to_string),
        }
    }

    fn objects() -> Vec<Object> {
        ["users", "user_roles", "orders"]
            .iter()
            .map(|n| Object {
                schema: Some("public".to_string()),
                name: n.to_string(),
                kind: Kind::Table,
            })
            .collect()
    }

    fn cols(table: &str, names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|n| Column {
                table: table.to_string(),
                name: n.to_string(),
                type_name: None,
            })
            .collect()
    }

    fn labels(c: &[Candidate]) -> Vec<&str> {
        c.iter().map(|c| c.label.as_str()).collect()
    }

    #[test]
    fn after_from_and_join_the_slot_is_a_table() {
        assert_eq!(at("SELECT * FROM us|").unwrap().slot, Slot::Table);
        assert_eq!(at("SELECT * FROM a JOIN |").unwrap().slot, Slot::Table);
        assert_eq!(at("SELECT * FROM a, |").unwrap().slot, Slot::Table);
        assert_eq!(at("UPDATE |").unwrap().slot, Slot::Table);
        assert_eq!(at("INSERT INTO o|").unwrap().slot, Slot::Table);
    }

    #[test]
    fn select_lists_predicates_and_calls_are_column_slots() {
        assert_eq!(at("SELECT |").unwrap().slot, Slot::Column);
        assert_eq!(at("SELECT a, b|").unwrap().slot, Slot::Column);
        assert_eq!(
            at("SELECT * FROM t WHERE x = |").unwrap().slot,
            Slot::Column
        );
        assert_eq!(at("SELECT count(|").unwrap().slot, Slot::Column);
        assert_eq!(
            at("SELECT * FROM t ORDER BY a, |").unwrap().slot,
            Slot::Column
        );
    }

    #[test]
    fn after_a_finished_name_the_slot_is_a_keyword() {
        assert_eq!(at("SELECT * FROM t |").unwrap().slot, Slot::Keyword);
        assert_eq!(at("|").unwrap().slot, Slot::Keyword);
    }

    #[test]
    fn nothing_pops_up_inside_strings_or_comments() {
        assert_eq!(at("SELECT 'us|"), None);
        assert_eq!(at("SELECT 'a' -- us|"), None);
        assert_eq!(at("SELECT /* us|"), None);
        assert!(at("SELECT 'a' |").is_some());
    }

    #[test]
    fn the_prefix_and_qualifier_are_read_back_from_the_caret() {
        let s = at("SELECT u.na| FROM users u").unwrap();
        assert_eq!(s.prefix, "na");
        assert_eq!(s.qualifier.as_deref(), Some("u"));
        assert_eq!(s.slot, Slot::Column);
        let s = at("SELECT \"Weird\".| FROM t").unwrap();
        assert_eq!(s.qualifier.as_deref(), Some("Weird"));
    }

    #[test]
    fn tables_after_the_caret_still_count() {
        let s = at("SELECT | FROM users u JOIN public.orders AS o ON o.uid = u.id").unwrap();
        assert_eq!(
            s.tables,
            vec![
                table("users", Some("u")),
                TableRef {
                    schema: Some("public".to_string()),
                    name: "orders".to_string(),
                    alias: Some("o".to_string()),
                },
            ]
        );
    }

    #[test]
    fn the_name_being_typed_is_not_a_reference() {
        assert!(at("SELECT * FROM us|").unwrap().tables.is_empty());
    }

    #[test]
    fn only_the_statement_under_the_caret_is_read() {
        let s = at("SELECT * FROM a; SELECT | FROM b; SELECT * FROM c").unwrap();
        assert_eq!(s.tables, vec![table("b", None)]);
    }

    #[test]
    fn a_qualifier_narrows_the_column_sources_to_that_alias() {
        let s = at("SELECT o.| FROM users u JOIN orders o").unwrap();
        let names: Vec<&str> = s.column_sources().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["orders"]);
        let s = at("SELECT users.| FROM users").unwrap();
        assert_eq!(s.column_sources().len(), 1);
    }

    #[test]
    fn table_slot_offers_only_matching_objects() {
        let s = at("SELECT * FROM us|").unwrap();
        let c = candidates(&s, PG, &objects(), &[]);
        assert_eq!(labels(&c), ["users", "user_roles"]);
    }

    #[test]
    fn column_slot_ranks_columns_before_tables_and_keywords() {
        let s = at("SELECT na| FROM users").unwrap();
        let c = candidates(&s, PG, &objects(), &cols("users", &["name", "id"]));
        assert_eq!(c[0].label, "name");
        assert_eq!(c[0].kind, Kind::Column);
        assert!(c.iter().all(|c| c.label != "id"));
    }

    #[test]
    fn fuzzy_matches_rank_below_prefix_matches() {
        let s = at("SELECT * FROM rl|").unwrap();
        let c = candidates(&s, PG, &objects(), &[]);
        assert_eq!(labels(&c), ["user_roles"]);
        let s = at("SELECT * FROM ro|").unwrap();
        assert_eq!(labels(&candidates(&s, PG, &objects(), &[])), ["user_roles"]);
        let s = at("SELECT * FROM rs|").unwrap();
        let c = candidates(&s, PG, &objects(), &[]);
        assert_eq!(labels(&c), ["users", "orders", "user_roles"]);
    }

    #[test]
    fn keywords_follow_the_case_being_typed() {
        let s = at("SELECT * FROM t wher|").unwrap();
        let c = candidates(&s, PG, &[], &[]);
        assert_eq!(c[0].insert, "where");
        let s = at("SELECT * FROM t WHER|").unwrap();
        assert_eq!(candidates(&s, PG, &[], &[])[0].insert, "WHERE");
    }

    #[test]
    fn names_that_need_quotes_are_inserted_quoted() {
        assert_eq!(quote_ident("users", PG), "users");
        assert_eq!(quote_ident("Users", PG), "\"Users\"");
        assert_eq!(quote_ident("Users", SqlDialect::Sqlite), "Users");
        assert_eq!(quote_ident("order", SqlDialect::Mysql), "`order`");
        assert_eq!(quote_ident("a\"b", PG), "\"a\"\"b\"");
        assert_eq!(quote_ident("two words", SqlDialect::Mysql), "`two words`");
    }

    #[test]
    fn a_schema_qualifier_in_a_table_slot_lists_that_schema() {
        let mut objs = objects();
        objs.push(Object {
            schema: Some("audit".to_string()),
            name: "events".to_string(),
            kind: Kind::Table,
        });
        let s = at("SELECT * FROM audit.|").unwrap();
        assert_eq!(labels(&candidates(&s, PG, &objs, &[])), ["events"]);
    }

    #[test]
    fn a_caret_off_a_char_boundary_is_refused() {
        assert_eq!(site("SELECT é", 8, PG), None);
    }
}
