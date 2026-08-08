# `datagrep-drv-elasticsearch` tests

## Unit tests — no server needed

```sh
export CARGO_TARGET_DIR=/Users/nurchudlori/Projects/datagrep/target-es
cargo test -p datagrep-drv-elasticsearch --lib
```

120 tests over fixtures, in-process: `Predicate` -> Query DSL compilation
(including the injection cases), JSON -> `Value` mapping (`Absent` vs `Null`,
`scaled_float`/`long` precision, `binary`), PIT/`search_after` resume-token
round-trips, URL and auth config parsing, `_cat`/`_alias`/`_mapping` response
parsing, `FieldTrie` inference, and the tasks-API cancel bookkeeping.

## Integration tests — need a real Elasticsearch

`tests/integration.rs` is `#[ignore]`d by default.

### Start a disposable Elasticsearch

```sh
docker run --rm -d --name dg-test-es -p 9200:9200 \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  docker.elastic.co/elasticsearch/elasticsearch:8.15.0
```

**Startup-wait caveat — this is the one that bites.** The container returns
from `docker run` immediately, but Elasticsearch itself needs roughly
**30–60 seconds** to bootstrap (JVM start, index recovery, cluster election)
and will refuse or reset connections until it is up. Do not run the tests
straight after `docker run`; poll for a green cluster first:

```sh
until curl -sf "http://localhost:9200/_cluster/health?wait_for_status=green&timeout=5s" \
        >/dev/null; do sleep 2; done
```

On a machine with little free RAM the container can also be OOM-killed
silently during bootstrap — if the poll never finishes, check
`docker logs dg-test-es`. Capping the heap helps:
`-e "ES_JAVA_OPTS=-Xms1g -Xmx1g"`.

### Run them

```sh
export CARGO_TARGET_DIR=/Users/nurchudlori/Projects/datagrep/target-es
cargo test -p datagrep-drv-elasticsearch --test integration -- --ignored --test-threads=1
```

`--test-threads=1` is recommended: the cancellation test deliberately loads
one node with ~10 s of scripted work, and a single-node container running
several of these concurrently gets slow enough to make timing assertions
flaky. The tests are otherwise isolated — each creates its own index named
`datagrep_es_test_<label>_<nanos>_<n>` and deletes it on the way out — so they
are safe to run repeatedly, and concurrently if you want to.

Point them somewhere else with:

```sh
DATAGREP_TEST_ES="http://user:pass@es.example.com:9200" \
  cargo test -p datagrep-drv-elasticsearch --test integration -- --ignored
```

### Tear down

```sh
docker rm -f dg-test-es
```

## What each integration test proves

- **`streams_100k_documents_in_incremental_batches_with_flat_rss`** — the
  bounded-memory contract. 100 000 documents arrive as ≥ 90 bounded batches at
  a 1000-row hint (never one buffered result), every document is seen exactly
  once, the cursor reports the server's own `took`, a resume token exists
  mid-stream, and the process's resident set grows by well under 128 MiB
  across the whole scan — i.e. the driver is not accumulating the result.
- **`a_resume_token_continues_the_scan_where_it_stopped`** — the
  idle-auto-disconnect story. Three pages are read, the cursor is
  **closed** (which releases the point-in-time, exactly as the core does when
  it disconnects), and a fresh cursor built from the token alone returns
  precisely the remaining 700 documents with no document delivered twice.
- **`heterogeneous_documents_emit_schema_delta_add_column_events`** — the
  grid growing a column mid-stream instead of refetching. Four
  differently-shaped documents (seeded as raw JSON text so the
  `_source` key order on the wire is under the test's control) produce exactly
  one `SchemaDelta::AddColumn` per newly-observed field, in first-seen order,
  never re-announced — and a field missing from a document resolves to
  `Absent`, not to a fake null.
- **`cancel_mid_slow_query_reaches_the_server_and_returns_control`** — honest
  cancellation, end to end. A ~10 s scripted filter query (written so
  Elasticsearch cannot terminate the collection early and the JIT cannot fold
  the loop away) is cancelled after 2 s: the outcome is
  `CancelOutcome::ServerCancelled` — a task the tasks API confirmed, not an
  embellished client abandon — control returns in well under the query's own
  runtime, the pull reports `Cancelled` rather than an error, and the
  connection is immediately usable again.
- **`catalog_lists_indices_maps_fields_and_infers_shape`** — catalog
  laziness. Two levels, a prefix-narrowed `_cat/indices` listing, fields from
  **that one index's** `_mapping` (including nested and multi-fields),
  `describe` reporting document count, store size, a `fields` array and an
  `indexes` array while leaving `ObjectDetail::schema` `None` (because
  `SCHEMA_DECLARED` is false), sampling-based `infer_shape` with a truthful
  presence ratio, and bounded prefix completion.
- **`value_mapping_preserves_precision_bytes_and_the_absent_null_distinction`**
  — against the engine's own responses: a `long` of 2^53+1 survives exactly, a
  `scaled_float` comes back as `Decimal("123.456")` while a `double` correctly
  stays an `f64`, a `binary` field decodes to real bytes, an explicit JSON
  null is `Null`, and a field that was never written is absent.
- **`counts_filters_and_explain_report_what_they_actually_did`** —
  `EXACT_COUNT_CHEAP` being false, honestly: `Op::Count { exact: true }` runs
  `_count` and returns 20 000, `exact: false` returns 10 000 labelled
  `LOWER BOUND` with a matching notice, a compiled `And(Eq, Lt)` predicate
  really filters to 50, and both `EXPLAIN` forms work (`_validate/query` for
  the plan, `profile: true` for real timings).
- **`capabilities_refusals_and_read_only_are_honest`** — the flags a live ES 8
  connection reports, `ServerInfo` naming `pit+search_after`, transactions
  refused rather than downgraded, and `set_read_only(true)` returning
  `Enforcement::Client` and actually refusing a write while still allowing
  reads.
- **`native_console_requests_and_bound_parameters_work`** — Kibana-console
  text, a bare JSON search body against the default index, and a `$1`
  parameter bound into the parsed tree as a typed value.

## Note on seeding

The driver deliberately generates no writes (`EDITABLE_RESULTS` and `DDL` are
both off), so fixtures are created with a plain `reqwest` client rather than
through the seam under test. That is intentional: the tests exercise the read
path, and nothing in the write path is being silently vouched for.
