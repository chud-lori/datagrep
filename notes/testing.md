# Testing

`cargo test --workspace` runs everything that needs no server.

Live-engine suites are `#[ignore]`d and gated behind an env var, so they only run
when you point them at a server. **They matter** — every critical bug found in
this project so far was found by running these, not by the unit tests.

## Postgres

```
docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=secret --name dg-pg postgres:16
DATAGREP_TEST_PG=postgres://postgres:secret@localhost:5432/postgres \
  cargo test -p datagrep-drv-postgres -- --ignored
docker rm -f dg-pg
```

## MySQL / MariaDB

```
docker run --rm -d -p 3306:3306 -e MYSQL_ROOT_PASSWORD=secret --name dg-my mysql:8
DATAGREP_TEST_MYSQL=mysql://root:secret@localhost:3306/mysql \
  cargo test -p datagrep-drv-mysql -- --ignored
docker rm -f dg-my
```

Swap `mysql:8` for `mariadb:11` to exercise the MariaDB path — the driver detects
the flavour from `@@version` and adjusts (`ANALYZE` instead of `EXPLAIN ANALYZE`,
`max_statement_time` instead of `max_execution_time`, JSON as `LONGTEXT`).

## Redis

```
docker run --rm -d -p 6379:6379 --name dg-redis redis:7
DATAGREP_TEST_REDIS=redis://localhost:6379 \
  cargo test -p datagrep-drv-redis -- --ignored
docker rm -f dg-redis
```

## MongoDB

```
docker run --rm -d -p 27017:27017 --name dg-mongo mongo:7
DATAGREP_TEST_MONGO=mongodb://localhost:27017 \
  cargo test -p datagrep-drv-mongo -- --ignored
docker rm -f dg-mongo
```

## Elasticsearch

```
docker run --rm -d -p 9200:9200 -e discovery.type=single-node \
  -e xpack.security.enabled=false --name dg-es \
  docker.elastic.co/elasticsearch/elasticsearch:8.15.0
# needs ~60s to go green — poll, don't assume
DATAGREP_TEST_ES=http://localhost:9200 \
  cargo test -p datagrep-drv-elasticsearch -- --ignored
docker rm -f dg-es
```

## Benchmark fixtures

`fixtures/postgres/` seeds six datasets used for performance work: `bench_wide`
(1M rows × 24 mixed-type columns), `bench_narrow` (10M × 3), `bench_lowcard`
(exercises dictionary encoding), `bench_catalog` (200 schemas × 500 tables =
100k relations), `bench_slow` (cancel testing), and `bench_hostile` (one 10 MB
value). Full seed takes ~75s on an M1.

```
cd fixtures/postgres && docker compose up -d
```

**Do not trust the healthcheck.** The official Postgres image runs init scripts
against a temporary Unix-socket-only server, so `pg_isready` reports healthy
*before seeding finishes*. Poll for a sentinel over TCP instead:

```
until psql "$URL" -tAc "select count(*) from bench_wide" 2>/dev/null | grep -q 1000000; do sleep 2; done
```

SQLite needs nothing: `fixtures/sqlite/seed.sql` builds a 2M-row table in ~2.3s.

## FFI smoke test

Exercises the C ABI the macOS app links against — streaming, windowed reads,
cell kinds, cancel latency, NULL-pointer safety.

```
bash crates/datagrep-ffi/tests/run_smoke.sh
```

## CI gate

```
bash ci/gates.sh
```

Runs fmt, clippy (`-D warnings`), the workspace tests, and greps for banned
patterns from the design's anti-pattern list — `unbounded_channel` in
`datagrep-core` is a hard failure, as is `ControlFlow::Poll` or a free-running
`tokio::time::interval` (the timer wheel must be armed on demand). Waivers go in
`ci/grep-allowlist.txt` with a reason.

Binary size and crate count are checked against [`budget.toml`](../budget.toml).

**A green gate does not mean the app works.** It means the tests that ran
passed. Five critical bugs — including silent data loss on export and a driver
deadlock — coexisted with `ALL GATES PASSED`, because each sat behind an
`#[ignore]` or on a path no test executed. Run the live suites.

## macOS app

```
cd ui/macos && swift build -c release && ./build-app.sh
```

The app can screenshot itself, which is how UI changes get verified without
Screen Recording permission:

```
./.build/release/datagrep-app --screenshot /tmp/shot.png 8      # delay in seconds
./.build/release/datagrep-app --theme-flip-shot /tmp/l.png /tmp/d.png
```

`--theme-flip-shot` snapshots, flips `NSApp.appearance` at runtime, and snapshots
again — proving dark-mode assets re-resolve live rather than only at launch.

If the Swift build fails with `FileManager has no member 'default'` or
`stat error: No such file or directory`, run `rm -rf .build`. That's stale
absolute paths from a directory move, not a code error.
