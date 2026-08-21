mod common;

use datagrep_api::{FetchHint, Payload, Request, Shape};

#[tokio::test]
async fn streams_100k_rows_in_incrementally_sized_batches() {
    let conn = common::connect_memory().await;
    let mut cursor = conn
        .execute(Request::native(
            "WITH RECURSIVE series(x) AS ( \
                 SELECT 1 UNION ALL SELECT x + 1 FROM series WHERE x < 100000 \
             ) SELECT x FROM series",
        ))
        .await
        .expect("execute failed");

    match cursor.shape() {
        Shape::Table(schema) => assert_eq!(schema.fields.len(), 1, "one projected column"),
        other => panic!("expected Shape::Table, got {other:?}"),
    }

    let hint = FetchHint {
        max_rows: 777,
        max_bytes: 4 * 1024 * 1024,
        target_ms: 80,
    };
    let mut batch_sizes = Vec::new();
    let mut total_rows = 0u64;
    while let Some(batch) = cursor.next_batch(hint).await.expect("fetch failed") {
        let Payload::Rows(rows) = batch.payload else {
            panic!("expected a Rows payload for a Table-shaped cursor");
        };
        assert!(!rows.is_empty(), "a returned batch must never be empty");
        assert!(
            rows.len() <= hint.max_rows as usize,
            "batch of {} rows exceeds the {} row hint",
            rows.len(),
            hint.max_rows
        );
        total_rows += rows.len() as u64;
        batch_sizes.push(rows.len());
    }

    assert_eq!(total_rows, 100_000, "every row must arrive exactly once");
    assert!(
        batch_sizes.len() > 1,
        "100k rows at a 777-row hint must take more than one batch, got {batch_sizes:?}"
    );
    for &n in &batch_sizes[..batch_sizes.len() - 1] {
        assert_eq!(
            n, 777,
            "every batch but the last should be exactly the hinted size"
        );
    }
    assert!(*batch_sizes.last().expect("at least one batch") <= 777);

    // `close()` on an already-exhausted cursor must be a harmless no-op.
    cursor
        .close()
        .await
        .expect("close after exhaustion should succeed");
}
