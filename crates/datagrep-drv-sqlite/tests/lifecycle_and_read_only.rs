//! Worker-thread lifecycle (design §3.4: one dedicated thread per
//! connection, cleanly joined on close — no leak) and the
//! `PRAGMA query_only` read-only guardrail (design §3.8 layer 1).

mod common;

use std::time::Duration;

use datagrep_api::{DbError, Enforcement, Request};

/// `Result::expect_err`/`unwrap_err` require `T: Debug`, but `T` here is
/// `Box<dyn Cursor>`, which isn't `Debug` (design: trait objects across the
/// seam don't carry it). This is the non-generic equivalent.
fn expect_err<T>(result: Result<T, DbError>, msg: &str) -> DbError {
    match result {
        Ok(_) => panic!("{msg}"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn worker_thread_shuts_down_cleanly_on_close() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("SELECT 1"))
        .await
        .expect("warm-up query failed");

    // `SqliteConnection::close` joins the worker thread internally via
    // `spawn_blocking`; bounding it with a timeout turns "the thread never
    // exits" (a leak) into a failing test instead of a hang.
    let closed = tokio::time::timeout(Duration::from_secs(5), conn.close()).await;
    assert!(
        closed.is_ok(),
        "close() did not return within 5s — the worker thread likely leaked"
    );
    closed
        .expect("timeout")
        .expect("close() itself returned an error");

    // Idempotent per the trait doc.
    conn.close()
        .await
        .expect("second close() must be a harmless no-op");

    // "After this every call returns Closed."
    let result = conn.execute(Request::native("SELECT 1")).await;
    let err = expect_err(result, "execute after close should fail");
    assert!(matches!(err, DbError::Closed), "got {err:?}");
}

#[tokio::test]
async fn read_only_pragma_enforces_server_side_and_reads_still_work() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(id INTEGER PRIMARY KEY)"))
        .await
        .expect("create table failed");

    let enforcement = conn
        .set_read_only(true)
        .await
        .expect("set_read_only failed");
    assert_eq!(
        enforcement,
        Enforcement::Server,
        "PRAGMA query_only is enforced by SQLite itself, not just our client classifier"
    );

    let result = conn
        .execute(Request::native("INSERT INTO t(id) VALUES (1)"))
        .await;
    let err = expect_err(result, "a write must fail while query_only is on");
    match err {
        DbError::Query { message, .. } => {
            assert!(
                message.to_lowercase().contains("read"),
                "expected a read-only rejection, got: {message}"
            );
        }
        other => panic!("expected DbError::Query, got {other:?}"),
    }

    conn.execute(Request::native("SELECT * FROM t"))
        .await
        .expect("reads must still work while read-only");

    conn.set_read_only(false)
        .await
        .expect("set_read_only(false) failed");
    conn.execute(Request::native("INSERT INTO t(id) VALUES (1)"))
        .await
        .expect("write should succeed once read-only is turned off");
}

#[tokio::test]
async fn read_only_connection_config_starts_enforced() {
    // A distinct on-disk file, since a read-only connection to a
    // not-yet-created `:memory:` database can't bootstrap the schema.
    let dir = std::env::temp_dir().join(format!("datagrep-sqlite-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir failed");
    let path = dir.join("ro.db");
    let path_str = path.to_string_lossy().into_owned();

    {
        let setup = common::connect_with(&path_str, false).await;
        setup
            .execute(Request::native("CREATE TABLE t(id INTEGER PRIMARY KEY)"))
            .await
            .expect("create table failed");
        setup.close().await.expect("close failed");
    }

    let conn = common::connect_with(&path_str, true).await;
    let result = conn
        .execute(Request::native("INSERT INTO t(id) VALUES (1)"))
        .await;
    let err = expect_err(result, "a connection opened read-only must reject writes");
    assert!(matches!(err, DbError::Query { .. }));
    conn.execute(Request::native("SELECT * FROM t"))
        .await
        .expect("reads must work on a read-only connection");

    let _ = std::fs::remove_dir_all(&dir);
}
