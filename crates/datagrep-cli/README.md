# datagrep-cli

The first runnable face of `datagrep` — `CoreApi` over a terminal (design §4
killer feature #4: "Same core, three faces: GUI, TUI, CLI — identical
keybindings and config"). Nothing here connects to a database, opens the
profile store, or initializes TLS before a subcommand actually needs it, so
`datagrep --help` / `datagrep profiles list` are near-instant (design P1, ≤250ms).

```
datagrep query -f q.sql --format json | jq
```

## Quickstart — create a SQLite profile, query it, export CSV

Every command below was actually run against the built binary; the output
shown is real, not illustrative. `$DATAGREP` is the compiled binary
(`cargo build -p datagrep-cli` → `target/debug/datagrep`, or `target/release/datagrep`).

```console
$ export DATAGREP_CONFIG_DIR=$(mktemp -d)   # optional: isolates the profile store

$ $DATAGREP profiles add local "sqlite:///tmp/app.db"
created profile `local` (sqlite)

$ $DATAGREP query --profile local -c "
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, note TEXT);
INSERT INTO users (id, name, note) VALUES (1, 'alice', NULL);
INSERT INTO users (id, name, note) VALUES (2, 'bob', '');
INSERT INTO users (id, name, note) VALUES (3, 'carol', 'likes SQL');
"

$ $DATAGREP query --profile local -c "SELECT id, name, note FROM users ORDER BY id"
id  | name  | note
----+-------+----------
1   | alice | NULL
2   | bob   |
3   | carol | likes SQL
(3 rows)

$ $DATAGREP export --profile local -c "SELECT id, name, note FROM users ORDER BY id" \
    --format csv -o /tmp/users.csv
# progress goes to stderr, e.g. "3 rows (3000 rows/sec)"; stdout stays empty/pipeable

$ cat /tmp/users.csv
id,name,note
1,alice,
2,bob,
3,carol,likes SQL
```

Notice `NULL` (row 1) reads differently from the genuinely empty string (row
2) — that distinction is the point of `--format table`; see "NULL vs empty
vs Absent" below.

## Commands

| Command | What it does |
|---|---|
| `datagrep query --profile <name> [-f file.sql \| -c "SQL" \| -] [--format table\|json\|ndjson\|csv\|tsv] [--limit N] [--timeout 30s] [-o file]` | Splits input with `datagrep-lang`, honors `-- @limit`/`@timeout`/`@connection`/`@readonly` directives, runs statements in order, streams every result window by window. |
| `datagrep export --profile p -c "SQL" --format csv -o big.csv` | Same streaming path as `query`, but always to a file; rows/sec progress on stderr. |
| `datagrep profiles list\|add\|remove\|show\|export\|import` | Plain-text, git-committable profiles. `add` splits an inline password out of the URL into a keychain `SecretRef`; `show` prints `secret: ••••`. |
| `datagrep catalog --profile p [path...] [--describe path]` | Lists one level of the catalog lazily — one query per call, never a crawl. |
| `datagrep history list\|search` | The FTS5-backed history in `datagrep-profiles`; every statement `query` runs is recorded here. |
| `datagrep doctor [--profile p]` | Resolved config paths, registered drivers with capabilities decoded, whether a profile's secret resolves, and a connection round-trip time. |

Global: `--verbose` wires `tracing-subscriber` to stderr (quiet by default).
Table-format color is on only when stdout is a TTY and `NO_COLOR` is unset.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | query error (a statement failed, or was blocked by `@readonly`) |
| 2 | usage error (bad args, unknown profile, empty input) |
| 130 | cancelled (Ctrl-C) |

## NULL vs empty string vs `Absent`

`--format table` renders three states distinctly: `NULL` (dimmed on a color
TTY), a real empty string (nothing between delimiters), and `(absent)` for a
genuinely missing field (`Value::Absent` — only reachable through a
document-shaped result; no driver in this build produces one, since only
`postgres`/`sqlite` are registered and both are `Shape::Table`, but the
rendering path is implemented and tested). `--format json`/`ndjson` map
`Null` → JSON `null` and `Absent` → the key is **omitted entirely** — the one
output format that can actually say "not here" without inventing a sentinel.
CSV/TSV have no third state on the wire; both `NULL` and `Absent` render as
an empty field there, which is CSV's own limitation, not a shortcut this
crate takes.

## Cancellation

Ctrl-C cancels the query currently streaming (if any) via `CoreApi::cancel`
and prints the *real* `CancelReport::message` — not a canned string — before
exiting 130. Verified for real against this build:

```console
$ $DATAGREP query --profile local --format ndjson -o /tmp/bignum.ndjson -c \
    "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x < 50000000) SELECT x FROM cnt" &
$ sleep 0.3 && kill -INT $!
error: stopped receiving results; asking the server to cancel…
$ echo $?
130
$ wc -l < /tmp/bignum.ndjson
   92000
```

The rows already streamed to the file before the interrupt are kept — a
cancel is not a wipe.

## CoreApi gaps found while building this crate

`CoreApi` was read in full and treated as the only entry point (per this
crate's build instructions: "do not reach around it into drivers"). Building
a real frontend against it surfaced gaps worth fixing in `datagrep-core`, in
descending order of impact:

1. **No store-free execution path, so `export` cannot honor design
   §3.2/§5.1's "export never goes through the result store."** `CoreApi`
   exposes exactly one way to run a statement and read rows —
   `run_query`/`get_rows` — and `get_rows` always answers out of
   `datagrep_core::store::ResultStore`. There is no lower-level façade (e.g. a
   raw `Cursor`/`Batch` stream) a frontend can drive directly. `datagrep export`
   in this crate is therefore built on the identical path `datagrep query` uses.
   It still never accumulates more than one bounded window's rows in *this
   process* (see `cmd/streaming.rs`), and the store itself is bounded and
   spills rather than buffering unboundedly — but it is not the
   store-bypassing path the design describes. This is the gap most worth
   closing; it is the one place this crate's behavior diverges from a
   named design invariant rather than from an omission.

2. **`ExecOpts.row_limit`/`timeout`/`read_only_assert` are declared but never
   read.** `grep` across `datagrep-drv-postgres` and `datagrep-drv-sqlite` for
   `row_limit`/`read_only_assert`/`ExecOpts` turns up nothing but the struct
   literal that builds the `Request`; `datagrep-core` never inspects them either.
   So `--limit`/`@limit` and `--timeout`/`@timeout` are enforced **client
   side** in this crate (count rows / check a deadline, then call
   `CoreApi::cancel`), not by the server stopping early — a `--limit 1`
   against a slow query still pays for however much the server produces
   before the client's next window request lands. `opts` is still filled in
   honestly on every `Request` so a driver that starts reading it gets real
   values for free.

3. **DDL/DML acknowledgement never reaches `CoreApi`.** `Shape::Ack {
   affected, message }` exists in `datagrep-api`, and both drivers' `AckCursor`
   *carry* an affected-row count — but `AckCursor::next_batch` returns
   `Batch::default()` (`Payload::Empty`), and `datagrep_core::store::convert()`
   only ever admits `Payload::Rows`/`Docs`/`Pairs`. An `Ack` chunk is
   silently dropped before it reaches `ResultStore`, so `StoreState` never
   carries the count. From the CLI's perspective, `DELETE FROM t` that
   removes 500 rows is indistinguishable from `SELECT * FROM t WHERE
   1=0` — both show as "0 rows, no columns." `datagrep query` says so explicitly
   in its footer note rather than pretending otherwise; see `git log` on
   `cmd/streaming.rs` for the exact wording.

4. **No way to learn a `Shape::Table` result's columns when it has zero
   rows.** Column names arrive baked into the first admitted `RecordBatch`
   chunk (`ChunkBody::Table`); neither `datagrep-drv-postgres` nor
   `datagrep-drv-sqlite` ever populates `Batch::schema_delta` (`grep` confirms
   both always emit `Vec::new()`), and `StoreState.chunks` stays empty
   forever for a genuinely empty result. `datagrep query` cannot print a header
   for `SELECT id, name FROM t WHERE 1=0` — it says so in the footer rather
   than silently omitting the header with no explanation.

5. **`CoreApi` wraps `Catalog::children` (as `list_catalog`) but not
   `describe`/`infer_shape`/`complete`.** `datagrep catalog --describe` reaches
   one level further into the public seam — `CoreApi::session(id)
   .acquire().await?.catalog().describe(...)` — which is still entirely
   `datagrep-core`/`datagrep-api` public API (`Session`, `ConnLease`, `Catalog`), never
   a driver crate directly, but it skips the panic-isolation wrapper
   `list_catalog` gets from `datagrep_core::api`'s internal `guarded(...)`
   helper.

6. **Secrets never reach a running session.**
   `datagrep_core::session::Session::acquire` always builds
   `ResolvedConfig::without_secrets(self.config.clone())` (see the comment
   at that call site) — there is no seam for a frontend to hand a resolved
   secret to `CoreApi`. `Context::open_profile` in this crate works around
   it by resolving the secret itself and folding the plaintext value back
   into `ConnectionConfig.values` before calling
   `CoreApi::add_profile_full` (both drivers fall back to reading a field
   straight out of `ConnectionConfig.values` when `ResolvedConfig.secrets`
   is empty — that's the only reason this works at all). The on-disk
   profile never holds the secret, but the resolved value sits in a
   plain, un-zeroized `String` for the life of the process's `CoreApi`
   profile — weaker than the `SecretString` guarantee it started as.

## Deviations from the ticket, and why

- **No `[lib]` target.** Integration tests (`tests/cli.rs`) drive the
  compiled binary directly via `env!("CARGO_BIN_EXE_datagrep")` rather than
  linking a library crate — black-box, exercises real argv parsing and real
  exit codes, and needed no `Cargo.toml`/structural change.
- **Terminal width for `--format table` comes from `$COLUMNS`, falling back
  to a fixed 120 columns** — a real `ioctl`/`TIOCGWINSZ` read needs a crate
  outside this crate's dependency list. Column widths are measured in
  `char`s, not display (grapheme) width, so wide CJK cells can overflow
  their column slightly; documented in `format/table.rs`.
- **JSON/NDJSON cells are always JSON strings, even for numeric columns**
  (e.g. `{"id":"1"}`, not `{"id":1}`). This follows directly from the
  already-written `value_text::CellText`/`format::json` (three states:
  `Null`/`Absent`/`Text(String)` — no numeric variant), which this crate's
  build instructions listed as already written and out of scope to redesign.
  Worth revisiting if a consumer needs typed JSON.
- **`datagrep catalog`'s laziness is proven behaviorally, not by an actual driver
  query-count assertion** in this crate's own test suite (`--describe`
  returns columns; a bare listing returns only that level's names, never
  recursing into columns on its own). A literal per-level SQL query count is
  better proved inside `datagrep-drv-sqlite`/`datagrep-drv-postgres`'s own test
  suites, which can instrument the connection directly; this crate only has
  `CoreApi` to observe through.
- **The workspace does not build in place right now.** `crates/datagrep-drv-redis`
  is mid-write by a sibling agent (`src/` has no `lib.rs`/`main.rs` yet), so
  `cargo build` from the real workspace root fails on that unrelated member.
  Every build/test/clippy/fmt run for this crate was verified against an
  isolated scratch workspace (symlinked copies of exactly the crates
  `datagrep-cli` depends on, plus a root `Cargo.toml` mirroring the real one's
  `workspace.package`/`workspace.dependencies`) with
  `CARGO_TARGET_DIR=/Users/nurchudlori/Projects/dbx/target-cli`, so the
  compiled artifacts still land in the real target dir. Nothing in
  `crates/datagrep-cli/` itself was touched to work around this.
