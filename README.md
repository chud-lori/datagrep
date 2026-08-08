# datagrep

A lightweight database client for SQL and NoSQL in one app — *the client you don't
have to close to get your laptop back.*

Native macOS app (SwiftUI + AppKit) and a CLI, both over one Rust engine.

## Engines

| Engine | Status |
|---|---|
| PostgreSQL | working |
| SQLite | working |
| Redis | working |
| MongoDB | working |
| MySQL / MariaDB | in progress |
| Elasticsearch | in progress |

## Build

```
cargo build --release                  # engine + CLI  → target/release/datagrep
cd ui/macos && ./build-app.sh          # macOS app     → ui/macos/datagrep.app
```

The app needs no Xcode — Command Line Tools are enough, since it builds with
Swift Package Manager rather than `xcodebuild`.

## CLI quickstart

```console
$ datagrep profiles add local "sqlite:///tmp/app.db"
created profile `local` (sqlite)

$ datagrep query --profile local -c "SELECT id, name, note FROM users ORDER BY id"
id  | name  | note
----+-------+----------
1   | alice | NULL
2   | bob   |
3   | carol | likes SQL
(3 rows)

$ datagrep export --profile local -c "SELECT * FROM events" --format csv -o events.csv
$ datagrep query --profile prod -f report.sql --format json | jq '.[].email'
```

`NULL` (row 1) renders differently from an empty string (row 2), and differently
again from a field that is *absent* from a document — three distinct facts that
most clients collapse into one blank cell.

Commands: `query`, `export`, `profiles`, `catalog`, `history`, `doctor`.
A password in a connection URL is moved to the OS keychain on import; profiles
store only a reference to it.

## Block directives

Any statement can carry per-block settings as comments:

```sql
-- @limit 200
-- @timeout 30s
-- @connection staging
-- @readonly
SELECT * FROM events;
```

## How it stays small

Results stream. The driver, a bounded channel, and the result store form a
pipeline where nothing runs more than two chunks ahead — so when the UI stops
consuming, the driver stops reading its socket, the TCP window closes, and the
server stops producing. A million-row result never becomes a million resident
rows.

Schema browsing is lazy: one cheap query per level you expand, never a crawl of
the whole catalog on connect.

The performance budget is machine-readable in [`budget.toml`](budget.toml) and
enforced by [`ci/gates.sh`](ci/gates.sh).

## Layout

```
crates/
  datagrep-api/        the stable seam: Driver / Connection / Cursor / Catalog /
                       Value / Shape / Capabilities. ~5 small deps, by rule.
  datagrep-core/       streaming pipeline, result store, sessions, query lifecycle
  datagrep-lang/       SQL splitting, highlighting, Mongo shell + Redis parsing
  datagrep-drv-*/      one crate per engine
  datagrep-ffi/        C ABI the macOS app links against
  datagrep-cli/        the terminal frontend
  datagrep-profiles/   connection store, query history (SQLite)
  datagrep-secrets/    keychain / env / exec credential resolution
  datagrep-tunnel/     SSH tunnels (in-process, no listening port)
ui/macos/              the macOS app
fixtures/              seeded benchmark datasets
notes/                 engineering notes — testing, UX study, reports
                       (docs/ is left free for the GitHub Pages site)
```

Rules that keep it honest: drivers never see Arrow or the UI, `datagrep-core`
never names a concrete driver, and `if driver_id == …` above `datagrep-api` is
banned — any such branch means a missing capability flag.

## Testing

`cargo test --workspace` runs everything that needs no server.
See [`notes/testing.md`](notes/testing.md) for the live-engine suites.
