//! # datagrep-drv-sqlite
//!
//! SQLite driver for `datagrep`, implementing the frozen `datagrep-api` seam
//! over [`rusqlite`].
//!
//! ## The concurrency shape
//!
//! rusqlite is a synchronous, blocking API and a `rusqlite::Connection` is
//! not `Sync` (its statement handles borrow it). A sync driver therefore
//! wraps the whole cursor on a blocking thread with an mpsc bridge, and
//! never calls `block_on` inside an async task. Concretely:
//!
//! - [`SqliteConnection::open`] spawns one dedicated `std::thread` per
//!   connection ([`connection::run_worker`]) that owns the `rusqlite::Connection`
//!   for its entire life. It is a plain OS thread, not `spawn_blocking`: a
//!   DB client holds only a handful of connections (unlike a server pool),
//!   so paying for one real thread per connection is cheap and avoids
//!   statement handles crossing a pool.
//! - Every async trait method (`execute`, `begin`, `set_read_only`, …) sends
//!   a [`connection::WorkerMsg`] over a `std::sync::mpsc` channel and awaits a
//!   `tokio::sync::oneshot` reply. The channel send never blocks the async
//!   task on database I/O — it only blocks (briefly, non-async) on channel
//!   capacity, and the reply is awaited, not polled.
//! - [`cursor::SqliteCursor::next_batch`] sends `FetchBatch { max_rows }`;
//!   the worker thread steps the open prepared statement that many times and
//!   replies with a [`datagrep_api::Batch`]. The cursor never touches SQLite
//!   directly — it is `Send` but does no I/O itself.
//! - Cancellation is out-of-band: [`rusqlite::InterruptHandle`] is `Send +
//!   Sync` and does not go through the worker's command queue at all — the
//!   [`canceller::SqliteCanceller`] calls `interrupt()` directly from
//!   whatever task holds it, which is exactly what lets a cancel reach a
//!   worker thread stuck deep inside a long `sqlite3_step`.
//!
//! ## Type honesty
//! SQLite has type *affinity*, not enforced column types: a column declared
//! `INTEGER` can still store a `TEXT` value. `datagrep-drv-sqlite` never guesses
//! past what a cell's actual SQLite storage class says — see
//! [`value::sqlite_value_to_datagrep`] for the one exception (`BOOLEAN`-declared
//! 0/1 columns), which is explicitly called out as a mapping, not a coercion.

#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

mod canceller;
mod catalog;
mod compile;
mod connection;
mod cursor;
mod driver;
mod error;
mod scan;
mod transaction;
mod value;

pub use canceller::SqliteCanceller;
pub use catalog::SqliteCatalog;
pub use connection::SqliteConnection;
pub use cursor::SqliteCursor;
pub use driver::SqliteDriver;
pub use transaction::SqliteTransaction;
pub use value::quote_ident;
