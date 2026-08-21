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

/// A bare HTTP client for *seeding* only. Fixtures are set up out of band —
/// the driver now generates guarded single-document `Op::Mutate` writes, but a
/// read test must not depend on the write path it is not exercising, and bulk
/// seeding is not something the (deliberately non-`_bulk`) write path does.
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

/// Seed documents written as raw JSON text, so `_source`'s key order on the
/// wire is exactly what the test wrote. `serde_json::json!` would alphabetize
/// it (its `Map` is a `BTreeMap` without the crate-wide `preserve_order`
/// feature), which would make a key-order assertion test nothing at all.
async fn bulk_raw(index: &str, docs: &[&str]) {
    let mut ndjson = String::new();
    for doc in docs {
        ndjson.push_str(&serde_json::json!({ "index": { "_index": index } }).to_string());
        ndjson.push('\n');
        ndjson.push_str(doc);
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

/// Seed anything that is not an index — a template, say — at an arbitrary
/// path, for the same reason as [`create_index`]: a read test must not depend
/// on a write path it is not exercising.
async fn put_json(path: &str, body: serde_json::Value) {
    let resp = seeder()
        .put(format!("{}/{path}", es_url()))
        .json(&body)
        .send()
        .await
        .expect("put");
    assert!(
        resp.status().is_success(),
        "PUT /{path} failed: {}",
        resp.text().await.unwrap_or_default()
    );
}

async fn delete_path(path: &str) {
    let _ = seeder().delete(format!("{}/{path}", es_url())).send().await;
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

/// The bounded-memory contract, whole: 100 000 documents arrive as many
/// bounded batches — never one buffered result — every document is seen
/// exactly once, and the process's resident memory does not grow with the
/// result size.
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
        matches!(cursor.shape(), Shape::Documents { root_hint: Some(h), .. } if h.to_string() == "_source"),
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

    // The memory invariant: the driver never accumulates the result. A
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
/// far as the driver is concerned, the connection) has gone away — the
/// idle-auto-disconnect story.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn a_resume_token_continues_the_scan_where_it_stopped() {
    let index = unique_index("resume");
    create_index(
        &index,
        serde_json::json!({ "properties": { "n": { "type": "long" } }}),
    )
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
    // Close it properly: this releases the point-in-time, which is exactly
    // what the core does when it disconnects an idle connection. The token
    // must survive that — resuming re-opens a point-in-time and continues
    // from the `search_after` position.
    cursor.close().await.unwrap();
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
/// order, never re-announced.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn heterogeneous_documents_emit_schema_delta_add_column_events() {
    let index = unique_index("hetero");
    create_index(
        &index,
        serde_json::json!({ "properties": { "n": { "type": "long" } }}),
    )
    .await;
    // Each document introduces at least one field the previous ones lacked.
    // Written as raw text so `_source` arrives in this exact key order, which
    // is what the AddColumn ordering assertion below is really testing.
    bulk_raw(
        &index,
        &[
            r#"{"n":1,"a":"first"}"#,
            r#"{"n":2,"a":"again","b":7}"#,
            r#"{"n":3,"c":{"nested":true}}"#,
            r#"{"n":4}"#,
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

/// Cancellation, end to end: a genuinely slow search is cancelled through the
/// task API, control returns immediately, the outcome honestly reports a
/// server-side cancel, and the connection survives.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn cancel_mid_slow_query_reaches_the_server_and_returns_control() {
    let index = unique_index("cancel");
    create_index(
        &index,
        serde_json::json!({ "properties": { "n": { "type": "long" } }}),
    )
    .await;
    let docs: Vec<serde_json::Value> = (0..10_000).map(|i| serde_json::json!({ "n": i })).collect();
    bulk(&index, &docs).await;

    let conn: Arc<dyn Connection> = Arc::from(connect(Some(&index)).await);
    assert!(conn.capabilities().flags.contains(Caps::SERVER_CANCEL));

    // ~10 s of real work that Elasticsearch cannot short-circuit: a `script`
    // *filter* (so the driver's `_shard_doc` sort cannot terminate the
    // collection early) whose predicate matches nothing (so every document
    // must be evaluated) and whose loop depends on a doc value (so the JIT
    // cannot fold it away).
    let slow = format!(
        "POST /{index}/_search\n{}",
        serde_json::json!({
            "query": { "bool": { "filter": { "script": { "script": { "source":
                "double s = 0; for (int i = 0; i < 40000; i++) { s += Math.sqrt(i * 1.0) + doc['n'].value; } return s > 1e18;" } } } } }
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
    assert!(after
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .is_some());

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
        page.items
            .iter()
            .map(|n| n.path.to_string())
            .collect::<Vec<_>>()
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
    let Payload::Docs(docs) = cursor
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap()
        .payload
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
    assert!(matches!(field(doc, "_source.tags"), Some(Value::Array(_))));
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
    assert_eq!(
        field(doc, "_index"),
        Some(Value::Str(Arc::from(index.as_str())))
    );

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
    let batch = cheap
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap();
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
    let Payload::Docs(docs) = plan
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap()
        .payload
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
    create_index(
        &index,
        serde_json::json!({ "properties": { "n": { "type": "long" } }}),
    )
    .await;
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
    let mut health = conn
        .execute(Request::native("GET /_cluster/health"))
        .await
        .unwrap();
    let Payload::Docs(docs) = health
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap()
        .payload
    else {
        panic!("expected docs")
    };
    assert!(field(&docs[0], "cluster_name").is_some());

    // A bare body targets the default index and streams.
    let mut bare = conn
        .execute(Request::native(r#"{"query":{"match_all":{}}}"#))
        .await
        .unwrap();
    let Payload::Docs(docs) = bare
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap()
        .payload
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
    let Payload::Docs(docs) = bound
        .next_batch(FetchHint::default())
        .await
        .unwrap()
        .unwrap()
        .payload
    else {
        panic!("expected docs")
    };
    assert_eq!(docs.len(), 1);
    assert_eq!(field(&docs[0], "_source.n"), Some(Value::I64(2)));

    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// **The version-conflict loop, whole, against a real cluster.**
///
/// A guarded write is only worth having if a conflict is recoverable, and the
/// recovery is the same three steps every time: the write is refused, the
/// document is read back, the edit is re-sent against the version that read
/// returned. Every one of those steps rests on an assumption about the server
/// that no fixture can check — that a stale `if_seq_no` really does 409 rather
/// than overwrite, that a scan by identity really does return the *current*
/// `_seq_no`, and that the same edit really is accepted once re-guarded.
///
/// This is the shape `datagrep_reread_documents` builds for the grid's
/// three-column conflict view: `Op::Scan` over the document's own index,
/// filtered by the rest of its identity, `limit: 2` so an identity that
/// answers twice can be told from one that answers once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn a_stale_guard_is_refused_then_re_read_and_re_applied() {
    let index = unique_index("conflict");
    create_index(
        &index,
        serde_json::json!({ "properties": {
            "status": { "type": "keyword" }, "owner": { "type": "keyword" }
        }}),
    )
    .await;
    bulk(
        &index,
        &[serde_json::json!({ "status": "open", "owner": "amy" })],
    )
    .await;

    let conn = connect(Some(&index)).await;

    // The scan a re-read runs: the index is the object, the rest of the
    // identity are terms inside it.
    let reread = |id: Option<&str>| {
        let mut terms = Vec::new();
        if let Some(id) = id {
            terms.push(Predicate::Eq {
                field: FieldPath::field("_id"),
                value: Value::Str(Arc::from(id)),
            });
        }
        Request::Op(Op::Scan {
            path: ObjectPath::new(vec![Arc::from(index.as_str())]),
            filter: (!terms.is_empty()).then(|| Predicate::And(terms)),
            order: Vec::new(),
            project: None,
            limit: Some(2),
            resume: None,
        })
    };

    let read_one = |req: Request| {
        let conn = &conn;
        async move {
            let mut cursor = conn.execute(req).await.expect("scan");
            let mut docs: Vec<Value> = Vec::new();
            while let Some(batch) = cursor
                .next_batch(FetchHint::default())
                .await
                .expect("batch")
            {
                if let Payload::Docs(hits) = batch.payload {
                    docs.extend(hits);
                }
            }
            cursor.close().await.expect("close");
            docs
        }
    };

    // 1. Load it. The scan carries the guard, because every page asks for it.
    let loaded = read_one(reread(None)).await;
    assert_eq!(loaded.len(), 1);
    let Some(Value::Str(doc_id)) = field(&loaded[0], "_id") else {
        panic!("a hit must carry its _id")
    };
    let loaded_seq = field(&loaded[0], "_seq_no").expect("a scan must carry _seq_no");
    let loaded_term = field(&loaded[0], "_primary_term").expect("…and _primary_term");
    assert_eq!(
        field(&loaded[0], "_source.status"),
        Some(Value::Str(Arc::from("open")))
    );

    // 2. Somebody else moves it — a different field, so a rebase would not be
    //    overwriting their work.
    let resp = seeder()
        .post(format!(
            "{}/{index}/_update/{doc_id}?refresh=true",
            es_url()
        ))
        .json(&serde_json::json!({ "doc": { "owner": "bo" } }))
        .send()
        .await
        .expect("out-of-band update");
    assert!(resp.status().is_success(), "seed update failed: {resp:?}");

    let key = vec![
        (
            FieldPath::field("_index"),
            Value::Str(Arc::from(index.as_str())),
        ),
        (FieldPath::field("_id"), Value::Str(doc_id.clone())),
    ];
    let update = |expect: Vec<(FieldPath, Value)>| {
        Request::Op(Op::Mutate(datagrep_api::request::MutationBatch {
            mutations: vec![datagrep_api::request::Mutation::Update {
                path: ObjectPath::new(vec![Arc::from(index.as_str())]),
                key: key.clone(),
                sets: vec![(FieldPath::field("status"), Value::Str(Arc::from("done")))],
                expect,
            }],
        }))
    };

    // 3. The stale guard is refused — reported per row, not thrown, and the
    //    document is NOT overwritten.
    let refused = read_one(update(vec![
        (FieldPath::field("_seq_no"), loaded_seq.clone()),
        (FieldPath::field("_primary_term"), loaded_term.clone()),
    ]))
    .await;
    assert_eq!(refused.len(), 1, "one report row per mutation");
    assert_eq!(
        field(&refused[0], "outcome"),
        Some(Value::Str(Arc::from("failed"))),
        "a stale guard must not be applied"
    );
    assert_eq!(field(&refused[0], "conflict"), Some(Value::Bool(true)));

    // 4. Re-read by identity: exactly one document, a FRESH guard, and the
    //    other person's change visible — which is what the middle column of
    //    the conflict view shows.
    let now = read_one(reread(Some(&doc_id))).await;
    assert_eq!(now.len(), 1, "an identity answers exactly once");
    let fresh_seq = field(&now[0], "_seq_no").expect("_seq_no");
    let fresh_term = field(&now[0], "_primary_term").expect("_primary_term");
    assert_ne!(
        fresh_seq, loaded_seq,
        "the re-read must carry the CURRENT version"
    );
    assert_eq!(
        field(&now[0], "_source.owner"),
        Some(Value::Str(Arc::from("bo"))),
        "the re-read shows what the server holds now"
    );
    assert_eq!(
        field(&now[0], "_source.status"),
        Some(Value::Str(Arc::from("open"))),
        "and the refused write really did not land"
    );

    // 5. Rebase: the same edit, re-guarded against that version, applies.
    let applied = read_one(update(vec![
        (FieldPath::field("_seq_no"), fresh_seq),
        (FieldPath::field("_primary_term"), fresh_term),
    ]))
    .await;
    assert_eq!(
        field(&applied[0], "outcome"),
        Some(Value::Str(Arc::from("applied"))),
        "re-applying onto the current version is accepted"
    );

    let after = read_one(reread(Some(&doc_id))).await;
    assert_eq!(
        field(&after[0], "_source.status"),
        Some(Value::Str(Arc::from("done"))),
        "the user's edit landed"
    );
    assert_eq!(
        field(&after[0], "_source.owner"),
        Some(Value::Str(Arc::from("bo"))),
        "and it merged onto the other change rather than reverting it"
    );

    conn.close().await.unwrap();
    drop_index(&index).await;
}

/// The root describe's fourth source: what the cluster is running.
///
/// `_tasks` is queried unfiltered, so the listing request itself is always in
/// the answer — which is exactly what makes this checkable without arranging
/// for a long-running reindex.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn the_root_describe_reports_running_tasks_alongside_health() {
    let conn = connect(None).await;
    let detail = conn
        .catalog()
        .describe(&ObjectPath::root())
        .await
        .expect("describe the cluster");
    let extra = |k: &str| -> Option<String> {
        detail
            .extra
            .iter()
            .find(|(key, _)| &**key == k)
            .map(|(_, v)| v.to_string())
    };

    // The three sources P1-4 landed still answer…
    assert!(
        extra("status").is_some(),
        "cluster health must still render"
    );
    assert!(extra("shard_count").is_some(), "shards must still render");
    // …and the fourth one does too.
    let count: usize = extra("task_count")
        .expect("task_count")
        .parse()
        .expect("a number");
    assert!(
        count >= 1,
        "the tasks listing always contains at least the request asking"
    );
    let actions: serde_json::Value =
        serde_json::from_str(&extra("task_actions").expect("task_actions")).unwrap();
    assert!(
        actions
            .as_object()
            .expect("an object")
            .contains_key("cluster:monitor/tasks/lists"),
        "actions keep the cluster's own spelling, got {actions}"
    );
    let running: serde_json::Value =
        serde_json::from_str(&extra("running_tasks").expect("running_tasks")).unwrap();
    let rows = running.as_array().expect("an array");
    assert!(!rows.is_empty());
    assert!(
        rows[0]["running_time_ms"].is_i64(),
        "running time is milliseconds, from the server's nanos"
    );
    assert!(rows[0]["action"].is_string());

    conn.close().await.unwrap();
}

/// Index templates in the root describe: three systems, three sources.
///
/// The listing must find a template this test authored *and* the ones the
/// server ships with — a stock 8.15 cluster has 45 composable, 44 component
/// and 5 legacy templates before anybody authors one, which is the whole
/// reason the listings are capped and counted separately.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live Elasticsearch; see tests/README.md"]
async fn the_root_describe_lists_all_three_template_systems() {
    let label = unique_index("tpl");
    let component = format!("{label}_component");
    let composable = format!("{label}_composable");
    put_json(
        &format!("_component_template/{component}"),
        serde_json::json!({ "template": { "mappings": { "properties": {
            "level": { "type": "keyword" } } } } }),
    )
    .await;
    put_json(
        &format!("_index_template/{composable}"),
        serde_json::json!({
            "index_patterns": [format!("{label}-*")],
            "priority": 500,
            "composed_of": [component],
            "version": 7,
            "template": { "settings": { "number_of_shards": 1 } }
        }),
    )
    .await;

    let conn = connect(None).await;
    let detail = conn
        .catalog()
        .describe(&ObjectPath::root())
        .await
        .expect("describe the cluster");
    let extra = |k: &str| -> Option<String> {
        detail
            .extra
            .iter()
            .find(|(key, _)| &**key == k)
            .map(|(_, v)| v.to_string())
    };

    // The sources that were already there still answer.
    assert!(
        extra("status").is_some(),
        "cluster health must still render"
    );
    assert!(extra("task_count").is_some(), "tasks must still render");

    for key in ["index_templates", "component_templates", "legacy_templates"] {
        let count: usize = extra(&format!("{key}_count"))
            .unwrap_or_else(|| panic!("{key}_count"))
            .parse()
            .expect("a number");
        assert!(count > 0, "a stock cluster ships {key}, got {count}");
        let listing: serde_json::Value =
            serde_json::from_str(&extra(key).unwrap_or_else(|| panic!("{key}"))).unwrap();
        assert!(listing
            .as_array()
            .expect("an array")
            .iter()
            .all(|row| row["name"].is_string()));
    }

    // The composable template this test wrote, with its patterns still an
    // array rather than the `_cat` rendering `"[…-*]"`.
    let listing: serde_json::Value =
        serde_json::from_str(&extra("index_templates").unwrap()).unwrap();
    let mine = listing
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == serde_json::json!(composable))
        .expect("the template this test authored is listed");
    assert_eq!(
        mine["index_patterns"],
        serde_json::json!([format!("{label}-*")])
    );
    assert_eq!(mine["priority"], serde_json::json!(500));
    assert_eq!(mine["composed_of"], serde_json::json!([component]));
    assert_eq!(mine["version"], serde_json::json!(7));
    assert_eq!(mine["data_stream"], serde_json::json!(false));
    assert_eq!(mine["template_keys"], serde_json::json!(["settings"]));

    // A composable template stores only the *name* of what it is composed of,
    // so the component listing is what stops that name being a dead reference.
    let components: serde_json::Value =
        serde_json::from_str(&extra("component_templates").unwrap()).unwrap();
    let comp = components
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == serde_json::json!(component))
        .expect("the component is listed");
    assert_eq!(comp["template_keys"], serde_json::json!(["mappings"]));

    conn.close().await.unwrap();
    delete_path(&format!("_index_template/{composable}")).await;
    delete_path(&format!("_component_template/{component}")).await;
}
