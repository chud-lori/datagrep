//! Integration tests against a live MySQL or MariaDB server.
//!
//! Ignored by default; run with a server URL in `DATAGREP_TEST_MYSQL`:
//!
//! ```sh
//! docker run --rm -d --name dg-mysql -p 3306:3306 -e MYSQL_ROOT_PASSWORD=secret mysql:8
//! DATAGREP_TEST_MYSQL='mysql://root:secret@127.0.0.1:3306/mysql' \
//!     cargo test -p datagrep-drv-mysql -- --ignored
//! docker stop dg-mysql
//! ```
//!
//! The same suite runs against MariaDB (`mariadb:11`) by pointing the URL at
//! it — see `tests/README.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use datagrep_api::config::ResolvedConfig;
use datagrep_api::driver::{ConnectCtx, Connection, FetchHint, Payload};
use datagrep_api::request::{ExecOpts, Op, Request};
use datagrep_api::value::{TzSpec, Value};
use datagrep_api::{Caps, DbError, Driver, ObjectPath};

use datagrep_drv_mysql::MySqlDriver;

fn test_url() -> String {
    std::env::var("DATAGREP_TEST_MYSQL")
        .expect("set DATAGREP_TEST_MYSQL=mysql://root:secret@127.0.0.1:3306/mysql to run")
}

async fn connect() -> Box<dyn Connection> {
    let driver = MySqlDriver::new();
    let cfg = driver.parse_url(&test_url()).expect("parse test url");
    let resolved = ResolvedConfig::without_secrets(cfg);
    driver
        .connect(&resolved, ConnectCtx::default())
        .await
        .expect("connect to test server")
}

fn native(sql: impl Into<Arc<str>>) -> Request {
    Request::native(sql)
}

fn native_params(sql: impl Into<Arc<str>>, params: Vec<Value>) -> Request {
    Request::Native {
        text: sql.into(),
        params,
        opts: ExecOpts::default(),
    }
}

/// Run a statement expected to produce an Ack (DDL, INSERT, …).
async fn run_ddl(conn: &dyn Connection, sql: &str) {
    let mut cur = conn.execute(native(sql)).await.unwrap_or_else(|e| {
        panic!("statement failed: {sql}: {e}");
    });
    while cur
        .next_batch(FetchHint::default())
        .await
        .expect("ack")
        .is_some()
    {}
}

/// Collect every row of a request (test helper only — production code never
/// collects).
async fn collect_rows(conn: &dyn Connection, req: Request) -> Vec<Vec<Value>> {
    let mut cur = conn.execute(req).await.expect("execute");
    let mut rows = Vec::new();
    while let Some(batch) = cur.next_batch(FetchHint::default()).await.expect("batch") {
        if let Payload::Rows(mut r) = batch.payload {
            rows.append(&mut r);
        }
    }
    rows
}

