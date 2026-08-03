# `dbx-drv-mongo` integration tests

Unit tests (`src/*.rs`, `cargo test -p dbx-drv-mongo --lib`) need no server:
BSON<->`Value` mapping, `Predicate`->filter compilation (including the
NoSQL-injection case), `FieldTrie` inference math, keyset resume-token
round-trips, and URL parsing all run against fixtures in-process.

`tests/integration.rs` needs a real `mongod` and is `#[ignore]`d by default.

## Running

Start a disposable MongoDB:

```sh
docker run --rm -p 27017:27017 mongo
```

Then, from the repo root:

```sh
export CARGO_TARGET_DIR=/Users/nurchudlori/Projects/dbx/target-mongo
cargo test -p dbx-drv-mongo --test integration -- --ignored
```

By default the tests connect to `mongodb://localhost:27017`. Point them
elsewhere with:

```sh
DBX_TEST_MONGO="mongodb://user:pass@host:27017" \
  cargo test -p dbx-drv-mongo --test integration -- --ignored
```

Each test creates its own throwaway database (`dbx_drv_mongo_test_<label>_
<timestamp>_<counter>`) and drops it when it finishes, so tests are safe to
run concurrently (`--ignored --test-threads=4`, say) and safe to run
repeatedly against the same server.

## What each test proves

- `streams_100k_documents_in_incremental_batches` — a `find` over 100k
  documents comes back as many bounded batches (never one giant buffered
  result), and every document is seen exactly once (design §3.2's streaming
  contract).
- `heterogeneous_collection_emits_schema_delta_add_column_events` — a
  collection with documents of different shapes produces
  `SchemaDelta::AddColumn` exactly once per newly-observed field, in
  first-seen order, never re-announced (design risk #7 / ticket item 3).
- `nested_documents_round_trip_exactly` — nested objects, arrays,
  `Decimal128`, booleans, and an explicit `null` all survive the BSON->
  `Value` mapping intact (design §3.1 "never lose bytes"; `Absent`-vs-`Null`
  distinction).
- `cancel_mid_long_query_returns_control_promptly` — cancelling a
  deliberately slow `find` (`$where: "sleep(4000) || true"`) returns control
  in well under the query's own runtime, and the connection remains usable
  afterward — the design §3.3 "stop button always returns control instantly"
  contract, independent of whether this particular MongoDB user has
  `killOp` privileges.
- `catalog_lists_and_infers` — `listDatabases`/`listCollections` surface the
  seeded database/collection, and `infer_shape` reports the correct sample
  count, presence ratio, and heterogeneous type set for a field that is
  missing from one document and a different type in another.

## Requirements

- MongoDB 4.0+ recommended (the plain `docker run mongo` image above is a
  standalone `mongod`, so `Caps::TRANSACTIONS` will report `false` for it —
  that's expected and correct, not a test failure; deploy a single-node
  replica set if you want to exercise `begin()`/`Transaction`).
- `$where`/server-side JS must be enabled (the community server's default)
  for the cancellation test.
