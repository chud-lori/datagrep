# dbx-drv-redis integration tests

Everything under `tests/` except `common/mod.rs` is `#[ignore]`d by default —
these are end-to-end tests against a **real Redis server**, not unit tests.

## Start a disposable Redis

```
docker run -d --rm -p 6379:6379 redis:7
```

**Never point these tests at a real database.** Every test starts by calling
`common::flush`, which runs `FLUSHDB` against whatever `DBX_TEST_REDIS`
points at. The default is `redis://localhost:6379` — only ever run this
suite against the disposable container above (or an equivalent throwaway
instance).

## Run them

```
export DBX_TEST_REDIS=redis://localhost:6379   # optional; this is the default
cargo test -p dbx-drv-redis -- --ignored --test-threads=1
```

`--test-threads=1` matters: every test flushes the whole database first, so
running the suite concurrently means one test's `flush()` can wipe another
test's data mid-run. Individual test binaries can also be run one at a time:

```
cargo test -p dbx-drv-redis --test scan_streaming -- --ignored
cargo test -p dbx-drv-redis --test cancel -- --ignored
cargo test -p dbx-drv-redis --test values_and_mutate -- --ignored
cargo test -p dbx-drv-redis --test catalog -- --ignored
```

## What each file proves

- **`scan_streaming.rs`** — the load-bearing contract (design §3.1
  requirement 2/3, §5.2). Seeds 50k plain keys and one 100k-field HASH, then
  asserts: `Op::Scan` browses the keyspace incrementally (many small
  batches, never one giant one); `HSCAN` pages the big hash the same way
  rather than returning it whole; a `resume_token` taken mid-scan and handed
  to a brand-new cursor continues with no gaps; and — checked from *outside*
  the driver via `INFO commandstats`, not by reading the source — `KEYS` is
  never sent to the server at any point.
- **`cancel.rs`** — cancelling a running `Op::Scan` (design §3.3) stops the
  client-side SCAN loop promptly (`DbError::Cancelled`, `ClientAbandoned`
  outcome, no server-side kill needed since Redis commands are atomic) and
  leaves the connection usable for the next request.
- **`values_and_mutate.rs`** — type-aware single-key fetch (`TYPE` then the
  right bounded reader for string/hash/set/zset/list); a missing key maps to
  `Value::Absent`, never `Value::Null`; `Op::Count` (`DBSIZE` and per-key
  `HLEN`); `Op::Mutate` (`SET`/`HSET`/`DEL`, atomic via one `MULTI`/`EXEC`
  pipeline, independently verified with raw `GET`/`HGET`/`EXISTS`);
  `Request::Native` dispatching a multi-line pipeline and shaping the last
  command's reply; and a hand-typed `SCAN ...` line in `Request::Native`
  routing through the same paging cursor the structured path uses.
- **`catalog.rs`** — `RedisCatalog` against a real server: the three-level
  `db-index -> keyspace-prefix -> key` hierarchy, `Enumeration::ScanOnly{
  requires_prefix: true}` actually being *enforced* (listing the
  keyspace-prefix level without an explicit prefix is refused, not silently
  turned into a full scan), and `describe()` reporting a real key's type.

## A note on `SCAN`'s `COUNT`

`scan_streaming.rs`'s per-batch assertions tolerate up to 2x the requested
`FetchHint::max_rows` rather than a strict `<=`. This isn't slack in the
driver — it's Redis's own documented behavior: `COUNT` bounds how much
*work* the server does per round, not the exact size of the reply, and the
driver forwards that hint honestly rather than fabricating a hard cap it
would have to enforce by silently dropping (and permanently losing) whatever
came back over the line, since a SCAN cursor can't resume from the middle of
a hash-table bucket.
