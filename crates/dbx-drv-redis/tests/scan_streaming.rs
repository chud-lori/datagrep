//! The load-bearing integration contract (design §3.1 requirement 2/3,
//! §5.2): browsing a real keyspace and a real huge hash goes through
//! `SCAN`/`HSCAN`, incrementally, and `KEYS` is **never** sent — proven from
//! outside the driver via `INFO commandstats`, not just by reading the
//! source. Run with `cargo test -p dbx-drv-redis --test scan_streaming --
//! --ignored` against `DBX_TEST_REDIS` (default `redis://localhost:6379`;
//! see `README.md`).

mod common;

use std::collections::HashSet;

use dbx_api::{FetchHint, Op, Payload, Request, ResumeToken, Value};

const KEY_COUNT: u32 = 50_000;
const HASH_FIELD_COUNT: u32 = 100_000;
const HASH_KEY: &str = "dbxtest:bighash";
const KEY_PREFIX: &str = "dbxtest:k:";

#[tokio::test]
#[ignore]
async fn scan_browses_50k_keys_incrementally_never_using_keys() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_keys(&mut raw, KEY_PREFIX, KEY_COUNT).await;

    let keys_before = common::command_call_count(&mut raw, "keys").await;

    let conn = common::connect().await;
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: dbx_api::ObjectPath::new(vec![std::sync::Arc::from("0")]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("Op::Scan execute failed");

    let hint = FetchHint {
        max_rows: 1000,
        ..FetchHint::default()
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut batch_count = 0u32;
    let mut max_batch_len = 0usize;
    while let Some(batch) = cursor.next_batch(hint).await.expect("SCAN batch failed") {
        let Payload::Pairs(pairs) = batch.payload else {
            panic!("expected Shape::Pairs payload from a keyspace SCAN");
        };
        // Redis's own docs are explicit that `COUNT` is a hint for how much
        // *work* one round does, not a hard cap on the reply size — actual
        // batches routinely land a little over. Bounding this driver's
        // batches to *roughly* the hint (never "the whole keyspace in one
        // shot") is the real "memory-flat" claim; a hard per-item cap here
        // would mean silently dropping the overflow, since a SCAN cursor
        // can't resume from the middle of a hash-table bucket (data loss,
        // exactly what the cursor module doc says this driver refuses).
        assert!(
            pairs.len() <= hint.max_rows as usize * 2,
            "a SCAN batch of {} pairs is wildly over the {}-row hint — not incremental",
            pairs.len(),
            hint.max_rows
        );
        max_batch_len = max_batch_len.max(pairs.len());
        for (k, _type) in pairs {
            if let Value::Str(s) = k {
                if s.starts_with(KEY_PREFIX) {
                    seen.insert(s.to_string());
                }
            }
        }
        batch_count += 1;
    }

    assert_eq!(
        seen.len(),
        KEY_COUNT as usize,
        "SCAN must eventually surface every key present for its whole duration"
    );
    assert!(
        batch_count > 10,
        "50k keys at a 1000-row hint must take many round trips, got {batch_count}"
    );
    assert!(
        max_batch_len < KEY_COUNT as usize,
        "no single batch held anywhere near the full 50k keyspace — this is the \"memory-flat\" claim"
    );

    let keys_after = common::command_call_count(&mut raw, "keys").await;
    assert_eq!(
        keys_before, keys_after,
        "KEYS must never be emitted by any code path in this driver (design §5.2)"
    );
}

#[tokio::test]
#[ignore]
async fn hscan_pages_a_100k_field_hash_rather_than_returning_it_whole() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_hash(&mut raw, HASH_KEY, HASH_FIELD_COUNT).await;

    let conn = common::connect().await;
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: dbx_api::ObjectPath::new(vec![
                std::sync::Arc::from("0"),
                std::sync::Arc::from("dbxtest:"),
                std::sync::Arc::from(HASH_KEY),
            ]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("Op::Scan on the big hash failed");

    let hint = FetchHint {
        max_rows: 2000,
        ..FetchHint::default()
    };
    let mut total_fields = 0u64;
    let mut batches = 0u32;
    while let Some(batch) = cursor.next_batch(hint).await.expect("HSCAN batch failed") {
        let Payload::Pairs(pairs) = batch.payload else {
            panic!("expected Shape::Pairs for a hash-typed key");
        };
        // See the sibling SCAN test for why this is a generous multiple of
        // the hint rather than a strict `<=`: `COUNT` bounds server-side
        // work per round, not the reply size (Redis's own documented
        // behavior for the whole SCAN family, HSCAN included).
        assert!(
            pairs.len() <= hint.max_rows as usize * 2,
            "HSCAN batch of {} fields is wildly over the {}-field hint — the hash came back whole",
            pairs.len(),
            hint.max_rows
        );
        total_fields += pairs.len() as u64;
        batches += 1;
    }

    assert_eq!(
        total_fields, HASH_FIELD_COUNT as u64,
        "every field must be seen exactly once"
    );
    assert!(
        batches > 10,
        "a 100k-field hash at a 2000-field hint must take many round trips, got {batches}"
    );
}

#[tokio::test]
#[ignore]
async fn resume_token_continues_exactly_where_it_left_off() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_keys(&mut raw, KEY_PREFIX, 5_000).await;

    let conn = common::connect().await;
    let scan_req = |resume: Option<ResumeToken>| {
        Request::Op(Op::Scan {
            path: dbx_api::ObjectPath::new(vec![std::sync::Arc::from("0")]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume,
        })
    };

    // First cursor: take exactly one batch, remember the resume token, then
    // abandon it (never call next_batch again) — this is the "auto-disconnect,
    // resume later" scenario resume_token exists for (design §3.5).
    let mut first = conn
        .execute(scan_req(None))
        .await
        .expect("first scan failed");
    let hint = FetchHint {
        max_rows: 500,
        ..FetchHint::default()
    };
    let first_batch = first
        .next_batch(hint)
        .await
        .expect("first batch failed")
        .expect("expected at least one batch out of 5000 keys");
    let Payload::Pairs(first_pairs) = first_batch.payload else {
        panic!("expected Pairs");
    };
    let mut seen: HashSet<String> = first_pairs
        .into_iter()
        .filter_map(|(k, _)| match k {
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .collect();
    let token = first
        .resume_token()
        .expect("cursor not yet exhausted, must have a resume token");

    // Second cursor: resume from that exact token and drain the rest.
    let mut second = conn
        .execute(scan_req(Some(token)))
        .await
        .expect("resumed scan failed");
    while let Some(batch) = second.next_batch(hint).await.expect("resumed batch failed") {
        let Payload::Pairs(pairs) = batch.payload else {
            panic!("expected Pairs");
        };
        for (k, _) in pairs {
            if let Value::Str(s) = k {
                seen.insert(s.to_string());
            }
        }
    }

    assert_eq!(
        seen.len(),
        5_000,
        "resuming from the token must cover the rest of the keyspace with no gap"
    );
}