/// A 100,000-row SELECT with no table: five cross-joined digit derived
/// tables. Portable across MySQL and MariaDB (no CTE depth variables).
fn seq_100k_sql() -> String {
    let digits = "(SELECT 0 x UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL \
         SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL \
         SELECT 8 UNION ALL SELECT 9)";
    format!(
        "SELECT t1.x + t2.x*10 + t3.x*100 + t4.x*1000 + t5.x*10000 AS n \
         FROM {digits} t1, {digits} t2, {digits} t3, {digits} t4, {digits} t5"
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn server_info_reports_actual_product_and_version() {
    let conn = connect().await;
    let info = conn.server_info();
    assert!(
        &*info.product == "MySQL" || &*info.product == "MariaDB",
        "product: {}",
        info.product
    );
    assert!(!info.version.is_empty());
    // MariaDB must be reported as MariaDB, not mislabeled MySQL.
    if info.version.to_ascii_lowercase().contains("mariadb") {
        assert_eq!(&*info.product, "MariaDB");
    }
    let caps = conn.capabilities();
    assert!(caps.flags.contains(Caps::SERVER_CANCEL));
    println!(
        "connected to {} {} (EXPLAIN_ANALYZE={})",
        info.product,
        info.version,
        caps.flags.contains(Caps::EXPLAIN_ANALYZE)
    );
    conn.close().await.unwrap();
}

/// Streaming proof: with 100k rows in flight, the first batch surfaces after
/// only `max_rows` rows and long before the stream is complete — the driver
/// never collects.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn streaming_100k_rows_first_batch_arrives_before_completion() {
    let conn = connect().await;
    let started = Instant::now();
    let mut cur = conn.execute(native(seq_100k_sql())).await.expect("execute");

    let hint = FetchHint {
        max_rows: 500,
        ..FetchHint::default()
    };
    let first = cur
        .next_batch(hint)
        .await
        .expect("first batch")
        .expect("at least one batch");
    let t_first = started.elapsed();
    let first_len = match &first.payload {
        Payload::Rows(rows) => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert!(
        first_len <= 500,
        "first batch must respect the hint, got {first_len} rows"
    );
    assert_eq!(
        cur.stats().rows,
        first_len as u64,
        "stats must show only the streamed prefix, not a buffered result"
    );

    let mut total = first_len as u64;
    while let Some(batch) = cur.next_batch(hint).await.expect("batch") {
        if let Payload::Rows(rows) = batch.payload {
            total += rows.len() as u64;
        }
    }
    let t_done = started.elapsed();
    assert_eq!(total, 100_000);
    assert!(
        t_first < t_done,
        "first batch ({t_first:?}) must arrive before completion ({t_done:?})"
    );
    println!("first batch after {t_first:?}, full 100k after {t_done:?}");
    conn.close().await.unwrap();
}

/// DECIMAL round-trips as an exact string — never through f64, which would
/// silently round it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn decimal_round_trips_as_string() {
    let conn = connect().await;
    let db = "dgit_decimal";
    run_ddl(&*conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    run_ddl(&*conn, &format!("CREATE DATABASE {db}")).await;
    run_ddl(
        &*conn,
        &format!("CREATE TABLE {db}.d (v DECIMAL(38,18) NOT NULL)"),
    )
    .await;

    // The value is bound as a Decimal parameter, and cannot survive f64.
    let exact = "12345678901234567890.123456789012345678";
    run_ddl_req(
        &*conn,
        native_params(
            format!("INSERT INTO {db}.d (v) VALUES (?)"),
            vec![Value::Decimal(Arc::from(exact))],
        ),
    )
    .await;

    let rows = collect_rows(&*conn, native(format!("SELECT v FROM {db}.d"))).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Value::Decimal(Arc::from(exact)),
        "DECIMAL must survive byte-exact"
    );

    run_ddl(&*conn, &format!("DROP DATABASE {db}")).await;
    conn.close().await.unwrap();
}

async fn run_ddl_req(conn: &dyn Connection, req: Request) {
    let mut cur = conn.execute(req).await.expect("execute");
    while cur
        .next_batch(FetchHint::default())
        .await
        .expect("ack")
        .is_some()
    {}
}

/// Cancel mid-`SLEEP(30)` via `KILL QUERY` from the pooled second
/// connection; control returns quickly and the connection stays usable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn cancel_mid_sleep_returns_cancelled_and_connection_survives() {
    let conn = connect().await;
    let canceller = conn.canceller();
    assert_eq!(
        canceller.kind(),
        datagrep_api::CancelKind::ServerSide,
        "MySQL cancellation is a real server-side kill"
    );

    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        canceller.cancel().await
    });

    let started = Instant::now();
    let outcome: Result<(), DbError> = async {
        let mut cur = conn.execute(native("SELECT SLEEP(30)")).await?;
        while cur.next_batch(FetchHint::default()).await?.is_some() {}
        Ok(())
    }
    .await;
    let elapsed = started.elapsed();

    match outcome {
        Err(DbError::Cancelled) => {}
        // `SELECT SLEEP(30)` interrupted mid-sleep returns a row (value 1)
        // on some versions instead of erroring; then the query just ends
        // early. Both are legitimate KILL observations — but it must NOT
        // take the full 30s.
        Ok(()) => {}
        Err(other) => panic!("expected Cancelled (or early end), got {other}"),
    }
    assert!(
        elapsed < Duration::from_secs(15),
        "cancel must not wait out the sleep; took {elapsed:?}"
    );
    let cancel_outcome = cancel_task.await.expect("join").expect("cancel");
    println!("cancel outcome: {cancel_outcome:?}, elapsed {elapsed:?}");

    // The connection must be reusable after the kill.
    let rows = collect_rows(&*conn, native("SELECT 41 + 1")).await;
    assert_eq!(rows, vec![vec![Value::I64(42)]]);
    conn.close().await.unwrap();
}

