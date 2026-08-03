//! # dbx-profiles — local persistence (design §3.7, §3.8)
//!
//! One SQLite file, WAL, opened lazily off the startup path. Tables:
//! `folder`, `profile`, `tunnel`, `query_history` (+ FTS5), `saved_query`,
//! `editor_tab`, `kv`. The `plugin` table from §3.7 is **not** implemented
//! yet — there is no plugin host to populate it, and shipping dead DDL for a
//! host that doesn't exist just means migrating it again once the host's
//! real shape (sha256, granted hosts) is known.
//!
//! ## Secret hygiene (§3.8)
//! [`Profile`] and [`Tunnel`] have no field that can hold a secret — only an
//! opaque `secret_ref` pointing into the OS keychain. [`Store::create_profile`]
//! and [`Store::update_profile`] additionally reject any `config` key that
//! merely *looks* secret-shaped (`password`, `token`, `key`, ...) before it
//! ever reaches SQLite. TOML export therefore excludes secrets structurally,
//! not by filtering.
//!
//! ## Concurrency (design's blocking-pool rule)
//! All SQLite access happens on one dedicated worker thread owned by
//! [`Store`]; every public method is `async` and channels its request to
//! that thread. Construction ([`Store::open`]) does not touch disk — the
//! worker thread, the connection, migrations, and the on-open retention trim
//! only happen on the first real call.

#![warn(rust_2018_idioms)]
#![deny(missing_debug_implementations)]

mod db;
mod error;
mod export;
mod model;
mod queries;
mod secrets;
mod store;

pub use db::RetentionPolicy;
pub use error::ProfilesError;
pub use export::{ExportBundle, ImportStrategy, ImportSummary};
pub use model::{
    new_id, now_ms, EditorTab, Env, Folder, HistoryEntry, HistoryStatus, NewHistoryEntry, Profile,
    SavedQuery, Tunnel,
};
pub use secrets::validate_no_secrets;
pub use store::Store;
