//! Savepoint-based nested transactions: `NESTED_TRANSACTIONS` behavior is
//! reached via plain SQL — see the module doc on `transaction.rs` for why
//! there is no typed "begin a nested transaction" API.

mod common;

use datagrep_api::{FetchHint, Payload, Request, TxOpts, Value};

#[tokio::test]
async fn savepoint_nesting_commit_and_rollback() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)",
    ))
    .await
    .expect("create table failed");

    let tx = conn.begin(TxOpts::default()).await.expect("begin failed");

    tx.execute(Request::native("INSERT INTO t(id, v) VALUES (1, 'a')"))
        .await
        .expect("insert 1 failed");

    // A savepoint that gets rolled back: row 2 must not survive.
    tx.execute(Request::native("SAVEPOINT sp1"))
        .await
        .expect("savepoint sp1 failed");
    tx.execute(Request::native("INSERT INTO t(id, v) VALUES (2, 'b')"))
        .await
        .expect("insert 2 failed");
    tx.execute(Request::native("ROLLBACK TO sp1"))
        .await
        .expect("rollback to sp1 failed");
    tx.execute(Request::native("RELEASE sp1"))
        .await
        .expect("release sp1 failed");

    // A savepoint that gets released (kept): row 3 must survive.
    tx.execute(Request::native("SAVEPOINT sp2"))
        .await
        .expect("savepoint sp2 failed");
    tx.execute(Request::native("INSERT INTO t(id, v) VALUES (3, 'c')"))
        .await
        .expect("insert 3 failed");
    tx.execute(Request::native("RELEASE sp2"))
        .await
        .expect("release sp2 failed");

    tx.commit().await.expect("commit failed");

    let mut cursor = conn
        .execute(Request::native("SELECT id FROM t ORDER BY id"))
        .await
        .expect("select failed");
    let mut ids = Vec::new();
    while let Some(batch) = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("fetch failed")
    {
        if let Payload::Rows(rows) = batch.payload {
            ids.extend(rows.into_iter().map(|r| r[0].clone()));
        }
    }
    assert_eq!(
        ids,
        vec![Value::I64(1), Value::I64(3)],
        "row 2 (inside the rolled-back savepoint) must be absent"
    );
}

#[tokio::test]
async fn rollback_discards_the_whole_transaction() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(id INTEGER PRIMARY KEY)"))
        .await
        .expect("create table failed");

    let tx = conn.begin(TxOpts::default()).await.expect("begin failed");
    tx.execute(Request::native("INSERT INTO t(id) VALUES (1)"))
        .await
        .expect("insert failed");
    tx.rollback().await.expect("rollback failed");

    let mut cursor = conn
        .execute(Request::native("SELECT COUNT(*) FROM t"))
        .await
        .expect("count failed");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("fetch failed")
        .expect("expected a row");
    match batch.payload {
        Payload::Rows(rows) => assert_eq!(rows[0][0], Value::I64(0)),
        other => panic!("expected Rows, got {other:?}"),
    }
}
