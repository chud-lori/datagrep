//! The one runtime.
//!
//! ```text
//! tokio runtime    worker_threads = clamp(available_parallelism()-1, 2, 4)
//! blocking pool    max 8, keep_alive 10s
//! ```
//!
//! Four workers, not `num_cpus`: this is an I/O-bound desktop app. 32 worker
//! threads on a workstation costs ~32 × 2 MB of stack reservation and steals
//! cores from whatever the user is compiling, and buys nothing — the work here
//! is waiting on sockets, not saturating CPUs.
//!
//! It is **process-global** rather than per-[`crate::DatagrepCore`] on purpose:
//!
//! - a `tokio::runtime::Runtime` must not be dropped from inside one of its
//!   own worker threads, and a global one is simply never dropped, so
//!   `datagrep_core_free` can never hit that panic;
//! - two cores (e.g. a main window and a preferences probe) share the four
//!   workers instead of doubling them.
//!
//! It is created on the first `datagrep_core_new` and lives for the life of the
//! process. Spawning four parked worker threads is sub-millisecond and touches
//! no socket, so `datagrep_core_new` still "never blocks".

use std::time::Duration;

use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;

static RUNTIME: OnceCell<Runtime> = OnceCell::new();

/// The process-global runtime, started on first use.
pub fn runtime() -> Result<&'static Runtime, String> {
    RUNTIME.get_or_try_init(build)
}

fn build() -> Result<Runtime, String> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1))
        .unwrap_or(2)
        .clamp(2, 4);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(8)
        .thread_keep_alive(Duration::from_secs(10))
        .thread_name("datagrep-worker")
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the datagrep runtime: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_is_shared_and_capped_at_four_workers() {
        let a = runtime().expect("runtime");
        let b = runtime().expect("runtime");
        assert!(std::ptr::eq(a, b), "the runtime must be process-global");
        let metrics = a.metrics();
        assert!(
            (2..=4).contains(&metrics.num_workers()),
            "workers must stay capped at 4, got {}",
            metrics.num_workers()
        );
    }
}
