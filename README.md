<div align="center">

<img src="docs/appicon.svg" alt="" width="104" height="104">

# datagrep

**Every database in one native app. Free and open source.**

Postgres, MySQL, SQLite, Redis, MongoDB and Elasticsearch — SQL and documents as
equals, not one bolted onto the other. A native macOS app and a CLI, both over
one Rust engine. No Electron, no JVM. A native Linux app (Qt6/C++) over the same
engine is in progress — see [`ui/linux`](ui/linux/).

[![release](https://img.shields.io/github/v/release/chud-lori/datagrep?color=2E7D4F&label=release)](https://github.com/chud-lori/datagrep/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/chud-lori/datagrep/ci.yml?branch=main&label=CI)](https://github.com/chud-lori/datagrep/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![platform](https://img.shields.io/badge/platform-macOS%2014%2B-lightgrey)

[Install](#install) · [Engines](#engines) · [In the app](#in-the-app) ·
[CLI](#cli-quickstart) · [Security](#security) · [Contributing](#contributing)

</div>

---

## Contents

- [Why](#why)
- [Engines](#engines)
- [Install](#install)
- [Uninstall](#uninstall)
- [Build from source](#build-from-source)
- [In the app](#in-the-app)
- [CLI quickstart](#cli-quickstart)
- [Block directives](#block-directives)
- [How it stays small](#how-it-stays-small)
- [Security](#security)
- [Layout](#layout)
- [Contributing](#contributing)
- [License](#license)

## Why

Most clients make you choose. DataGrip and DBeaver cover a lot of engines and
cost you a JVM and a gigabyte of disk; the lightweight ones tend to speak one
database well and treat the rest as an afterthought.

datagrep is a **24 MB native app** that cold-starts in about 250 ms, opens SQL
and NoSQL connections side by side, and streams results instead of loading them
— a million-row result never becomes a million resident rows. Both numbers are
printed by the app itself (`MEASURE cold start` on stderr) and by `du -sh`, so
you do not have to take them on trust.

It is also honest about what it does not do. `NULL`, an empty string, and a
field *absent* from a document are three different facts, and it renders them
three different ways. Read-only mode tells you whether the server is enforcing
it or datagrep is. Where a limit is hit, it says so instead of showing a count
that looks final.

## Engines

| Engine | App | CLI | Notes |
|---|:---:|:---:|---|
| PostgreSQL | ✅ | ✅ | |
| SQLite | ✅ | ✅ | |
| MySQL / MariaDB | ✅ | — | |
| MongoDB | ✅ | — | schema inferred from a sample, and labelled as such |
| Redis | ✅ | — | scan-only enumeration; never `KEYS *` |
| Elasticsearch | ✅ | — | read-only, no writes yet |

> [!NOTE]
> The CLI currently registers PostgreSQL and SQLite only. The other four are
> reachable from the app. Wiring them into the CLI is one line per driver and is
> tracked as open work.

## Install

### DMG

Each [release](https://github.com/chud-lori/datagrep/releases) ships
`datagrep-macos.dmg` — open it and drag `datagrep.app` onto the Applications
shortcut.

> [!WARNING]
> **First launch from the DMG shows a scary warning.** datagrep is not notarized
> (there is no Apple Developer account behind it), so macOS says it "could not
> verify datagrep is free of malware", and on recent versions the only obvious
> button is *Move to Bin*. The app is fine — macOS simply cannot attribute it to
> a registered developer. Open it once via **System Settings → Privacy &
> Security → Open Anyway**, or clear the download flag yourself:
>
> ```
> xattr -dr com.apple.quarantine /Applications/datagrep.app
> ```
>
> Ctrl-click → **Open** worked on older macOS but no longer does on macOS 15.

### Script

Installs the CLI. Add `--app` and you get **both** the CLI and the app — the
app is added, not swapped in. Nothing a browser downloaded, so nothing is
quarantined and the warning above never appears.

```
▶ curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash
▶ curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash -s -- --app
```

The app checks for a newer release once per launch and only notifies you — it
never downloads or installs anything on its own, and the check can be turned off.

## Uninstall

```
▶ curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash -s -- --uninstall
▶ curl -fsSL https://raw.githubusercontent.com/chud-lori/datagrep/main/install.sh | bash -s -- --uninstall --app
```

The first removes the CLI from `~/.local/bin`; the second removes
`datagrep.app` as well. By hand, that is `rm ~/.local/bin/datagrep` and
`rm -rf /Applications/datagrep.app`.

> [!NOTE]
> Uninstalling deliberately leaves your data alone — connections, query history
> and saved editors survive a reinstall. To remove those too:
>
> ```
> rm -rf ~/Library/Application\ Support/datagrep   # connections, history, editor tabs
> rm -rf ~/.config/datagrep                        # the CLI's own profile store
> ```
>
> Saved passwords live in the login keychain under the service `datagrep` and
> are removed with the connection that owns them. Any left behind can be deleted
> in Keychain Access by searching for `datagrep`.

## Build from source

```
▶ cargo build --release                  # engine + CLI  → target/release/datagrep
▶ cd ui/macos && ./build-app.sh          # macOS app     → ui/macos/datagrep.app
▶ cd ui/linux && cmake -S . -B build && cmake --build build   # Linux app (Qt6, in progress)
```

> [!TIP]
> No Xcode required — Command Line Tools are enough, because the app builds with
> Swift Package Manager rather than `xcodebuild`.

## In the app

Connections are editable, and any of them can be marked read-only. The badge
says how real that promise is: *enforced by the server* only when the engine
itself refuses writes, *blocked by datagrep only* when the guard is our
client-side classifier — it never claims more protection than exists. Give a
connection a colour and it tints the window, the sidebar and the titlebar;
datagrep does not decide what your colours mean.

Click a table and the schema pane shows its columns with types, nullability and
primary keys, its indexes, and row/size estimates. For MongoDB the fields are
inferred from a sample, and the pane says so — a field missing from the sample
may still exist in the collection.

Editors belong to a connection: each one keeps its own tabs, and closing a tab
with unsaved SQL asks first. Quitting never does, because the session comes back
exactly as you left it.

⌘Y opens the query history, searchable, over the same store the CLI's `history`
command reads. The grid has a row-number gutter, and a result that stops at the
500,000-row cap says "stopped at the 500,000-row limit — result incomplete"
rather than showing a count that looks final.

**Not there yet:** inline cell editing, autocomplete, an export UI, foreign-key
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
$ datagrep query --profile reports -f report.sql --format json | jq '.[].email'
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
-- @connection reports
-- @readonly
SELECT * FROM events;
```

## How it stays small

Results stream. The driver, a bounded channel, and the result store form a
pipeline where nothing runs more than two chunks ahead — so when the UI stops
consuming, the driver stops reading its socket, the TCP window closes, and the
server stops producing.

Schema browsing is lazy: one cheap query per level you expand, never a crawl of
the whole catalog on connect.

The performance budget is machine-readable in [`budget.toml`](budget.toml) and
enforced by [`ci/gates.sh`](ci/gates.sh).

## Security

A database client holds live credentials, so this is engineered rather than
assumed: secrets live in the OS keychain and profiles store only a `keychain:`
reference (exports have no field that can hold a secret), `cargo audit` and
`cargo deny` gate every PR and run weekly against the unchanged tree, and every
FFI entry point contains panics behind `catch_unwind`.

> [!IMPORTANT]
> Two limits worth knowing before you connect anything real:
> - **PostgreSQL and MySQL connections are not TLS-encrypted yet.** Both refuse
>   rather than downgrade silently, and the built-in SSH tunnel is the answer
>   across untrusted networks.
> - **An imported profile bundle can carry `exec:` secret references**, which run
>   on first connect. Read a bundle before importing it.

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

## Contributing

```
▶ ./ci/gates.sh          # the same Tier-1 gate CI runs: fmt, clippy, tests,
                         # supply-chain scan, anti-pattern greps, size budget
▶ cargo test --workspace # everything that needs no live server
```

`ci/gates.sh` is the contract — if it passes locally it passes in CI, because it
is the same script. The live-engine suites are described in
[`notes/testing.md`](notes/testing.md).

## License

[Apache-2.0](LICENSE).
