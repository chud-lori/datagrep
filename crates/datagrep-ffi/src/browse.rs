use std::ffi::c_char;

use crate::ffi_util::{cstr, guard, parse_path_json, to_c_string};

/// How many rows a sidebar click asks for, as a directive the user can edit.
const BROWSE_LIMIT: u32 = 500;

/// # Safety
/// String arguments are NULL or NUL-terminated; `err_out` is NULL or a writable slot.
#[no_mangle]
pub unsafe extern "C" fn datagrep_browse_statement(
    driver_id: *const c_char,
    path_json: *const c_char,
    database: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut c_char {
    guard(
        err_out,
        std::ptr::null_mut(),
        "datagrep_browse_statement",
        || {
            // SAFETY: NUL-terminated strings per the contract; cstr rejects NULL and non-UTF-8 before any deref.
            let driver_id = unsafe { cstr(driver_id, "driver_id") }?;
            let path = parse_path_json(unsafe { cstr(path_json, "path_json") }?)?;
            let database = if database.is_null() {
                None
            } else {
                // SAFETY: non-NULL (checked) and NUL-terminated per the contract.
                Some(unsafe { cstr(database, "database") }?)
            };
            Ok(to_c_string(browse_statement(driver_id, &path, database)?))
        },
    )
}

/// The statement a click on one catalog object should run, in that engine's own
/// language — including the directive line, whose comment marker is the
/// language's own.
pub fn browse_statement(
    driver_id: &str,
    path: &[String],
    database: Option<&str>,
) -> Result<String, String> {
    match driver_id {
        "sqlite" => match path {
            [schema, table] => Ok(sql(&format!(
                "{}.{}",
                double_quoted(schema),
                double_quoted(table)
            ))),
            _ => Err(shape(driver_id, "[database, table]", path)),
        },
        "postgres" => match path {
            [db, schema, table] => {
                same_database(db, database, "PostgreSQL cannot query across databases")?;
                Ok(sql(&format!(
                    "{}.{}",
                    double_quoted(schema),
                    double_quoted(table)
                )))
            }
            _ => Err(shape(driver_id, "[database, schema, table]", path)),
        },
        "mysql" => match path {
            [db, table] => Ok(sql(&format!(
                "{}.{}",
                back_quoted(db),
                back_quoted(table)
            ))),
            _ => Err(shape(driver_id, "[database, table]", path)),
        },
        "mongodb" => match path {
            [collection] => Ok(mongo(collection)),
            [db, collection] => {
                same_database(
                    db,
                    database,
                    "a MongoShell statement names a collection, not a database",
                )?;
                Ok(mongo(collection))
            }
            _ => Err(shape(driver_id, "[database, collection]", path)),
        },
        "elasticsearch" => match path {
            [index] => Ok(format!(
                "# @limit {BROWSE_LIMIT}\nGET /{index}/_search\n{{ \"query\": {{ \"match_all\": {{}} }} }}"
            )),
            _ => Err(shape(driver_id, "[index]", path)),
        },
        // A Redis value's shape decides its command, so a key is not browsable from its path alone.
        "redis" => Err(
            "datagrep does not browse a Redis key from the sidebar — GET, HGETALL and LRANGE \
             read different shapes, so type the one this key needs"
                .to_string(),
        ),
        other => Err(format!("`{other}` has no browse statement")),
    }
}

fn sql(object: &str) -> String {
    format!("-- @limit {BROWSE_LIMIT}\nSELECT * FROM {object};")
}

fn mongo(collection: &str) -> String {
    let addressed = if is_plain_identifier(collection) {
        format!("db.{collection}")
    } else {
        format!("db.getCollection({})", json_string(collection))
    };
    format!("# @limit {BROWSE_LIMIT}\n{addressed}.find({{}})")
}

/// The catalog can list databases a statement cannot reach.
fn same_database(named: &str, open: Option<&str>, why: &str) -> Result<(), String> {
    match open {
        Some(open) if open == named => Ok(()),
        Some(open) => Err(format!(
            "`{named}` is not the database this connection is open on (`{open}`), and {why} — \
             open a connection to `{named}` to browse it"
        )),
        None => Err(format!(
            "this connection does not name a database, and {why} — say which database the \
             connection opens to browse `{named}`"
        )),
    }
}

fn shape(driver_id: &str, expected: &str, path: &[String]) -> String {
    format!(
        "`{driver_id}` browses an object at {expected}, not `{}`",
        path.join("/")
    )
}

fn double_quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn back_quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

fn is_plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_sql_dialect_quotes_with_its_own_marks() {
        assert_eq!(
            browse_statement("sqlite", &path(&["main", "users"]), None).unwrap(),
            "-- @limit 500\nSELECT * FROM \"main\".\"users\";"
        );
        assert_eq!(
            browse_statement("mysql", &path(&["shop", "orders"]), None).unwrap(),
            "-- @limit 500\nSELECT * FROM `shop`.`orders`;"
        );
        assert_eq!(
            browse_statement(
                "postgres",
                &path(&["shop", "public", "users"]),
                Some("shop")
            )
            .unwrap(),
            "-- @limit 500\nSELECT * FROM \"public\".\"users\";"
        );
    }

    #[test]
    fn a_name_that_needs_escaping_is_escaped_not_rejected() {
        assert_eq!(
            browse_statement("sqlite", &path(&["main", "we\"ird"]), None).unwrap(),
            "-- @limit 500\nSELECT * FROM \"main\".\"we\"\"ird\";"
        );
        assert_eq!(
            browse_statement("mysql", &path(&["db", "we`ird"]), None).unwrap(),
            "-- @limit 500\nSELECT * FROM `db`.`we``ird`;"
        );
        assert_eq!(
            browse_statement("mongodb", &path(&["shop", "we ird"]), Some("shop")).unwrap(),
            "# @limit 500\ndb.getCollection(\"we ird\").find({})"
        );
    }

    #[test]
    fn mongo_addresses_a_plain_name_the_way_a_person_would_type_it() {
        assert_eq!(
            browse_statement("mongodb", &path(&["shop", "orders"]), Some("shop")).unwrap(),
            "# @limit 500\ndb.orders.find({})"
        );
        assert_eq!(
            browse_statement("mongodb", &path(&["orders"]), None).unwrap(),
            "# @limit 500\ndb.orders.find({})"
        );
    }

    #[test]
    fn elasticsearch_asks_for_hits_not_rows() {
        assert_eq!(
            browse_statement("elasticsearch", &path(&["events"]), None).unwrap(),
            "# @limit 500\nGET /events/_search\n{ \"query\": { \"match_all\": {} } }"
        );
    }

    #[test]
    fn a_database_the_statement_cannot_reach_is_refused_by_name() {
        let err =
            browse_statement("mongodb", &path(&["other", "orders"]), Some("shop")).unwrap_err();
        assert!(err.contains("`other`"), "{err}");
        assert!(err.contains("`shop`"), "{err}");

        let err = browse_statement("mongodb", &path(&["other", "orders"]), None).unwrap_err();
        assert!(err.contains("does not name a database"), "{err}");

        let err = browse_statement("postgres", &path(&["other", "public", "t"]), Some("shop"))
            .unwrap_err();
        assert!(err.contains("across databases"), "{err}");
    }

    #[test]
    fn a_path_that_does_not_name_an_object_says_what_was_expected() {
        let err = browse_statement("postgres", &path(&["shop", "public"]), None).unwrap_err();
        assert!(err.contains("[database, schema, table]"), "{err}");
        let err = browse_statement("elasticsearch", &path(&[]), None).unwrap_err();
        assert!(err.contains("[index]"), "{err}");
    }

    #[test]
    fn redis_and_unknown_drivers_refuse_rather_than_guess() {
        assert!(browse_statement("redis", &path(&["0", "user:1"]), None).is_err());
        assert!(browse_statement("duckdb", &path(&["main", "t"]), None).is_err());
    }

    #[test]
    fn every_driver_with_a_language_either_browses_or_says_why_not() {
        for id in [
            "sqlite",
            "postgres",
            "mysql",
            "mongodb",
            "elasticsearch",
            "redis",
        ] {
            assert!(
                crate::query::language_for_driver(id).is_some(),
                "{id} lost its language"
            );
            let out = browse_statement(id, &path(&["a", "b"]), Some("a"));
            assert!(
                out.is_ok() || out.unwrap_err().len() > 20,
                "{id} refused without a reason"
            );
        }
    }
}