/// The known mysql gotcha: an undrained result poisons the connection and
/// the error surfaces on the NEXT query. The driver must drain on every exit
/// path — including a cursor that is simply dropped mid-stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn undrained_result_does_not_poison_connection() {
    let conn = connect().await;

    // Case 1: pull one batch of a 100k-row result, then drop the cursor
    // without close() or draining.
    {
        let mut cur = conn.execute(native(seq_100k_sql())).await.expect("execute");
        let first = cur
            .next_batch(FetchHint {
                max_rows: 100,
                ..FetchHint::default()
            })
            .await
            .expect("first batch");
        assert!(first.is_some());
        // cur dropped here, ~99_900 rows still on the wire.
    }
    let rows = collect_rows(&*conn, native("SELECT 1 + 1")).await;
    assert_eq!(rows, vec![vec![Value::I64(2)]], "conn poisoned by drop");

    // Case 2: drop the cursor without fetching anything at all.
    {
        let cur = conn.execute(native(seq_100k_sql())).await.expect("execute");
        drop(cur);
    }
    let rows = collect_rows(&*conn, native("SELECT 2 + 2")).await;
    assert_eq!(rows, vec![vec![Value::I64(4)]], "conn poisoned by drop");

    // Case 3: explicit close() mid-stream.
    {
        let mut cur = conn.execute(native(seq_100k_sql())).await.expect("execute");
        let _ = cur
            .next_batch(FetchHint {
                max_rows: 10,
                ..FetchHint::default()
            })
            .await
            .expect("batch");
        cur.close().await.expect("close");
    }
    let rows = collect_rows(&*conn, native("SELECT 3 + 3")).await;
    assert_eq!(rows, vec![vec![Value::I64(6)]], "conn poisoned by close");

    conn.close().await.unwrap();
}

