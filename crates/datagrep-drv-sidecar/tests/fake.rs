use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use datagrep_api::caps::{Caps, LanguageId, ParamStyle};
use datagrep_api::config::{ConfigValue, ConnectionConfig, ResolvedConfig};
use datagrep_api::driver::{CancelOutcome, ConnectCtx, Connection, Driver, FetchHint};
use datagrep_api::request::{ExecOpts, Op, Request};
use datagrep_api::shape::{ObjectPath, Shape};
use datagrep_api::value::{TzSpec, Value};
use datagrep_api::{DbError, Enforcement, ListOpts, ObjectKind, Payload};
use datagrep_drv_sidecar::{EngineManifest, SidecarDriver};

static FAKE: EngineManifest = EngineManifest {
    id: "fake",
    display_name: "Fake",
    program: "unused",
    url_schemes: &["fake"],
    language: LanguageId::Unclassified,
    flags: Caps::SERVER_CANCEL,
    param_style: ParamStyle::None,
    identifier_quote: '"',
    default_port: 1,
    path_key: "database",
    path_label: "Database",
    levels: &[("schema", ObjectKind::Schema), ("table", ObjectKind::Table)],
    env: &[],
};

const PASSWORD: &str = "hunter2-sidecar-test";

fn driver(mode: &str) -> SidecarDriver {
    SidecarDriver::with_program(
        &FAKE,
        env!("CARGO_BIN_EXE_datagrep-sidecar-fake"),
        vec![mode.into()],
    )
}

fn config(password: &str) -> ResolvedConfig {
    let mut values = BTreeMap::new();
    values.insert("host".into(), ConfigValue::Str("db".into()));
    values.insert("user".into(), ConfigValue::Str("scott".into()));
    values.insert("password".into(), ConfigValue::Str(password.into()));
    ResolvedConfig::without_secrets(ConnectionConfig {
        driver: Arc::from("fake"),
        values,
    })
}

async fn within<T>(limit: Duration, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(limit, fut)
        .await
        .expect("hung instead of failing")
}

async fn connect(d: &SidecarDriver) -> Box<dyn Connection> {
    within(
        Duration::from_secs(10),
        d.connect(&config(PASSWORD), ConnectCtx::default()),
    )
    .await
    .expect("connect")
}

fn hint(max_rows: u32) -> FetchHint {
    FetchHint {
        max_rows,
        ..FetchHint::default()
    }
}

fn detail<'a>(conn: &'a dyn Connection, key: &str) -> &'a str {
    conn.server_info()
        .details
        .iter()
        .find(|(k, _)| &**k == key)
        .map(|(_, v)| &**v)
        .unwrap_or_default()
}

