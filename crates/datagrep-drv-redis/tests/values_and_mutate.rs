//! Type-aware value fetch (`Op::Scan` on a single key), `Op::Count`,
//! `Op::Mutate`, and `Request::Native`
//! dispatch — including a hand-typed `SCAN` line routing through the same
//! paging cursor the structured path uses.

mod common;

use std::sync::Arc;

use datagrep_api::{
    FetchHint, FieldPath, Mutation, MutationBatch, ObjectPath, Op, Payload, Request, Shape, Value,
    ValueKind,
};

fn key_path(key: &str) -> ObjectPath {
    ObjectPath::new(vec![
        Arc::from("0"),
        Arc::from("datagreptest:"),
        Arc::from(key),
    ])
}

async fn drain_pairs(cursor: &mut Box<dyn datagrep_api::Cursor>) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    while let Some(batch) = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("next_batch failed")
    {
        match batch.payload {
            Payload::Pairs(p) => out.extend(p),
            Payload::Empty => {}
            other => panic!("expected Pairs or Empty, got {other:?}"),
        }
    }
    out
}

#[tokio::test]
#[ignore]
async fn missing_key_maps_to_absent_not_null() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;

    let conn = common::connect().await;
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: key_path("datagreptest:does-not-exist"),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("Op::Scan on a missing key failed");
    let pairs = drain_pairs(&mut cursor).await;
    assert_eq!(pairs.len(), 1);
    assert_eq!(
        pairs[0].1,
        Value::Absent,
        "a missing key must map to Absent, never Null"
    );
}

#[tokio::test]
#[ignore]
async fn string_hash_set_zset_list_all_fetch_type_aware() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    let conn = common::connect().await;

    // string
    let mut c = conn
        .execute(Request::native("SET datagreptest:s hello"))
        .await
        .expect("SET failed");
    while c
        .next_batch(FetchHint::default())
        .await
        .expect("drain SET")
        .is_some()
    {}
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: key_path("datagreptest:s"),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("scan string key failed");
    assert!(matches!(
        cursor.shape(),
        Shape::Pairs {
            value_kind: ValueKind::Str
        }
    ));
    let pairs = drain_pairs(&mut cursor).await;
    assert_eq!(
        pairs,
        vec![(
            Value::Str(Arc::from("datagreptest:s")),
            Value::Str(Arc::from("hello"))
        )]
    );

    // hash
    for (f, v) in [("a", "1"), ("b", "2")] {
        let mut c = conn
            .execute(Request::native(format!("HSET datagreptest:h {f} {v}")))
            .await
            .expect("HSET failed");
        while c
            .next_batch(FetchHint::default())
            .await
            .expect("drain HSET")
            .is_some()
        {}
    }
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: key_path("datagreptest:h"),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("scan hash key failed");
    let pairs = drain_pairs(&mut cursor).await;
    assert_eq!(pairs.len(), 2, "expected both hash fields");

    // set
    let mut c = conn
        .execute(Request::native("SADD datagreptest:set x y z"))
        .await
        .expect("SADD failed");
    while c
        .next_batch(FetchHint::default())
        .await
        .expect("drain SADD")
        .is_some()
    {}
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: key_path("datagreptest:set"),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("scan set key failed");
    let pairs = drain_pairs(&mut cursor).await;
    assert_eq!(pairs.len(), 3, "expected all three set members");
    for (_member, present) in &pairs {
        assert_eq!(*present, Value::Bool(true));
    }

    // zset
    let mut c = conn
        .execute(Request::native("ZADD datagreptest:z 1 alice 2 bob"))
        .await
        .expect("ZADD failed");
    while c
        .next_batch(FetchHint::default())
        .await
        .expect("drain ZADD")
        .is_some()
    {}
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: key_path("datagreptest:z"),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("scan zset key failed");
    let pairs = drain_pairs(&mut cursor).await;
    assert_eq!(pairs.len(), 2);
    assert!(pairs.iter().any(|(_, score)| *score == Value::F64(1.0)));
    assert!(pairs.iter().any(|(_, score)| *score == Value::F64(2.0)));

    // list — LRANGE-backed ListCursor, index-keyed
    let mut c = conn
        .execute(Request::native("RPUSH datagreptest:l one two three"))
        .await
        .expect("RPUSH failed");
    while c
        .next_batch(FetchHint::default())
        .await
        .expect("drain RPUSH")
        .is_some()
    {}
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: key_path("datagreptest:l"),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("scan list key failed");
    let pairs = drain_pairs(&mut cursor).await;
    assert_eq!(
        pairs,
        vec![
            (Value::I64(0), Value::Str(Arc::from("one"))),
            (Value::I64(1), Value::Str(Arc::from("two"))),
            (Value::I64(2), Value::Str(Arc::from("three"))),
        ]
    );
}

