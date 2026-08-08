# TEST-REPORT.md — datagrep against live servers

Tester agent, 2026-08-08. Everything below was **run, not read**: live `postgres:16`
(fixtures on :55432, seeded per `fixtures/README.md`), `redis:7` (:6379), `mongo:7`
(:27017), the release CLI (`target-test/release/datagrep`), the driver integration
suites, the FFI smoke test, and `ci/gates.sh`. All containers were torn down at the
end. Repro commands for every failure are inline.

**Bottom line: the streaming thesis holds at the driver layer, but the product
silently loses data at ~500k rows through both `query` and `export`, the Postgres
driver deadlocks forever on interleaved use, the Postgres catalog panics on every
table listing, and running the CLI test suite deletes the user's real keychain
credentials. Three of these four were found within the first hour of pointing the
code at a real server for the first time.**

---

## 1. What was stood up

| Fixture | How | Result |
|---|---|---|
| Postgres fixtures | `cd fixtures/postgres && docker compose up -d`, then polled the `bench_catalog_s200.t500` sentinel over the documented healthcheck trap | Seeded in ~2 min (README says ~75 s; close). Verified: `bench_wide` 1,000,000 · `bench_narrow` 10,000,000 · `bench_lowcard` 1,000,000 · `bench_catalog` 100,000 relations · `bench_hostile.huge` = 10,485,760 bytes |
| Redis | `docker run --rm -d -p 6379:6379 redis:7` | up |
| MongoDB | `docker run --rm -d -p 27017:27017 mongo:7` | up |

The README's healthcheck trap is real — sentinel polling was required; the
container answered queries before `bench_catalog_s200.t500` existed.

---

## 2. CRITICAL FAILURES (reproducible, with exact errors)

### F1 — Silent data loss: `query` AND `export` truncate at ~500k rows, exit 0, no warning

The `soft_row_cap = 500_000` (design §3.2, meant for the GUI grid's
"[Load more] **[Export all]**" affordance) is applied to every CoreApi path,
**including `export` — the path that exists precisely to bypass it**:

- `crates/datagrep-core/src/store.rs:89`, `query.rs:558`, `api.rs:377`, and
  **`export.rs:167`** all hardcode `soft_row_cap: 500_000`.

Measured, against live PG:

| Command | Rows in result | Rows delivered | Exit | Warning emitted |
|---|---|---|---|---|
| `query --format ndjson -c "SELECT * FROM bench_wide"` | 1,000,000 | 504,540 (run 1), 501,736 (run 2) | 0 | none |
| `export --format csv -o wide.csv` same query | 1,000,000 | 500,040 — stderr says `done: 500040 rows written` | 0 | none |
| `export` of `bench_narrow` | 10,000,000 | 526,109 | 0 | none |
| `query -c "…WHERE id <= 550000"` | 550,000 | 540,501 | 0 | stderr **empty** |
| `query --limit 600000 -c "…id <= 600000"` | 600,000 | 540,501 | 0 | `--limit` does **not** lift the cap |

Three aggravating details, all verified:

1. **The cutoff is nondeterministic** (500,040 / 501,736 / 504,540 / 526,109 /
   540,501): the cap check (`feeder.rs:405` and `:499`) races with batches in
   flight, and the overshoot scales with fetch speed. So identical runs return
   different row counts, all with exit 0.
2. **The delivered rows are not even a prefix.** In one 1M-row run the output
   contained exactly ids 504,321→1,000,000 followed by 1→6,056 (one contiguous
   hole of 498,264 rows: 6,057–504,320). That ordering is Postgres synchronized
   seq-scan starting mid-table — legal for an unordered SELECT — but combined
   with the cap it means a capped result is an *arbitrary contiguous slice*, not
   "the first 500k rows".
3. **The core knows and the CLI doesn't say.** The FFI smoke test's own status
   JSON reports `"state":"capped","rows_loaded":502000`, and `history list`
   records `ok … 540500 rows`. The information exists at every layer; the CLI
   surfaces none of it — no stderr note, no non-zero exit, and export prints
   `done:`.

Repro:
```sh
datagrep export --profile fix -c "SELECT * FROM bench_wide" --format csv -o /tmp/w.csv
# stderr: "done: 500040 rows written ..." ; exit 0 ; table has 1,000,000 rows
```

### F2 — Postgres driver deadlocks forever on any interleaved use (2 of 8 integration tests hang)

First-ever live run of `cargo test -p datagrep-drv-postgres --test integration -- --ignored`:

