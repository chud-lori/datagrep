use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bson::doc;

use datagrep_api::catalog::{ListOpts, ObjectKind};
use datagrep_api::config::{ConfigValue, ConnectionConfig, ResolvedConfig};
use datagrep_api::driver::{ConnectCtx, Connection, Driver, FetchHint, Payload};
use datagrep_api::request::{DdlOp, Op, Request};
use datagrep_api::shape::{ObjectPath, SchemaDelta};
use datagrep_api::value::Value;

use datagrep_drv_mongo::driver::MongoDriver;

fn test_uri() -> String {
    std::env::var("DATAGREP_TEST_MONGO").unwrap_or_else(|_| "mongodb://localhost:27017".to_string())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_db(label: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("datagrep_drv_mongo_test_{label}_{ts}_{n}")
}

async fn connect(database: &str) -> Arc<dyn Connection> {
    let driver = MongoDriver::new();
    let mut cfg: ConnectionConfig = driver
        .parse_url(&test_uri())
        .expect("valid DATAGREP_TEST_MONGO uri");
    cfg.values.insert(
        "database".to_string(),
        ConfigValue::Str(database.to_string()),
    );
    let resolved = ResolvedConfig::without_secrets(cfg);
    let conn = driver
        .connect(&resolved, ConnectCtx::default())
        .await
        .expect("connect to test mongod (is DATAGREP_TEST_MONGO reachable?)");
    Arc::from(conn)
}

async fn raw_client() -> mongodb::Client {
    mongodb::Client::with_uri_str(&test_uri())
        .await
        .expect("connect to test mongod")
}

async fn drop_db(database: &str) {
    let client = raw_client().await;
    let _ = client.database(database).drop().await;
}

fn scan(collection: &str) -> Request {
    Request::Op(Op::Scan {
        path: ObjectPath::new(vec![Arc::from(collection)]),
        filter: None,
        order: vec![],
        project: None,
        limit: None,
        resume: None,
    })
}

#[tokio::test]
#[ignore]
async fn streams_100k_documents_in_incremental_batches() {
    let db = unique_db("stream100k");
    let client = raw_client().await;
    let coll = client.database(&db).collection::<bson::Document>("items");
    const N: usize = 100_000;
    let docs: Vec<bson::Document> = (0..N)
        .map(|i| doc! { "i": i as i64, "s": format!("row-{i}") })
        .collect();
    for chunk in docs.chunks(2_000) {
        coll.insert_many(chunk.to_vec())
            .await
            .expect("seed insert_many");
    }

    let conn = connect(&db).await;
    let mut cursor = conn.execute(scan("items")).await.expect("execute scan");
    let hint = FetchHint {
        max_rows: 1_000,
        ..FetchHint::default()
    };

    let mut total = 0u64;
    let mut batches = 0u64;
    while let Some(batch) = cursor.next_batch(hint).await.expect("next_batch") {
        batches += 1;
        match batch.payload {
            Payload::Docs(docs) => {
                assert!(
                    docs.len() as u32 <= hint.max_rows,
                    "a batch must never exceed the fetch hint"
                );
                total += docs.len() as u64;
            }
            other => panic!("expected Payload::Docs, got {other:?}"),
        }
    }
    assert_eq!(
        total, N as u64,
        "every document must be streamed exactly once"
    );
    assert!(
        batches >= (N as u64 / 1_000),
        "expected incremental batches (~{}), got {batches}",
        N / 1_000
    );

    drop_db(&db).await;
}

#[tokio::test]
#[ignore]
async fn heterogeneous_collection_emits_schema_delta_add_column_events() {
    let db = unique_db("schemadelta");
    let client = raw_client().await;
    let coll = client.database(&db).collection::<bson::Document>("items");
    coll.insert_many(vec![
        doc! { "a": 1_i32 },
        doc! { "a": 2_i32, "b": "hello" },
        doc! { "a": 3_i32, "c": true },
    ])
    .await
    .expect("seed insert_many");

    let conn = connect(&db).await;
    let mut cursor = conn.execute(scan("items")).await.expect("execute scan");
    // Force one document per batch so field growth is visibly incremental.
    let hint = FetchHint {
        max_rows: 1,
        ..FetchHint::default()
    };

    let mut added: Vec<String> = Vec::new();
    while let Some(batch) = cursor.next_batch(hint).await.expect("next_batch") {
        for delta in batch.schema_delta {
            if let SchemaDelta::AddColumn { field } = delta {
                added.push(field.name.to_string());
            }
        }
    }

    assert_eq!(
        added.iter().filter(|n| n.as_str() == "a").count(),
        1,
        "each field name is announced exactly once: {added:?}"
    );
    assert!(added.contains(&"b".to_string()));
    assert!(added.contains(&"c".to_string()));
    assert_eq!(
        added.len(),
        added.iter().collect::<std::collections::HashSet<_>>().len(),
        "no field name is announced twice"
    );

    drop_db(&db).await;
}

#[tokio::test]
#[ignore]
async fn nested_documents_round_trip_exactly() {
    let db = unique_db("nested");
    let client = raw_client().await;
    let coll = client.database(&db).collection::<bson::Document>("items");
    let inserted = doc! {
        "name": "amy",
        "tags": ["a", "b", "c"],
        "address": {
            "city": "sg",
            "zip": "000000",
            "geo": { "lat": 1.5, "lng": 103.8 },
        },
        "scores": [1_i32, 2_i32, 3_i32],
        "price": bson::Decimal128::from_str("19.99").unwrap(),
        "active": true,
        "deleted_at": bson::Bson::Null,
    };
    coll.insert_one(inserted.clone())
        .await
        .expect("seed insert_one");

    let conn = connect(&db).await;
    let mut cursor = conn.execute(scan("items")).await.expect("execute scan");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("next_batch")
        .expect("one document");
    let Payload::Docs(docs) = batch.payload else {
        panic!("expected Payload::Docs");
    };
    assert_eq!(docs.len(), 1);
    let Value::Document(got) = &docs[0] else {
        panic!("expected a document value");
    };

    assert_eq!(got.get("name"), Some(&Value::Str(Arc::from("amy"))));
    match got.get("price") {
        Some(Value::Decimal(s)) => assert_eq!(s.as_ref(), "19.99"),
        other => panic!("expected Decimal(\"19.99\"), got {other:?}"),
    }
    assert_eq!(got.get("active"), Some(&Value::Bool(true)));
    // A stored NULL is present, not Absent.
    assert_eq!(got.get("deleted_at"), Some(&Value::Null));
    match got.get("tags") {
        Some(Value::Array(items)) => {
            assert_eq!(items.len(), 3);
            assert_eq!(items[1], Value::Str(Arc::from("b")));
        }
        other => panic!("expected an array, got {other:?}"),
    }
    match got.get("address") {
        Some(Value::Document(address)) => {
            assert_eq!(address.get("city"), Some(&Value::Str(Arc::from("sg"))));
            match address.get("geo") {
                Some(Value::Document(geo)) => {
                    assert_eq!(geo.get("lat"), Some(&Value::F64(1.5)));
                }
                other => panic!("expected nested geo document, got {other:?}"),
            }
        }
        other => panic!("expected a nested document, got {other:?}"),
    }
    // No field on the original document goes missing or gets renamed.
    for (k, _) in inserted.iter() {
        if k == "_id" {
            continue;
        }
        assert!(got.get(k).is_some(), "field {k:?} lost in round trip");
    }

    drop_db(&db).await;
}

#[tokio::test]
#[ignore]
async fn cancel_mid_long_query_returns_control_promptly() {
    let db = unique_db("cancel");
    let client = raw_client().await;
    client
        .database(&db)
        .collection::<bson::Document>("items")
        .insert_one(doc! { "x": 1_i32 })
        .await
        .expect("seed insert_one");

    let conn = connect(&db).await;
    let canceller = conn.canceller();

    let slow = conn.clone();
    let handle = tokio::spawn(async move {
        slow.execute(Request::native(
            r#"db.items.find({ $where: "sleep(4000) || true" })"#,
        ))
        .await
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let start = Instant::now();
    let outcome = canceller.cancel().await;
    let cancel_elapsed = start.elapsed();

    assert!(
        cancel_elapsed < Duration::from_secs(3),
        "cancel() took {cancel_elapsed:?}, expected near-instant return"
    );
    println!("cancel outcome: {outcome:?} (kind: {:?})", canceller.kind());

    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    let mut cursor = conn
        .execute(scan("items"))
        .await
        .expect("connection still usable after cancel");
    assert!(cursor.next_batch(FetchHint::default()).await.is_ok());

    drop_db(&db).await;
}

#[tokio::test]
#[ignore]
async fn catalog_lists_and_infers() {
    let db = unique_db("catalog");
    let client = raw_client().await;
    let coll = client.database(&db).collection::<bson::Document>("people");
    coll.insert_many(vec![
        doc! { "name": "amy", "age": 30_i32 },
        doc! { "name": "bo" },
        doc! { "name": "cy", "age": "unknown" },
    ])
    .await
    .expect("seed insert_many");

    let conn = connect(&db).await;
    let catalog = conn.catalog();

    let dbs = catalog
        .children(&ObjectPath::root(), ListOpts::default())
        .await
        .expect("list databases");
    assert!(
        dbs.items
            .iter()
            .any(|n| n.path.parts() == [Arc::from(db.as_str())]),
        "seeded database must appear in listDatabases: {:?}",
        dbs.items
            .iter()
            .map(|n| n.path.to_string())
            .collect::<Vec<_>>()
    );

    let colls = catalog
        .children(
            &ObjectPath::new(vec![Arc::from(db.as_str())]),
            ListOpts::default(),
        )
        .await
        .expect("list collections");
    assert!(colls
        .items
        .iter()
        .any(|n| n.path.parts().last().map(|p| p.as_ref()) == Some("people")));

    let inferred = catalog
        .infer_shape(
            &ObjectPath::new(vec![Arc::from(db.as_str()), Arc::from("people")]),
            100,
        )
        .await
        .expect("infer_shape");
    assert_eq!(inferred.sampled, 3);
    let age = inferred
        .root
        .iter()
        .find(|(name, _)| name.as_ref() == "age")
        .expect("age field inferred")
        .1
        .clone();
    assert_eq!(age.present, 2, "age missing from one of the three docs");
    assert_eq!(
        age.types.len(),
        2,
        "age is heterogeneous (I64 and Str) and both must stay visible: {:?}",
        age.types
    );

    let detail = catalog
        .describe(&ObjectPath::new(vec![
            Arc::from(db.as_str()),
            Arc::from("people"),
        ]))
        .await
        .expect("describe collection");
    assert!(detail
        .extra
        .iter()
        .any(|(k, _)| k.as_ref() == "inferred_schema"));

    drop_db(&db).await;
}

async fn ddl(conn: &dyn Connection, op: DdlOp) -> Result<(), datagrep_api::DbError> {
    let mut cur = conn.execute(Request::Op(Op::Ddl(op))).await?;
    while cur.next_batch(FetchHint::default()).await?.is_some() {}
    Ok(())
}

#[tokio::test]
#[ignore]
async fn structured_ddl_round_trips_through_the_catalog() {
    let db = unique_db("ddl");
    let client = raw_client().await;
    client
        .database(&db)
        .collection::<bson::Document>("widgets")
        .insert_one(doc! { "name": "a" })
        .await
        .expect("seed");

    let conn = connect(&db).await;
    let listed = conn
        .catalog()
        .children(
            &ObjectPath::new(vec![Arc::from(db.as_str())]),
            ListOpts {
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .expect("list collections");
    let widgets = listed
        .items
        .iter()
        .find(|n| &*n.path.parts()[1] == "widgets")
        .expect("widgets should be listed")
        .clone();
    assert_eq!(widgets.kind, ObjectKind::Collection);

    ddl(
        conn.as_ref(),
        DdlOp::CreateIndex {
            path: widgets.path.clone(),
            name: Arc::from("widgets_name"),
            fields: vec![datagrep_api::FieldPath::field("name")],
            unique: true,
            if_not_exists: true,
        },
    )
    .await
    .expect("create index");
    let index_names = |client: mongodb::Client, db: String| async move {
        client
            .database(&db)
            .collection::<bson::Document>("widgets")
            .list_index_names()
            .await
            .expect("list indexes")
    };
    assert!(index_names(client.clone(), db.clone())
        .await
        .contains(&"widgets_name".to_string()));

    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: widgets.path.child("widgets_name"),
            kind: ObjectKind::Index,
            if_exists: false,
        },
    )
    .await
    .expect("drop index");
    assert!(!index_names(client.clone(), db.clone())
        .await
        .contains(&"widgets_name".to_string()));

    ddl(
        conn.as_ref(),
        DdlOp::Rename {
            from: widgets.path.clone(),
            to: ObjectPath::new(vec![Arc::from(db.as_str()), Arc::from("gadgets")]),
            kind: ObjectKind::Collection,
        },
    )
    .await
    .expect("rename collection");
    let names = client
        .database(&db)
        .list_collection_names()
        .await
        .expect("list collections");
    assert!(names.contains(&"gadgets".to_string()) && !names.contains(&"widgets".to_string()));

    let gadgets = ObjectPath::new(vec![Arc::from(db.as_str()), Arc::from("gadgets")]);
    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: gadgets.clone(),
            kind: ObjectKind::Collection,
            if_exists: true,
        },
    )
    .await
    .expect("drop collection");
    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: gadgets.clone(),
            kind: ObjectKind::Collection,
            if_exists: true,
        },
    )
    .await
    .expect("dropping it again is a no-op");
    drop_db(&db).await;
}

