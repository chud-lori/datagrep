//! # datagrep-ffi — the C ABI a native Swift/AppKit app links against
//!
//! The desktop UI is just a `CoreApi` client: it links the core in-process,
//! with no IPC boundary at all. This crate is that link — an `extern "C"` skin
//! over [`datagrep_core::CoreApi`] and nothing else. It owns no database logic;
//! every call here is a thin, panic-proof translation of a `CoreApi` call into
//! pointers, JSON, and byte slices.
//!
//! The header at `include/datagrep.h` is the contract and is hand-written, not
//! generated: it is frozen against a Swift app being written in parallel.
//! Every function below is declared there in the same order.
//!
//! ## The four invariants this crate exists to preserve
//!
//! 1. **Only the visible window is ever materialised.** See
//!    [`rows::datagrep_query_rows`]: it calls `CoreApi::get_rows(qid, off..off+len)`
//!    and formats exactly that rectangle.
//!    [`datagrep_core::store::WindowStatus::Pending`] becomes
//!    `datagrep_rows_pending() == true` — a skeleton to draw, never a block to wait
//!    on.
//! 2. **No allocation per cell per frame.** Cell text is produced *once per
//!    window* into a single buffer owned by the [`rows::DatagrepRows`], and
//!    `Utf8`/`LargeUtf8` Arrow columns and `Arc<str>` document values are not
//!    copied at all — `datagrep_rows_cell` hands back a pointer straight into
//!    the Arrow buffer, which the window keeps alive.
//! 3. **The stop button always returns instantly.**
//!    [`query::datagrep_query_cancel`] never awaits; the server half's real answer
//!    arrives later and is picked up by the query's supervisor task.
//! 4. **`Absent` is not `NULL`.** `datagrep_rows_cell_kind` reports `2` for
//!    [`datagrep_api::Value::Absent`] and `1` for [`datagrep_api::Value::Null`].
//!    That distinction is the entire reason the document model exists, and
//!    the ABI carries it as a first-class value.
//!
//! ## Threading
//!
//! One process-global multi-thread tokio runtime with `worker_threads` capped
//! at 4 — four workers, not `num_cpus`: this is an I/O-bound desktop app, and
//! sizing the pool to the CPU only steals cores from the UI. Nothing in this
//! crate ever calls `block_on` from inside a runtime thread; the progress
//! callback is invoked from a worker thread and the header says so, because
//! hopping to the main queue is the caller's job.
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

pub use crate::core::{datagrep_core_free, datagrep_core_new, datagrep_string_free, DatagrepCore};
pub use catalog::{datagrep_catalog_children_json, datagrep_catalog_describe_json};
pub use profiles::{datagrep_profiles_add, datagrep_profiles_list_json, datagrep_profiles_remove};
pub use query::{
    datagrep_query_cancel, datagrep_query_free, datagrep_query_on_progress, datagrep_query_run,
    datagrep_query_status_json, DatagrepProgressFn, DatagrepQuery,
};
pub use rows::{
    datagrep_query_rows, datagrep_rows_cell, datagrep_rows_cell_detail_json,
    datagrep_rows_cell_kind, datagrep_rows_columns, datagrep_rows_count, datagrep_rows_free,
    datagrep_rows_pending, DatagrepRows,
};
