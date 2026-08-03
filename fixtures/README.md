# fixtures/

Seeded datasets for the design doc's §6 measurement harness. These are the
inputs the Tier-2 nightly perf gates (and manual local profiling) run
against — see `ci/README.md` for how Tier-1 (every PR) and Tier-2 (nightly,
real hardware) split.

## Postgres — `fixtures/postgres/`

```
cd fixtures/postgres
docker compose up -d
docker compose logs -f postgres   # watch seeding progress
```

`seed.sql` runs automatically on first boot via
`docker-entrypoint-initdb.d`, against a fresh `dbx-fixtures-pgdata` volume.
Connection string once seeded:

```
postgres://dbx:dbx@localhost:55432/dbx_fixtures
```

To reseed from scratch (e.g. after editing `seed.sql`):

```
docker compose down -v   # -v drops the volume, so init scripts rerun
docker compose up -d
```

**Gotcha: `docker compose up -d --wait` and the healthcheck both go green
before seeding is actually done.** The official `postgres` image starts a
*temporary* Unix-socket-only server to run
`docker-entrypoint-initdb.d/*` scripts, then restarts as the real
TCP-listening server once they finish. `pg_isready` (what the healthcheck
in `docker-compose.yml` runs) succeeds against *either* server, so
`--wait`/`health: healthy` can report ready while `seed.sql` is still
mid-`INSERT`. Measured directly: this genuinely races on this dataset size.
**Don't gate anything on the healthcheck alone.** Instead, wait for the log
line that only the finished script prints:

```
docker compose logs postgres | grep -q 'bench_catalog (relations)'
```

or just poll for the tables to exist:

```
until docker exec dbx-fixtures-postgres psql -U dbx -d dbx_fixtures -c 'select 1 from bench_catalog_s200.t500' >/dev/null 2>&1; do sleep 2; done
```

### Fixtures

| Table/view | Shape | Proves |
|---|---|---|
| `bench_wide` | 1,000,000 rows × 24 mixed-type columns (int/numeric/bool/text/timestamptz/date/time/interval/array/jsonb/uuid/inet/bytea) | The headline streaming fixture (§6), ~1.2 GB on the wire. Every `Value` variant round-trips. |
| `bench_narrow` | 10,000,000 rows × 3 columns | Sustained throughput; the fixture that dominates seed wall-clock. |
| `bench_lowcard` | 1,000,000 rows × 6 text columns, ≤50 distinct values each | Dictionary encoding actually fires — cardinality is 0.005–0.005% of row count, far under the design's <10% threshold (§5.1). |
| `bench_catalog_s1..s200.t1..t500` | 200 schemas × 500 tables = 100,000 relations | Catalog introspection stays O(1), not O(schema size) (§5.2). Backs the M1 exit criterion "100k-relation catalog connect <400ms". |
| `bench_slow` (view over `bench_slow_rows()`) | Set-returning function, `delay_ms` per row, default 1,000,000 rows × 5ms | P17 cancel testing — start streaming, cancel mid-stream, assert the connection is usable again inside budget, at any offset (not just one fixed `pg_sleep(N)` point). |
| `bench_hostile` | 1 row, one 10 MB `text` value | Proves graceful truncation in the grid/preview path, not an OOM (§5.2: "loading the full result before showing row 1" is banned). Note: `pg_column_size()` reports a small TOAST-compressed on-disk size because the value is highly repetitive (`repeat('x', ...)`) — `length()`/`octet_length()` and the wire size the driver actually receives are the full 10,485,760 bytes. |

### Seed timings

Measured locally (Apple Silicon, Docker Desktop, local NVMe, `postgres:16`,
`fsync=off`): the full seed — 1M + 10M + 1M row inserts, 100,000 DDL
statements for `bench_catalog`, `bench_hostile`, `bench_slow` — completed
in **~75 seconds** end to end (container start to "ready to accept
connections" on the final server). `bench_narrow`'s 10M-row insert
dominates.

**Do not assume CI runners are this fast.** Design doc §6 budgets **15–25
minutes** for this same seed on typical shared CI hardware (network-backed
disk, shared vCPUs, no host page cache warm from a prior run) — that
estimate is what `ci/README.md`'s Tier-1/Tier-2 split is built around, and
it's the reason Tier-1 (every PR) never runs this seed. If you rerun this
locally, expect something between these two numbers depending on your
disk.

**This must not run on every CI job.** Per §6: "seeding 1M+ rows every CI
run costs 15–25 min and you will delete the job." The intended path,
**not yet implemented, tracked as a TODO**: bake a seeded `dbx-fixtures-pgdata`
volume (or the whole container) into a versioned image/snapshot, published
once and pulled by nightly Tier-2 runs — reseed only when `seed.sql`
changes, not every run.

## SQLite — `fixtures/sqlite/`

```
sqlite3 fixtures/sqlite/bench.db < fixtures/sqlite/seed.sql
```

`bench_sqlite`: 2,000,000 rows, generated via `WITH RECURSIVE` (no server,
no container). Measured locally: **~2.3 seconds** end to end, ~49 MB
database file. This is the "fastest CI signal" fixture per §6 — prefer it
over bringing up `fixtures/postgres/` for anything that doesn't
specifically need Postgres wire-protocol behavior, streaming cancellation,
or catalog scale.

## Determinism

Every fixture that uses `random()` is seeded via `SELECT setseed(0.42);`
at the top of `fixtures/postgres/seed.sql`, once per session — matching
design doc §6's "deterministic, seed 42" fixture requirement (0.42 because
Postgres's `setseed()` takes a float in `[-1, 1]`, not an integer).
Columns derived purely from the row number (`generate_series`) need no
seed; they're already deterministic. SQLite's `bench_sqlite` uses no
randomness at all — `WITH RECURSIVE` plus arithmetic on the row number is
deterministic by construction.