#[tokio::test]
#[ignore]
async fn preconditions_the_engine_cannot_express_are_refused() {
    let db = unique_db("ddl_precondition");
    let conn = connect(&db).await;
    let coll = ObjectPath::new(vec![Arc::from(db.as_str()), Arc::from("items")]);

    let err = ddl(
        conn.as_ref(),
        DdlOp::CreateIndex {
            path: coll.clone(),
            name: Arc::from("items_n"),
            fields: vec![datagrep_api::FieldPath::field("n")],
            unique: false,
            if_not_exists: false,
        },
    )
    .await
    .expect_err("cannot promise to fail on an existing index");
    assert!(format!("{err}").contains("no-op"), "{err}");

    for (kind, path) in [
        (
            ObjectKind::Database,
            ObjectPath::new(vec![Arc::from(db.as_str())]),
        ),
        (ObjectKind::Collection, coll.clone()),
    ] {
        let err = ddl(
            conn.as_ref(),
            DdlOp::Drop {
                path,
                kind,
                if_exists: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err}").contains("never existed"),
            "{kind:?}: {err}"
        );
    }

    let raw = raw_client().await;
    let ok = raw
        .database(&db)
        .run_command(doc! { "drop": "definitely_not_there" })
        .await;
    assert!(
        ok.is_ok(),
        "mongod is expected to answer ok for a missing collection: {ok:?}"
    );

    // With the guard, the same drop goes through.
    ddl(
        conn.as_ref(),
        DdlOp::Drop {
            path: ObjectPath::new(vec![Arc::from(db.as_str())]),
            kind: ObjectKind::Database,
            if_exists: true,
        },
    )
    .await
    .expect("drop database");
}
