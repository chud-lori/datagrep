#![warn(rust_2018_idioms)]
// Every entry point is an unsafe fn; this lint forces each raw deref into an unsafe block carrying its own SAFETY note.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod catalog;
pub mod cells;
pub mod core;
pub mod drivers;
pub mod ffi_util;
pub mod mutate;
pub mod profiles;
pub mod query;
pub mod reread;
pub mod rows;
pub mod runtime;
pub mod safety;

pub use crate::core::{datagrep_core_free, datagrep_core_new, datagrep_string_free, DatagrepCore};
pub use catalog::{datagrep_catalog_children_json, datagrep_catalog_describe_json};
pub use mutate::datagrep_mutate;
pub use profiles::{datagrep_profiles_add, datagrep_profiles_list_json, datagrep_profiles_remove};
pub use query::{
    datagrep_query_cancel, datagrep_query_free, datagrep_query_on_progress, datagrep_query_run,
    datagrep_query_status_json, DatagrepProgressFn, DatagrepQuery,
};
pub use reread::datagrep_reread_documents;
pub use rows::{
    datagrep_query_rows, datagrep_rows_cell, datagrep_rows_cell_detail_json,
    datagrep_rows_cell_kind, datagrep_rows_column_names_json, datagrep_rows_columns,
    datagrep_rows_count, datagrep_rows_envelope_json, datagrep_rows_free, datagrep_rows_pending,
    DatagrepRows,
};
pub use safety::{
    datagrep_safety_evaluate_json, datagrep_safety_pending_json, datagrep_safety_satisfy,
};
