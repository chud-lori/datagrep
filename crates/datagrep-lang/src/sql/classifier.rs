use datagrep_api::SqlDialect;

use super::lexer::{lex_chunks, Chunk, QuoteKind};
use crate::StatementClass;

enum Lexeme<'a> {
    Word(&'a str),
    QuotedIdent,
    Open,
    Close,
    Comma,
    Dot,
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_word_continue(b: u8) -> bool {
    is_word_start(b) || b.is_ascii_digit()
}

fn significant_lexemes(stmt: &str) -> Vec<Lexeme<'_>> {
    let chunks = lex_chunks(stmt, SqlDialect::Postgres);
    let bytes = stmt.as_bytes();
    let mut out = Vec::new();
    for chunk in chunks {
        let range = match chunk {
            Chunk::Code(range) => range,
            Chunk::Quoted(_, QuoteKind::DoubleIdent) => {
                out.push(Lexeme::QuotedIdent);
                continue;
            }
            _ => continue,
        };
        let mut i = range.start;
        let end = range.end;
        while i < end {
            let b = bytes[i];
            if b.is_ascii_whitespace() {
                i += 1;
            } else if is_word_start(b) {
                let start = i;
                i += 1;
                while i < end && is_word_continue(bytes[i]) {
                    i += 1;
                }
                out.push(Lexeme::Word(&stmt[start..i]));
            } else {
                match b {
                    b'(' => out.push(Lexeme::Open),
                    b')' => out.push(Lexeme::Close),
                    b',' => out.push(Lexeme::Comma),
                    b'.' => out.push(Lexeme::Dot),
                    _ => {}
                }
                i += 1;
            }
        }
    }
    out
}

pub fn classify(stmt: &str) -> StatementClass {
    let toks = significant_lexemes(stmt);
    classify_from(&toks, 0).0
}

fn classify_keyword(word: &str) -> Option<StatementClass> {
    use StatementClass::*;
    const READ: &[&str] = &["SHOW", "EXPLAIN"];
    const WRITE: &[&str] = &[
        "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "REPLACE", "COPY",
    ];
    const DDL: &[&str] = &["CREATE", "ALTER", "DROP", "TRUNCATE", "COMMENT", "RENAME"];
    const TCL: &[&str] = &["BEGIN", "COMMIT", "ROLLBACK", "SAVEPOINT", "START"];
    const ADMIN: &[&str] = &["GRANT", "REVOKE", "VACUUM", "ANALYZE", "SET", "KILL"];

    let hit = |set: &[&str]| set.iter().any(|k| word.eq_ignore_ascii_case(k));
    if hit(READ) {
        Some(Read)
    } else if hit(WRITE) {
        Some(Write)
    } else if hit(DDL) {
        Some(Ddl)
    } else if hit(TCL) {
        Some(Tcl)
    } else if hit(ADMIN) {
        Some(Admin)
    } else {
        None
    }
}

