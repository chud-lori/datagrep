//! Real end-to-end round trips for value mapping (unit-tested in isolation
//! in `value.rs`; this proves the same thing through `execute()`) and the
//! structured `Op::Scan`/`Op::Count`/`Op::Mutate` surface.

mod common;

use std::sync::Arc;

use bytes::Bytes;
use dbx_api::{
    Connection, ExecOpts, FetchHint, FieldPath, Mutation, MutationBatch, Op, Payload, Predicate,
    Request, Shape, SortKey, Value,
};

async fn first_row(conn: &dyn Connection, sql: &str) -> Vec<Value> {
    let mut cursor = conn
        .execute(Request::native(sql))
        .await
        .expect("execute failed");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("fetch failed")
        .expect("expected a row");
    match batch.payload {
        Payload::Rows(rows) => rows.into_iter().next().expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[tokio::test]
async fn every_storage_class_round_trips_through_real_execution() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native(
        "CREATE TABLE t(i INTEGER, r REAL, s TEXT, b BLOB, n INTEGER, flag BOOLEAN)",
    ))
    .await
    .expect("create table failed");
    conn.execute(Request::Native {
        text: Arc::from("INSERT INTO t VALUES (?, ?, ?, ?, ?, ?)"),
        params: vec![
            Value::I64(42),
            Value::F64(2.5),
            Value::Str(Arc::from("hi")),
            Value::Bytes(Bytes::from_static(b"\x01\x02")),
            Value::Null,
            Value::Bool(true),
        ],
        opts: ExecOpts::default(),
    })
    .await
    .expect("insert failed");

    let row = first_row(&*conn, "SELECT i, r, s, b, n, flag FROM t").await;
    assert_eq!(row[0], Value::I64(42));
    assert_eq!(row[1], Value::F64(2.5));
    assert_eq!(row[2], Value::Str(Arc::from("hi")));
    assert_eq!(row[3], Value::Bytes(Bytes::from_static(b"\x01\x02")));
    assert_eq!(row[4], Value::Null);
    assert_eq!(
        row[5],
        Value::Bool(true),
        "BOOLEAN decl type + 0/1 storage maps to Bool"
    );
}

#[tokio::test]
async fn ack_shape_for_ddl_and_writes() {
    let conn = common::connect_memory().await;
    let mut cursor = conn
        .execute(Request::native("CREATE TABLE t(id INTEGER PRIMARY KEY)"))
        .await
        .expect("ddl failed");
    assert!(matches!(cursor.shape(), Shape::Ack { .. }));
    assert!(
        cursor
            .next_batch(FetchHint::default())
            .await
            .expect("fetch on an Ack cursor should not error")
            .is_none(),
        "an Ack-shaped cursor never yields a batch"
    );

    let mut insert = conn
        .execute(Request::native("INSERT INTO t(id) VALUES (1), (2), (3)"))
        .await
        .expect("insert failed");
    match insert.shape() {
        Shape::Ack { affected, .. } => assert_eq!(*affected, Some(3)),
        other => panic!("expected Ack, got {other:?}"),
    }
    assert!(insert
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn op_scan_filters_sorts_and_never_interpolates_the_predicate() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
    ))
    .await
    .expect("create table failed");
    conn.execute(Request::native(
        "INSERT INTO users(id, name, age) VALUES \
         (1, 'alice', 30), (2, 'bob', 25), (3, 'carol', 40)",
    ))
    .await
    .expect("seed failed");

    let req = Request::Op(Op::Scan {
        path: dbx_api::ObjectPath::new(vec![Arc::from("users")]),
        filter: Some(Predicate::Ge {
            field: FieldPath::field("age"),
            value: Value::I64(28),
        }),
        order: vec![SortKey {
            path: FieldPath::field("age"),
            desc: true,
            nulls_first: false,
        }],
        project: Some(vec![FieldPath::field("name")]),
        limit: None,
        resume: None,
    });
    let mut cursor = conn.execute(req).await.expect("Op::Scan failed");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .expect("expected rows");
    let Payload::Rows(rows) = batch.payload else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.as_ref(),
            other => panic!("expected Str, got {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec!["carol", "alice"],
        "age >= 28, sorted by age desc"
    );
}

#[tokio::test]
async fn op_count_is_exact() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native("CREATE TABLE t(id INTEGER PRIMARY KEY)"))
        .await
        .unwrap();
    conn.execute(Request::native(
        "INSERT INTO t(id) VALUES (1), (2), (3), (4)",
    ))
    .await
    .unwrap();

    let req = Request::Op(Op::Count {
        path: dbx_api::ObjectPath::new(vec![Arc::from("t")]),
        filter: Some(Predicate::Gt {
            field: FieldPath::field("id"),
            value: Value::I64(2),
        }),
        exact: true,
    });
    let mut cursor = conn.execute(req).await.expect("Op::Count failed");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap();
    let Payload::Rows(rows) = batch.payload else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0], Value::I64(2));
}

#[tokio::test]
async fn op_mutate_insert_update_delete_round_trip() {
    let conn = common::connect_memory().await;
    conn.execute(Request::native(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)",
    ))
    .await
    .unwrap();

    let path = dbx_api::ObjectPath::new(vec![Arc::from("t")]);

    // Insert.
    let doc = Value::Document(Arc::new(dbx_api::Document::from_fields(vec![
        (Arc::from("id"), Value::I64(1)),
        (Arc::from("v"), Value::Str(Arc::from("first"))),
    ])));
    conn.execute(Request::Op(Op::Mutate(MutationBatch {
        mutations: vec![Mutation::Insert {
            path: path.clone(),
            doc,
        }],
    })))
    .await
    .expect("insert mutation failed");
    assert_eq!(
        first_row(&*conn, "SELECT v FROM t WHERE id = 1").await[0],
        Value::Str(Arc::from("first"))
    );

    // Update, keyed by the declared PRIMARY KEY (`id`).
    conn.execute(Request::Op(Op::Mutate(MutationBatch {
        mutations: vec![Mutation::Update {
            path: path.clone(),
            key: vec![Value::I64(1)],
            sets: vec![(FieldPath::field("v"), Value::Str(Arc::from("second")))],
        }],
    })))
    .await
    .expect("update mutation failed");
    assert_eq!(
        first_row(&*conn, "SELECT v FROM t WHERE id = 1").await[0],
        Value::Str(Arc::from("second"))
    );

    // Delete.
    conn.execute(Request::Op(Op::Mutate(MutationBatch {
        mutations: vec![Mutation::Delete {
            path,
            key: vec![Value::I64(1)],
        }],
    })))
    .await
    .expect("delete mutation failed");
    let mut cursor = conn
        .execute(Request::native("SELECT COUNT(*) FROM t"))
        .await
        .unwrap();
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap();
    let Payload::Rows(rows) = batch.payload else {
        panic!("expected Rows")
    };
    assert_eq!(rows[0][0], Value::I64(0));
}
