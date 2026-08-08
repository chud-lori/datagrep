//! Integration tests against a real Postgres server. All `#[ignore]`d by
//! default; run with `DATAGREP_TEST_PG=1 cargo test -p datagrep-drv-postgres --test
//! integration -- --ignored --test-threads=1`. See `tests/README.md` for a
//! one-liner to start a throwaway server.
//!
//! Connection defaults to `localhost:5432`, user `postgres`, database
//! `postgres`, no password — override with `DATAGREP_TEST_PG_HOST`,
//! `DATAGREP_TEST_PG_PORT`, `DATAGREP_TEST_PG_USER`, `DATAGREP_TEST_PG_PASSWORD`,
//! `DATAGREP_TEST_PG_DB`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use datagrep_api::catalog::ListOpts;
use datagrep_api::config::{ConfigValue, ConnectionConfig, ResolvedConfig};
use datagrep_api::driver::{ConnectCtx, Connection, Driver, FetchHint};
use datagrep_api::request::Request;
use datagrep_api::shape::{ObjectPath, Shape};
use datagrep_api::value::Value;

use datagrep_drv_postgres::PostgresDriver;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

async fn connect() -> Box<dyn Connection> {
    let driver = PostgresDriver::new();
    let mut values = BTreeMap::new();
    values.insert(
        "host".to_string(),
        ConfigValue::Str(env_or("DATAGREP_TEST_PG_HOST", "localhost")),
    );
    values.insert(
        "port".to_string(),
        ConfigValue::Num(env_or("DATAGREP_TEST_PG_PORT", "5432").parse().unwrap()),
    );
    values.insert(
        "user".to_string(),
        ConfigValue::Str(env_or("DATAGREP_TEST_PG_USER", "postgres")),
    );
    values.insert(
        "database".to_string(),
        ConfigValue::Str(env_or("DATAGREP_TEST_PG_DB", "postgres")),
    );
    values.insert("tls".to_string(), ConfigValue::Str("disable".to_string()));
    if let Ok(pw) = std::env::var("DATAGREP_TEST_PG_PASSWORD") {
        values.insert("password".to_string(), ConfigValue::Str(pw));
    }
    let cfg = ResolvedConfig::without_secrets(ConnectionConfig {
        driver: Arc::from("postgres"),
        values,
    });
    driver.connect(&cfg, ConnectCtx::default()).await.expect(
        "connect to test postgres (set DATAGREP_TEST_PG_* env vars if not on localhost:5432/postgres)",
    )
}

/// Design §3.2: "first chunk renders before chunk 2 is requested" — proven
/// here by timing the first batch of a 100k-row stream against the whole
/// stream's completion time.
#[tokio::test]
#[ignore]
async fn streams_100k_rows_first_batch_arrives_fast() {
    let conn = connect().await;
    let start = Instant::now();
    let mut cursor = conn
        .execute(Request::native(
            "SELECT g, repeat('x', 50) AS filler FROM generate_series(1, 100000) AS g",
        ))
        .await
        .expect("execute");

    let first_batch = cursor
        .next_batch(FetchHint {
            max_rows: 500,
            ..FetchHint::default()
        })
        .await
        .expect("first batch")
        .expect("at least one batch");
    let first_batch_elapsed = start.elapsed();
    let first_batch_rows = match &first_batch.payload {
        datagrep_api::driver::Payload::Rows(rows) => rows.len(),
        other => panic!("expected Rows, got {other:?}"),
    };
    assert!(
        first_batch_rows > 0 && first_batch_rows <= 500,
        "got {first_batch_rows} rows in first batch"
    );

    let mut total_rows = first_batch_rows as u64;
    while let Some(batch) = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("batch")
    {
        if let datagrep_api::driver::Payload::Rows(rows) = &batch.payload {
            total_rows += rows.len() as u64;
        }
    }
    let total_elapsed = start.elapsed();

    assert_eq!(
        total_rows, 100_000,
        "must see every generated row exactly once"
    );
    assert!(
        first_batch_elapsed < total_elapsed / 4,
        "first batch ({first_batch_elapsed:?}) should be a small fraction of the full \
         stream's time ({total_elapsed:?}) — if it isn't, the driver is buffering \
         instead of streaming"
    );
}

