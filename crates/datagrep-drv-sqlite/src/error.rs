use datagrep_api::DbError;

pub(crate) fn map_sqlite_err(err: rusqlite::Error) -> DbError {
    use rusqlite::ffi::ErrorCode;
    use rusqlite::Error as E;

    match &err {
        E::SqliteFailure(ffi_err, msg) => {
            if ffi_err.code == ErrorCode::OperationInterrupted {
                return DbError::Cancelled;
            }
            DbError::Query {
                code: Some(format!("{:?}", ffi_err.code)),
                message: msg.clone().unwrap_or_else(|| err.to_string()),
                position: None,
            }
        }
        E::SqliteSingleThreadedMode => DbError::DriverPanic(err.to_string()),
        E::InvalidParameterName(_)
        | E::InvalidColumnIndex(_)
        | E::InvalidColumnName(_)
        | E::InvalidColumnType(..)
        | E::InvalidParameterCount(..)
        | E::InvalidPath(_) => DbError::Protocol(err.to_string()),
        E::ExecuteReturnedResults => DbError::Protocol(err.to_string()),
        E::QueryReturnedNoRows => DbError::Query {
            code: None,
            message: err.to_string(),
            position: None,
        },
        E::ToSqlConversionFailure(_) | E::FromSqlConversionFailure(..) => DbError::Query {
            code: None,
            message: err.to_string(),
            position: None,
        },
        _ => DbError::Query {
            code: None,
            message: err.to_string(),
            position: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_maps_to_cancelled() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let handle = conn.get_interrupt_handle();
        conn.progress_handler(
            1,
            Some(move || {
                handle.interrupt();
                false
            }),
        );

        let err = conn
            .query_row(
                "WITH RECURSIVE c(x) AS ( \
                     SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 1000000 \
                 ) SELECT count(*) FROM c",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect_err("interrupted query should error");
        assert!(matches!(map_sqlite_err(err), DbError::Cancelled));
    }

    #[test]
    fn generic_sqlite_failure_becomes_query_error() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let err = conn
            .execute_batch("SELECT * FROM no_such_table")
            .unwrap_err();
        match map_sqlite_err(err) {
            DbError::Query { message, .. } => assert!(message.contains("no such table")),
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
