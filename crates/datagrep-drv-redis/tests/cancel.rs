//! Cancellation of a Redis SCAN loop (design §3.3): "commands are atomic; a
//! 'long query' is our own SCAN loop → just stop." No server round trip is
//! needed to stop it — the cursor observes `CancelFlag` at its next
//! round-trip check and returns `DbError::Cancelled` promptly, and the
//! *connection* must stay usable afterward (mirrors `datagrep-drv-sqlite`'s own
//! cancellation test).

mod common;

use std::time::Duration;

use datagrep_api::{DbError, FetchHint, Op, Request};

const KEY_COUNT: u32 = 20_000;
const KEY_PREFIX: &str = "datagreptest:cancel:";

#[tokio::test]
#[ignore]
async fn cancel_stops_a_long_scan_loop_promptly_and_leaves_the_connection_usable() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_keys(&mut raw, KEY_PREFIX, KEY_COUNT).await;

    let conn = common::connect().await;
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: datagrep_api::ObjectPath::new(vec![std::sync::Arc::from("0")]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("Op::Scan execute failed");

    let canceller = conn.canceller();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let outcome = canceller
            .cancel()
            .await
            .expect("cancel() itself must not fail");
        // Redis cancellation is ClientAbandon for a plain SCAN loop — there
        // is no server-side kill for it, only our own loop stopping.
        assert_eq!(outcome, datagrep_api::CancelOutcome::ClientAbandoned);
    });

    // A tiny per-round COUNT forces many round trips over 20k keys, giving
    // the spawned cancel plenty of chances to land before the scan would
    // otherwise finish on its own.
    let hint = FetchHint {
        max_rows: 1,
        ..FetchHint::default()
    };
    let mut saw_cancelled = false;
    for _ in 0..KEY_COUNT {
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
        "expected the 20k-key scan (1 row/round) to observe a cancellation before finishing"
    );
    cancel_task.await.expect("cancel task panicked");

    // Load-bearing: the *connection* survives a cancelled *cursor* (mirrors
    // datagrep-drv-sqlite's cancel.rs — cancelling a cursor must never poison the
    // connection it came from).
    let mut check = conn
        .execute(Request::native("PING"))
        .await
        .expect("connection must still be usable after a cancelled scan");
    let batch = check
        .next_batch(FetchHint::default())
        .await
        .expect("fetch after cancel failed");
    assert!(
        batch.is_some(),
        "PING after cancel should still produce a reply"
    );
}