/// Catalog: lazy per-level listing plus `describe()` with the cross-driver
/// `indexes` JSON array (name, ordered columns, unique, primary).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn catalog_lists_levels_and_describe_reports_indexes() {
    let conn = connect().await;
    let db = "dgit_catalog";
    run_ddl(&*conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    run_ddl(&*conn, &format!("CREATE DATABASE {db}")).await;
    run_ddl(
        &*conn,
        &format!(
            "CREATE TABLE {db}.users (\
               tenant INT NOT NULL, \
               id INT NOT NULL AUTO_INCREMENT, \
               email VARCHAR(100), \
               first_name VARCHAR(50), \
               last_name VARCHAR(50), \
               PRIMARY KEY (tenant, id), \
               KEY idx_id (id), \
               UNIQUE KEY idx_email (email), \
               KEY idx_name (last_name, first_name))"
        ),
    )
    .await;
    run_ddl(
        &*conn,
        &format!("CREATE VIEW {db}.v_users AS SELECT id, email FROM {db}.users"),
    )
    .await;

    let catalog = conn.catalog();

    // Level shape: database → table → column, no fake schema tier.
    let levels = catalog.levels();
    assert_eq!(levels.len(), 3);

    // Root: databases (one bounded query).
    let dbs = catalog
        .children(&ObjectPath::root(), Default::default())
        .await
        .expect("databases");
    assert!(
        dbs.items.iter().any(|n| n.path.to_string() == db),
        "created database must be listed"
    );

    // Tables under the database, with table/view distinction.
    let tables = catalog
        .children(&ObjectPath::root().child(db), Default::default())
        .await
        .expect("tables");
    let users = tables
        .items
        .iter()
        .find(|n| n.path.parts().last().map(|s| &**s) == Some("users"))
        .expect("users listed");
    assert_eq!(users.kind, datagrep_api::ObjectKind::Table);
    let view = tables
        .items
        .iter()
        .find(|n| n.path.parts().last().map(|s| &**s) == Some("v_users"))
        .expect("view listed");
    assert_eq!(view.kind, datagrep_api::ObjectKind::View);

    // Columns in declared order.
    let cols = catalog
        .children(
            &ObjectPath::root().child(db).child("users"),
            Default::default(),
        )
        .await
        .expect("columns");
    let names: Vec<String> = cols
        .items
        .iter()
        .map(|n| n.path.parts().last().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["tenant", "id", "email", "first_name", "last_name"],
        "columns must come back in ordinal order"
    );

    // describe(): schema + identity + the indexes array.
    let detail = catalog
        .describe(&ObjectPath::root().child(db).child("users"))
        .await
        .expect("describe");
    let schema = detail.schema.expect("declared schema");
    assert_eq!(schema.fields.len(), 5);
    let identity = schema.identity.expect("pk identity");
    assert_eq!(
        identity.field_indices,
        vec![0, 1],
        "composite pk (tenant, id)"
    );
    let id_field = &schema.fields[1];
    assert!(id_field
        .flags
        .contains(datagrep_api::FieldFlags::AUTO_GENERATED));

    let indexes = detail
        .extra
        .iter()
        .find(|(k, _)| &**k == "indexes")
        .map(|(_, v)| v.to_string())
        .expect("indexes array present");
    assert!(
        indexes
            .contains(r#""name":"PRIMARY","columns":["tenant","id"],"unique":true,"primary":true"#),
        "PRIMARY with ordered columns: {indexes}"
    );
    assert!(
        indexes.contains(r#""name":"idx_email","columns":["email"],"unique":true,"primary":false"#),
        "unique secondary index: {indexes}"
    );
    assert!(
        indexes
            .contains(r#""name":"idx_name","columns":["last_name","first_name"],"unique":false"#),
        "composite non-unique index keeps column order: {indexes}"
    );

    // Completion: bounded server-side prefix query.
    let completions = catalog
        .complete(datagrep_api::CompletionCtx {
            text: Arc::from("SELECT * FROM use"),
            offset: 17,
            scope: Some(ObjectPath::root().child(db)),
        })
        .await
        .expect("complete");
    assert!(
        completions.iter().any(|c| &*c.label == "users"),
        "prefix completion should find the table: {completions:?}"
    );

    run_ddl(&*conn, &format!("DROP DATABASE {db}")).await;
    conn.close().await.unwrap();
}

/// The type mapping against a real server: every documented conversion in
/// one round trip, both text protocol (no params) and binary (with params).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn type_menagerie_decodes_honestly() {
    let conn = connect().await;
    let db = "dgit_types";
    run_ddl(&*conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    run_ddl(&*conn, &format!("CREATE DATABASE {db}")).await;
    run_ddl(
        &*conn,
        &format!(
            "CREATE TABLE {db}.t (\
               id INT NOT NULL PRIMARY KEY, \
               b TINYINT(1), \
               u BIGINT UNSIGNED, \
               dec38 DECIMAL(38,10), \
               f DOUBLE, \
               s VARCHAR(50), \
               bin VARBINARY(16), \
               d DATE, \
               tm TIME, \
               dt DATETIME, \
               ts TIMESTAMP NULL, \
               yr YEAR, \
               js JSON, \
               en ENUM('red','green'), \
               st SET('a','b'), \
               bt BIT(8), \
               nul INT)"
        ),
    )
    .await;
    run_ddl(
        &*conn,
        &format!(
            "INSERT INTO {db}.t VALUES (\
               1, TRUE, 18446744073709551615, '1234567890.1234567890', 1.5, 'héllo', \
               X'DEADBEEF', '2024-03-01', '-838:59:59', \
               '2024-01-02 03:04:05', '2024-01-02 03:04:05', 2024, \
               '{{\"k\": [1, 2]}}', 'green', 'a,b', b'10100001', NULL)"
        ),
    )
    .await;

    // Text protocol (no params) and binary protocol (bound param) must
    // decode identically.
    for req in [
        native(format!("SELECT * FROM {db}.t")),
        native_params(
            format!("SELECT * FROM {db}.t WHERE id = ?"),
            vec![Value::I64(1)],
        ),
    ] {
        let rows = collect_rows(&*conn, req).await;
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r[0], Value::I64(1));
        assert_eq!(r[1], Value::Bool(true), "TINYINT(1) is Bool");
        assert_eq!(r[2], Value::U64(u64::MAX), "BIGINT UNSIGNED max");
        assert_eq!(
            r[3],
            Value::Decimal(Arc::from("1234567890.1234567890")),
            "DECIMAL exact, string-backed"
        );
        assert_eq!(r[4], Value::F64(1.5));
        assert_eq!(r[5], Value::Str(Arc::from("héllo")));
        assert_eq!(
            r[6],
            Value::Bytes(bytes::Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF]))
        );
        assert_eq!(r[7], Value::Date(19783), "2024-03-01");
        assert_eq!(
            r[8],
            Value::Time {
                nanos: -((838 * 3600 + 59 * 60 + 59) * 1_000_000_000i64)
            },
            "TIME is a signed duration"
        );
        let expected_micros = 19724 * 86_400_000_000i64 // 2024-01-02
            + (3 * 3600 + 4 * 60 + 5) * 1_000_000;
        assert_eq!(
            r[9],
            Value::Timestamp {
                micros: expected_micros,
                tz: TzSpec::Naive
            },
            "DATETIME is naive"
        );
        assert_eq!(
            r[10],
            Value::Timestamp {
                micros: expected_micros,
                tz: TzSpec::Utc
            },
            "TIMESTAMP is UTC (session pinned to +00:00)"
        );
        assert_eq!(r[11], Value::I64(2024), "YEAR");
        // MySQL has a native JSON wire type → Value::Json. MariaDB's JSON is
        // an alias for LONGTEXT (no JSON wire type exists), so Str is the
        // truthful decode of what that server actually declares.
        let is_mariadb = &*conn.server_info().product == "MariaDB";
        match (&r[12], is_mariadb) {
            (Value::Json(j), false) => assert!(j.contains("\"k\""), "raw JSON text: {j}"),
            (Value::Str(j), true) => assert!(j.contains("\"k\""), "JSON-as-longtext: {j}"),
            (other, _) => panic!("unexpected JSON decode (mariadb={is_mariadb}): {other:?}"),
        }
        assert_eq!(r[13], Value::Str(Arc::from("green")), "ENUM");
        assert_eq!(r[14], Value::Str(Arc::from("a,b")), "SET");
        assert_eq!(
            r[15],
            Value::Bytes(bytes::Bytes::from_static(&[0b1010_0001])),
            "BIT"
        );
        assert_eq!(r[16], Value::Null);
    }

    run_ddl(&*conn, &format!("DROP DATABASE {db}")).await;
    conn.close().await.unwrap();
}

