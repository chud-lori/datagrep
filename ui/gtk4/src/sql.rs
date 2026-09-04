use std::ops::Range;

/// The statement asked for, plus the ORDER BY/WHERE the grid's headers and cells added.
#[derive(Debug, Clone, Default)]
pub struct Derived {
    base: String,
    driver: String,
    sort: Option<(String, bool)>,
    filters: Vec<(String, String)>,
}

impl Derived {
    /// A new question: the old ORDER BY may name a column this result has not got.
    pub fn ask(&mut self, sql: &str, driver: &str) {
        self.base = sql.to_owned();
        self.driver = driver.to_owned();
        self.sort = None;
        self.filters.clear();
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The engine the statement was asked under, which the sidebar may since have left.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    pub fn sort_by(&mut self, column: &str, ascending: bool) {
        self.sort = Some((column.to_owned(), ascending));
    }

    pub fn filter(&mut self, column: &str, value: &str) {
        self.filters.retain(|(c, _)| c != column);
        self.filters.push((column.to_owned(), value.to_owned()));
    }

    pub fn clear(&mut self) {
        self.sort = None;
        self.filters.clear();
    }

    pub fn is_derived(&self) -> bool {
        self.sort.is_some() || !self.filters.is_empty()
    }

    /// One level of wrapping only — sorting twice re-wraps the base, never nests.
    pub fn sql(&self) -> String {
        if !self.is_derived() {
            return self.base.clone();
        }
        let inner = self.base.trim().trim_end_matches(';').trim_end();
        let mut sql = format!("SELECT * FROM (\n{inner}\n) AS datagrep_result");
        if !self.filters.is_empty() {
            let clauses: Vec<String> = self
                .filters
                .iter()
                .map(|(column, value)| self.predicate(column, value))
                .collect();
            sql.push_str("\nWHERE (");
            sql.push_str(&clauses.join(") AND ("));
            sql.push(')');
        }
        if let Some((column, ascending)) = &self.sort {
            sql.push_str("\nORDER BY ");
            sql.push_str(&self.quote(column));
            sql.push_str(if *ascending { " ASC" } else { " DESC" });
        }
        sql
    }

    fn predicate(&self, column: &str, value: &str) -> String {
        let ident = self.quote(column);
        if value.is_empty() {
            format!("{ident} IS NULL OR {ident} = ''")
        } else {
            format!("{ident} = '{}'", value.replace('\'', "''"))
        }
    }

    // MySQL takes double-quoted identifiers only under ANSI_QUOTES, which datagrep does not set.
    fn quote(&self, name: &str) -> String {
        if self.driver.contains("mysql") {
            format!("`{}`", name.replace('`', "``"))
        } else {
            format!("\"{}\"", name.replace('"', "\"\""))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(sql: &str, driver: &str) -> Derived {
        let mut derived = Derived::default();
        derived.ask(sql, driver);
        derived
    }

    #[test]
    fn an_underived_statement_is_sent_exactly_as_typed() {
        let derived = asked("SELECT * FROM users;", "postgres");
        assert_eq!(derived.sql(), "SELECT * FROM users;");
        assert!(!derived.is_derived());
    }

    #[test]
    fn a_header_click_pushes_order_by_to_the_engine() {
        let mut derived = asked("SELECT * FROM users", "postgres");
        derived.sort_by("created at", true);
        assert_eq!(
            derived.sql(),
            "SELECT * FROM (\nSELECT * FROM users\n) AS datagrep_result\nORDER BY \"created at\" ASC"
        );
    }

    #[test]
    fn sorting_twice_re_wraps_the_base_rather_than_nesting_a_second_subquery() {
        let mut derived = asked("SELECT * FROM users", "postgres");
        derived.sort_by("id", true);
        derived.sort_by("id", false);
        assert_eq!(derived.sql().matches("datagrep_result").count(), 1);
        assert!(derived.sql().ends_with("ORDER BY \"id\" DESC"));
    }

    #[test]
    fn a_new_question_drops_the_previous_order_by() {
        let mut derived = asked("SELECT * FROM users", "postgres");
        derived.sort_by("id", true);
        derived.ask("SELECT * FROM orders", "postgres");
        assert!(!derived.is_derived());
        assert_eq!(derived.sql(), "SELECT * FROM orders");
    }

    #[test]
    fn the_trailing_semicolon_is_stripped_before_wrapping() {
        let mut derived = asked("SELECT 1;  ", "postgres");
        derived.sort_by("a", true);
        assert!(derived.sql().starts_with("SELECT * FROM (\nSELECT 1\n)"));
    }

    #[test]
    fn mysql_identifiers_are_backquoted_because_ansi_quotes_is_not_set() {
        let mut derived = asked("SELECT * FROM t", "mysql");
        derived.sort_by("or`der", true);
        assert!(derived.sql().ends_with("ORDER BY `or``der` ASC"));
    }

    #[test]
    fn a_filter_value_cannot_close_its_own_quote() {
        let mut derived = asked("SELECT * FROM t", "postgres");
        derived.filter("name", "O'Brien");
        assert!(derived.sql().contains("\"name\" = 'O''Brien'"));
    }

    #[test]
    fn filtering_by_an_empty_cell_matches_null_as_well_as_the_empty_string() {
        let mut derived = asked("SELECT * FROM t", "postgres");
        derived.filter("note", "");
        assert!(derived
            .sql()
            .contains("WHERE (\"note\" IS NULL OR \"note\" = '')"));
    }

    #[test]
    fn re_filtering_a_column_replaces_its_predicate_instead_of_stacking_one() {
        let mut derived = asked("SELECT * FROM t", "postgres");
        derived.filter("state", "new");
        derived.filter("state", "done");
        assert_eq!(derived.sql().matches("\"state\"").count(), 1);
        assert!(derived.sql().contains("'done'"));
    }
}

/// The four block directives — the entire meta-language datagrep adds to SQL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directives {
    pub limit: Option<i64>,
    pub timeout: Option<String>,
    pub connection: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub text: String,
    /// Char offsets into the source — the unit `GtkTextIter` counts in.
    pub range: Range<usize>,
    pub directives: Directives,
}

/// `-- @connection` beats the tab binding beats the window connection — text the user wrote outranks a picker.
pub fn effective_connection<'a>(
    directive: Option<&'a str>,
    binding: Option<&'a str>,
    window: Option<&'a str>,
) -> Option<&'a str> {
    let nonempty = |s: Option<&'a str>| s.filter(|s| !s.is_empty());
    nonempty(directive)
        .or_else(|| nonempty(binding))
        .or_else(|| nonempty(window))
}