/// A weaker, environment-independent RSS proxy: this process's own RSS
/// shouldn't grow by anything like the size of a 100k-row, ~5MB result set
/// if batches are actually being dropped as they're consumed rather than
/// accumulated. Best-effort (reads `/proc/self/status` on Linux, `ps` on
/// macOS) — informational rather than a hard CI gate, since RSS is noisy.
#[tokio::test]
#[ignore]
async fn streaming_does_not_retain_the_whole_result_set() {
    fn rss_kb() -> Option<u64> {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    return rest.trim().trim_end_matches(" kB").trim().parse().ok();
                }
            }
        }
        let pid = std::process::id();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    let before = rss_kb();
    let conn = connect().await;
    let mut cursor = conn
        .execute(Request::native(
            "SELECT g, repeat('y', 200) AS filler FROM generate_series(1, 100000) AS g",
        ))
        .await
        .expect("execute");
    let mut rows = 0u64;
    while let Some(batch) = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("batch")
    {
        if let datagrep_api::driver::Payload::Rows(r) = &batch.payload {
            rows += r.len() as u64;
        }
        // Each `Batch` is dropped here at the end of the loop body — nothing
        // upstream of this test is retaining previously-yielded batches.
    }
    let after = rss_kb();
    assert_eq!(rows, 100_000);

    if let (Some(before), Some(after)) = (before, after) {
        // The full result is ~20MB of row text; if the driver buffered
        // everything we'd expect RSS to grow by roughly that much. A few MB
        // of steady-state growth (allocator arenas, decoded String churn) is
        // fine; growth on the order of the dataset size is not.
        let grew_kb = after.saturating_sub(before);
        assert!(
            grew_kb < 15_000,
            "RSS grew {grew_kb} KB streaming a 100k-row result — looks like buffering, not streaming"
        );
    }
}

/// Design risk #4: NUMERIC must never round-trip through f64.
#[tokio::test]
#[ignore]
async fn numeric_round_trips_as_decimal_string() {
    let conn = connect().await;
    let mut cursor = conn
        .execute(Request::native(
            "SELECT 12345.6789::numeric AS n, 0.1::numeric AS tenth",
        ))
        .await
        .expect("execute");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("batch")
        .expect("one row");
    let datagrep_api::driver::Payload::Rows(rows) = batch.payload else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Decimal(Arc::from("12345.6789")));
    // 0.1 is famously inexact as f64 (0.1000000000000000055...); as a
    // Postgres numeric literal it must come back exactly "0.1".
    assert_eq!(rows[0][1], Value::Decimal(Arc::from("0.1")));
}

