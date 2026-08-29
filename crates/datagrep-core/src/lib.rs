#![warn(rust_2018_idioms)]
#![deny(missing_debug_implementations)]

pub mod api;
pub mod convert;
pub mod export;
pub mod feeder;
pub mod query;
pub mod registry;
pub mod safety;
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
pub use safety::{SafetyDecision, SafetyGate, SafetyStatement};
pub use session::{ConnectionHandle, PinnedConn, Session, SessionRegistry};
pub use store::{
    DocSegment, GlobalBudget, MemoryPolicy, ResultStore, RowWindow, SpillPolicy, StorePhase,
    StoreState, WindowSlice, WindowStatus,
};
pub use timer::{TimerKey, TimerWheel};

pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn read<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn write<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}