- `catalog_children_on_seeded_schema` — **hung 14 m 43 s** (until killed).
  `pg_stat_activity`: connection `idle in transaction`, last query
  `SELECT current_database()`. Deterministic — reproduced twice, hangs at 60 s,
  120 s, forever.
- `scan_op_streams_with_identity` — **hung >120 s** (until killed). Server shows
  `idle in transaction`, last query `SELECT * FROM "datagrep_scan_test" LIMIT 5`.

Mechanism (from the hang states + source): every SELECT-ish `execute()` takes the
connection mutex as an `OwnedMutexGuard` and hands it to the cursor's actor
(`crates/datagrep-drv-postgres/src/connection.rs:127`, `actor.rs:74`); the guard is
held until the cursor is **dropped**, not merely drained. Every other operation on
the same connection — `catalog()` methods (`catalog.rs:34` and friends) and the next
`execute()` (`connection.rs:98`) — awaits that same mutex with no timeout. So:

> hold any open result cursor + do anything else on that connection = permanent hang.

In the catalog test the trigger is `catalog.children()` while a fully-read cursor
variable is still in scope; in the scan test it's the cleanup `DROP TABLE` after the
scan cursor. This is the GUI's bread-and-butter interleaving (results grid open +
schema tree click), and it never returns an error — it just freezes.

The other 6 tests pass when run one-per-process:
`cancel_mid_sleep_leaves_connection_usable`, `non_select_returns_ack_shape_without_a_portal`,
`numeric_round_trips_as_decimal_string`, `quote_ident_survives_a_hostile_identifier`,
`streaming_does_not_retain_the_whole_result_set`, `streams_100k_rows_first_batch_arrives_fast` — all `ok`.

### F3 — Postgres catalog panics listing tables in ANY schema, and on `--describe`

```
$ datagrep catalog --profile fix datagrep_fixtures bench_catalog_s137
thread 'tokio-rt-worker' panicked at crates/datagrep-drv-postgres/src/catalog.rs:330:43:
error retrieving column 1: error deserializing column 1

$ datagrep catalog --profile fix --describe datagrep_fixtures.bench_catalog_s137.t250
thread 'main' panicked at crates/datagrep-drv-postgres/src/catalog.rs:424:27:
error retrieving column 1: error deserializing column 1
```

Root cause: both sites `row.get::<_, String>(1)` on `c.relkind`, but `relkind` is
Postgres type `"char"` (1-byte), which tokio-postgres refuses to decode as `String`
(it maps to `i8`; the query needs `c.relkind::text`). Panics (`get` not `try_get`)
crash the process instead of returning `DbError`. Levels 0 (databases) and 1
(schemas) work; level 2 (tables) and describe are 100 % broken against a real
server — proof this path had never been executed live.

### F4 — `cargo test -p datagrep-cli` deletes the user's REAL keychain credentials

`crates/datagrep-cli/tests/cli.rs:171-175`:

```rust
// Best-effort cleanup of the real keychain entry this test created.
let _ = Command::new("security")
    .args(["delete-generic-password", "-s", "datagrep"])
    .output();
```

`security delete-generic-password -s datagrep` deletes the **first** entry matching
the service, regardless of account — i.e. whatever real profile secret the user has.
This bit me **twice during this session**: sibling agents ran the CLI tests
(03:32:58Z and again later) and my working profile started failing with

```
error: keychain error for service `datagrep`, account `19fdf6c69e1-0-11f39:password`:
No matching entry found in secure storage
```

while the keychain contained only the test's own orphaned entry. Any user who runs
the test suite loses their saved database passwords. (The in-crate unit test at
`src/cmd/profiles.rs` cleans up correctly via its own `SecretRef`; only the
integration test is destructive.)

### F5 — `INSERT … RETURNING` fails outright on Postgres

```
$ datagrep query --profile fix -c "…; INSERT INTO ret_test(v) VALUES ('a') RETURNING id;"
error: query failed [25006]: cannot execute INSERT in a read-only transaction   (exit 1)
```

Known gap (comment at `connection.rs:120-127`: statements with columns are wrapped
in a READ ONLY transaction for the portal), but it is user-facing and it breaks a
bread-and-butter statement. Verified end-to-end.

---

## 3. Central claims, measured (vs budget.toml / design §5)

