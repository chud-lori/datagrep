//! `datagrep-drv-mysql` — the MySQL/MariaDB driver behind `datagrep-api`'s
//! `Driver` seam.
//!
//! # Streaming: never buffer a whole result
//!
//! `MySqlConnection::execute` never materializes a full result. Rows are
//! pulled off the socket one at a time via `QueryResult::next()`, only while
//! a `FetchBatch` command is being served — between pulls nothing is read,
//! the TCP window closes, and the server stops producing. `collect()` is
//! never called on a user query result.
//!
//! mysql_async has the same lifetime constraint tokio-postgres does: the
//! streaming `QueryResult<'a>` mutably borrows its `Conn`, while
//! `Connection::execute` must return a `'static Box<dyn Cursor>`. The same
//! actor-task answer is used (see `actor.rs`): the `Conn` and the in-flight
//! result live on one task's stack; only owned channel handles cross out.
//! (The Mongo driver skipped the actor because its session/cursor pair is
//! fully owned; MySQL's is not.)
//!
//! **Undrained-result poisoning**: a MySQL connection with an unconsumed
//! result set fails on the *next* statement. Every actor exit path drains
//! the in-flight result (see `actor.rs` module docs); the integration test
//! `undrained_result_does_not_poison_connection` proves it.
//!
//! # Cancellation
//!
//! `KILL QUERY <connection_id>` from a second, pooled connection
//! (`canceller.rs`); the pool opens no connection until the first cancel.
//! The killed statement surfaces `ER_QUERY_INTERRUPTED` (1317), mapped to
//! `DbError::Cancelled`; the primary connection survives and is reusable.
//! Outcome is reported as `Requested` — the server acks the KILL statement,
//! not the victim's death.
//!
//! # MySQL vs MariaDB
//!
//! One driver, id `"mysql"`, same wire protocol. The real product/version is
//! read from `@@version` at connect (MariaDB reports e.g.
//! `10.11.6-MariaDB`, sometimes behind a `5.5.5-` compat prefix) and
//! reported verbatim in `ServerInfo`. The only dialect fork is EXPLAIN
//! ANALYZE spelling (`EXPLAIN ANALYZE` on MySQL 8.0.18+, `ANALYZE` on
//! MariaDB 10.1+), gated by the version-aware `EXPLAIN_ANALYZE` capability.
//!
//! # Known `datagrep-api` gaps found while implementing this driver
//!
//! 1. **`NESTED_TRANSACTIONS` is declared but unreachable through the
//!    seam.** MySQL supports SAVEPOINTs, but `datagrep_api::Transaction` has
//!    no `begin`/savepoint method — nesting can only be exercised by raw
//!    `SAVEPOINT` text through `Transaction::execute`. The flag is set
//!    because the engine capability is real; the seam can't drive it
//!    structurally yet.
//! 2. **`ExecOpts::read_only_assert` cannot be verified server-side per
//!    statement.** MySQL has session-level read-only only (which
//!    `set_read_only` uses, `Enforcement::Server`); a per-statement assert
//!    is client-side classification, which belongs above the driver.
//! 3. **No way to surface warning *text* mid-stream.** `SHOW WARNINGS`
//!    needs the connection, which is busy while a result streams; only the
//!    protocol's warning *count* is available at end-of-set, so
//!    `Batch::notices` carries a count summary rather than full messages.
//!
//! # Deviations
//!
//! - **TLS is not compiled in** (mysql_async `minimal` feature set, matching
//!   the sibling drivers' "TLS deferred" posture). No `tls` config field is
//!   offered, so nothing can silently downgrade.
//! - **The session time zone is pinned to `+00:00` at connect** so that
//!   `TIMESTAMP` (UTC-normalized by the engine) arrives as its UTC reading
//!   and is tagged `TzSpec::Utc`, while `DATETIME` stays `TzSpec::Naive`.
//!   Without the pin, TIMESTAMP values would be rendered in the server's
//!   default zone and a `Utc` tag would be a lie.
//! - **`Cursor::resume_token` always returns `None`** (v1, same stance as
//!   the sibling drivers): the server-side result state dies with the
//!   in-flight stream; keyset resume belongs to `Op::Scan { resume }`.
//! - **Multi-statement scripts stream only the LAST statement's result**;
//!   preceding statements run to completion and are drained (their row
//!   results are discarded). One `Request` maps to one `Cursor` by seam
//!   design, so this is the honest interpretation of `MULTI_STATEMENT`.
//! - **Row identity for `EDITABLE_RESULTS` comes from column metadata**
//!   (PRI_KEY_FLAG + single origin table), with a known composite-PK
//!   partial-selection blind spot; the exactly-one-row rollback in the
//!   mutation path is the backstop (see `actor.rs::detect_identity`).
//! - **`Op::Count { exact: false }` still runs a real `COUNT(*)`**; the
//!   `table_rows` estimate is surfaced via `describe()` instead.
//! - **MariaDB JSON columns decode as `Value::Str`, not `Value::Json`.**
//!   MariaDB has no JSON wire type — its `JSON` is an alias for `LONGTEXT`
//!   and arrives as an ordinary text column; `Str` is the truthful decode of
//!   what that server declares. MySQL's native JSON type decodes as
//!   `Value::Json` raw text.

pub mod actor;
pub mod canceller;
pub mod catalog;
pub mod connection;
pub mod cursor;
pub mod driver;
pub mod error;
pub mod sql;
pub mod transaction;
pub mod value;

pub use driver::MySqlDriver;
