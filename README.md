# datagrep

**Every database in one native app. Free and open source.**

Postgres, MySQL, SQLite, Redis and MongoDB — SQL and documents as equals, not one
bolted onto the other. A native macOS app (SwiftUI + AppKit) and a CLI, both over
one Rust engine. No Electron, no JVM.

## Engines

| Engine | Status |
|---|---|
| PostgreSQL | working |
| SQLite | working |
| Redis | working |
| MongoDB | working |
| MySQL / MariaDB | working |
| Elasticsearch | working (read-only — no writes yet) |

The macOS app reaches all six. The CLI currently registers PostgreSQL and SQLite
only — the other four surface through the app.

## Install

Each [release](https://github.com/chud-lori/datagrep/releases) ships
`datagrep-macos.dmg`: open it and drag `datagrep.app` onto the Applications
shortcut. The installer script does the same and also installs the CLI:

```
curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash
curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash -s -- --app
```

The app checks for a newer release once per launch and only notifies you — it
never downloads or installs anything on its own, and the check can be turned off.
datagrep is not notarized, so a browser-downloaded build needs one Ctrl-click →
**Open** on first launch; installing via the script avoids that.

## Build

```
cargo build --release                  # engine + CLI  → target/release/datagrep
cd ui/macos && ./build-app.sh          # macOS app     → ui/macos/datagrep.app
```

The app needs no Xcode — Command Line Tools are enough, since it builds with
Swift Package Manager rather than `xcodebuild`.

## In the app

Connections are editable, and any of them can be marked read-only. The badge
says how real that promise is: *enforced by the server* only when the engine
itself refuses writes, *blocked by datagrep only* when the guard is our
client-side classifier — it never claims more protection than exists.

Click a table and the schema pane shows its columns with types, nullability and
primary keys, its indexes, and row/size estimates. For MongoDB the fields are
inferred from a sample, and the pane says so — a field missing from the sample
may still exist in the collection.

⌘Y opens the query history, searchable, over the same store the CLI's `history`
command reads. The grid has a row-number gutter, and a result that stops at the
500,000-row cap says "stopped at the 500,000-row limit — result incomplete"
rather than showing a count that looks final.

Not there yet: inline cell editing, autocomplete, an export UI, foreign-key
click-through, ER diagrams.

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

## Security

A database client holds live credentials, so this is engineered, not assumed:
secrets live in the OS keychain and profiles store only a `keychain:` reference
(exports have no field that can hold a secret), `cargo audit` + `cargo deny`
gate every PR and run weekly against the unchanged tree, and every FFI entry
point contains panics behind `catch_unwind`. Two limits worth knowing up front:
PostgreSQL and MySQL connections are **not** TLS-encrypted yet — use the
built-in SSH tunnel across untrusted networks — and an imported profile bundle
can carry `exec:` secret references, so read one before importing it.

The full threat model, the open gaps, and how to report a vulnerability
privately: [SECURITY.md](SECURITY.md).

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