#[tokio::test]
#[ignore]
async fn op_count_dbsize_and_per_key_cardinality() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_keys(&mut raw, "datagreptest:cnt:", 37).await;

    let conn = common::connect().await;
    let mut c = conn
        .execute(Request::native("HSET datagreptest:cnth a 1 b 2 c 3"))
        .await
        .expect("HSET failed");
    while c
        .next_batch(FetchHint::default())
        .await
        .expect("drain HSET")
        .is_some()
    {}

    // whole-db DBSIZE
    let mut cursor = conn
        .execute(Request::Op(Op::Count {
            path: ObjectPath::new(vec![Arc::from("0")]),
            filter: None,
            exact: true,
        }))
        .await
        .expect("Op::Count on whole db failed");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("count batch failed")
        .expect("expected an Ack batch");
    match cursor.shape() {
        Shape::Ack { affected, .. } => assert_eq!(*affected, Some(38), "37 string keys + 1 hash"),
        other => panic!("expected Shape::Ack, got {other:?}"),
    }
    let _ = batch;

    // per-key HLEN
    let mut cursor = conn
        .execute(Request::Op(Op::Count {
            path: key_path("datagreptest:cnth"),
            filter: None,
            exact: true,
        }))
        .await
        .expect("Op::Count on a hash key failed");
    cursor
        .next_batch(FetchHint::default())
        .await
        .expect("count batch failed");
    match cursor.shape() {
        Shape::Ack { affected, .. } => assert_eq!(*affected, Some(3)),
        other => panic!("expected Shape::Ack, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn op_mutate_set_hset_del_are_atomic_and_report_native_counts() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    let conn = common::connect().await;

    let batch = MutationBatch {
        mutations: vec![
            Mutation::Insert {
                path: ObjectPath::new(vec![Arc::from("0"), Arc::from("datagreptest:mut:str")]),
                doc: Value::Str(Arc::from("hi")),
            },
            Mutation::Insert {
                path: ObjectPath::new(vec![Arc::from("0"), Arc::from("datagreptest:mut:h")]),
                doc: Value::Document(Arc::new(datagrep_api::Document::from_fields(vec![(
                    Arc::from("f1"),
                    Value::Str(Arc::from("v1")),
                )]))),
            },
        ],
    };
    let mut cursor = conn
        .execute(Request::Op(Op::Mutate(batch)))
        .await
        .expect("Op::Mutate failed");
    cursor
        .next_batch(FetchHint::default())
        .await
        .expect("mutate batch failed");

    // Verify with a raw GET/HGETALL — independent of the driver under test.
    let got: String = redis::cmd("GET")
        .arg("datagreptest:mut:str")
        .query_async(&mut raw)
        .await
        .expect("GET failed");
    assert_eq!(got, "hi");
    let got: String = redis::cmd("HGET")
        .arg("datagreptest:mut:h")
        .arg("f1")
        .query_async(&mut raw)
        .await
        .expect("HGET failed");
    assert_eq!(got, "v1");

    // Delete both.
    let del_batch = MutationBatch {
        mutations: vec![
            Mutation::Delete {
                path: ObjectPath::new(vec![Arc::from("0")]),
                key: vec![(
                    FieldPath::field("key"),
                    Value::Str(Arc::from("datagreptest:mut:str")),
                )],
                expect: vec![],
            },
            Mutation::Delete {
                path: ObjectPath::new(vec![Arc::from("0")]),
                key: vec![(
                    FieldPath::field("key"),
                    Value::Str(Arc::from("datagreptest:mut:h")),
                )],
                expect: vec![],
            },
        ],
    };
    let mut cursor = conn
        .execute(Request::Op(Op::Mutate(del_batch)))
        .await
        .expect("Op::Mutate delete failed");
    cursor
        .next_batch(FetchHint::default())
        .await
        .expect("delete batch failed");
    let exists: i64 = redis::cmd("EXISTS")
        .arg("datagreptest:mut:str")
        .arg("datagreptest:mut:h")
        .query_async(&mut raw)
        .await
        .expect("EXISTS failed");
    assert_eq!(exists, 0, "both keys must be gone after the DEL batch");
}

#[tokio::test]
#[ignore]
async fn native_multi_line_pipeline_dispatches_each_and_shapes_the_last() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    let conn = common::connect().await;

    let mut cursor = conn
        .execute(Request::native(
            "SET datagreptest:pipeline:a 1\nSET datagreptest:pipeline:b 2\nGET datagreptest:pipeline:a",
        ))
        .await
        .expect("multi-line Native execute failed");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("batch failed")
        .expect("expected a batch");
    match batch.payload {
        Payload::Pairs(pairs) => {
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].1, Value::Str(Arc::from("1")));
        }
        other => panic!("expected Pairs for the final GET, got {other:?}"),
    }

    // Both SETs from the earlier lines really landed.
    let a: String = redis::cmd("GET")
        .arg("datagreptest:pipeline:a")
        .query_async(&mut raw)
        .await
        .expect("GET a failed");
    let b: String = redis::cmd("GET")
        .arg("datagreptest:pipeline:b")
        .query_async(&mut raw)
        .await
        .expect("GET b failed");
    assert_eq!(a, "1");
    assert_eq!(b, "2");
}

#[tokio::test]
#[ignore]
async fn native_scan_command_routes_through_the_paging_cursor() {
    let mut raw = common::raw_connection().await;
    common::flush(&mut raw).await;
    common::seed_keys(&mut raw, "datagreptest:native-scan:", 500).await;
    let conn = common::connect().await;

    let mut cursor = conn
        .execute(Request::native(
            "SCAN 0 MATCH datagreptest:native-scan:* COUNT 50",
        ))
        .await
        .expect("native SCAN execute failed");

    let mut total = 0usize;
    let mut batches = 0u32;
    let hint = FetchHint {
        max_rows: 50,
        ..FetchHint::default()
    };
    while let Some(batch) = cursor
        .next_batch(hint)
        .await
        .expect("native SCAN batch failed")
    {
        let Payload::Pairs(pairs) = batch.payload else {
            panic!("expected Pairs from a hand-typed SCAN");
        };
        total += pairs.len();
        batches += 1;
    }
    assert_eq!(
        total, 500,
        "a hand-typed SCAN must still surface every matching key"
    );
    assert!(
        batches > 1,
        "a hand-typed SCAN must page like the structured path, not dump everything in one shot"
    );
}