fn classify_from(toks: &[Lexeme<'_>], idx: usize) -> (StatementClass, usize) {
    let Some(Lexeme::Word(first)) = toks.get(idx) else {
        return (StatementClass::Unknown, idx);
    };
    if first.eq_ignore_ascii_case("WITH") {
        return (classify_with(toks, idx + 1), toks.len());
    }
    if first.eq_ignore_ascii_case("SELECT") || first.eq_ignore_ascii_case("VALUES") {
        return (prove_read(&toks[idx..]), idx + 1);
    }
    match classify_keyword(first) {
        Some(class) => (class, idx + 1),
        None => (StatementClass::Unknown, idx + 1),
    }
}

fn classify_with(toks: &[Lexeme<'_>], mut idx: usize) -> StatementClass {
    if matches!(toks.get(idx), Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("RECURSIVE")) {
        idx += 1;
    }
    let mut body_class = StatementClass::Read;
    loop {
        // CTE name.
        match toks.get(idx) {
            Some(Lexeme::Word(_)) => idx += 1,
            _ => return StatementClass::Unknown,
        }
        // Optional column list.
        if matches!(toks.get(idx), Some(Lexeme::Open)) {
            idx = match skip_balanced(toks, idx) {
                Some(i) => i,
                None => return StatementClass::Unknown,
            };
        }
        // AS
        match toks.get(idx) {
            Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("AS") => idx += 1,
            _ => return StatementClass::Unknown,
        }
        // Optional [NOT] MATERIALIZED.
        if matches!(toks.get(idx), Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("NOT")) {
            idx += 1;
        }
        if matches!(toks.get(idx), Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("MATERIALIZED"))
        {
            idx += 1;
        }
        // CTE body.
        match toks.get(idx) {
            Some(Lexeme::Open) => {
                let end = match skip_balanced(toks, idx) {
                    Some(i) => i,
                    None => return StatementClass::Unknown,
                };
                if body_class == StatementClass::Read {
                    body_class = classify_from(&toks[idx + 1..end - 1], 0).0;
                }
                idx = end;
            }
            _ => return StatementClass::Unknown,
        }
        match toks.get(idx) {
            Some(Lexeme::Comma) => {
                idx += 1;
                continue;
            }
            _ => break,
        }
    }
    match classify_from(toks, idx).0 {
        StatementClass::Read => body_class,
        class => class,
    }
}

// Keywords and type names that take a parenthesis without calling anything.
const SYNTAX_BEFORE_PAREN: &[&str] = &[
    "AGAINST",
    "ALL",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "BETWEEN",
    "BINARY",
    "BIT",
    "BY",
    "CASE",
    "CHARACTER",
    "CUBE",
    "DECIMAL",
    "DISTINCT",
    "ELSE",
    "EXCEPT",
    "EXISTS",
    "FILTER",
    "FLOAT",
    "FROM",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INDEX",
    "INTERSECT",
    "INTERVAL",
    "IS",
    "JOIN",
    "LATERAL",
    "LIKE",
    "LIMIT",
    "MATCH",
    "MATERIALIZED",
    "NCHAR",
    "NOT",
    "NUMERIC",
    "NVARCHAR",
    "OFFSET",
    "ON",
    "OR",
    "OVER",
    "ROLLUP",
    "ROW",
    "SELECT",
    "SETS",
    "SOME",
    "THEN",
    "TIMESTAMP",
    "UNION",
    "USING",
    "VALUES",
    "VARBINARY",
    "VARCHAR",
    "VARYING",
    "WHEN",
    "WHERE",
];

// Built-ins with no side effects; grows only by review, anything absent fails the proof.
const PURE_FUNCTIONS: &[&str] = &[
    // Aggregates and window functions.
    "any_value",
    "array_agg",
    "avg",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bool_and",
    "bool_or",
    "count",
    "cume_dist",
    "dense_rank",
    "every",
    "first_value",
    "group_concat",
    "grouping",
    "json_agg",
    "json_group_array",
    "json_group_object",
    "json_object_agg",
    "jsonb_agg",
    "jsonb_object_agg",
    "lag",
    "last_value",
    "lead",
    "max",
    "min",
    "mode",
    "nth_value",
    "ntile",
    "percent_rank",
    "percentile_cont",
    "percentile_disc",
    "rank",
    "row_number",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "string_agg",
    "sum",
    "total",
    "var_pop",
    "var_samp",
    "variance",
    // Conditionals and casts.
    "cast",
    "coalesce",
    "convert",
    "greatest",
    "if",
    "ifnull",
    "iif",
    "isnull",
    "least",
    "nullif",
    "try_cast",
    // Strings.
    "ascii",
    "bit_length",
    "btrim",
    "char",
    "char_length",
    "character_length",
    "chr",
    "concat",
    "concat_ws",
    "decode",
    "encode",
    "format",
    "hex",
    "initcap",
    "instr",
    "left",
    "length",
    "locate",
    "lower",
    "lpad",
    "ltrim",
    "md5",
    "octet_length",
    "overlay",
    "position",
    "quote_ident",
    "quote_literal",
    "regexp_like",
    "regexp_match",
    "regexp_matches",
    "regexp_replace",
    "regexp_substr",
    "repeat",
    "replace",
    "reverse",
    "right",
    "rpad",
    "rtrim",
    "sha1",
    "sha2",
    "soundex",
    "split_part",
    "starts_with",
    "strpos",
    "substr",
    "substring",
    "to_hex",
    "translate",
    "trim",
    "unhex",
    "upper",
    // Numbers.
    "abs",
    "ceil",
    "ceiling",
    "cos",
    "degrees",
    "div",
    "exp",
    "floor",
    "ln",
    "log",
    "log10",
    "mod",
    "pi",
    "pow",
    "power",
    "radians",
    "rand",
    "random",
    "round",
    "sign",
    "sin",
    "sqrt",
    "tan",
    "trunc",
    "truncate",
    "width_bucket",
    // Dates and times.
    "age",
    "clock_timestamp",
    "convert_tz",
    "curdate",
    "current_date",
    "current_time",
    "current_timestamp",
    "curtime",
    "date",
    "date_add",
    "date_format",
    "date_part",
    "date_sub",
    "date_trunc",
    "datediff",
    "datetime",
    "day",
    "dayofmonth",
    "dayofweek",
    "dayofyear",
    "extract",
    "from_unixtime",
    "hour",
    "julianday",
    "last_day",
    "localtime",
    "localtimestamp",
    "make_date",
    "make_interval",
    "make_timestamp",
    "minute",
    "month",
    "now",
    "quarter",
    "second",
    "statement_timestamp",
    "str_to_date",
    "strftime",
    "sysdate",
    "time",
    "timestampadd",
    "timestampdiff",
    "timezone",
    "to_char",
    "to_date",
    "to_number",
    "to_timestamp",
    "transaction_timestamp",
    "unix_timestamp",
    "unixepoch",
    "utc_timestamp",
    "week",
    "weekday",
    "year",
    // JSON.
    "json_array",
    "json_array_elements",
    "json_array_elements_text",
    "json_array_length",
    "json_build_array",
    "json_build_object",
    "json_contains",
    "json_each",
    "json_each_text",
    "json_extract",
    "json_extract_path",
    "json_extract_path_text",
    "json_keys",
    "json_length",
    "json_object",
    "json_object_keys",
    "json_type",
    "json_typeof",
    "json_unquote",
    "json_valid",
    "json_value",
    "jsonb_array_elements",
    "jsonb_array_elements_text",
    "jsonb_array_length",
    "jsonb_build_array",
    "jsonb_build_object",
    "jsonb_each",
    "jsonb_each_text",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_object_keys",
    "jsonb_path_query",
    "jsonb_pretty",
    "jsonb_set",
    "jsonb_typeof",
    "row_to_json",
    "to_json",
    "to_jsonb",
    // Arrays, sets and misc.
    "array_append",
    "array_cat",
    "array_length",
    "array_lower",
    "array_position",
    "array_remove",
    "array_to_string",
    "array_upper",
    "cardinality",
    "current_database",
    "current_schema",
    "gen_random_uuid",
    "generate_series",
    "string_to_array",
    "typeof",
    "unnest",
];

// SELECT/VALUES is Read only when proven: INTO or a locking clause is a Write, an unvetted call is Unknown.
fn prove_read(toks: &[Lexeme<'_>]) -> StatementClass {
    let word_at = |i: usize| match toks.get(i) {
        Some(Lexeme::Word(w)) => Some(*w),
        _ => None,
    };
    let is = |w: Option<&str>, k: &str| w.is_some_and(|w| w.eq_ignore_ascii_case(k));
    let listed = |set: &[&str], w: &str| set.iter().any(|k| w.eq_ignore_ascii_case(k));

    let mut class = StatementClass::Read;
    for (i, tok) in toks.iter().enumerate() {
        let word = word_at(i);
        let next = word_at(i + 1);
        if is(word, "INTO")
            || (is(word, "FOR") && ["UPDATE", "SHARE", "NO", "KEY"].iter().any(|k| is(next, k)))
            || (is(word, "LOCK") && is(next, "IN"))
        {
            return StatementClass::Write;
        }
        if !matches!(tok, Lexeme::Word(_) | Lexeme::QuotedIdent)
            || !matches!(toks.get(i + 1), Some(Lexeme::Open))
        {
            continue;
        }
        let prev = i.checked_sub(1).and_then(|p| toks.get(p));
        // `AS alias(col, ...)` names columns, it calls nothing.
        if matches!(prev, Some(Lexeme::Word(w)) if w.eq_ignore_ascii_case("AS")) {
            continue;
        }
        let vetted = !matches!(prev, Some(Lexeme::Dot))
            && word.is_some_and(|w| listed(SYNTAX_BEFORE_PAREN, w) || listed(PURE_FUNCTIONS, w));
        if !vetted {
            class = StatementClass::Unknown;
        }
    }
    class
}

fn skip_balanced(toks: &[Lexeme<'_>], idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = idx;
    loop {
        match toks.get(i)? {
            Lexeme::Open => depth += 1,
            Lexeme::Close => depth -= 1,
            _ => {}
        }
        i += 1;
        if depth == 0 {
            return Some(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementClass::*;

    #[test]
    fn basic_dml_and_ddl_and_tcl_and_admin() {
        assert_eq!(classify("SELECT * FROM t"), Read);
        assert_eq!(classify("VALUES (1)"), Read);
        assert_eq!(classify("SHOW TABLES"), Read);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), Write);
        assert_eq!(classify("UPDATE t SET x=1"), Write);
        assert_eq!(classify("DELETE FROM t"), Write);
        assert_eq!(
            classify("MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE"),
            Write
        );
        assert_eq!(classify("COPY t FROM stdin"), Write);
        assert_eq!(classify("CREATE TABLE t (x int)"), Ddl);
        assert_eq!(classify("ALTER TABLE t ADD COLUMN y int"), Ddl);
        assert_eq!(classify("DROP TABLE t"), Ddl);
        assert_eq!(classify("TRUNCATE t"), Ddl);
        assert_eq!(classify("BEGIN"), Tcl);
        assert_eq!(classify("COMMIT"), Tcl);
        assert_eq!(classify("ROLLBACK"), Tcl);
        assert_eq!(classify("GRANT SELECT ON t TO u"), Admin);
        assert_eq!(classify("VACUUM t"), Admin);
        assert_eq!(classify("SET search_path = public"), Admin);
        assert_eq!(classify("frobnicate everything"), Unknown);
        assert_eq!(classify(""), Unknown);
    }

    #[test]
    fn dml_with_returning_is_still_write() {
        assert_eq!(
            classify("INSERT INTO t (v) VALUES ('a') RETURNING id"),
            Write
        );
        assert_eq!(classify("UPDATE t SET v = 'b' RETURNING id, v"), Write);
        assert_eq!(classify("DELETE FROM t WHERE id = 1 RETURNING *"), Write);
        assert_eq!(
            classify("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x RETURNING id"),
            Write
        );
    }

    #[test]
    fn explain_variants_are_always_read() {
        assert_eq!(classify("EXPLAIN SELECT 1"), Read);
        assert_eq!(classify("EXPLAIN ANALYZE SELECT 1"), Read);
        assert_eq!(classify("EXPLAIN (FORMAT JSON) SELECT 1"), Read);
        assert_eq!(classify("explain select 1"), Read);
    }

    #[test]
    fn leading_comments_and_whitespace_are_skipped() {
        assert_eq!(classify("  \n-- a comment\n/* another */  SELECT 1"), Read);
        assert_eq!(classify("-- @limit 10\nINSERT INTO t VALUES (1)"), Write);
    }

    #[test]
    fn with_select_is_read() {
        assert_eq!(classify("WITH cte AS (SELECT 1) SELECT * FROM cte"), Read);
        assert_eq!(
            classify("WITH RECURSIVE cte AS (SELECT 1) SELECT * FROM cte"),
            Read
        );
    }

    #[test]
    fn with_insert_is_write_not_read() {
        assert_eq!(
            classify("WITH cte AS (SELECT 1) INSERT INTO t SELECT * FROM cte"),
            Write
        );
    }

    #[test]
    fn with_multiple_ctes_including_nested_parens() {
        let stmt = "WITH a AS (SELECT (1 + (2 * 3)) AS x), b AS (SELECT * FROM a) \
                    DELETE FROM t USING b WHERE t.id = b.x";
        assert_eq!(classify(stmt), Write);
    }

    #[test]
    fn with_cte_column_list_and_materialized() {
        let stmt = "WITH a (x, y) AS MATERIALIZED (SELECT 1, 2) UPDATE t SET x = 1";
        assert_eq!(classify(stmt), Write);
    }

    #[test]
    fn with_malformed_falls_back_to_unknown_not_panic() {
        assert_eq!(classify("WITH"), Unknown);
        assert_eq!(classify("WITH a AS"), Unknown);
        assert_eq!(classify("WITH a AS ("), Unknown);
    }

    #[test]
    fn semicolons_and_parens_inside_strings_do_not_confuse_classification() {
        assert_eq!(classify("SELECT '(WITH INSERT'"), Read);
    }

    #[test]
    fn select_that_writes_or_locks_is_write() {
        for stmt in [
            "SELECT * INTO backup_users FROM users",
            "SELECT * FROM users INTO OUTFILE '/tmp/users.csv'",
            "SELECT * FROM users INTO DUMPFILE '/tmp/users.bin'",
            "SELECT id INTO @last FROM users LIMIT 1",
            "SELECT * FROM accounts FOR UPDATE",
            "SELECT * FROM accounts FOR SHARE",
            "SELECT * FROM accounts FOR NO KEY UPDATE",
            "select * from accounts for key share",
            "SELECT * FROM accounts LOCK IN SHARE MODE",
            "SELECT pg_sleep(1) INTO t",
        ] {
            assert_eq!(classify(stmt), Write, "{stmt}");
        }
    }

    #[test]
    fn select_calling_an_unvetted_function_is_not_read() {
        for stmt in [
            "SELECT pg_terminate_backend(4242)",
            "SELECT setval('orders_id_seq', 1)",
            "SELECT nextval('orders_id_seq')",
            "SELECT pg_sleep(3600)",
            "SELECT GET_LOCK('deploy', 0)",
            "SELECT lo_unlink(16401)",
            "SELECT pg_advisory_lock(1)",
            "SELECT load_file('/etc/passwd')",
            "SELECT * FROM dblink('host=x', 'select 1') AS t(a int)",
            "SELECT my_udf(id) FROM users",
            "SELECT lower (name), sleep (5) FROM users",
            "SELECT evil.lower(name) FROM users",
            "SELECT \"pg_sleep\"(1)",
            "SELECT * FROM users WHERE id IN (SELECT pg_sleep(1))",
            "VALUES (pg_sleep(1))",
            "WITH a AS (SELECT pg_sleep(1)) SELECT * FROM a",
            "WITH a AS (SELECT 1) SELECT pg_sleep(1) FROM a",
        ] {
            assert_eq!(classify(stmt), Unknown, "{stmt}");
        }
    }

    #[test]
    fn with_data_modifying_cte_is_not_read() {
        assert_eq!(
            classify("WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone"),
            Write
        );
        assert_eq!(
            classify("WITH a AS (SELECT 1), b AS (SELECT * INTO x FROM a) SELECT * FROM b"),
            Write
        );
    }

    #[test]
    fn common_reads_are_proven() {
        for stmt in [
            "SELECT u.id, o.total FROM users u JOIN orders o ON (o.user_id = u.id)",
            "SELECT * FROM a LEFT JOIN b USING (id)",
            "SELECT * FROM (SELECT id FROM users) AS s",
            "SELECT * FROM users WHERE id IN (1, 2, 3)",
            "SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.uid = u.id)",
            "SELECT * FROM users WHERE NOT EXISTS (SELECT 1) AND (a = 1 OR b = 2)",
            "SELECT COUNT(*), count(DISTINCT id), SUM(total), AVG(total) FROM orders",
            "SELECT count(*) FILTER (WHERE paid) FROM orders",
            "SELECT id, row_number() OVER (PARTITION BY uid ORDER BY ts) FROM orders",
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY total) FROM orders",
            "SELECT CAST(x AS int), CAST(y AS varchar(10)), z::numeric(10, 2) FROM t",
            "SELECT coalesce(nullif(trim(name), ''), 'n/a'), upper(concat(a, b)) FROM t",
            "SELECT date_trunc('day', now()), extract(YEAR FROM ts) FROM t",
            "SELECT substring(name FROM 1 FOR 3), round(abs(x), 2) FROM t",
            "SELECT string_agg(name, ','), array_agg(id), json_agg(t) FROM t GROUP BY g",
            "SELECT data->>'k', jsonb_extract_path_text(data, 'k') FROM t",
            "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(n, s)",
            "SELECT * FROM t WHERE x = ANY (ARRAY[1, 2])",
            "SELECT a FROM t UNION (SELECT b FROM u)",
            "SELECT * FROM t WHERE note = 'copy into archive for update'",
            "SELECT * FROM t -- for update\nWHERE id = 1",
            "SELECT /* into outfile */ 1",
            "SELECT \"into\" FROM t",
            "VALUES (1), (2)",
            "WITH cte AS (SELECT id FROM users) SELECT count(*) FROM cte",
            "WITH a (x, y) AS (SELECT 1, 2) SELECT x FROM a",
        ] {
            assert_eq!(classify(stmt), Read, "{stmt}");
        }
    }
}
