//! `datagrep-drv-mongo` — the MongoDB driver behind `datagrep-api`'s `Driver` seam
//! (the design doc §3.1, §3.2, §3.3, §3.6, §5.1,
//! risk #2 "does `Shape`/`Value` actually stretch to non-rectangular data?",
//! risk #6 "document-grid UX").
//!
//! # Design decisions worth stating up front
//!
//! - **The MongoShell parser is entirely `datagrep-lang`'s.** This crate only
//!   consumes `datagrep_lang::mongo::{parse, MongoStatement, ParsedMongo}`
//!   (`connection.rs::execute_text`) and dispatches the result to the
//!   official `mongodb` driver — no reimplementation, no translation of the
//!   parsed text (design §3.6).
//! - **NoSQL injection (§3.8).** `filter.rs::compile_predicate` compiles
//!   every comparison to an explicit operator (`{field: {"$eq": v}}`), never
//!   the bare `{field: v}` shorthand, so a parameter value shaped like
//!   `{"$ne": null}` can never promote itself into operator position — see
//!   `filter.rs`'s module doc and its
//!   `ne_null_shaped_parameter_value_cannot_alter_query_structure` test.
//! - **`Absent` vs `Null`.** `value.rs::bson_to_value` only ever produces
//!   `Value::Null` for a BSON document that actually stores a null; a field
//!   missing from the document is never synthesized as anything —
//!   `cursor.rs` reads through `bson::Document::iter()`, which simply never
//!   yields absent keys, so the distinction is preserved by construction.
//! - **No actor task for transactions**, unlike `datagrep-drv-postgres`'s
//!   `actor.rs`. `mongodb::ClientSession` is owned (not borrowed from
//!   `Client`, unlike `tokio_postgres::Transaction<'a>`), so
//!   `transaction.rs` shares it behind a plain `Arc<Mutex<ClientSession>>`
//!   — see that module's doc comment for the full reasoning.
//! - **Cancellation degrades honestly (§3.3).** `maxTimeMS` is set on every
//!   request (`connection.rs::DEFAULT_MAX_TIME`, or the caller's
//!   `ExecOpts::timeout`). `killOp` is probed once per connection and
//!   cached; when unavailable (or when nothing is currently tagged
//!   in-flight), `cancel()` honestly reports `CancelOutcome::ClientAbandoned`
//!   rather than claiming a kill that didn't happen — see `canceller.rs`.
//!
//! # Known `datagrep-api` gaps found while implementing this driver
//!
//! 1. **`Mutation::Update`/`Delete::key` carries `key: Vec<Value>` with no
//!    field names**, the same gap `datagrep-drv-postgres` reports. For MongoDB
//!    there is no `RowSchema::identity` at all on `Shape::Documents` to fall
//!    back on, so the only sound reading is "the `_id` value" — this driver
//!    requires `key.len() == 1` and refuses anything else
//!    (`connection.rs::id_filter`).
//! 2. **No portable way to express "route reads to a secondary" or any
//!    other genuinely server-enforced read-only mode** through
//!    `Connection::set_read_only`. This driver returns `Enforcement::Client`
//!    honestly rather than either lying about `Server` or leaving the
//!    method unimplemented.
//! 3. **`TxOpts::isolation`/`read_only` have no MongoDB transaction
//!    equivalent** (Mongo transactions are snapshot/majority-consistency
//!    based, not on the SQL isolation ladder) — accepted by `begin()` but
//!    not mapped to anything, rather than silently downgraded to a
//!    misleading isolation level.
//!
//! # Deviations
//!
//! - **`Op::Mutate` does not open an implicit transaction** the way
//!   `datagrep-drv-postgres` wraps every mutation batch in one. On a standalone
//!   `mongod` there is nothing to wrap it in, and doing so conditionally
//!   only when `TRANSACTIONS` is available would make cross-batch atomicity
//!   silently depend on deployment topology. Each mutation is still checked
//!   for "exactly one document affected" and the batch stops on the first
//!   mismatch, but earlier mutations in the same batch are **not** rolled
//!   back automatically in v1.
//! - **Explicit-transaction `execute()` supports a deliberately smaller
//!   surface** than the top-level connection: `find`/`insertOne`/
//!   `updateOne`/`updateMany`/`deleteOne`/`deleteMany` and `Op::Mutate`.
//!   `aggregate`, raw command documents, and `EXPLAIN` are refused with
//!   `DbError::Unsupported` inside an explicit transaction rather than
//!   silently running outside it (`transaction.rs`).
//! - **`EXPLAIN` (`Op::Explain`) supports `find`/`aggregate` shell chains,
//!   raw command documents, and `Op::Scan`** — not `Op::Count`/`Op::Mutate`
//!   (`connection.rs::build_explainable_command`).
//! - **Catalog listing (`children()`) does one unpaginated fetch** per level
//!   with client-side prefix/limit filtering rather than a true server-side
//!   resume token; `Page::next` is always `None`. Database/collection counts
//!   are small enough on real deployments that this never approaches
//!   Postgres's thousand-table case — see `catalog.rs`.
//! - **Legacy BSON binary subtype 3 (`UuidOld`) is not decoded as
//!   `Value::Uuid`** (its byte order is driver/locale-dependent with no
//!   single correct interpretation) — stays `Value::Bytes`, raw and intact.
//! - **BSON `u64` values beyond `i64::MAX`** widen to `Bson::Double` when
//!   encoding a `Value::U64` back to BSON (Mongo has no unsigned integer
//!   type) — astronomically rare in practice, documented in `value.rs`.
//! - **datagrep-lang's `ObjectId("...")` shell constructor compiles to
//!   `Value::Str(<hex>)`**, not `Value::Unsupported` (datagrep-lang cannot depend
//!   on `bytes::Bytes`, per its own crate report). `value.rs`'s
//!   `value_to_bson_for_field` recovers the intent narrowly: only the `_id`
//!   field of a shell-text filter/update/insert document, and only when the
//!   string is exactly a 24-hex-character ObjectId shape.

pub mod canceller;
pub mod catalog;
pub mod connection;
pub mod cursor;
pub mod driver;
pub mod error;
pub mod filter;
pub mod transaction;
pub mod value;

pub use driver::MongoDriver;
