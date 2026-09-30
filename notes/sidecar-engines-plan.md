# Sidecar engines: out-of-process Go drivers

**Status:** plan for review. No code written. Prepared 2026-09-30 against `origin/main` @ ef6e3c5.
**Question this answers:** how can datagrep add engines (ClickHouse, Oracle, Cassandra, Trino,
Neo4j, Snowflake, BigQuery, etcd, ...) without writing a native Rust driver for each?

Every claim about datagrep cites `file:line` on that commit. Size and memory figures for Go
binaries are **estimates** until Phase 0 measures them; they are marked as such.

A reference client ships this pattern (per-engine driver processes over stdio, roughly 50 engines,
Go and Java). It was studied for protocol, lifecycle, packaging and recovery. Both projects are
Apache-2.0, but nothing is copied: the structures differ, and section 10 lists what we take as lessons and what we
deliberately do differently.

Jump to: [0. Decisions](#0-decisions-at-a-glance) · [1. Plug-in point](#1-where-the-sidecar-plugs-in)
· [2. Wire protocol](#2-wire-protocol) · [3. Lifecycle](#3-lifecycle) · [4. Secrets](#4-secrets)
· [5. Packaging](#5-packaging-and-distribution) · [6. Go layout](#6-go-module-layout-and-versioning)
· [7. Security](#7-security-review) · [8. Phases](#8-phased-rollout) · [9. Risks](#9-risks)
· [10. Reference lessons](#10-lessons-from-the-reference-client)

---

## 0. Decisions at a glance

| # | Decision | Recommendation |
|---|---|---|
| D1 | How core sees a sidecar | One Rust crate `datagrep-drv-sidecar` implements the existing `Driver`/`Connection`/`Cursor`/`Catalog`/`Canceller` traits. Core, the safety gate and the feeder are unchanged. |
| D2 | Who declares the language | A **Rust-side engine manifest** compiled into the app. The sidecar's handshake must agree or the connect fails. The sidecar never chooses its classifier. |
| D3 | Unclassifiable language | Fails closed: every statement counts as a write. Proposal: a new explicit `LanguageId::Unclassified` rather than borrowing `Cypher`. |
| D4 | Transport | stdio pipes (stdin/stdout), stderr is for logs only. No sockets, no ports. |
| D5 | Framing | 4-byte big-endian length prefix + JSON body. Request/response with ids; `cancel` is the only fire-and-forget message. |
| D6 | Streaming | Pull: one `fetch` = one batch, carrying the `FetchHint` verbatim. No read-ahead. |
| D7 | Process granularity | One process per **profile** (keyed by config fingerprint), multiplexing that profile's pooled connections; exits when the last connection closes. |
| D8 | Secrets | Sent inside the `connect` frame over the pipe. Never argv, never env. Child env is cleared to an allowlist. |
| D9 | Go layout | `sidecars/common` module plus one module and one binary per engine. |
| D10 | Distribution | Phase 0-1: pilot bundled. Phase 2+: download on demand, pinned by SHA-256 values compiled into the app. Flatpak uses extensions instead of downloads. |
| D11 | Pilot engine | **ClickHouse** via `clickhouse-go` v2 (section 8). |

---

## 1. Where the sidecar plugs in

### 1.1 The seam already fits

The driver traits are object-safe and async (`crates/datagrep-api/src/driver.rs:40-104`):
`Driver::connect` returns a `Box<dyn Connection>`, `Connection::execute(Request)` returns a
`Box<dyn Cursor>`, and `Cursor::next_batch(FetchHint)` is a **pull** API (`driver.rs:82`). Each of
these maps one-to-one onto a request/response message. The sidecar crate is "just another driver":

```
CoreApi ─ SessionRegistry ─ Session(pool) ─ ConnectionHandle::execute ── gate.admit ──┐
                                                                                      ▼
                         datagrep-drv-sidecar (Rust, in-process) ── frames on stdin/stdout ──▶ Go sidecar ──▶ DB
```

Registration uses the existing `CoreApi::register_driver` (`crates/datagrep-core/src/api.rs:78-84`).
`register_drivers` (`crates/datagrep-ffi/src/drivers.rs:5-20`) gains a loop over the engine
manifests, each registered as `SidecarDriver::new(manifest)`.

### 1.2 The safety gate applies unchanged, provided the language comes from Rust

- The gate runs in `ConnectionHandle::execute` **before** the request becomes a message
  (`crates/datagrep-core/src/session.rs:162-170`, `gate.admit` at `safety.rs:217-219`). A refused
  statement never reaches the sidecar's stdin, just as it never reaches a native driver's socket.
- The gate is built at session open from `driver.capabilities().language`
  (`session.rs:656`). That call is **synchronous and happens before any connection exists**, so
  `SidecarDriver::capabilities()` cannot ask the process. It must answer from a manifest that is
  compiled into the Rust side (D2). This also answers the trust question: a buggy or hostile
  sidecar cannot pick a lenient classifier for itself.
- At connect, the handshake reports the sidecar's `language`. If it differs from the manifest,
  `connect` fails with `DbError::Protocol`. The sidecar may **narrow** `Caps` flags
  (`caps.rs:4-26`), for example drop `SERVER_CANCEL` against an old server. It may never widen them
  or change `language`.
- Classification is `language_for(id).classify(stmt)` (`safety.rs:288-290`). A script counts as a
  read only if every statement is `StatementClass::Read` (`safety.rs:293-313`).
  `Op::Mutate`/`Op::Ddl` are always writes (`safety.rs:315-320`).

### 1.3 Declaring a dialect, and failing closed

| Engine | LanguageId today | Classifier today | Action |
|---|---|---|---|
| ClickHouse | `Sql(Clickhouse)` exists (`caps.rs:64`) | **Postgres rules** (`crates/datagrep-lang/src/lib.rs:93`) | Phase 0: audit the ClickHouse-only statements (`SYSTEM`, `OPTIMIZE`, `KILL`, `ALTER ... UPDATE/DELETE`, `EXCHANGE`, `ATTACH/DETACH`, `INSERT ... SELECT`, `SET`) and give ClickHouse its own keyword table. |
| Oracle, Trino | none | none | Add `SqlDialect::Oracle` / `Trino` with classifier tests before the engine ships. |
| Cassandra (CQL) | none | none | Add `LanguageId::Cql`. CQL is small: `SELECT` reads, the rest writes. |
| Neo4j | `Cypher` (`caps.rs:54`) | `FALLBACK`, which returns `Unknown` (`lib.rs:98`, `lib.rs:125-127`) | A conservative Cypher classifier: read only if there is no `CREATE/MERGE/SET/DELETE/REMOVE/DROP/LOAD CSV/CALL`. |
| etcd, others | none | none | `Unclassified` until a classifier exists. |

**Fail-closed rule (already true in code):** `FALLBACK.classify` returns `Unknown`, and `is_read`
treats anything that is not `Read` as a write (`safety.rs:304`). So an engine with no classifier
gets the most cautious rung behaviour: on `warn_writes`/`auth_writes`, every statement prompts,
including `SELECT`. That is correct but noisy. It is why each engine's classifier is an
exit criterion for shipping it, not a follow-up.

D3 proposes `LanguageId::Unclassified`, mapped to `FALLBACK`. Declaring `Cypher` for an engine
that is not Cypher would work but lie. This is the one change to the `datagrep-api` enum that the
plan needs early. `LanguageId` is serde-derived (`caps.rs:48`), so adding a variant is additive.

### 1.4 What core does *not* see: the per-driver tables in FFI, CLI and UIs

Core is driver-agnostic. The layers above it are not. These tables are keyed by driver id, and a
sidecar engine missing from them fails **silently**:

| Table | Location | What a missing entry does |
|---|---|---|
| `driver_for` | `crates/datagrep-ffi/src/drivers.rs:22-34` | `plaintext_config` skips the secret splice (`crates/datagrep-ffi/src/core.rs:110`): the connect runs **without the password** and looks like bad credentials. |
| `driver_for` (CLI copy) | `crates/datagrep-cli/src/drivers.rs:18-24`, used at `context.rs:71` | Same problem, in the CLI. |
| `driver_for_url` / `known_driver_ids` | `ffi/drivers.rs:36-66`, `cli/drivers.rs:14-34` | The URL is not routed, and the engine is not listed. |
| `language_for_driver` | `crates/datagrep-ffi/src/query.rs:476-486` (CLI copy `cli/drivers.rs:36-47`) | The FFI-side split falls back to "one statement" (`query.rs:464-467`). |
| `guard_fields` / `object_path_field` | `ffi/query.rs:488-500` | No CAS guard and no path field. That is harmless for SQL engines. |
| `browse_statement` | `crates/datagrep-ffi/src/browse.rs:34` | "Open table" returns an error. |
| secret keys for profile JSON | `ffi/profiles.rs:352` | The secret field is not redacted in the `get_json` output. |
| UI engine lists | `ui/macos/Sources/DatagrepKit/EngineStyle.swift`, `ConnectionURL.swift`; `ui/linux/src/ui/ConnectionDialog.cpp`, `EngineIcon.cpp`; `ui/gtk4/src/engine.rs`, `connection_dialog.rs` | The engine cannot be picked in the UI. |

**Decision:** the engine manifest (section 6.3) becomes the single source for id, display name,
URL schemes, config schema, language, browse template and catalog levels. Phase 0 changes the Rust
tables above to consult the registered driver or manifest, not a `match`. Phase 1 adds one FFI
entry point, `datagrep_engines_json`, and moves the three UIs off their hard-coded lists. Without
this step, every engine costs about 10 edits across 4 languages, which defeats the point of the
project.

---

## 2. Wire protocol

### 2.1 Transport: stdio

| Option | For | Against |
|---|---|---|
| **stdio pipes** | Private to the parent/child pair: no path or port that another local process can reach, so no auth is needed. Works unchanged in Flatpak. Parent death gives the child EOF, which stops orphans. | One byte stream per process, so we multiplex by id (cheap). |
| Unix socket / named pipe | Allows several streams. | Needs a filesystem path plus permissions, a race window, and Windows differences. Nothing needs it. |
| Loopback TCP | Easy to debug. | Any local user can connect, so it needs a token. Firewall prompts on macOS. |

**stdio.** stdout carries frames only. stderr carries logs only (section 3.5). stdin EOF means
"shut down now".

### 2.2 Framing and encoding: length-prefixed JSON

| Option | Verdict |
|---|---|
| **u32-BE length + JSON** | **Chosen.** `serde_json` is already a runtime dependency (`crates/datagrep-ffi/Cargo.toml:44`, as well as the ES and Mongo drivers), and Go has `encoding/json` in the stdlib. Zero new crates against the P16d crate budget (`budget.toml` P16d: 400 target / 600 fail). The length prefix lets the reader reject an oversized frame **before** allocating. |
| NDJSON | Works too (JSON escapes newlines), but the reader has to scan 4 MiB lines to find the boundary, and caps are enforced only after buffering. |
| MessagePack | Faster and smaller for rows. Adds `rmp-serde` and a Go library. It is kept as a negotiated upgrade: the handshake carries `codec`, and Phase 0 benchmarks JSON against the P6 streaming budget first. |
| protobuf / gRPC | Adds `tonic`/`prost`/`h2`/`hyper`/`tower` (tens of crates), `protoc` in three build systems, and HTTP/2 over a pipe. Too much machinery for a request/response protocol with one caller. |

Frame cap: `4 × FetchHint.max_bytes` (default `max_bytes` is 4 MiB, `driver.rs:173`), so 16 MiB.
Anything larger is a protocol violation, and the process is killed.

### 2.3 Messages

The envelope is `{"id":u64,"m":"<method>","p":{...}}`, answered by
`{"id":u64,"ok":{...}}` or `{"id":u64,"err":{...}}`. The sidecar sends no
unsolicited frames: **every stdout frame answers a request id**, and anything else is a
violation. The method set mirrors the traits. There are no per-engine methods (section 10).

| Method | Trait it serves | Notes |
|---|---|---|
| `hello {protocol:[min,max], app_version, codec}` | spawn | The reply gives `protocol`, `engine`, `engine_version`, `language`, `caps` (narrow only), `max_frame`. |
| `connect {conn, config, secrets, connect_timeout_ms, application_name}` | `Driver::connect` (`driver.rs:50`) | The reply gives `ServerInfo` and `Capabilities`. `conn` is chosen by the parent. |
| `ping {conn}` | `Connection::ping` | |
| `execute {conn, request, opts}` | `Connection::execute` | `request` = `Native{text, params}` in Phase 0. `Op::*` returns `Unsupported` until an engine opts in. The reply gives `cursor` and `shape`. |
| `fetch {cursor, max_rows, max_bytes, target_ms}` | `Cursor::next_batch` (`driver.rs:82`) | The reply gives `batch` or `null` (EOF), plus `stats` (`CursorStats`, `driver.rs:212-218`) and `resume` (base64). |
| `close_cursor {cursor}` | `Cursor::close` | Also sent from the Rust cursor's `Drop` if `close` was never called. |
| `cancel {conn}` | `Canceller::cancel` (`driver.rs:100-104`) | No id-matched reply. The in-flight request finishes with `err.kind = cancelled`. |
| `set_read_only {conn, on}` | `Connection::set_read_only` | The reply gives `Enforcement` (`driver.rs:142-146`). ClickHouse maps this to `readonly=1`, so `Server`. |
| `catalog.children / describe {conn, path, opts}` | `Catalog` (`catalog.rs:11-29`) | `levels` come from the manifest. `infer_shape`/`complete` return `Unsupported` / empty in Phase 0. |
| `begin / commit / rollback` | `Transaction` | Phase 3. Until then `Caps::TRANSACTIONS` is off and `begin` returns `Unsupported`. |
| `disconnect {conn}` | `Connection::close` | |

`shape` and `batch` use a wire mirror type, not `serde` on the api types. `Shape` has no serde
derive (`crates/datagrep-api/src/shape.rs:9`), and derived serde for `Value` is wrong on the wire:
`Bytes` would become an integer array and `Uuid([u8;16])` 16 integers (`value.rs:29`, `value.rs:43`).
The mirror type sits in the sidecar crate. The `datagrep-api` seam gains no derive (its dependency
list "is a decision", `Cargo.toml:11-15`).

### 2.4 Streaming and backpressure

The feeder already provides the backpressure. It reserves channel capacity **before** it calls
`next_batch` (`crates/datagrep-core/src/feeder.rs:322-343`), with a bound of 2
(`feeder.rs:14`). Because `fetch` is one request for one batch and the sidecar never reads ahead,
the chain is:

`UI stops reading` → feeder parks → no `fetch` → Go does not call `rows.Next()` → driver
socket buffer fills → TCP window closes → the server stops sending.

Contract: the sidecar returns **at most** `max_rows` rows, stops at `max_bytes` or `target_ms`
when it can, and may return fewer rows, zero included. The adaptive sizing in
`feeder.rs:377` works unchanged. Memory held in the sidecar is one batch per open cursor, plus
whatever block the Go driver buffers. `clickhouse-go` holds one server block, so the pilot sets
`max_block_size` from the hint.

### 2.5 Value mapping

`Table` payload rows are JSON arrays, untagged, and interpreted against the declared
`FieldDef.logical` (`shape.rs:33-63`). `Docs`/`Pairs`/`Graph` payloads are self-describing, so they
use the tagged form. Any cell that does not match its declared column type (for example ClickHouse
`Variant`/`Dynamic`) uses the tagged form `{"$t":tag,"v":...}`.

| `Value` (`value.rs:19-58`) | Wire | Note |
|---|---|---|
| `Null` / `Absent` | `null` / `{"$t":"absent"}` | |
| `Bool`, `I64`, `U64` | JSON bool / number | Both `serde_json` and Go parse the full int64/uint64 range exactly. |
| `F64` | number, or `"NaN"`/`"Infinity"`/`"-Infinity"` | JSON has no NaN. |
| `Decimal`, `Str`, `Json` | string | Decimal stays text, matching `Decimal(Arc<str>)`. |
| `Bytes`, `Geo` (WKB), `Unsupported.raw` | base64 string | |
| `Date` / `Time` | days since epoch / nanos | Same units as the enum. |
| `Timestamp` | `{"us":i64,"tz":"utc"|"naive"|"+hh:mm"|"<IANA>"}` | `TzSpec` (`value.rs:11-17`). |
| `Interval` | `[months, days, nanos]` | |
| `Uuid` | canonical string | |
| `Array`, `Vector` | array | |
| `Document` | `[[key, value], ...]` | Keeps field order, which a JSON object does not. |
| `Ref` | `{"$t":"ref","target":[...],"key":[...]}` | |

### 2.6 Errors

`err` is `{kind, code?, message, position?}`. Its mapping to `DbError` (`error.rs:4-61`):

| `kind` | `DbError` | Recoverable? (`error.rs:63-77`) |
|---|---|---|
| `connect`, `auth`, `tls` | `Connect` / `Auth` / `Tls` | no |
| `query`, `conflict` | `Query{code,message,position}` / `Conflict` | yes |
| `timeout`, `cancelled`, `unsupported`, `resource`, `config` | same-named variant | yes |
| `panic` (Go `recover()` in the dispatcher) | `DriverPanic` | no, so the connection is poisoned |
| transport: EOF, bad frame, oversized frame, unknown id, `kind:"safety"` | `Protocol` + redacted stderr tail | no |

Non-recoverable errors poison the connection through the existing path (`session.rs:259-262`,
`is_fatal` at `session.rs:272`), and the pool dials a fresh one. That behaviour matches what a Rust
driver panic produces today (`guarded`, `session.rs:276-289`). `Safety` is minted **only** by the
parent's gate. A sidecar that sends it is violating the protocol.

---

## 3. Lifecycle

### 3.1 Granularity: one process per profile

| Option | Idle memory (N = open profiles) | Crash blast radius | Secret exposure | Verdict |
|---|---|---|---|---|
| per connection | up to 4 processes per profile (`PoolPolicy.max_size = 4`, `session.rs:39`) | 1 connection | its profile | Too many processes. |
| **per profile** | N processes | 1 profile | **its profile only** | **Chosen.** |
| per engine | 1 process per engine | every profile on that engine | every profile on that engine | Cheapest, but one crash drops prod and dev together, and one process holds every password. |

`Driver::connect` receives no profile id (`driver.rs:50-54`, `ConnectCtx` at `driver.rs:120-125`),
and the registry caches one `Driver` per id (`crates/datagrep-core/src/registry.rs:39-42`). So
`SidecarDriver` keeps `HashMap<Fingerprint, Weak<Process>>`, where the fingerprint is a hash of the
engine id and the resolved config, secret included. It is held only in memory. No api change is
needed. The protocol carries `conn` ids from day one, so switching to per-engine later is a policy
change, not a protocol change.

A process lives while any of its connections lives. The pool reaps idle connections after 5 min
(`session.rs:41`, `reap` at `session.rs:521`), so an idle profile costs **zero** processes after
that, plus a 30 s linger to avoid respawn churn.

### 3.2 Spawn

- The child gets `env_clear()` plus an allowlist (`HOME`, `TMPDIR`, `LANG`/`LC_*`, `TZ`,
  `SSL_CERT_FILE`/`SSL_CERT_DIR`), plus per-engine additions declared in the manifest (for
  example `GOOGLE_APPLICATION_CREDENTIALS` for BigQuery, Phase 3). Proxy variables pass only when
  the user has configured a proxy.
- The child gets `GOMEMLIMIT` (soft heap cap, default 256 MiB) and `GOGC`. On Unix `pre_exec` sets
  `RLIMIT_CORE=0` (no core dump holding a password). On Linux it also sets `PR_SET_PDEATHSIG`. On
  macOS there is no pdeathsig, so stdin EOF does that job.
- The binary's SHA-256 is checked against the manifest before the first spawn of each app run (§7).

### 3.3 Timeouts

| Phase | Limit | On expiry |
|---|---|---|
| spawn + `hello` | 5 s | `DbError::Connect("engine sidecar did not start: <stderr tail>")` |
| `connect` | `ConnectCtx.connect_timeout` (default 30 s, `session.rs:42`). Core already wraps it (`session.rs:545-566`). | The sidecar also receives it, so the Go driver gives up at the same time. |
| `execute` | `ExecOpts.timeout` (`request.rs:107-112`), forwarded, plus a 2 s Rust-side grace | `Timeout`, then `cancel` |
| `fetch` | none (long streams are legitimate) | The user cancels. |
| `cancel` acknowledged (in-flight request ends) | 2 s | **SIGKILL** the process. All of its connections poison and re-dial. This is the backstop for P17 ("cancel to connection usable", target 100 ms). |
| `disconnect` / shutdown | 2 s after stdin close | SIGKILL |

### 3.4 Health, recovery, restart

- **Liveness:** a Rust task awaits `child.wait()`. On exit, every pending request gets
  `Protocol("sidecar exited: <status>; <tail>")`, and every connection on the process poisons.
- **Restart is lazy:** the next `Driver::connect` spawns a fresh process. **No automatic replay**
  of an in-flight `execute`: re-running a statement could apply a write twice. The one exception
  is read-only catalog calls, which retry once on a fresh process.
- **Circuit breaker:** three abnormal exits for one fingerprint within 60 s make `connect` fail
  fast with `Connect("the <engine> engine keeps crashing; see the log")` for 5 minutes or until the
  user reconnects explicitly. This stops a crash loop from spinning the CPU.
- **Ping:** `Connection::ping` goes over the protocol. There is no separate heartbeat, because a
  heartbeat is a periodic wakeup and P13 budgets idle wakeups at 2/s.

### 3.5 stderr

A dedicated reader task forwards lines to `tracing` under target `sidecar::<engine>`, rate-limited.
It keeps a 64 KiB ring buffer tail for error messages. **Before any tail reaches a `DbError` or the
UI, the Rust side replaces every secret value it sent to that process with `••••`.** Go driver
libraries have logged DSNs before, and the parent is the only party that knows exactly which
strings to scrub.

---

## 4. Secrets

**Today:** the secret is resolved from the OS keychain (`crates/datagrep-secrets`), then spliced
into `config.values` as a plain `ConfigValue::Str` (`crates/datagrep-ffi/src/core.rs:94-118`). The
session then connects with `ResolvedConfig::without_secrets(config)` (`session.rs:536`). The
`SecretString` wrapper, which zeroizes on drop (`config.rs:60-92`), is therefore not what reaches
drivers.

**For sidecars:**

1. The parent removes every `ConfigField` with `secret: true` (`config.rs:13-20`) from `config`
   and sends those values in a separate `secrets` map inside the `connect` frame, over the stdin pipe.
2. **Not argv.** `ps` shows argv to every local user. **Not env.** Env can be read from
   `/proc/<pid>/environ`, is inherited by grandchildren, and is commonly dumped by SDK debug logging.
3. The pipe is anonymous and `O_CLOEXEC`, and only the parent and child hold its ends.
4. `sidecars/common` exposes a `Secret` type whose `String()`/`MarshalJSON` print `••••`, so an
   accidental `%v` in engine code does not leak it.
5. **Residual risk, accepted:** Go strings cannot be zeroed, and drivers copy the password into
   their own structs. That is the same exposure as an in-process Rust driver today, where the
   spliced `ConfigValue::Str` is not zeroized either.
6. The sidecar never talks to the keychain. That keeps the macOS keychain ACL bound to the app's
   signing identity only (the re-prompt issue is tied to that identity), and no new keychain prompt
   appears per engine.
7. Per-profile processes (D7) mean one process sees one profile's credential.

---

## 5. Packaging and distribution

### 5.1 Size

The app is about 25 MB installed today. Budgets (`budget.toml`): **P10** installer compressed,
22 MB target / 35 MB fail. **P11** installed on disk, 55 MB target / 90 MB fail. Build flags are
`CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -buildvcs=false`.

| Engine sidecar | Go driver | Est. stripped size | Est. compressed | Bundle? |
|---|---|---|---|---|
| ClickHouse (pilot) | `clickhouse-go/v2` | 10-12 MB | 4-5 MB | Phase 0-1: yes |
| Oracle | `go-ora/v2` (pure Go, no Instant Client) | 8-10 MB | 3-4 MB | download |
| Cassandra | `gocql` | 7-9 MB | 3-4 MB | download |
| Trino | `trino-go-client` | 7-8 MB | 3 MB | download |
| Neo4j | `neo4j-go-driver/v5` | 8-9 MB | 3-4 MB | download |
| etcd | `etcd/client/v3` (gRPC) | 18-22 MB | 7-9 MB | download |
| Snowflake | `gosnowflake` (Arrow, cloud SDKs) | 45-60 MB | 18-25 MB | download only |
| BigQuery | `cloud.google.com/go/bigquery` | 40-55 MB | 16-22 MB | download only |

All of these are **estimates**. Phase 0 publishes measured numbers for the pilot and a baseline
"hello" sidecar. Each binary carries its own Go runtime (about 2 MB): that is the price of one
binary per engine (section 6.1).

**Consequence:** bundling the pilot takes the app from about 25 to about 36 MB installed, inside
P11. Bundling three small engines lands on the P11 target. The cloud SDKs alone would break P10.
So the rule is: bundle at most one engine, download the rest.

### 5.2 Per target

| Target | Location | Build | Sign / integrity | Notes |
|---|---|---|---|---|
| **macOS .app** | Bundled: `Contents/Helpers/datagrep-engine-<id>`. Downloaded: `~/Library/Application Support/datagrep/engines/<id>/<sha256>/` | `GOOS=darwin GOARCH=arm64`. The shipped app is arm64-only (`release.yml:50`), so no universal binary is needed. | `build-app.sh:143-154` signs the bundle without `--deep`, so every helper must be signed **first** (inside-out) with the same identity. Once Developer ID lands (`release.yml:153` says it is not notarized today): add `--options runtime --timestamp`. Go runs under the hardened runtime with no entitlements. Downloaded binaries sit outside the sealed bundle: verify them by SHA-256 (§7) and notarize them separately, as a zip submission. Files the app downloads itself carry no quarantine xattr, so Gatekeeper does not block them. | |
| **Linux .deb / .rpm** | `/usr/lib/datagrep/engines/` | Static (`CGO_ENABLED=0`). No glibc dependency at all, so the ubuntu-22.04 pin (`.github/workflows/linux-package.yml:24`) does not constrain Go. | Optional sub-packages `datagrep-engine-<id>` via the same `fpm` pattern (`packaging/build-packages.sh:40-71`). | |
| **AppImage** | Bundled engine inside the AppDir. Downloads go to `$XDG_DATA_HOME/datagrep/engines/` (the squashfs is read-only). | same static binary | SHA-256 pinned | |
| **Flatpak** | Bundled engine in `/app/libexec/datagrep/engines/`. Further engines as **Flatpak extensions** (`io.github.chud_lori.datagrep.Engine.<id>`), not runtime downloads. | Add `org.freedesktop.Sdk.Extension.golang`. The build is offline (`packaging/flatpak/io.github.chud_lori.datagrep.yml:11-15`), so Go modules need a vendored or generated sources file, like `cargo-sources.json`. | Network is already granted (`yml:35`). Sidecars inherit the sandbox. **Unverified:** whether Flathub review accepts apps that download executables at runtime. Extensions avoid the question. | |
| **CLI** | Next to the binary, then the XDG data dir | same | same | `install.sh` gains `--engine <id>`. |

### 5.3 Download on demand (Phase 2)

- The artifacts are GitHub Release assets from this repo's own tag, one per engine and platform.
- **Pinned by hash, not by signature key:** the app's compiled-in manifest carries the SHA-256 for
  each `(engine, platform)` that belongs to that exact app version. The app runs only those bytes.
  There is no separate signing key to manage or leak. The integrity root is the app binary, which
  is itself signed. Reproducible Go builds (`-trimpath`, pinned toolchain) keep hashes stable
  across unchanged releases, and the cache is keyed by hash, so an unchanged engine is not
  re-downloaded.
- Install is atomic: download to a temp file, verify, `rename` into `<sha256>/`, then set the
  executable bit. Old hashes are garbage-collected on the next start.

---

## 6. Go module layout and versioning

### 6.1 One binary per engine

| | One binary per engine | One multi-engine binary |
|---|---|---|
| Download granularity | per engine | all or nothing (it would carry BigQuery's gRPC stack for a ClickHouse user) |
| CVE / bump blast radius | one engine | every engine |
| govulncheck findings | scoped to that engine | union |
| Duplicate Go runtime | about 2 MB each | once |

**One binary per engine.**

### 6.2 Tree

```
sidecars/
  go.work                      # local dev only
  common/                      # module .../sidecars/common
    frame/                     # u32 length + JSON, frame cap
    rpc/                       # dispatcher, per-request goroutine, panic recover -> kind:"panic"
    wire/                      # Value/Shape/Batch encoders (section 2.5), error kinds (section 2.6)
    cancel/                    # conn -> context.CancelFunc registry
    secret/                    # redacting Secret type
  engines/
    clickhouse/                # module, own go.mod/go.sum -> datagrep-engine-clickhouse
    ...
  conformance/                 # golden request/response frames, replayed by Go AND Rust tests
```

Each engine is a separate module, so one engine's dependency graph never shows up in another's
`go.sum` or vulnerability report. The Go toolchain is pinned with the `toolchain` directive, and CI
runs with `GOFLAGS=-mod=readonly`.

### 6.3 Rust side

- `crates/datagrep-drv-sidecar`: process manager, framing, id demux (one reader task, one writer
  task), trait adapters, error mapping, stderr tail. It needs `tokio` with `process` and
  `io-util` features, plus `serde_json`, both already in the tree.
- `EngineManifest` (a static table in that crate): `id`, `display_name`, `url_schemes`,
  `ConfigSchema` (`config.rs:7-30`), `Capabilities` including `language`
  (`caps.rs:28-37`), catalog `LevelDef`s, browse template, env allowlist,
  `artifacts: [(target, sha256)]`, `protocol: u32`.

### 6.4 Compatibility handshake

- `protocol` is one integer. The parent offers `[min, max]`, and the sidecar picks one or exits
  with a clear stderr line. A breaking change bumps the integer. Additive fields are ignored by
  older peers (unknown JSON fields are skipped on both sides).
- Engine versions follow the **app version**. Sidecars are built from the same tag and pinned by
  hash, so a production mismatch cannot happen. The handshake protects dev builds and hand-copied
  binaries. There is no per-module version file.

---

## 7. Security review

**Trust model:** the sidecar is first-party code built from this repo, but it links third-party
driver code that we do not audit line by line. The process boundary protects against its
**bugs** (crash, hang, memory blow-up, garbage output). It does not protect against a
**malicious** sidecar: that sidecar holds a valid credential and can do anything the credential
allows. Against malice, the control is the supply chain.

| The sidecar can | The sidecar cannot |
|---|---|
| Reach the network and the user's files. There is no OS sandbox outside Flatpak. That is the same reach as the app. | Read the keychain, or any other profile's secret (D7, section 4). |
| Use its profile's credential. | Bypass the safety gate: the gate runs in the parent before the frame is written (`session.rs:164`). |
| Return wrong or hostile data. The UI already treats DB data as untrusted. | Choose a lenient classifier (D2), or forge a safety grant: the protocol has no grant message, and a `safety` error from it is a violation. |
| Crash or hang. | Take the app down: it is a separate address space. A frame over the cap, an unknown id or an unsolicited frame gets the process killed. `GOMEMLIMIT`, plus the parent's kill, bounds memory. |
| Claim `Enforcement::Server` for read-only. | Change what the gate classifies. Read-only here is defence in depth, not the only line. |

**Supply chain, added to `ci/gates.sh` next to cargo-audit/deny (`gates.sh:51-90`):**

- `govulncheck ./...` per engine module. It fails on reachable vulnerabilities, which is the same
  bar as `cargo audit --deny warnings`.
- A licence check with the same allowlist spirit as `deny.toml [licenses]` (for example
  `go-licenses check`). The drivers above are Apache-2.0, BSD-3 or MIT. **AGPL/GPL is banned**.
- `go.sum` is committed, `-mod=readonly` is set, the toolchain is pinned, and Dependabot is enabled
  for `gomod`.
- The CI job installs Go in `.github/workflows/ci.yml`. The gate fails in CI and warns locally when
  Go is missing, following the `supply_chain_missing` pattern (`gates.sh:53-60`).
- The release computes SHA-256 values from the CI build and writes them into the manifest. A
  reproducibility check rebuilds once and compares.

---

## 8. Phased rollout

### Pilot: ClickHouse

| Criterion | ClickHouse | Oracle (go-ora) | Cassandra (gocql) |
|---|---|---|---|
| Mature pure-Go driver | yes, official | yes | yes |
| Real user value | high (analytics; ad-hoc SQL is the main use) | very high | medium |
| Is a Rust driver a real alternative? | **Weak.** The official Rust crate is built around typed `Row` structs, which suits ad-hoc result sets poorly. The Go driver exposes dynamic column types. | No pure-Rust driver (Rust options need Oracle Instant Client). | Yes (`scylla`), which argues against spending the sidecar pilot on it. |
| Language work before shipping | `Sql(Clickhouse)` **already exists** (`caps.rs:64`). Needs a classifier audit only. | New dialect plus classifier | New `LanguageId` plus classifier |
| Exercises the protocol | Server-side cancel (native protocol cancel), streaming blocks, rich types (Decimal, DateTime64, UUID, Array, Map, LowCardinality, Nullable) | Types, cursors | `ResumeToken` paging (PageState) |
| CI cost | Small Docker image, starts in seconds | Heavy image (GBs, slow start) | Slow start |

**ClickHouse is the pilot.** It needs the least non-protocol work (the dialect enum exists), it
stresses cancel, streaming and type mapping hardest, and it is cheap to run in CI. **Oracle is
first in Phase 2**, because it has the most value that no Rust driver offers.

### Phases

Estimates are in engineer-days for one engineer who knows this codebase.

| Phase | Scope | Exit criteria | Effort |
|---|---|---|---|
| **0: protocol + pilot** | Protocol spec plus `conformance/` fixtures. `datagrep-drv-sidecar` (spawn, demux, adapters, cancel escalation, stderr redaction, circuit breaker). `EngineManifest`. De-hardcode the Rust-side tables in §1.4 (FFI and CLI). `sidecars/common`. ClickHouse engine (Native requests, catalog children/describe, read-only, cancel). ClickHouse classifier. CI: Go toolchain, `go test`/`vet`/`govulncheck`, a ClickHouse Docker integration test, fault injection (kill mid-stream, garbage frame, oversized frame, cancel that never acknowledges). | CLI `datagrep query` against real ClickHouse streams 1M rows. The safety gate refuses an `ALTER` on `auth_writes` and **the server never sees it** (checked through `system.query_log`). Measured P4, P6, P13 and P17 with one sidecar, compared to budget. Sizes measured. | **25-30 d** |
| **1: ship the pilot everywhere** | FFI `datagrep_engines_json`. macOS, Qt and GTK4 connection dialogs and engine icons read the engine list from it. Helper signing in `build-app.sh`. deb/rpm/AppImage staging. Flatpak Go extension plus vendored modules. | A ClickHouse profile can be created and queried in all three UIs and all four package formats. | **12-16 d** |
| **2: download + three engines** | Engine installer (hash-pinned, atomic, cache by hash) with an "install engine" affordance in the three UIs. Flatpak extensions. **Oracle** (`SqlDialect::Oracle`), **Cassandra** (`LanguageId::Cql`, `ResumeToken` paging), **Trino** (`SqlDialect::Trino`). | Each engine ships with its classifier tests and a Docker integration test. | **6-8 d** infrastructure + **5-8 d per engine**: **21-32 d** |
| **3: graph, KV, cloud, transactions** | **Neo4j** (`Payload::Graph`, Cypher classifier). **etcd** (`Pairs`). **Snowflake / BigQuery** (key-pair, OAuth browser flow, service-account; download only). Protocol `begin/commit/rollback`. SSH tunnels by passing a connected socket fd to the child (`SCM_RIGHTS`); a Go `Dialer` wraps it. Needed because today's tunnel is an in-process `DuplexStream` (`crates/datagrep-tunnel/src/bridge.rs:3-17`) that a child cannot use. | Scoped per engine when started | **30+ d**, re-estimated then |

### Non-goals

- **JVM/JDBC sidecars.** Go only.
- **In-process Go** (`c-shared` library loaded into the app). That gives up crash isolation and
  puts two runtimes and two signal handlers in one process.
- **Third-party or user-supplied sidecars / plugin marketplace.** Only first-party binaries pinned
  by the app's manifest run.
- **Replacing the six native Rust drivers.**
- **Per-engine RPC methods.** Engine-specific features go through `Request::Native` text or
  generic `Op`s. If the trait surface is not enough, the traits change for everyone.
- **Write editing (`Op::Mutate`) and `Op::Ddl` for sidecar engines** before Phase 3. They return
  `Unsupported`.
- **SSH tunnels before Phase 3.** The native drivers do not have them wired either
  (`tunnel_id: None`, `crates/datagrep-ffi/src/profiles.rs:191`).
- **Windows.** The design is portable (pipes, no signals in the protocol), but it is not a release
  target today.

---

## 9. Risks

| # | Risk | Likelihood / impact | Mitigation |
|---|---|---|---|
| **R1** | **Resource budgets.** The budgets count the **whole process tree** (`budget.toml:3-7`). Each Go process adds an estimated 8-15 MB idle RSS against P4 (3 idle connections: +18 MB target / +40 MB fail). GC and scavenger wakeups count against P13 (2/s target). | High / high | Per-profile process, exit at 0 connections + 30 s linger, `GOMEMLIMIT`/`GOGC` tuning. **Phase 0 exit criterion:** measured numbers. If P4 fails, the fallback is per-engine processes (same protocol, section 3.1). |
| **R2** | **Hidden per-driver hard-coding** in FFI, CLI and three UIs (§1.4). An entry missing from `driver_for` drops the password **silently** (`ffi/core.rs:110`, `cli/context.rs:71`). | Certain / high | De-hardcode in Phase 0 (Rust) and Phase 1 (UIs), before engine #2. A test asserts that every registered engine resolves through every table. |
| **R3** | **Safety classification for new languages.** A classifier borrowed from another dialect (ClickHouse runs on Postgres rules today, `lang/lib.rs:93`) can call a write a read, which bypasses the safety ladder. No classifier means FALLBACK, which is safe but prompts on every `SELECT`. | Medium / high (the ladder is datagrep's differentiator) | Language pinned in the Rust manifest (D2). Per-language classifier tests are a shipping gate for each engine. Fail-closed default (D3). |
| R4 | macOS signing of nested and downloaded binaries. Today the app is ad-hoc signed and not notarized (`release.yml:153`), and `build-app.sh` does not sign inside-out. | Medium / medium | Sign helpers first in Phase 1. SHA-256 pinning does not depend on Apple signing. |
| R5 | JSON throughput on large streams (P6: 1M rows, +260 MB target). | Medium / medium | Measure in Phase 0. The negotiated `codec` allows MessagePack without a protocol version bump. |
| R6 | Go dependency surface. The cloud SDKs pull hundreds of modules. | Medium / medium | Per-engine modules, `govulncheck` gate, cloud SDKs download-only and Phase 3. |
| R7 | Flatpak/Flathub policy on runtime-downloaded executables. | Unverified | Use extensions for Flatpak. Downloads only on other targets. |

---

## 10. Lessons from the reference client

These come from reading its `agents/` tree and its Rust agent crate. Nothing is copied.

| It does | We take / we differ |
|---|---|
| stdio, one JSON message per line, JSON-RPC 2.0 | **Take** stdio. **Differ:** length-prefix framing, so caps apply before allocation. |
| `handshake` with a protocol version and capability strings | **Take.** Add that the language comes from the parent's manifest, not the agent. |
| One shared runtime per agent key plus launch fingerprint, many sessions per process | **Take** the fingerprint idea, keyed per profile rather than per engine, for secret isolation. |
| Typed errors carrying a session disposition (keep / quarantine / replace runtime) into a recovery policy | **Take** the idea. Ours derives from `DbError::is_recoverable` plus transport failures, and adds a circuit breaker. No replay of statements. |
| Separate `cancel_session` call; stderr tail collector | **Take** both. **Add** secret redaction of the tail. |
| Scrubs proxy variables from the child env | **Go further:** `env_clear()` plus an allowlist. |
| Downloads agents into a per-user directory; artifact hash optional | **Differ:** hash **mandatory** and compiled into the app. |
| Per-module versions auto-bumped in a versions file | **Differ:** one version (the app's), pinned by hash. |
| Many per-engine RPC methods (etcd auth, Mongo cursors, ...), and the Rust side of the protocol grew to hundreds of KB | **Differ, deliberately:** the method set mirrors datagrep's traits and stays closed. |
| JVM agents plus a managed JRE download | **Non-goal.** |