fn assert_protocol(err: DbError) -> String {
    match err {
        DbError::Protocol(m) => m,
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn streams_in_hint_sized_batches_until_eof() {
    let d = driver("ok");
    let conn = connect(&d).await;
    let mut cursor = conn.execute(Request::native("stream 1234")).await.unwrap();
    let Shape::Table(schema) = cursor.shape() else {
        panic!("expected a table")
    };
    assert_eq!(schema.fields.len(), 2);
    let mut sizes = Vec::new();
    while let Some(batch) = cursor.next_batch(hint(500)).await.unwrap() {
        let Payload::Rows(rows) = batch.payload else {
            panic!("expected rows")
        };
        sizes.push(rows.len());
    }
    assert_eq!(sizes, [500, 500, 234]);
    assert_eq!(cursor.stats().rows, 1234);
    assert_eq!(cursor.stats().batches, 3);
    assert!(cursor.next_batch(hint(500)).await.unwrap().is_none());
    cursor.close().await.unwrap();
    conn.close().await.unwrap();
}

#[tokio::test]
async fn cells_decode_against_the_declared_shape() {
    let conn = connect(&driver("ok")).await;
    let mut cursor = conn.execute(Request::native("types")).await.unwrap();
    let batch = cursor.next_batch(hint(10)).await.unwrap().unwrap();
    let Payload::Rows(rows) = batch.payload else {
        panic!("expected rows")
    };
    let row = &rows[0];
    assert_eq!(row[0], Value::Bool(true));
    assert_eq!(row[1], Value::I64(i64::MIN));
    assert_eq!(row[2], Value::Decimal(Arc::from("12345678901234567890.5")));
    assert_eq!(row[3], Value::Null);
    assert_eq!(
        row[4],
        Value::Bytes(bytes::Bytes::from_static(&[0, 1, 255]))
    );
    assert_eq!(
        row[5],
        Value::Timestamp {
            micros: 1_700_000_000_000_000,
            tz: TzSpec::Utc
        }
    );
    assert_eq!(
        row[6],
        Value::Uuid([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef
        ])
    );
    assert!(matches!(row[7], Value::F64(f) if f.is_nan()));
    assert_eq!(row[8], Value::Str(Arc::from("not a number")));
}

#[tokio::test]
async fn an_ack_has_no_rows_and_needs_no_close() {
    let conn = connect(&driver("ok")).await;
    let mut cursor = conn.execute(Request::native("ack")).await.unwrap();
    assert!(matches!(
        cursor.shape(),
        Shape::Ack {
            affected: Some(3),
            ..
        }
    ));
    assert!(cursor.next_batch(hint(10)).await.unwrap().is_none());
    cursor.close().await.unwrap();
}

#[tokio::test]
async fn engine_errors_keep_their_kind_and_a_query_error_keeps_the_connection() {
    let conn = connect(&driver("ok")).await;
    match conn.execute(Request::native("boom")).await {
        Err(DbError::Query { code, message, .. }) => {
            assert_eq!(code.as_deref(), Some("FAKE-1"));
            assert!(message.contains("ORA-00942"), "{message}");
        }
        other => panic!("expected a query error, got {:?}", other.err()),
    }
    conn.ping().await.expect("still usable");
    assert!(matches!(
        conn.execute(Request::native("panic")).await.err(),
        Some(DbError::DriverPanic(_))
    ));
}

#[tokio::test]
async fn a_sidecar_cannot_answer_with_a_safety_error() {
    let conn = connect(&driver("ok")).await;
    let err = conn.execute(Request::native("safety")).await.err().unwrap();
    assert_protocol(err);
}

#[tokio::test]
async fn generated_operations_are_unsupported() {
    let conn = connect(&driver("ok")).await;
    let op = Request::Op(Op::Count {
        path: ObjectPath::root(),
        filter: None,
        exact: true,
    });
    assert!(matches!(
        conn.execute(op).await.err(),
        Some(DbError::Unsupported { .. })
    ));
}

#[tokio::test]
async fn a_crash_mid_stream_is_a_clean_error_with_the_secret_masked() {
    let d = driver("ok");
    let conn = connect(&d).await;
    let mut cursor = conn
        .execute(Request::native("crash-after 2"))
        .await
        .unwrap();
    assert!(cursor.next_batch(hint(10)).await.unwrap().is_some());
    let err = within(Duration::from_secs(5), cursor.next_batch(hint(10)))
        .await
        .unwrap_err();
    let message = assert_protocol(err);
    assert!(message.contains("exited"), "{message}");
    assert!(message.contains("lost connection"), "{message}");
    assert!(!message.contains(PASSWORD), "secret leaked: {message}");
    assert!(message.contains("••••"), "{message}");
    assert_protocol(conn.ping().await.unwrap_err());

    // The next connect starts a fresh process.
    let again = connect(&d).await;
    again.ping().await.unwrap();
}

#[tokio::test]
async fn death_by_signal_mid_stream_does_not_hang() {
    let conn = connect(&driver("ok")).await;
    let mut cursor = conn.execute(Request::native("stream 10")).await.unwrap();
    cursor.next_batch(hint(5)).await.unwrap();
    let err = within(
        Duration::from_secs(5),
        conn.execute(Request::native("abort")),
    )
    .await
    .err()
    .unwrap();
    assert_protocol(err);
    assert_protocol(
        within(Duration::from_secs(5), cursor.next_batch(hint(5)))
            .await
            .unwrap_err(),
    );
}

#[tokio::test]
async fn an_oversized_frame_kills_the_sidecar_before_anything_is_allocated() {
    let conn = connect(&driver("ok")).await;
    let started = Instant::now();
    let err = within(
        Duration::from_secs(5),
        conn.execute(Request::native("oversized")),
    )
    .await
    .err()
    .unwrap();
    let message = assert_protocol(err);
    assert!(message.contains("cap"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_protocol(conn.ping().await.unwrap_err());
}

#[tokio::test]
async fn garbage_unsolicited_and_oversized_batches_are_violations() {
    for (stmt, needle) in [("garbage", "unreadable"), ("unsolicited", "not pending")] {
        let conn = connect(&driver("ok")).await;
        let err = within(Duration::from_secs(5), conn.execute(Request::native(stmt)))
            .await
            .err()
            .unwrap();
        let message = assert_protocol(err);
        assert!(message.contains(needle), "{stmt}: {message}");
    }
    let conn = connect(&driver("ok")).await;
    let mut cursor = conn.execute(Request::native("flood")).await.unwrap();
    let message = assert_protocol(cursor.next_batch(hint(3)).await.unwrap_err());
    assert!(message.contains("4 rows for a 3-row fetch"), "{message}");
}

#[tokio::test]
async fn cancel_ends_a_blocked_fetch_and_the_connection_stays_usable() {
    let conn = connect(&driver("ok")).await;
    let mut cursor = conn.execute(Request::native("hang")).await.unwrap();
    let canceller = conn.canceller();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel().await
    });
    let err = within(Duration::from_secs(5), cursor.next_batch(hint(10)))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Cancelled), "{err:?}");
    assert_eq!(cancel.await.unwrap().unwrap(), CancelOutcome::Requested);
    conn.ping().await.unwrap();
}

