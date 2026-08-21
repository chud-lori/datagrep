use std::time::Duration;

use once_cell::sync::OnceCell;
use tokio::runtime::Runtime;

static RUNTIME: OnceCell<Runtime> = OnceCell::new();

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