/// Multi-statement scripts (split by datagrep-lang): preceding statements
/// execute, the last one streams.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn multi_statement_script_streams_last_result() {
    let conn = connect().await;
    let db = "dgit_multi";
    run_ddl(&*conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    let script = format!(
        "CREATE DATABASE {db}; \
         CREATE TABLE {db}.t (n INT); \
         INSERT INTO {db}.t VALUES (1), (2), (3); \
         SELECT COUNT(*) FROM {db}.t"
    );
    let rows = collect_rows(&*conn, native(script)).await;
    assert_eq!(rows, vec![vec![Value::I64(3)]]);
    run_ddl(&*conn, &format!("DROP DATABASE {db}")).await;
    conn.close().await.unwrap();
}

/// `set_read_only` is server-enforced, and honestly reported as such.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn set_read_only_is_server_enforced() {
    let conn = connect().await;
    let db = "dgit_ro";
    run_ddl(&*conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    run_ddl(&*conn, &format!("CREATE DATABASE {db}")).await;
    run_ddl(&*conn, &format!("CREATE TABLE {db}.t (n INT)")).await;

    let enforcement = conn.set_read_only(true).await.expect("set read only");
    assert_eq!(enforcement, datagrep_api::Enforcement::Server);

    let write = conn
        .execute(native(format!("INSERT INTO {db}.t VALUES (1)")))
        .await;
    let failed = match write {
        Err(_) => true,
        Ok(mut cur) => cur.next_batch(FetchHint::default()).await.is_err(),
    };
    assert!(failed, "the server itself must refuse the write");

    conn.set_read_only(false).await.expect("set read write");
    run_ddl(&*conn, &format!("INSERT INTO {db}.t VALUES (1)")).await;

    run_ddl(&*conn, &format!("DROP DATABASE {db}")).await;
    conn.close().await.unwrap();
}

