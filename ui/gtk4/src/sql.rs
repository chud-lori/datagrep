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