#[tokio::test]
async fn a_sidecar_that_ignores_cancel_is_killed_after_the_grace_period() {
    let conn = connect(&driver("ok")).await;
    let mut cursor = conn.execute(Request::native("deaf")).await.unwrap();
    let canceller = conn.canceller();
    let started = Instant::now();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel().await
    });
    let err = within(Duration::from_secs(6), cursor.next_batch(hint(10)))
        .await
        .unwrap_err();
    assert_protocol(err);
    assert_eq!(
        cancel.await.unwrap().unwrap(),
        CancelOutcome::ClientAbandoned
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_statement_timeout_has_a_parent_side_backstop() {
    let conn = connect(&driver("ok")).await;
    let req = Request::Native {
        text: Arc::from("sleep"),
        params: Vec::new(),
        opts: ExecOpts {
            timeout: Some(Duration::from_millis(100)),
            ..ExecOpts::default()
        },
    };
    let err = within(Duration::from_secs(5), conn.execute(req))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, DbError::Timeout), "{err:?}");
}

#[tokio::test]
async fn the_secret_travels_only_in_the_connect_frame() {
    // The fake refuses the connect if the password is in its argv, env or config.
    let conn = connect(&driver("ok")).await;
    let env = detail(&*conn, "env");
    for name in env.split(',').filter(|n| !n.is_empty()) {
        assert!(
            [
                "HOME",
                "TMPDIR",
                "LANG",
                "TZ",
                "SSL_CERT_FILE",
                "SSL_CERT_DIR",
                "GOMEMLIMIT"
            ]
            .contains(&name)
                || name.starts_with("LC_"),
            "{name} reached the sidecar"
        );
    }
}

#[tokio::test]
async fn sidecar_stderr_is_redacted_before_it_reaches_an_error() {
    let conn = connect(&driver("ok")).await;
    let err = within(
        Duration::from_secs(5),
        conn.execute(Request::native("leak")),
    )
    .await
    .err()
    .unwrap();
    let message = assert_protocol(err);
    assert!(message.contains("dialing"), "{message}");
    assert!(!message.contains(PASSWORD), "secret leaked: {message}");
}

#[tokio::test]
async fn the_handshake_cannot_change_the_language_or_widen_capabilities() {
    for mode in ["wrong-language", "widen-caps"] {
        let err = within(
            Duration::from_secs(10),
            driver(mode).connect(&config(PASSWORD), ConnectCtx::default()),
        )
        .await
        .err()
        .unwrap();
        let message = assert_protocol(err);
        assert!(
            message.contains("language") || message.contains("capabilities"),
            "{mode}: {message}"
        );
    }
}

#[tokio::test]
async fn a_sidecar_that_never_says_hello_times_out() {
    let err = within(
        Duration::from_secs(10),
        driver("silent").connect(&config(PASSWORD), ConnectCtx::default()),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(&err, DbError::Connect(m) if m.contains("did not answer")),
        "{err:?}"
    );
}

#[tokio::test]
async fn one_process_per_profile_and_it_exits_with_its_last_connection() {
    let d = driver("ok");
    let a = connect(&d).await;
    let b = connect(&d).await;
    let pid = detail(&*a, "pid").to_string();
    assert!(!pid.is_empty());
    assert_eq!(detail(&*b, "pid"), pid, "same profile, same process");
    let other = within(
        Duration::from_secs(10),
        d.connect(&config("another-password"), ConnectCtx::default()),
    )
    .await
    .unwrap();
    assert_ne!(
        detail(&*other, "pid"),
        pid,
        "another profile, another process"
    );

    let pid: i32 = pid.parse().unwrap();
    a.close().await.unwrap();
    drop(a);
    b.close().await.unwrap();
    drop(b);
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only probes whether the pid exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "sidecar outlived its connections"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn catalog_and_read_only_go_over_the_protocol() {
    let conn = connect(&driver("ok")).await;
    assert_eq!(conn.set_read_only(true).await.unwrap(), Enforcement::Client);
    let catalog = conn.catalog();
    assert_eq!(catalog.levels().len(), 2);
    let page = catalog
        .children(&ObjectPath::root(), ListOpts::default())
        .await
        .unwrap();
    assert_eq!(page.items[0].path.to_string(), "HR");
    let detail = catalog
        .describe(&ObjectPath::new(vec![Arc::from("HR"), Arc::from("EMP")]))
        .await
        .unwrap();
    assert_eq!(detail.node.path.to_string(), "HR.EMP");
    assert_eq!(detail.schema.unwrap().fields[0].name.as_ref(), "ID");
}