/// EXPLAIN and (where the server supports it) EXPLAIN ANALYZE / ANALYZE,
/// gated by the version-probed capability.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn explain_and_explain_analyze_respect_capability() {
    let conn = connect().await;
    let explain = Request::Op(Op::Explain {
        inner: Box::new(native("SELECT 1")),
        analyze: false,
    });
    let rows = collect_rows(&*conn, explain).await;
    assert!(!rows.is_empty(), "EXPLAIN produces a plan");

    let analyze = Request::Op(Op::Explain {
        inner: Box::new(native("SELECT 1")),
        analyze: true,
    });
    if conn.capabilities().flags.contains(Caps::EXPLAIN_ANALYZE) {
        let rows = collect_rows(&*conn, analyze).await;
        assert!(!rows.is_empty(), "EXPLAIN ANALYZE produces timed plan rows");
    } else {
        assert!(matches!(
            conn.execute(analyze).await,
            Err(DbError::Unsupported { .. })
        ));
    }
    conn.close().await.unwrap();
}

/// Transactions: rollback discards, commit persists; savepoints work via
/// native SQL inside the pinned transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live server: set DATAGREP_TEST_MYSQL"]
async fn transactions_commit_rollback_and_savepoints() {
    let conn = connect().await;
    let db = "dgit_txn";
    run_ddl(&*conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    run_ddl(&*conn, &format!("CREATE DATABASE {db}")).await;
    run_ddl(&*conn, &format!("CREATE TABLE {db}.t (n INT)")).await;

    // Rollback discards.
    let txn = conn.begin(Default::default()).await.expect("begin");
    let mut cur = txn
        .execute(native(format!("INSERT INTO {db}.t VALUES (1)")))
        .await
        .expect("insert in txn");
    while cur
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .is_some()
    {}
    drop(cur);
    txn.rollback().await.expect("rollback");
    let rows = collect_rows(&*conn, native(format!("SELECT COUNT(*) FROM {db}.t"))).await;
    assert_eq!(rows, vec![vec![Value::I64(0)]], "rollback must discard");

    // Commit persists; a savepoint (NESTED_TRANSACTIONS) partially unwinds.
    let txn = conn.begin(Default::default()).await.expect("begin");
    for sql in [
        format!("INSERT INTO {db}.t VALUES (1)"),
        "SAVEPOINT sp1".to_string(),
        format!("INSERT INTO {db}.t VALUES (2)"),
        "ROLLBACK TO SAVEPOINT sp1".to_string(),
    ] {
        let mut cur = txn.execute(native(sql)).await.expect("txn stmt");
        while cur
            .next_batch(FetchHint::default())
            .await
            .unwrap()
            .is_some()
        {}
    }
    txn.commit().await.expect("commit");
    let rows = collect_rows(&*conn, native(format!("SELECT COUNT(*) FROM {db}.t"))).await;
    assert_eq!(
        rows,
        vec![vec![Value::I64(1)]],
        "only the pre-savepoint insert survives"
    );

    run_ddl(&*conn, &format!("DROP DATABASE {db}")).await;
    conn.close().await.unwrap();
}