/// Design §3.3: cancel is racy ("Requested", never a guaranteed ack) but the
/// connection must remain usable afterward.
#[tokio::test]
#[ignore]
async fn cancel_mid_sleep_leaves_connection_usable() {
    let conn = connect().await;
    let canceller = conn.canceller();

    let cancel_task = {
        let canceller = canceller.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            canceller.cancel().await
        })
    };

    let result = conn.execute(Request::native("SELECT pg_sleep(5)")).await;
    // Either the execute() call itself errors (cancelled mid-prepare/portal
    // setup) or it succeeds and the *next* batch pull fails — accept either,
    // but require some observable effect from the cancel.
    let saw_cancel_effect = match result {
        Err(_) => true,
        Ok(mut cursor) => cursor.next_batch(FetchHint::default()).await.is_err(),
    };

    let outcome = cancel_task
        .await
        .expect("cancel task join")
        .expect("cancel() itself must not error");
    assert_eq!(outcome, datagrep_api::driver::CancelOutcome::Requested);
    assert!(
        saw_cancel_effect,
        "pg_sleep(5) should not have completed normally within the test"
    );

    // The connection must still be usable — a poisoned connection here would
    // be a correctness bug per design §3.5.
    conn.ping().await.expect("connection must survive a cancel");
    let mut cursor2 = conn
        .execute(Request::native("SELECT 1"))
        .await
        .expect("execute after cancel");
    let batch = cursor2
        .next_batch(FetchHint::default())
        .await
        .expect("batch after cancel")
        .expect("one row");
    match batch.payload {
        datagrep_api::driver::Payload::Rows(rows) => assert_eq!(rows[0][0], Value::I64(1)),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Design item 5: catalog browsing on a seeded schema, one query per level.
#[tokio::test]
#[ignore]
async fn catalog_children_on_seeded_schema() {
    let conn = connect().await;
    conn.execute(Request::native(
        "DROP SCHEMA IF EXISTS datagrep_catalog_test CASCADE",
    ))
    .await
    .ok();
    conn.execute(Request::native("CREATE SCHEMA datagrep_catalog_test"))
        .await
        .expect("create schema");
    conn.execute(Request::native(
        "CREATE TABLE datagrep_catalog_test.widgets (id serial PRIMARY KEY, name text NOT NULL)",
    ))
    .await
    .expect("create table");

    let catalog = conn.catalog();
    let current_db_cursor = conn
        .execute(Request::native("SELECT current_database()"))
        .await
        .expect("current db");
    let mut current_db_cursor = current_db_cursor;
    let batch = current_db_cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap();
    let datagrep_api::driver::Payload::Rows(rows) = batch.payload else {
        panic!()
    };
    let Value::Str(dbname) = &rows[0][0] else {
        panic!()
    };

    let schemas = catalog
        .children(
            &ObjectPath::new(vec![dbname.clone()]),
            ListOpts {
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .expect("list schemas");
    assert!(
        schemas
            .items
            .iter()
            .any(|n| &*n.path.parts()[1] == "datagrep_catalog_test"),
        "seeded schema should be listed"
    );

    let tables = catalog
        .children(
            &ObjectPath::new(vec![dbname.clone(), Arc::from("datagrep_catalog_test")]),
            ListOpts {
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .expect("list tables");
    assert_eq!(tables.items.len(), 1);
    assert_eq!(&*tables.items[0].path.parts()[2], "widgets");

    let columns = catalog
        .children(
            &ObjectPath::new(vec![
                dbname.clone(),
                Arc::from("datagrep_catalog_test"),
                Arc::from("widgets"),
            ]),
            ListOpts {
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .expect("list columns");
    let names: Vec<String> = columns
        .items
        .iter()
        .map(|n| n.path.parts()[3].to_string())
        .collect();
    assert_eq!(names, vec!["id".to_string(), "name".to_string()]);

    let detail = catalog
        .describe(&ObjectPath::new(vec![
            dbname.clone(),
            Arc::from("datagrep_catalog_test"),
            Arc::from("widgets"),
        ]))
        .await
        .expect("describe");
    let schema = detail.schema.expect("declared schema");
    assert_eq!(schema.fields.len(), 2);
    let identity = schema.identity.expect("primary key detected");
    assert_eq!(identity.field_indices, vec![0]);

    conn.execute(Request::native("DROP SCHEMA datagrep_catalog_test CASCADE"))
        .await
        .ok();
}

/// Sanity check on the read-only auto-wrap: a `SELECT` compiled from
/// `Op::Scan` streams through the extended-protocol portal path and yields a
/// `Table` shape with the identity resolved for a plain single-table scan.
#[tokio::test]
#[ignore]
async fn scan_op_streams_with_identity() {
    let conn = connect().await;
    conn.execute(Request::native("DROP TABLE IF EXISTS datagrep_scan_test"))
        .await
        .ok();
    conn.execute(Request::native(
        "CREATE TABLE datagrep_scan_test (id serial PRIMARY KEY, v int NOT NULL)",
    ))
    .await
    .expect("create");
    conn.execute(Request::native(
        "INSERT INTO datagrep_scan_test (v) SELECT * FROM generate_series(1, 10)",
    ))
    .await
    .expect("seed rows");

    let op = datagrep_api::request::Op::Scan {
        path: ObjectPath::new(vec![Arc::from("datagrep_scan_test")]),
        filter: None,
        order: vec![],
        project: None,
        limit: Some(5),
        resume: None,
    };
    let mut cursor = conn.execute(Request::Op(op)).await.expect("scan");
    match cursor.shape() {
        Shape::Table(schema) => assert!(
            schema.identity.is_some(),
            "single-table scan should resolve a PK"
        ),
        other => panic!("expected Table shape, got {other:?}"),
    }
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap();
    if let datagrep_api::driver::Payload::Rows(rows) = batch.payload {
        assert_eq!(rows.len(), 5);
    }
    assert!(
        cursor.resume_token().is_none(),
        "v1: resume_token is always None (see crate docs)"
    );

    conn.execute(Request::native("DROP TABLE datagrep_scan_test"))
        .await
        .ok();
}

/// Regression test for the connection-wide deadlock (TEST-REPORT.md F2).
///
/// The shape that used to hang forever: an open, deliberately **half-read**
/// cursor pins its session, and then the caller does something else on the
/// same `Connection` — browses the catalog (the GUI's "results grid open,
/// click the schema tree") and runs another query. Both used to await the
/// connection-wide client mutex with no timeout, so the driver froze at 0%
/// CPU with the server showing `idle in transaction`.
///
/// Every step is wrapped in a real deadline: the whole point is that these
/// operations *return*, so a regression must fail the test, not hang the
/// suite the way the shipped one did.
#[tokio::test]
#[ignore]
async fn catalog_and_queries_work_while_a_cursor_is_open() {
    const DEADLINE: Duration = Duration::from_secs(20);
    let conn = connect().await;

    // Deliberately partial: 10k rows available, 10 pulled. The cursor is not
    // drained, so its session stays pinned for the rest of the test.
    let mut cursor = conn
        .execute(Request::native(
            "SELECT g FROM generate_series(1, 10000) AS g",
        ))
        .await
        .expect("execute");
    let first = cursor
        .next_batch(FetchHint {
            max_rows: 10,
            ..FetchHint::default()
        })
        .await
        .expect("first batch")
        .expect("at least one batch");
    match &first.payload {
        datagrep_api::driver::Payload::Rows(rows) => assert_eq!(rows.len(), 10),
        other => panic!("expected Rows, got {other:?}"),
    }

    // 1. Catalog browsing must not queue behind the open cursor.
    let catalog = conn.catalog();
    let databases = tokio::time::timeout(
        DEADLINE,
        catalog.children(
            &ObjectPath::new(vec![]),
            ListOpts {
                limit: 100,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("catalog.children() must not hang while a cursor is open")
    .expect("list databases");
    assert!(!databases.items.is_empty(), "at least one database");

    let dbname: Arc<str> = Arc::from(env_or("DATAGREP_TEST_PG_DB", "postgres"));
    let schemas = tokio::time::timeout(
        DEADLINE,
        catalog.children(
            &ObjectPath::new(vec![dbname.clone()]),
            ListOpts {
                limit: 500,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("catalog.children(schemas) must not hang while a cursor is open")
    .expect("list schemas");
    assert!(schemas
        .items
        .iter()
        .any(|n| &*n.path.parts()[1] == "pg_catalog"));

    // 2. Listing relations exercises the `relkind::text` decode (F3) *and*
    //    does it on a second session while the first is pinned.
    let relations = tokio::time::timeout(
        DEADLINE,
        catalog.children(
            &ObjectPath::new(vec![dbname.clone(), Arc::from("pg_catalog")]),
            ListOpts {
                limit: 50,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("catalog.children(relations) must not hang while a cursor is open")
    .expect("list relations — decoding relkind must not panic");
    assert!(!relations.items.is_empty(), "pg_catalog has relations");
    assert!(
        relations
            .items
            .iter()
            .any(|n| n.kind == datagrep_api::catalog::ObjectKind::View),
        "pg_catalog is full of views; if none came back as View, relkind decoded wrong"
    );

    // `describe` reads relkind too, from its own query — the second of the
    // two sites that panicked. Point it at a view so the decoded value is
    // actually load-bearing.
    let detail = tokio::time::timeout(
        DEADLINE,
        catalog.describe(&ObjectPath::new(vec![
            dbname.clone(),
            Arc::from("pg_catalog"),
            Arc::from("pg_views"),
        ])),
    )
    .await
    .expect("describe must not hang while a cursor is open")
    .expect("describe — decoding relkind must not panic");
    assert_eq!(detail.node.kind, datagrep_api::catalog::ObjectKind::View);

    // 3. A whole second query must not queue behind the open cursor either.
    let mut other = tokio::time::timeout(DEADLINE, conn.execute(Request::native("SELECT 42")))
        .await
        .expect("a second execute() must not hang while a cursor is open")
        .expect("second query");
    let batch = tokio::time::timeout(DEADLINE, other.next_batch(FetchHint::default()))
        .await
        .expect("second query's batch must not hang")
        .expect("batch")
        .expect("one row");
    match batch.payload {
        datagrep_api::driver::Payload::Rows(rows) => assert_eq!(rows[0][0], Value::I64(42)),
        other => panic!("expected Rows, got {other:?}"),
    }

    // 4. …and the original cursor is still perfectly usable afterwards.
    let more = tokio::time::timeout(DEADLINE, cursor.next_batch(FetchHint::default()))
        .await
        .expect("the original cursor must still stream")
        .expect("batch")
        .expect("more rows");
    match &more.payload {
        datagrep_api::driver::Payload::Rows(rows) => assert!(!rows.is_empty()),
        other => panic!("expected Rows, got {other:?}"),
    }

    // 5. Closing the connection must not block on the still-pinned session.
    tokio::time::timeout(DEADLINE, conn.close())
        .await
        .expect("close() must not hang behind an open cursor")
        .expect("close");
}

/// The other half of "never hang": once every pooled session is pinned, an
/// acquire *waits* — with a deadline — instead of blocking forever, and is
/// served the moment a session comes back. Proven by pinning
/// `MAX_SESSIONS` un-drained cursors and then racing one more query against
/// releasing one of them.
#[tokio::test]
#[ignore]
async fn a_query_at_the_session_cap_waits_and_is_served_not_wedged() {
    let conn = connect().await;

    let mut cursors = Vec::new();
    for _ in 0..datagrep_drv_postgres::pool::MAX_SESSIONS {
        let mut cursor = conn
            .execute(Request::native(
                "SELECT g FROM generate_series(1, 1000) AS g",
            ))
            .await
            .expect("execute");
        // Pull one short batch and stop: the portal is *not* drained, so this
        // cursor keeps its session pinned.
        cursor
            .next_batch(FetchHint {
                max_rows: 10,
                ..FetchHint::default()
            })
            .await
            .expect("batch")
            .expect("rows");
        cursors.push(cursor);
    }

    // Every session is now pinned. One more query has to wait — so release a
    // cursor while it waits and check it gets served promptly.
    let spare = cursors.pop().expect("a cursor to release");
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(15),
            conn.execute(Request::native("SELECT 7")),
        ),
        async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(spare);
        },
    );
    let mut cursor = result
        .expect("a query at the session cap must wait with a deadline, never hang")
        .expect("and must succeed once a session is released");
    let batch = cursor
        .next_batch(FetchHint::default())
        .await
        .expect("batch")
        .expect("one row");
    match batch.payload {
        datagrep_api::driver::Payload::Rows(rows) => assert_eq!(rows[0][0], Value::I64(7)),
        other => panic!("expected Rows, got {other:?}"),
    }

    // The still-pinned cursors are untouched by any of that.
    for cursor in cursors.iter_mut() {
        let more = cursor
            .next_batch(FetchHint::default())
            .await
            .expect("the pinned cursors must still stream")
            .expect("more rows");
        assert!(matches!(
            more.payload,
            datagrep_api::driver::Payload::Rows(_)
        ));
    }
}

/// `set_read_only` must bind the whole logical connection, not just whichever
/// socket happened to be idle when it was called. Once a connection can own
/// several sessions, a write slipping onto a freshly dialled one would turn a
/// safety switch into a lie — so this pins the first session with an open
/// cursor and checks the write is still refused on the second.
#[tokio::test]
#[ignore]
async fn read_only_binds_every_pooled_session_not_just_the_first() {
    let conn = connect().await;
    conn.execute(Request::native("DROP TABLE IF EXISTS datagrep_ro_test"))
        .await
        .ok();
    conn.execute(Request::native("CREATE TABLE datagrep_ro_test (id int)"))
        .await
        .expect("create");

    assert_eq!(
        conn.set_read_only(true).await.expect("set read only"),
        datagrep_api::driver::Enforcement::Server
    );

    // Pin the session that `set_read_only` just configured, so the write
    // below is forced onto a different, newly dialled one.
    let mut cursor = conn
        .execute(Request::native(
            "SELECT g FROM generate_series(1, 1000) AS g",
        ))
        .await
        .expect("execute");
    cursor
        .next_batch(FetchHint {
            max_rows: 10,
            ..FetchHint::default()
        })
        .await
        .expect("batch")
        .expect("rows");

    let write = conn
        .execute(Request::native("INSERT INTO datagrep_ro_test VALUES (1)"))
        .await;
    // SQLSTATE 25006 = read_only_sql_transaction: the *server* refused it, so
    // this cannot pass for some unrelated reason (a busy pool, a missing
    // table). Read-only is a property of the connection, not of one socket.
    match write {
        Err(datagrep_api::error::DbError::Query { code, .. }) => {
            assert_eq!(
                code.as_deref(),
                Some("25006"),
                "expected a server-side read-only refusal"
            )
        }
        Err(other) => panic!("expected a server-side read-only refusal, got {other:?}"),
        Ok(_) => panic!(
            "the write succeeded on a freshly dialled pooled session — set_read_only did not \
             bind the whole connection"
        ),
    }

    conn.set_read_only(false).await.expect("back to read-write");
    drop(cursor);
    conn.execute(Request::native("DROP TABLE datagrep_ro_test"))
        .await
        .ok();
}

#[tokio::test]
#[ignore]
async fn non_select_returns_ack_shape_without_a_portal() {
    let conn = connect().await;
    conn.execute(Request::native("DROP TABLE IF EXISTS datagrep_ack_test"))
        .await
        .ok();
    conn.execute(Request::native("CREATE TABLE datagrep_ack_test (id int)"))
        .await
        .expect("create");
    let mut cursor = conn
        .execute(Request::native(
            "INSERT INTO datagrep_ack_test VALUES (1), (2), (3)",
        ))
        .await
        .expect("insert");
    match cursor.shape() {
        Shape::Ack { affected, .. } => assert_eq!(*affected, Some(3)),
        other => panic!("expected Ack, got {other:?}"),
    }
    assert!(
        cursor
            .next_batch(FetchHint::default())
            .await
            .unwrap()
            .is_some(),
        "one empty batch, then done"
    );
    assert!(cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .is_none());
    conn.execute(Request::native("DROP TABLE datagrep_ack_test"))
        .await
        .ok();
}

#[tokio::test]
#[ignore]
async fn quote_ident_survives_a_hostile_identifier() {
    // End-to-end proof that identifiers with an embedded quote round-trip
    // safely through `quote_ident` rather than breaking the statement.
    let conn = connect().await;
    let hostile = "weird\"table";
    conn.execute(Request::native(format!(
        "DROP TABLE IF EXISTS {}",
        datagrep_drv_postgres::sql::quote_ident(hostile).unwrap()
    )))
    .await
    .ok();
    let create = format!(
        "CREATE TABLE {} (id int)",
        datagrep_drv_postgres::sql::quote_ident(hostile).unwrap()
    );
    conn.execute(Request::native(create))
        .await
        .expect("create with hostile identifier");
    let drop = format!(
        "DROP TABLE {}",
        datagrep_drv_postgres::sql::quote_ident(hostile).unwrap()
    );
    let result = conn.execute(Request::native(drop)).await;
    assert!(
        result.is_ok(),
        "should be able to drop the table it just created"
    );
}
