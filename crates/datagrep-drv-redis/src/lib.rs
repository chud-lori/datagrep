//! # datagrep-drv-redis
//!
//! Redis driver for `datagrep`, implementing the frozen `datagrep-api` seam
//! (the design doc §3.1) over the [`redis`] crate's async
//! `connection-manager` (auto-reconnecting, cheaply cloneable — no internal
//! mutex needed to make [`RedisConnection`] `Send + Sync`).
//!
//! ## The one rule this whole crate exists to enforce
//!
//! **Never `KEYS *`.** Design §5.2 calls it out by name: "one click DOSes
//! the user's production database." Every browse path — the catalog
//! (`catalog.rs`), structured `Op::Scan` (`connection.rs`), and even a
//! hand-typed `SCAN`/`HSCAN`/`SSCAN`/`ZSCAN` in `Request::Native`
//! (`connection.rs::native_scan_cursor`) — goes through the *same* cursor
//! mechanics: an opaque server-side cursor, `MATCH`, `COUNT`, one round trip
//! per page. `RedisCatalog::levels()` marks every level below `db-index` as
//! [`datagrep_api::Enumeration::ScanOnly { requires_prefix: true }`] — design
//! §3.1's own description of this flag: "the single most important catalog
//! concept: it's what stops the app from firing `KEYS *` at a 40 GB Redis
//! because someone clicked a triangle." Past `KEY_ENUMERATION_DBSIZE_THRESHOLD`
//! keys, `KEY_ENUMERATION` itself turns off (`driver.rs`'s post-handshake
//! `DBSIZE` probe) — no "list all keys," ever, on a server that size.
//!
//! ## Cancellation (design §3.3)
//!
//! Redis commands are atomic; the only thing that can run "long" from this
//! driver's side is our own SCAN loop, and that stops the moment
//! `CancelFlag::is_cancelled()` is true at the next round-trip check — no
//! server round trip needed (`CancelKind::ClientAbandon`). The one
//! exception is a command that blocks the connection waiting on the server
//! (`BLPOP`, `WAIT`, `XREAD BLOCK`, …): for those, "just stop" would leave
//! the socket hung until the server's own timeout, so `RedisCanceller`
//! issues a real `CLIENT KILL ID` from a second connection instead
//! (`canceller.rs`). Which behavior applies is decided dynamically per
//! cancel, not as a static connection-wide capability bit — see
//! `driver.rs`'s `REDIS_CAPS` doc for why `Caps::SERVER_CANCEL` is
//! deliberately left unset rather than being a blanket (and partly
//! dishonest) flag.
//!
//! ## What's honestly not offered
//!
//! - **Transactions.** `begin()` returns `DbError::Unsupported` — Redis
//!   `MULTI`/`EXEC` is a single optimistic pipeline (no mid-transaction
//!   reads of your own writes, no savepoints), not what `datagrep-api`'s
//!   `Transaction` trait promises. `Op::Mutate` still gets real atomicity
//!   by using `MULTI`/`EXEC` internally for one batch (`connection.rs`).
//! - **Server-side read-only sessions.** `set_read_only` always returns
//!   [`datagrep_api::driver::Enforcement::Client`] — Redis has no session mode
//!   that makes the *server* refuse writes, so the UI's read-only badge
//!   must say it's client-enforced only (design §3.8).
//! - **TLS.** `rediss://` parses and round-trips through the connection
//!   form honestly, but `connect` fails fast rather than silently
//!   downgrading to plaintext — see `error.rs`'s module doc for the full
//!   dependency-list gap note (mirrors `datagrep-drv-postgres`'s same deviation).

#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

mod canceller;
mod catalog;
mod cmd;
mod connection;
mod cursor;
mod driver;
mod error;
mod value;

pub use canceller::RedisCanceller;
pub use catalog::RedisCatalog;
pub use connection::RedisConnection;
pub use cursor::{ListCursor, OneShotCursor, RedisPairsCursor, ScanFamily, StreamCursor};
pub use driver::RedisDriver;
pub use error::map_redis_error;