| Budget | Target / fail | Measured | Verdict |
|---|---|---|---|
| P8 first-row latency, localhost PG | 70 ms / 130 ms p50/p95, fail 250 ms p95 | `SELECT 1` time-to-first-byte incl. process spawn+connect, 10 runs: p50 ≈ 52 ms, one outlier 355 ms. `SELECT * FROM bench_wide` (1M×24): first byte **128 ms**, total 12.4 s → first row in ~1 % of total | **PASS** (streaming is real; outlier worth watching) |
| P6 RAM, 1M rows streamed | idle+260 MB / fail idle+480 MB | 1M-row ndjson stream (~380 MB emitted, ~1.2 GB wire): RSS sampled every 0.5 s — 2 MB → 112 → 143 → peak **174 MB** → 129 MB at end. `export`: peak RSS 155 MB, peak phys footprint 89 MB. Flat plateau, does **not** grow with dataset | **PASS** (with the huge caveat of F1: it also only delivered half the rows) |
| P17 cancel → usable | 100 ms / 400 ms | CLI SIGINT → exit: **3 ms** (`pg_sleep(60)`), **3 ms** (mid-stream `bench_slow`), 8 ms (mid-export). Exit 130, real message. `pg_stat_activity` after: **0 orphaned queries**. Driver test `cancel_mid_sleep_leaves_connection_usable`: ok. FFI smoke: cancel returned in 0.011 ms, rows-before-cancel kept | **PASS** — genuinely excellent |
| M1: 100k-relation catalog connect < 400 ms | <400 ms | `doctor` connect round-trip: **7.3–28.8 ms** against the 100k-relation DB; schema level (201 rows) lists in ~99 ms | **PASS on latency — but the feature then panics at table level (F3)** |
| Lazy CLI startup (P1-adjacent) | 250 ms | `--help` 12 ms; profile ops instant | PASS |

Streaming-first-render also holds for `--format table`: 400k-row table query
produced its first byte at 166 ms (not after the full 2.1 s run), peak RSS 60 MB —
column widths are clearly computed per-window, not from a full buffer.

---

## 4. Integration suites — full results

| Suite | Command | Result |
|---|---|---|
| Postgres (`DATAGREP_TEST_PG=1` + `_HOST/_PORT/_USER/_PASSWORD/_DB` → fixtures :55432) | `cargo test -p datagrep-drv-postgres --test integration -- --ignored --test-threads=1` | **6 pass / 2 hang forever (F2)**. The suite as shipped never completes — it deadlocks on test 2 of 8 and sat 14+ min at 0 % CPU until killed |
| Redis (`DATAGREP_TEST_REDIS=redis://localhost:6379`) | `cargo test -p datagrep-drv-redis -- --ignored --test-threads=1` | **13/13 pass** — scan paging, 100k-field HSCAN, resume tokens, KEYS-never-sent (verified via `INFO commandstats`), cancel, catalog, mutate |
| Mongo (`DATAGREP_TEST_MONGO=mongodb://localhost:27017`) | `cargo test -p datagrep-drv-mongo --test integration -- --ignored --test-threads=4` | **5/5 pass** — 100k-doc streaming, schema deltas, nested round-trip, cancel, catalog/infer |
| FFI smoke | `bash crates/datagrep-ffi/tests/run_smoke.sh` | **`SMOKE TEST PASSED (0 failures)`** — includes cancel (0.011 ms), NULL-safety, teardown; its status JSON is where `"state":"capped"` is visible |
| CI gate | `bash ci/gates.sh` | see §6 |

Note: the env vars are correctly the renamed `DATAGREP_*` forms everywhere; each
crate's `tests/README.md` matches reality. `DATAGREP_TEST_PG` is a flag (`=1`),
connection details ride in `DATAGREP_TEST_PG_*`.

---

## 5. CLI end-to-end — what works and smaller defects

Worked correctly (all against live Postgres unless noted): `profiles
add/list/show/export/import/remove` (round-trip verified into a second config dir);
all five formats; NULL vs empty-string distinction in `table` (and `json` maps
NULL→`null`); multi-statement; `-- @limit 5` honored; `-- @readonly` blocks DDL
client-side with a clear message and exit 1 (table verified absent server-side);
`history list` + FTS `history search`; `doctor`; SQLite profile create/insert/select;
unicode data round-trips in every format; the 10 MB `bench_hostile` cell truncates
with `…` in table mode (no OOM, RSS fine) and round-trips **byte-complete**
(10,485,781 bytes) in ndjson; syntax errors carry SQLSTATE (`[42601]`) and exit 1;
empty input exits 2; cancel exits 130 with the real cancel message.

Smaller defects found:

1. **`--format json`/`ndjson` stringify every scalar**: `SELECT 42::int, true, 3.5::float8, '{"k":1}'::jsonb`
   → `{"i":"42","b":"true","f":"3.5","js":"{\"k\": 1}"}`. Ints, bools, floats as
   JSON strings and **jsonb double-encoded**. The README's headline
   `--format json | jq` breaks for any numeric/boolean filter
   (`select(.b)`, `.i > 40`). Numeric-as-string is a defensible §risk-4 fidelity
   choice; bool/int/jsonb-as-string looks like over-application of it.
