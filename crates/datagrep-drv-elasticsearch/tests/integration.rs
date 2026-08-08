//! Integration tests against a real Elasticsearch. Every test is `#[ignore]`d
//! by default and needs a live server — see `tests/README.md` for the docker
//! one-liner and the startup-wait caveat.
//!
//! Point them at a server with `DATAGREP_TEST_ES` (default
//! `http://localhost:9200`). Each test creates its own throwaway index named
//! `datagrep_es_test_<label>_<nanos>` and deletes it on the way out, so the
//! suite is safe to run repeatedly and concurrently against one server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use datagrep_api::caps::Caps;
use datagrep_api::catalog::ListOpts;
use datagrep_api::driver::{
    CancelOutcome, ConnectCtx, Connection, Driver, Enforcement, FetchHint, Payload, TxOpts,
};
use datagrep_api::error::DbError;
use datagrep_api::request::{ExecOpts, Op, Predicate, Request};
use datagrep_api::shape::{ObjectPath, SchemaDelta, Shape};
use datagrep_api::value::{FieldPath, Value};
use datagrep_api::ConfigValue;

use datagrep_drv_elasticsearch::ElasticsearchDriver;

// ---------------------------------------------------------------- harness --

fn es_url() -> String {
    std::env::var("DATAGREP_TEST_ES").unwrap_or_else(|_| "http://localhost:9200".to_string())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_index(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("datagrep_es_test_{label}_{nanos}_{n}")
}

/// A bare HTTP client for *seeding* only. The driver deliberately refuses to
/// generate writes (`EDITABLE_RESULTS` and `DDL` are off), so fixtures are set
/// up out of band rather than through the seam under test.
fn seeder() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

async fn create_index(index: &str, mappings: serde_json::Value) {
    let body = serde_json::json!({
        "settings": { "number_of_shards": 1, "number_of_replicas": 0 },
        "mappings": mappings
    });
    let resp = seeder()
        .put(format!("{}/{index}", es_url()))
        .json(&body)
        .send()
        .await
        .expect("create index");
    assert!(
        resp.status().is_success(),
        "create index failed: {}",
        resp.text().await.unwrap_or_default()
    );
}

async fn bulk(index: &str, docs: &[serde_json::Value]) {
    for chunk in docs.chunks(10_000) {
        let mut ndjson = String::with_capacity(chunk.len() * 128);
        for doc in chunk {
            ndjson.push_str(&serde_json::json!({ "index": { "_index": index } }).to_string());
            ndjson.push('\n');
            ndjson.push_str(&doc.to_string());
            ndjson.push('\n');
        }
        let resp = seeder()
            .post(format!("{}/_bulk", es_url()))
            .header("content-type", "application/x-ndjson")
            .body(ndjson)
            .send()
            .await
            .expect("bulk");
        assert!(resp.status().is_success(), "bulk indexing failed");
    }
    let _ = seeder()
        .post(format!("{}/{index}/_refresh", es_url()))
        .send()
        .await;
}

async fn drop_index(index: &str) {
    let _ = seeder()
        .delete(format!("{}/{index}", es_url()))
        .send()
        .await;
}

async fn connect(default_index: Option<&str>) -> Box<dyn Connection> {
    let driver = ElasticsearchDriver::new();
    let mut cfg = driver.parse_url(&es_url()).expect("parse url");
    if let Some(index) = default_index {
        cfg.values
            .insert("index".to_string(), ConfigValue::Str(index.to_string()));
    }
    driver
        .connect(
            &datagrep_api::ResolvedConfig::without_secrets(cfg),
            ConnectCtx {
                connect_timeout: Some(Duration::from_secs(20)),
                application_name: Some(Arc::from("datagrep-it")),
                ..ConnectCtx::default()
            },
        )
        .await
        .expect("connect to elasticsearch")
}

/// Resident set size in bytes, via `ps` — no extra dependency, and it is the
/// same number a user watching Activity Monitor would see.
fn rss_bytes() -> u64 {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        * 1024
}

fn source_of(value: &Value) -> &Value {
    let Value::Document(doc) = value else {
        panic!("expected a document, got {value:?}")
    };
    doc.get("_source").expect("_source")
}

fn field(value: &Value, path: &str) -> Option<Value> {
    let Value::Document(doc) = value else {
        panic!("expected a document")
    };
    let path: FieldPath = path.parse().unwrap();
    doc.get_path(&path).cloned()
}

// ------------------------------------------------------------------ tests --

/// Design §3.2's whole contract: 100 000 documents arrive as many bounded
/// batches — never one buffered result — every document is seen exactly once,
/// and the process's resident memory does not grow with the result size.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn streams_100k_documents_in_incremental_batches_with_flat_rss() {
    let index = unique_index("stream");
    create_index(
        &index,
        serde_json::json!({ "properties": {
            "n": { "type": "long" },
            "grp": { "type": "keyword" },
            "txt": { "type": "text" }
        }}),
    )
    .await;
    let docs: Vec<serde_json::Value> = (0..100_000)
        .map(|i| {
            serde_json::json!({
                "n": i,
                "grp": format!("g{}", i % 50),
                "txt": format!("lorem ipsum dolor sit amet {i}")
            })
        })
        .collect();
    bulk(&index, &docs).await;

    let conn = connect(Some(&index)).await;
    assert!(
        conn.capabilities().flags.contains(Caps::SERVER_CANCEL),
        "an Elasticsearch 8 cluster must report a real server cancel"
    );

    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: ObjectPath::new(vec![Arc::from(index.as_str())]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .expect("open scan");

    assert!(
        matches!(cursor.shape(), Shape::Documents { root_hint: Some(h) } if h.to_string() == "_source"),
        "the grid must be pointed at _source"
    );

    let hint = FetchHint {
        max_rows: 1_000,
        ..FetchHint::default()
    };
    // Baseline after the first batch, so index-time and connection setup are
    // not counted as result-set growth.
    let mut first = cursor.next_batch(hint).await.unwrap().expect("first batch");
    let baseline_rss = rss_bytes();
    let mut seen = match std::mem::take(&mut first.payload) {
        Payload::Docs(docs) => docs.len() as u64,
        other => panic!("expected Payload::Docs, got {other:?}"),
    };
    let mut batches = 1u64;
    let mut peak_rss = baseline_rss;
    let mut mid_stream_token = None;

    while let Some(batch) = cursor.next_batch(hint).await.unwrap() {
        let Payload::Docs(docs) = batch.payload else {
            panic!("expected Payload::Docs")
        };
        assert!(
            docs.len() as u32 <= hint.max_rows,
            "a batch must respect the row hint"
        );
        seen += docs.len() as u64;
        batches += 1;
        if batches == 5 {
            mid_stream_token = cursor.resume_token();
        }
        peak_rss = peak_rss.max(rss_bytes());
    }

    assert_eq!(seen, 100_000, "every document exactly once");
    assert!(
        batches >= 90,
        "100k rows at a 1000-row hint must arrive incrementally, got {batches} batches"
    );
    assert_eq!(cursor.stats().rows, 100_000);
    assert_eq!(cursor.stats().batches, batches);
    assert!(
        cursor.stats().server_elapsed_micros.is_some(),
        "the server's own `took` must be reported"
    );

    // §3.2's memory invariant: the driver never accumulates the result. A
    // generous ceiling — the point is that it is bounded, not that it is zero.
    let growth = peak_rss.saturating_sub(baseline_rss);
    assert!(
        growth < 128 * 1024 * 1024,
        "resident memory grew by {} MiB while streaming 100k docs — the driver is buffering",
        growth / (1024 * 1024)
    );

    // A mid-stream token exists and is a real continuation.
    let token = mid_stream_token.expect("a resume token mid-stream");
    assert!(!token.0.is_empty());

    cursor.close().await.unwrap();
    assert!(
        matches!(cursor.next_batch(hint).await, Err(DbError::Closed)),
        "a closed cursor is closed"
    );
    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// A resume token really does continue the same scan after the cursor (and, as
/// far as the driver is concerned, the connection) has gone away — design
/// §3.5's idle-auto-disconnect story.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn a_resume_token_continues_the_scan_where_it_stopped() {
    let index = unique_index("resume");
    create_index(&index, serde_json::json!({ "properties": { "n": { "type": "long" } }}))
        .await;
    let docs: Vec<serde_json::Value> = (0..1_000).map(|i| serde_json::json!({ "n": i })).collect();
    bulk(&index, &docs).await;

    let conn = connect(Some(&index)).await;
    let scan = |resume| {
        Request::Op(Op::Scan {
            path: ObjectPath::new(vec![Arc::from(index.as_str())]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume,
        })
    };

    let mut cursor = conn.execute(scan(None)).await.unwrap();
    let hint = FetchHint {
        max_rows: 100,
        ..FetchHint::default()
    };
    let mut first_half = Vec::new();
    for _ in 0..3 {
        let Payload::Docs(docs) = cursor.next_batch(hint).await.unwrap().unwrap().payload else {
            panic!("expected docs")
        };
        first_half.extend(docs);
    }
    assert_eq!(first_half.len(), 300);
    let token = cursor.resume_token().expect("a resume token");
    // Deliberately do NOT close the cursor's context here: the token has to be
    // usable while the point-in-time is still alive, which is exactly the
    // idle-disconnect case.
    drop(cursor);

    let mut resumed = conn.execute(scan(Some(token))).await.unwrap();
    let mut second_half = Vec::new();
    while let Some(batch) = resumed.next_batch(hint).await.unwrap() {
        let Payload::Docs(docs) = batch.payload else {
            panic!("expected docs")
        };
        second_half.extend(docs);
    }
    assert_eq!(
        second_half.len(),
        700,
        "the resumed scan returns exactly the documents the first one had not"
    );

    // No document appears in both halves.
    let ids = |docs: &[Value]| -> Vec<String> {
        docs.iter()
            .map(|d| match field(d, "_id") {
                Some(Value::Str(s)) => s.to_string(),
                other => panic!("expected a string _id, got {other:?}"),
            })
            .collect()
    };
    let mut all = ids(&first_half);
    all.extend(ids(&second_half));
    let unique: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(unique.len(), 1_000, "no document is delivered twice");

    resumed.close().await.unwrap();
    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// Heterogeneous documents make the grid grow columns without refetching:
/// exactly one `AddColumn` per newly-observed `_source` field, in first-seen
/// order, never re-announced (design risk #7).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn heterogeneous_documents_emit_schema_delta_add_column_events() {
    let index = unique_index("hetero");
    create_index(&index, serde_json::json!({ "properties": { "n": { "type": "long" } }}))
        .await;
    // Each document introduces at least one field the previous ones lacked.
    bulk(
        &index,
        &[
            serde_json::json!({ "n": 1, "a": "first" }),
            serde_json::json!({ "n": 2, "a": "again", "b": 7 }),
            serde_json::json!({ "n": 3, "c": { "nested": true } }),
            serde_json::json!({ "n": 4 }),
        ],
    )
    .await;

    let conn = connect(Some(&index)).await;
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: ObjectPath::new(vec![Arc::from(index.as_str())]),
            filter: None,
            order: vec![datagrep_api::request::SortKey {
                path: FieldPath::field("n"),
                desc: false,
                nulls_first: false,
            }],
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .unwrap();

    // One document per batch, so the deltas are observed as the stream grows.
    let hint = FetchHint {
        max_rows: 1,
        ..FetchHint::default()
    };
    let mut announced: Vec<String> = Vec::new();
    let mut docs = Vec::new();
    while let Some(batch) = cursor.next_batch(hint).await.unwrap() {
        for delta in &batch.schema_delta {
            match delta {
                SchemaDelta::AddColumn { field } => announced.push(field.name.to_string()),
                other => panic!("unexpected delta {other:?}"),
            }
        }
        let Payload::Docs(d) = batch.payload else {
            panic!("expected docs")
        };
        docs.extend(d);
    }

    assert_eq!(docs.len(), 4);
    assert_eq!(
        announced,
        vec!["n", "a", "b", "c"],
        "one AddColumn per new field, in first-seen order, never re-announced"
    );

    // And the `Absent` distinction the whole driver exists for: document 4
    // carries no `a`, which resolves to absence rather than a fake null.
    assert_eq!(field(&docs[3], "_source.a"), None, "absent, not null");
    assert_eq!(
        field(&docs[0], "_source.a"),
        Some(Value::Str(Arc::from("first")))
    );
    assert!(matches!(source_of(&docs[2]), Value::Document(_)));

    cursor.close().await.unwrap();
    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// Design §3.3, end to end: a genuinely slow search is cancelled through the
/// task API, control returns immediately, the outcome honestly reports a
/// server-side cancel, and the connection survives.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn cancel_mid_slow_query_reaches_the_server_and_returns_control() {
    let index = unique_index("cancel");
    create_index(&index, serde_json::json!({ "properties": { "n": { "type": "long" } }}))
        .await;
    let docs: Vec<serde_json::Value> = (0..10_000).map(|i| serde_json::json!({ "n": i })).collect();
    bulk(&index, &docs).await;

    let conn: Arc<dyn Connection> = Arc::from(connect(Some(&index)).await);
    assert!(conn.capabilities().flags.contains(Caps::SERVER_CANCEL));

    // ~10 s of real work: a per-document scripted loop over 10k documents.
    let slow = format!(
        "POST /{index}/_search\n{}",
        serde_json::json!({
            "query": { "script_score": {
                "query": { "match_all": {} },
                "script": { "source":
                    "double s = 0; for (int i = 0; i < 20000; i++) { s += Math.sqrt(i * 1.0) + doc['n'].value; } return s > 0 ? 1.0 : 0.5;" }
            }}
        })
    );

    let mut cursor = conn
        .execute(Request::native(slow))
        .await
        .expect("execute returns as soon as the request is accepted");

    let canceller = conn.canceller();
    let started = Instant::now();
    let pulling = tokio::spawn(async move {
        let result = cursor.next_batch(FetchHint::default()).await;
        let _ = cursor.close().await;
        result
    });

    // Let the search actually start so there is a task to find.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let outcome = canceller.cancel().await.expect("cancel");
    assert_eq!(
        outcome,
        CancelOutcome::ServerCancelled,
        "with async search + the tasks API this must be a real server-side cancel, \
         not an embellished client abandon"
    );

    let pulled = tokio::time::timeout(Duration::from_secs(15), pulling)
        .await
        .expect("the stop button always returns control")
        .expect("no panic in the pulling task");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(12),
        "control came back after {elapsed:?} — the cancel did not take effect"
    );
    match pulled {
        Err(DbError::Cancelled) => {}
        // A cancel that lands between shard phases can also surface as the
        // engine's own partial-result error; both are honest outcomes, a
        // silently successful full result is not.
        Err(other) => assert!(
            other.to_string().to_lowercase().contains("cancel"),
            "expected a cancellation, got {other:?}"
        ),
        Ok(_) => panic!("the slow query completed — nothing was cancelled"),
    }

    // The connection is still usable afterwards.
    conn.ping().await.expect("connection survived the cancel");
    let mut after = conn
        .execute(Request::Op(Op::Count {
            path: ObjectPath::new(vec![Arc::from(index.as_str())]),
            filter: None,
            exact: true,
        }))
        .await
        .expect("a new request works after a cancel");
    assert!(after.next_batch(FetchHint::default()).await.unwrap().is_some());

    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// The catalog is lazy and truthful: indices list, one index's mapping is
/// fetched on demand (never the cluster's), `describe` reports the field list
/// plus document count and store size, and `infer_shape` samples.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn catalog_lists_indices_maps_fields_and_infers_shape() {
    let index = unique_index("catalog");
    create_index(
        &index,
        serde_json::json!({ "properties": {
            "n":     { "type": "long" },
            "title": { "type": "text", "fields": { "keyword": { "type": "keyword" } } },
            "addr":  { "properties": { "city": { "type": "keyword" } } }
        }}),
    )
    .await;
    bulk(
        &index,
        &[
            serde_json::json!({ "n": 1, "title": "a", "addr": { "city": "sg" } }),
            serde_json::json!({ "n": 2, "title": "b" }),
            serde_json::json!({ "title": "c" }),
        ],
    )
    .await;

    let conn = connect(Some(&index)).await;
    let catalog = conn.catalog();

    let levels = catalog.levels();
    assert_eq!(levels.len(), 2);
    assert_eq!(&*levels[0].name, "index");
    assert_eq!(&*levels[1].name, "field");

    // Prefix-narrowed listing — the server does the filtering.
    let page = catalog
        .children(
            &ObjectPath::root(),
            ListOpts {
                prefix: Some(Arc::from(index.as_str())),
                ..ListOpts::default()
            },
        )
        .await
        .expect("list indices");
    assert!(
        page.items.iter().any(|n| n.path.to_string() == index),
        "the seeded index must be listed, got {:?}",
        page.items.iter().map(|n| n.path.to_string()).collect::<Vec<_>>()
    );

    // Fields come from that one index's mapping.
    let fields = catalog
        .children(
            &ObjectPath::new(vec![Arc::from(index.as_str())]),
            ListOpts::default(),
        )
        .await
        .expect("list fields");
    let names: Vec<String> = fields.items.iter().map(|n| n.path.to_string()).collect();
    for expected in ["addr", "addr.city", "n", "title", "title.keyword"] {
        assert!(
            names.iter().any(|n| n == &format!("{index}.{expected}")),
            "mapping field {expected} missing from {names:?}"
        );
    }

    let detail = catalog
        .describe(&ObjectPath::new(vec![Arc::from(index.as_str())]))
        .await
        .expect("describe");
    assert!(
        detail.schema.is_none(),
        "SCHEMA_DECLARED is false, so describe must not fabricate a RowSchema"
    );
    let extra = |k: &str| -> Option<String> {
        detail
            .extra
            .iter()
            .find(|(key, _)| &**key == k)
            .map(|(_, v)| v.to_string())
    };
    assert_eq!(extra("document_count").as_deref(), Some("3"));
    assert!(
        extra("store_size_bytes").map(|v| v.parse::<u64>().unwrap()) > Some(0),
        "store size must be reported"
    );
    let fields_json: serde_json::Value =
        serde_json::from_str(&extra("fields").expect("fields array")).unwrap();
    assert!(fields_json.as_array().unwrap().len() >= 5);
    let indexes_json: serde_json::Value =
        serde_json::from_str(&extra("indexes").expect("indexes array")).unwrap();
    assert_eq!(indexes_json[0]["name"], serde_json::json!("_id"));
    assert_eq!(indexes_json[0]["primary"], serde_json::json!(true));

    // Sampling: `n` is absent from one of the three documents, and absence is
    // not a type.
    let inferred = catalog
        .infer_shape(&ObjectPath::new(vec![Arc::from(index.as_str())]), 100)
        .await
        .expect("infer_shape");
    assert_eq!(inferred.sampled, 3);
    let n_trie = &inferred
        .root
        .iter()
        .find(|(name, _)| &**name == "n")
        .expect("n in the inferred schema")
        .1;
    assert_eq!(n_trie.present, 2);
    assert!((n_trie.presence_ratio(3) - 2.0 / 3.0).abs() < 1e-9);

    // Completion is a bounded prefix query.
    let completions = catalog
        .complete(datagrep_api::catalog::CompletionCtx {
            text: Arc::from("addr"),
            offset: 4,
            scope: Some(ObjectPath::new(vec![Arc::from(index.as_str())])),
        })
        .await
        .expect("complete");
    assert!(completions.iter().any(|c| &*c.label == "addr.city"));

    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// The value mapping, end to end against the engine's own responses:
/// `scaled_float` keeps its decimal, `long` stays exact past f64, `binary`
/// decodes to bytes, an explicit null stays a null, and a missing field is
/// absent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn value_mapping_preserves_precision_bytes_and_the_absent_null_distinction() {
    let index = unique_index("values");
    create_index(
        &index,
        serde_json::json!({ "properties": {
            "id":    { "type": "long" },
            "price": { "type": "scaled_float", "scaling_factor": 1000 },
            "ratio": { "type": "double" },
            "blob":  { "type": "binary" },
            "flag":  { "type": "boolean" },
            "tags":  { "type": "keyword" }
        }}),
    )
    .await;
    bulk(
        &index,
        &[serde_json::json!({
            "id": 9007199254740993_i64,   // 2^53 + 1: an f64 cannot hold this
            "price": 123.456,
            "ratio": 123.456,
            "blob": "aGVsbG8=",
            "flag": true,
            "tags": ["a", "b"],
            "explicit_null": serde_json::Value::Null
        })],
    )
    .await;

    let conn = connect(Some(&index)).await;
    let mut cursor = conn
        .execute(Request::Op(Op::Scan {
            path: ObjectPath::new(vec![Arc::from(index.as_str())]),
            filter: None,
            order: Vec::new(),
            project: None,
            limit: None,
            resume: None,
        }))
        .await
        .unwrap();
    let Payload::Docs(docs) = cursor.next_batch(FetchHint::default()).await.unwrap().unwrap().payload
    else {
        panic!("expected docs")
    };
    let doc = &docs[0];

    assert_eq!(
        field(doc, "_source.id"),
        Some(Value::I64(9_007_199_254_740_993)),
        "a long must survive exactly, not through f64"
    );
    assert_eq!(
        field(doc, "_source.price"),
        Some(Value::Decimal(Arc::from("123.456"))),
        "a scaled_float must not round-trip through f64"
    );
    assert_eq!(
        field(doc, "_source.ratio"),
        Some(Value::F64(123.456)),
        "a double legitimately stays an f64"
    );
    assert_eq!(
        field(doc, "_source.blob"),
        Some(Value::Bytes(datagrep_api::Bytes::from_static(b"hello")))
    );
    assert_eq!(field(doc, "_source.flag"), Some(Value::Bool(true)));
    assert!(matches!(
        field(doc, "_source.tags"),
        Some(Value::Array(_))
    ));
    assert_eq!(
        field(doc, "_source.explicit_null"),
        Some(Value::Null),
        "an explicit null is present"
    );
    assert_eq!(
        field(doc, "_source.never_written"),
        None,
        "a field the document does not carry is absent, never null"
    );
    // The envelope pseudo-fields are there too.
    assert!(matches!(field(doc, "_id"), Some(Value::Str(_))));
    assert_eq!(field(doc, "_index"), Some(Value::Str(Arc::from(index.as_str()))));

    cursor.close().await.unwrap();
    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// `EXACT_COUNT_CHEAP` is off and the driver is honest about which count it
/// ran; filters compile to a real Query DSL; and `EXPLAIN` works both ways.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn counts_filters_and_explain_report_what_they_actually_did() {
    let index = unique_index("count");
    create_index(
        &index,
        serde_json::json!({ "properties": {
            "n": { "type": "long" }, "grp": { "type": "keyword" }
        }}),
    )
    .await;
    let docs: Vec<serde_json::Value> = (0..20_000)
        .map(|i| serde_json::json!({ "n": i, "grp": if i % 2 == 0 { "even" } else { "odd" } }))
        .collect();
    bulk(&index, &docs).await;

    let conn = connect(Some(&index)).await;
    let path = ObjectPath::new(vec![Arc::from(index.as_str())]);

    // Exact: a real `_count`.
    let exact = conn
        .execute(Request::Op(Op::Count {
            path: path.clone(),
            filter: None,
            exact: true,
        }))
        .await
        .unwrap();
    match exact.shape() {
        Shape::Ack { affected, message } => {
            assert_eq!(*affected, Some(20_000));
            assert!(message.as_deref().unwrap().contains("exact"));
        }
        other => panic!("expected Ack, got {other:?}"),
    }

    // Cheap: `hits.total`, capped at 10 000 and labelled as a lower bound.
    let mut cheap = conn
        .execute(Request::Op(Op::Count {
            path: path.clone(),
            filter: None,
            exact: false,
        }))
        .await
        .unwrap();
    match cheap.shape() {
        Shape::Ack { affected, message } => {
            assert_eq!(
                *affected,
                Some(10_000),
                "the default tracking limit, not the real total"
            );
            assert!(
                message.as_deref().unwrap().contains("LOWER BOUND"),
                "the UI must be told to render ≥ N"
            );
        }
        other => panic!("expected Ack, got {other:?}"),
    }
    let batch = cheap.next_batch(FetchHint::default()).await.unwrap().unwrap();
    assert!(batch
        .notices
        .iter()
        .any(|n| n.code.as_deref() == Some("es.total_is_lower_bound")));

    // A compiled predicate really filters, with typed values.
    let filtered = conn
        .execute(Request::Op(Op::Count {
            path: path.clone(),
            filter: Some(Predicate::And(vec![
                Predicate::Eq {
                    field: FieldPath::field("grp"),
                    value: Value::Str(Arc::from("even")),
                },
                Predicate::Lt {
                    field: FieldPath::field("n"),
                    value: Value::I64(100),
                },
            ])),
            exact: true,
        }))
        .await
        .unwrap();
    match filtered.shape() {
        Shape::Ack { affected, .. } => assert_eq!(*affected, Some(50)),
        other => panic!("expected Ack, got {other:?}"),
    }

    // EXPLAIN without running, then EXPLAIN ANALYZE with real timings.
    let scan = Request::Op(Op::Scan {
        path: path.clone(),
        filter: Some(Predicate::Eq {
            field: FieldPath::field("grp"),
            value: Value::Str(Arc::from("even")),
        }),
        order: Vec::new(),
        project: None,
        limit: None,
        resume: None,
    });
    let mut plan = conn
        .execute(Request::Op(Op::Explain {
            inner: Box::new(scan.clone()),
            analyze: false,
        }))
        .await
        .unwrap();
    let Payload::Docs(docs) = plan.next_batch(FetchHint::default()).await.unwrap().unwrap().payload
    else {
        panic!("expected docs")
    };
    assert!(
        field(&docs[0], "valid").is_some(),
        "_validate/query reports validity: {:?}",
        docs[0]
    );

    let mut profiled = conn
        .execute(Request::Op(Op::Explain {
            inner: Box::new(scan),
            analyze: true,
        }))
        .await
        .unwrap();
    let Payload::Docs(docs) = profiled
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap()
        .payload
    else {
        panic!("expected docs")
    };
    assert!(
        field(&docs[0], "profile").is_some(),
        "profile: true must report real per-shard timings"
    );

    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// The capability flags and the honest refusals behind them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn capabilities_refusals_and_read_only_are_honest() {
    let index = unique_index("caps");
    create_index(&index, serde_json::json!({ "properties": { "n": { "type": "long" } }})).await;
    bulk(&index, &[serde_json::json!({ "n": 1 })]).await;

    let conn = connect(Some(&index)).await;
    let caps = conn.capabilities();
    assert!(!caps.flags.contains(Caps::TRANSACTIONS));
    assert!(!caps.flags.contains(Caps::EXACT_COUNT_CHEAP));
    assert!(!caps.flags.contains(Caps::RANDOM_ACCESS_PAGE));
    assert!(!caps.flags.contains(Caps::SCHEMA_DECLARED));
    assert!(!caps.flags.contains(Caps::DDL));
    assert!(caps.flags.contains(Caps::EXPLAIN));
    assert!(caps.flags.contains(Caps::EXPRESSION_FILTER));
    assert!(caps.flags.contains(Caps::KEY_ENUMERATION));

    let info = conn.server_info();
    assert_eq!(&*info.product, "Elasticsearch");
    assert!(info
        .details
        .iter()
        .any(|(k, v)| &**k == "pagination" && &**v == "pit+search_after"));

    // Transactions are refused, not silently downgraded.
    assert!(matches!(
        conn.begin(TxOpts::default()).await,
        Err(DbError::Unsupported { .. })
    ));

    // Read-only is client-side, and says so.
    assert_eq!(conn.set_read_only(true).await.unwrap(), Enforcement::Client);
    let write = conn
        .execute(Request::Native {
            text: Arc::from(format!("POST /{index}/_doc\n{{\"n\":2}}").as_str()),
            params: Vec::new(),
            opts: ExecOpts::default(),
        })
        .await;
    assert!(
        matches!(write, Err(DbError::Unsupported { .. })),
        "a write must be refused while read-only"
    );
    // …and a read still works.
    conn.set_read_only(false).await.unwrap();
    let mut ok = conn
        .execute(Request::native(format!("GET /{index}/_search\n{{}}")))
        .await
        .unwrap();
    assert!(ok.next_batch(FetchHint::default()).await.unwrap().is_some());

    conn.close().await.unwrap();
    assert!(matches!(conn.ping().await, Err(DbError::Closed)));
    drop_index(&index).await;
}

/// Native console requests, including parameter binding into the parsed tree.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn native_console_requests_and_bound_parameters_work() {
    let index = unique_index("native");
    create_index(
        &index,
        serde_json::json!({ "properties": { "grp": { "type": "keyword" }, "n": { "type": "long" } }}),
    )
    .await;
    bulk(
        &index,
        &[
            serde_json::json!({ "grp": "a", "n": 1 }),
            serde_json::json!({ "grp": "b", "n": 2 }),
        ],
    )
    .await;

    let conn = connect(Some(&index)).await;

    // A non-search request comes back as one reply document.
    let mut health = conn.execute(Request::native("GET /_cluster/health")).await.unwrap();
    let Payload::Docs(docs) = health.next_batch(FetchHint::default()).await.unwrap().unwrap().payload
    else {
        panic!("expected docs")
    };
    assert!(field(&docs[0], "cluster_name").is_some());

    // A bare body targets the default index and streams.
    let mut bare = conn
        .execute(Request::native(r#"{"query":{"match_all":{}}}"#))
        .await
        .unwrap();
    let Payload::Docs(docs) = bare.next_batch(FetchHint::default()).await.unwrap().unwrap().payload
    else {
        panic!("expected docs")
    };
    assert_eq!(docs.len(), 2);

    // A parameter binds as a typed value in the parsed tree.
    let mut bound = conn
        .execute(Request::Native {
            text: Arc::from(r#"{"query":{"term":{"grp":{"value":"$1"}}}}"#),
            params: vec![Value::Str(Arc::from("b"))],
            opts: ExecOpts::default(),
        })
        .await
        .unwrap();
    let Payload::Docs(docs) = bound.next_batch(FetchHint::default()).await.unwrap().unwrap().payload
    else {
        panic!("expected docs")
    };
    assert_eq!(docs.len(), 1);
    assert_eq!(field(&docs[0], "_source.n"), Some(Value::I64(2)));

    conn.close().await.unwrap();
    drop_index(&index).await;
}
