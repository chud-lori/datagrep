//! Cancellation must actually reach a running query — otherwise the user
//! hits stop, the app looks responsive, and the server keeps burning.

mod common;

use std::time::Duration;

use datagrep_api::{DbError, FetchHint, Payload, Request, Value};

#[tokio::test]
async fn interrupt_mid_scan_surfaces_cancelled_and_connection_stays_usable() {
    let conn = common::connect_memory().await;
    let mut cursor = conn
        .execute(Request::native(
            "WITH RECURSIVE series(x) AS ( \
                 SELECT 1 UNION ALL SELECT x + 1 FROM series LIMIT 100000000 \
             ) SELECT x FROM series",
        ))
        .await
        .expect("execute failed");

    let canceller = conn.canceller();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = canceller.cancel().await;
    });

    let hint = FetchHint {
        max_rows: 500,
        max_bytes: 4 * 1024 * 1024,
        target_ms: 80,
    };
    let mut saw_cancelled = false;
    for _ in 0..20_000 {
        match cursor.next_batch(hint).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(DbError::Cancelled) => {
                saw_cancelled = true;
                break;
            }
            Err(other) => panic!("unexpected error mid-scan: {other:?}"),
        }
    }
    assert!(
        saw_cancelled,
        "expected the 100M-row scan to observe a cancellation before finishing"
    );
    cursor
        .close()
        .await
        .expect("closing the cancelled cursor should still succeed");

    // The load-bearing assertion: the *connection* survives a cancelled
    // *cursor*. Connection isolation exists to stop a driver panic from
    // poisoning a connection; a cooperative cancel must not do the same.
    let mut check = conn
        .execute(Request::native("SELECT 1"))
        .await
        .expect("connection must still be usable after a cancelled query");
    let batch = check
        .next_batch(FetchHint::default())
        .await
        .expect("fetch after cancel failed")
        .expect("expected exactly one row");
    match batch.payload {
        Payload::Rows(rows) => assert_eq!(rows, vec![vec![Value::I64(1)]]),
        other => panic!("expected Rows, got {other:?}"),
    }
}