2. **Bad password error is useless**: `error: connect failed: db error` (exit 1).
   The server's `password authentication failed for user "datagrep"` is swallowed.
   Bad host produces the similarly generic `error connecting to server`.
3. **Export progress spams stderr** ~every 15 ms when stderr is not a TTY (230+
   `N rows (rate)` lines for a 7 s export). Should be TTY-gated or throttled.
4. **Zero-row CSV emits nothing at all** — not even the header line.
5. Table format: CJK column names misalign columns (display-width vs char count);
   embedded tabs/newlines in values are printed raw and break row alignment.
6. `-c` values starting with `-` (e.g. a leading `-- @readonly` directive) are
   rejected by clap as an unknown flag; works via stdin/`-f`. Cosmetic but the
   directive syntax makes it likely.
7. Only `postgres` and `sqlite` are registered in the CLI (`doctor` confirms);
   redis/mongo drivers exist but have no CLI face. (Documented in the crate
   README, listed here for completeness.)
8. `CREATE TABLE` prints `(0 rows shown — … affected-row counts don't reach
   CoreApi today)` — ack counts exist in the driver (`Shape::Ack{affected:3}`
   test passes) but are lost before the CLI.

---

## 6. ci/gates.sh

`CARGO_TARGET_DIR=target-test bash ci/gates.sh` → **`gates.sh: ALL GATES PASSED`**.
Detail, verbatim:

- fmt, clippy (`--workspace --all-targets -D warnings`), `cargo test --workspace`:
  all OK (0 unit-test failures, including the two new sibling crates
  `datagrep-drv-mysql` / `datagrep-drv-elasticsearch` that appeared mid-session).
- grep-gates: OK with warnings —
  `WARN unbounded-channel /crates/datagrep-tunnel/src/host_key.rs:114` (1, non-blocking)
  and `WARN unwrap: 433 occurrence(s) (non-blocking)`.
- budget-check: `target-test/release/datagrep` is 7.7 MB — within P11 target (55 MB).
- count-crates: 392 unique crates — within P16d target (≤400), 8 of headroom left.

Caveat the gate itself cannot see: "ALL GATES PASSED" coexists with F1–F5 above —
every one of those bugs lives behind an `#[ignore]` or a code path no unit test
executes. The 482-passing-unit-tests number is real and insufficient.

Side effect worth knowing: `cargo test --workspace` inside gates runs the F4
keychain-deleting test, so running the CI gate locally also destroys the user's
saved credentials.

---

## 7. Test-harness observations

- The Postgres suite **cannot finish as shipped** (F2): with `--test-threads=1` it
  deadlocks on the second test alphabetically and the remaining six never run.
  Nothing in the harness times out; CI would hang until the job timeout.
- Rebuilds emitted paths under `/Users/nurchudlori/Projects/datagrep/…` (not
  `…/dbx`) for `datagrep-api`/`datagrep-drv-postgres` — stale absolute paths from a
  previous checkout location baked into build artifacts; harmless but confusing in
  backtraces.
- The fixtures healthcheck trap in `fixtures/README.md` is accurately documented
  and the suggested sentinel poll works.

## 8. Teardown

`docker compose down -v` on the fixtures (volume removed), `docker stop` on
`dg-test-redis` and `dg-test-mongo` (both were `--rm`). One container named
`dg-test-es` (Elasticsearch) was running at teardown time — it is a sibling
agent's, not started by this test run, and was left alone.

## 9. Ranked summary for the fixing agents

1. **F1** `export.rs:167` (+ CLI surfacing in `query`): silent, nondeterministic,
   non-prefix data loss at ~500k rows through both user-facing paths, exit 0.
2. **F2** `connection.rs`/`actor.rs`/`catalog.rs`: connection-wide mutex held for
   cursor lifetime ⇒ guaranteed permanent deadlock on interleaved use; hangs the
   integration suite itself.
3. **F3** `catalog.rs:330,:424`: `relkind` decoded as `String` ⇒ panic on every
   table listing/describe against live PG (`::text` it, and use `try_get`).
4. **F4** `tests/cli.rs:171-175`: test suite deletes the user's real keychain
   secrets (`-s datagrep` with no account) — delete the exact `SecretRef` the
   test created instead.
5. **F5** `INSERT … RETURNING` fails (25006) — known gap, but real users will hit
   it in week one.
6. §5 items 1–8, in roughly that order.
