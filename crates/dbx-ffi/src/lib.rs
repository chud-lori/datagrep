//! # dbx-ffi — the C ABI a native Swift/AppKit app links against
//!
//! Design §4.4: *"the desktop UI is just a `CoreApi` client"*, and §3:
//! *"The UI links the core in-process. No IPC boundary at all."* This crate is
//! that link — an `extern "C"` skin over [`dbx_core::CoreApi`] and nothing
//! else. It owns no database logic; every call here is a thin, panic-proof
//! translation of a `CoreApi` call into pointers, JSON, and byte slices.
//!
//! The header at `include/dbx.h` is the contract and is hand-written, not
//! generated: it is frozen against a Swift app being written in parallel.
//! Every function below is declared there in the same order.
//!
//! ## The four invariants this crate exists to preserve
//!
//! 1. **Only the visible window is ever materialised** (design §3.2). See
//!    [`rows::dbx_query_rows`]: it calls `CoreApi::get_rows(qid, off..off+len)`
//!    and formats exactly that rectangle.
//!    [`dbx_core::store::WindowStatus::Pending`] becomes
//!    `dbx_rows_pending() == true` — a skeleton to draw, never a block to wait
//!    on.
//! 2. **No allocation per cell per frame** (design §5.1). Cell text is
//!    produced *once per window* into a single buffer owned by the
//!    [`rows::DbxRows`], and `Utf8`/`LargeUtf8` Arrow columns and `Arc<str>`
//!    document values are not copied at all — `dbx_rows_cell` hands back a
//!    pointer straight into the Arrow buffer, which the window keeps alive.
//! 3. **The stop button always returns instantly** (design §3.3).
//!    [`query::dbx_query_cancel`] never awaits; the server half's real answer
//!    arrives later and is picked up by the query's supervisor task.
//! 4. **`Absent` is not `NULL`** (design §3.1). `dbx_rows_cell_kind` reports
//!    `2` for [`dbx_api::Value::Absent`] and `1` for [`dbx_api::Value::Null`].
//!    That distinction is the entire reason the document model exists, and
//!    the ABI carries it as a first-class value.
//!
//! ## Threading
//!
//! One process-global multi-thread tokio runtime with `worker_threads` capped
//! at 4 (design §3.4: *"4 workers, not `num_cpus`: this is an I/O-bound
//! desktop app"*). Nothing in this crate ever calls `block_on` from inside a
//! runtime thread; the progress callback is invoked from a worker thread and
//! the header says so, because hopping to the main queue is the caller's job.
//!
//! ## Panics
//!
//! Every entry point is wrapped in [`std::panic::catch_unwind`]. A panic
//! becomes an error string, never an unwind across the ABI (which is UB).
//! The workspace release profile keeps `panic = "unwind"`, so this works.

#![warn(rust_2018_idioms)]

pub mod catalog;
pub mod cells;
pub mod core;
pub mod drivers;
pub mod ffi_util;
pub mod profiles;
pub mod query;
pub mod rows;
pub mod runtime;

pub use crate::core::{dbx_core_free, dbx_core_new, dbx_string_free, DbxCore};
pub use catalog::{dbx_catalog_children_json, dbx_catalog_describe_json};
pub use profiles::{dbx_profiles_add, dbx_profiles_list_json, dbx_profiles_remove};
pub use query::{
    dbx_query_cancel, dbx_query_free, dbx_query_on_progress, dbx_query_run,
    dbx_query_status_json, DbxProgressFn, DbxQuery,
};
pub use rows::{
    dbx_query_rows, dbx_rows_cell, dbx_rows_cell_detail_json, dbx_rows_cell_kind,
    dbx_rows_columns, dbx_rows_count, dbx_rows_free, dbx_rows_pending, DbxRows,
};
