//! Maps [`rusqlite::Error`] onto the one error type that crosses the
//! `dbx-api` seam ([`DbError`]) — dbx-api's own doc comment on `DbError` is
//! explicit that drivers must not leak their own error enums upward.

use dbx_api::DbError;

/// Translate a rusqlite error into the coarse cross-seam [`DbError`].
///
/// SQLite's `SQLITE_INTERRUPT` (raised by [`rusqlite::InterruptHandle::interrupt`],
/// see `canceller.rs`) is mapped to [`DbError::Cancelled`] rather than
/// [`DbError::Query`] — the user asked for this, it is not a server-side
/// rejection of the statement (design §3.3: "the user cancelled; not a
/// failure, and the UI must not dress it as one").
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
        // `interrupt()` only affects a statement that is *actually running*
        // at the time — calling it on an idle connection is a no-op (the
        // next statement executes normally, as opposed to erroring). Rather
        // than racing two OS threads against each other (flaky under a busy
        // CI runner — the query can finish before a delayed interrupt ever
        // lands), this calls `interrupt()` from inside a `progress_handler`
        // callback, which SQLite invokes synchronously *during* the running
        // step. That makes "an in-flight step observes the interrupt"
        // deterministic. Real cross-thread cancellation is covered
        // end-to-end by `tests/cancel.rs`.
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
