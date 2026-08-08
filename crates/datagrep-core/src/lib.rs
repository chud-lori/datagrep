//! # datagrep-core — the only stateful orchestrator (design §3)
//!
//! Everything between the frontends and the drivers: the driver registry,
//! sessions and connection pools, the streaming pipeline (feeder → bounded
//! channel → result store), the query lifecycle, and the [`CoreApi`] façade
//! frontends consume.
//!
//! The load-bearing invariants, all from the design doc:
//!
//! - **§3.2 — the data-path channel bound is 2.** The feeder can never run
//!   more than two chunks ahead of the store; when the store stops admitting,
//!   the feeder blocks, the driver stops reading the socket, the TCP window
//!   closes, and the server stops producing. See [`feeder::DATA_CHANNEL_BOUND`].
//! - **§3.2/§3.3 — the memory contract.** A global result budget shared by
//!   every [`store::ResultStore`]; tabular data is Arrow, documents stay as
//!   `Value`s (arena-ready), overflow spills to unlinked Arrow IPC temp files.
//! - **§3.4 — no polling.** Every deadline goes through the armed-on-demand
//!   [`timer::TimerWheel`], which fully disarms when it has nothing to do.
//! - **§3.5 — isolation.** Driver calls run inside their own task; a driver
//!   panic is caught at the task boundary, converted to
//!   [`datagrep_api::DbError::DriverPanic`], and the connection is poisoned and
//!   evicted while the app lives.
//! - **§3.4 — token tree.** Cancellation is structured: session → connection
//!   → query. The stop button always returns instantly (§3.3); the server
//!   half of a cancel is fire-and-forget and its outcome is reported honestly.

#![warn(rust_2018_idioms)]
#![deny(missing_debug_implementations)]

pub mod api;
pub mod convert;
pub mod export;
pub mod feeder;
pub mod query;
pub mod registry;
pub mod session;
pub mod spill;
pub mod store;
pub mod timer;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use api::{CoreApi, Profile, ProfileId};
pub use convert::{rows_to_record_batch, BatchConverter};
pub use export::{run_export_on, ExportSink, ExportStats, SinkFlow, EXPORT_FETCH_HINT};
pub use feeder::{
    spawn_feeder, FeedState, FeederHandle, FeederPolicy, ParkReason, DATA_CHANNEL_BOUND,
};
pub use query::{CancelReport, QueryEvent, QueryId, QueryMgr, QueryStats};
pub use registry::DriverRegistry;
pub use session::{ConnectionHandle, PinnedConn, Session, SessionRegistry};
pub use store::{
    DocSegment, GlobalBudget, MemoryPolicy, ResultStore, RowWindow, SpillPolicy, StorePhase,
    StoreState, WindowSlice, WindowStatus,
};
pub use timer::{TimerKey, TimerWheel};

/// Recover a possibly-poisoned std mutex guard. A panic while holding one of
/// our internal locks never leaves data structurally broken (every critical
/// section is a small field update), so recovering beats propagating the
/// panic into unrelated tasks — the same isolation stance as design §3.5.
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Same recovery policy for RwLock reads.
pub(crate) fn read<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Same recovery policy for RwLock writes.
pub(crate) fn write<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}
