//! `datagrep-drv-postgres` — the Postgres driver behind `datagrep-api`'s `Driver` seam.
//!
//! # Streaming: never buffer a whole result
//!
//! `PgConnection::execute` never materializes a full result. A SELECT-ish
//! statement (anything whose prepared `Statement::columns()` is non-empty)
//! is bound to a named portal inside a transparently opened, read-only
//! `Transaction` and streamed batch-by-batch via `Transaction::query_portal`
//! (extended protocol `Bind` + `Execute(max_rows)`); a non-SELECT statement
//! runs directly via `Client::execute` with no transaction at all.
//!
//! Because `tokio_postgres::Transaction<'a>` borrows `&'a mut Client` and
//! `Connection::execute` must return a `'static Box<dyn Cursor>`, the
//! Transaction (and any portals bound within it) never leave the task that
//! opened them — see `actor.rs` for the actor design that replaces what
//! would otherwise need an unsafe self-referential struct.
//!
//! # Sessions: a cursor pins its connection
//!
//! One `PgConnection` is a *logical* connection backed by a small pool of
//! *physical* sessions (`pool.rs`). A cursor or an interactive transaction
//! pins the session it runs on, but only for as long as it really needs it:
//! a cursor releases its session the moment its portal is drained, and
//! anything that needs the server while a session is genuinely pinned takes a
//! different one from the pool. Nothing in this driver waits on another
//! operation without a deadline — at the pool's cap the wait ends in
//! `DbError::ResourceExhausted` naming what holds the sessions. `pool.rs`'s
//! module docs carry the full reasoning and the deadlock it replaced.
//!
//! # Known `datagrep-api` gaps found while implementing this driver
//!
//! 1. **Capabilities Postgres really has that `Caps` cannot express.**
//!    `NESTED_TRANSACTIONS`, `EXPLAIN_ANALYZE`, `MULTI_STATEMENT`,
//!    `POSITIONAL_PARAMS`, `EXPORT_STREAMING`, `EXPRESSION_FILTER` are not
//!    bits on `datagrep_api::caps::Caps` (`crates/datagrep-api/src/caps.rs` defines
//!    only ten flags). `driver::PG_CAPS` sets every flag that does exist and
//!    applies to Postgres.
//! 2. **(Resolved.)** `Mutation::Update`/`Delete` now carry their row
//!    identity as named `(FieldPath, Value)` pairs — the same shape as
//!    `sets` — so `sql::compile_mutation` builds the WHERE clause straight
//!    from the mutation. The per-mutation live `pg_index` lookup this driver
//!    used to need is gone.
//! 3. **`RowSchema` has no nullability signal for prepared-statement
//!    metadata.** Postgres's `RowDescription` (what `Statement::columns()`
//!    is built from) never reports nullability, so `FieldFlags::NULLABLE` is
//!    always left unset for query results (only `describe()`'s
//!    `pg_attribute.attnotnull` lookup can set it, for a browsed table).
//!
//! # Deviations
//!
//! - **TLS is deferred**: `connect` accepts a `tls` config
//!   field (`disable`/`require`/`verify-ca`/`verify-full`) so the connection
//!   form is honest about what the engine supports, but any mode other than
//!   `disable` fails fast with `DbError::Tls("TLS not yet implemented...")`
//!   rather than silently connecting in plaintext.
//! - **`INSERT/UPDATE ... RETURNING` via `Request::Native` at the top level**
//!   is not correctly supported: it has non-empty `columns()` like a SELECT,
//!   so it's routed through the read-only auto-wrap transaction and Postgres
//!   rejects the write. Full support needs the statement's command tag, not
//!   just its column list. `Op::Mutate` (which always runs read-write) is
//!   unaffected. Documented in `connection.rs::execute_native_or_scan`.
//! - **`Cursor::resume_token` always returns `None`** in v1: the portal dies
//!   with the wrapping transaction, which ends as soon
//!   as the cursor is drained, closed, or dropped, so there is nothing to
//!   resume into.
//! - **Postgres `NOTICE` messages are not surfaced** on `Batch::notices` —
//!   the background `Connection` future's `AsyncMessage::Notice` stream
//!   isn't wired up in v1.
//! - **`PgTransaction::execute` does not support `Op::Mutate`** — only the
//!   auto-committing path off `PgConnection::execute` does. Use the
//!   equivalent `Request::Native` UPDATE/INSERT/DELETE text inside an
//!   explicit transaction instead.
//! - **Nested/multi-segment `FieldPath`s compile to a `jsonb` `#>>` path
//!   expression** against the base column (`sql.rs::field_path_expr`) rather
//!   than anything schema-aware; correct for `jsonb` columns, a type error
//!   (not silent wrongness) against anything else.

pub mod actor;
pub mod canceller;
pub mod catalog;
pub mod connection;
pub mod cursor;
pub mod driver;
pub mod error;
pub mod pool;
pub mod sql;
pub mod transaction;
pub mod value;

pub use driver::PostgresDriver;