/// Top-level `;` split honouring '…', "…", $tag$…$tag$, `--`, `/*…*/` — the DatagrepKit.SQLBlocks rules.
pub fn split(source: &str) -> Vec<Block> {
    let chars: Vec<char> = source.chars().collect();
    let mut blocks = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    let at = |idx: usize, a: char, b: char| -> bool {
        idx + 1 < chars.len() && chars[idx] == a && chars[idx + 1] == b
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let closer = c;
            i += 1;
            while i < chars.len() {
                if chars[i] == closer {
                    if i + 1 < chars.len() && chars[i + 1] == closer {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if at(i, '-', '-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if at(i, '/', '*') {
            i += 2;
            while i < chars.len() && !at(i, '*', '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        if c == '$' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '$' && chars[j] != '\n' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '$' {
                let tag = &chars[i..=j];
                let mut k = j + 1;
                while k + tag.len() <= chars.len() {
                    if &chars[k..k + tag.len()] == tag {
                        k += tag.len();
                        break;
                    }
                    k += 1;
                }
                i = k.min(chars.len());
                continue;
            }
        }
        if c == ';' {
            blocks.push(make_block(&chars, start, i + 1));
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    if start < chars.len() {
        blocks.push(make_block(&chars, start, chars.len()));
    }
    blocks.retain(|b| !b.text.trim().is_empty());
    blocks
}

fn make_block(chars: &[char], lo: usize, hi: usize) -> Block {
    let text: String = chars[lo..hi].iter().collect();
    let directives = directives(&text);
    Block {
        text,
        range: lo..hi,
        directives,
    }
}

/// The block containing the caret, else the last one starting before it, else the first.
pub fn block_at(source: &str, caret: usize) -> Option<Block> {
    let blocks = split(source);
    if let Some(b) = blocks.iter().find(|b| b.range.contains(&caret)) {
        return Some(b.clone());
    }
    if let Some(b) = blocks.iter().rev().find(|b| b.range.start <= caret) {
        return Some(b.clone());
    }
    blocks.into_iter().next()
}

pub fn directives(text: &str) -> Directives {
    let mut d = Directives::default();
    for raw_line in text.split('\n') {
        let line = raw_line.trim();
        let Some(body) = line.strip_prefix("--").map(str::trim) else {
            continue;
        };
        let Some(body) = body.strip_prefix('@') else {
            continue;
        };
        let mut parts = body.splitn(2, ' ');
        let key = parts.next().unwrap_or("").trim().to_lowercase();
        let value = parts.next().unwrap_or("").trim();
        match key.as_str() {
            "limit" => d.limit = value.parse().ok(),
            "timeout" if !value.is_empty() => d.timeout = Some(value.to_string()),
            "connection" if !value.is_empty() => d.connection = Some(value.to_string()),
            "readonly" => d.read_only = true,
            _ => {}
        }
    }
    d
}

/// A fat-finger guardrail for `-- @readonly`, not an adversary defence.
pub fn is_write_statement(sql: &str) -> bool {
    let mut s = sql.trim();
    while let Some(rest) = s.strip_prefix("--") {
        s = rest
            .split_once('\n')
            .map(|(_, tail)| tail)
            .unwrap_or("")
            .trim();
    }
    let head: String = s
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    [
        "insert", "update", "delete", "drop", "truncate", "alter", "create", "grant", "revoke",
        "replace", "merge", "vacuum", "call", "copy",
    ]
    .contains(&head.as_str())
}

#[cfg(test)]
mod block_tests {
    use super::*;

    #[test]
    fn directive_beats_binding_beats_window() {
        assert_eq!(
            effective_connection(Some("prod"), Some("staging"), Some("dev")),
            Some("prod")
        );
        assert_eq!(
            effective_connection(None, Some("staging"), Some("dev")),
            Some("staging")
        );
        assert_eq!(effective_connection(None, None, Some("dev")), Some("dev"));
        assert_eq!(effective_connection(Some(""), Some(""), None), None);
    }

    #[test]
    fn splits_on_top_level_semicolons_only() {
        let src = "select ';' from a; select \"x;y\" from b; -- trailing; comment\nselect 1";
        let blocks = split(src);
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0].text.contains("from a"));
        assert!(blocks[2].text.contains("select 1"));
    }

    #[test]
    fn dollar_quoted_bodies_stay_one_block() {
        let src = "create function f() as $fn$ begin; end; $fn$ language plpgsql; select 2";
        let blocks = split(src);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].text.contains("$fn$"));
    }

    #[test]
    fn block_comment_hides_semicolons() {
        let blocks = split("select 1 /* a; b; */ from t; select 2");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn caret_falls_back_to_the_block_before_it() {
        let src = "select 1;\n\nselect 2;";
        let b = block_at(src, 4).unwrap();
        assert!(b.text.contains("select 1"));
        let b = block_at(src, src.chars().count()).unwrap();
        assert!(b.text.contains("select 2"));
        assert!(block_at("  ", 0).is_none());
    }

    #[test]
    fn directives_parse_from_comment_lines() {
        let d = directives("-- @limit 200\n-- @connection staging\n-- @readonly\nselect 1");
        assert_eq!(d.limit, Some(200));
        assert_eq!(d.connection.as_deref(), Some("staging"));
        assert!(d.read_only);
        assert_eq!(d.timeout, None);
    }

    #[test]
    fn write_detection_skips_leading_comments() {
        assert!(is_write_statement("-- note\nDELETE from t"));
        assert!(!is_write_statement("-- delete?\nselect 1"));
    }

    #[test]
    fn ranges_are_char_offsets() {
        let src = "séléct 1; select 2";
        let blocks = split(src);
        assert_eq!(blocks[1].range.start, 9);
        assert_eq!(blocks[1].range.end, src.chars().count());
    }
}
