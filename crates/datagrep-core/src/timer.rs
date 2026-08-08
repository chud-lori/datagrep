//! The armed-on-demand timer wheel — the core's answer to "no polling".
//!
//! One global task owns a [`DelayQueue`]. It arms a timer only while at least
//! one deadline exists and **fully disarms — parks on a [`Notify`] with no
//! timer registered — when the queue is empty**: zero connections, zero
//! queries, zero timer wakeups. An idle app should not burn battery, and a
//! free-running ticker costs a wakeup per interval forever whether or not
//! anything is pending. So `setInterval`-style loops and free-running
//! `tokio::time::interval` are banned in the core; every deadline (idle reap,
//! query timeout) goes through here instead.

use std::fmt;
use std::future::poll_fn;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::time::delay_queue::{self, DelayQueue};

use crate::lock;

/// What fires when a deadline elapses. A boxed closure keeps the wheel fully
/// decoupled from its clients (sessions, queries, toasts all schedule here).
/// The closure runs on the wheel's worker task, inside the runtime — spawn if
/// the work is more than a nudge.
type TimerEvent = Box<dyn FnOnce() + Send + 'static>;

/// Handle to a scheduled deadline; pass to [`TimerWheel::cancel`].
#[derive(Debug)]
pub struct TimerKey(delay_queue::Key);

struct Shared {
    queue: Mutex<DelayQueue<TimerEvent>>,
    /// Wakes the worker out of its parked state (or re-evaluates deadlines
    /// after an earlier-than-current insert).
    notify: Notify,
    /// Probe: how many times the worker's expiry future has been polled.
    /// Test-only observability for the "zero wakeups when empty" guarantee.
    polls: AtomicU64,
}

/// The one global deadline wheel. Cheap to clone via `Arc` at the call sites
/// that need it; the worker task is aborted when the wheel is dropped.
pub struct TimerWheel {
    shared: Arc<Shared>,
    worker: JoinHandle<()>,
}

impl TimerWheel {
    /// Spawn the (single) worker task. Must be called inside a tokio runtime.
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(DelayQueue::new()),
            notify: Notify::new(),
            polls: AtomicU64::new(0),
        });
        let worker = tokio::spawn(run_worker(shared.clone()));
        Self { shared, worker }
    }

    /// Schedule `event` to fire at `deadline`. Arms the worker if it was
    /// parked. Returns a key usable with [`TimerWheel::cancel`].
    pub fn schedule(&self, deadline: Instant, event: impl FnOnce() + Send + 'static) -> TimerKey {
        let key = lock(&self.shared.queue).insert_at(Box::new(event), deadline);
        self.shared.notify.notify_one();
        TimerKey(key)
    }

    /// Cancel a scheduled deadline. Returns `true` if it was still pending.
    /// The worker re-parks on its own once the queue drains.
    pub fn cancel(&self, key: TimerKey) -> bool {
        lock(&self.shared.queue).try_remove(&key.0).is_some()
    }

    /// Number of times the worker's expiry future has been polled — the probe
    /// behind the "fully disarms when empty" test. Stable when parked.
    pub fn probe_polls(&self) -> u64 {
        self.shared.polls.load(Ordering::SeqCst)
    }
}

impl Default for TimerWheel {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TimerWheel {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl fmt::Debug for TimerWheel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimerWheel")
            .field("pending", &lock(&self.shared.queue).len())
            .finish()
    }
}

/// Worker outcome of one expiry poll.
enum Step {
    Fired(TimerEvent),
    Empty,
}

async fn run_worker(shared: Arc<Shared>) {
    loop {
        // Fully disarmed state: queue empty → park on the Notify. No timer is
        // registered anywhere while we sit here. `notify_one` stores a permit,
        // so a schedule racing this check is not lost.
        if lock(&shared.queue).is_empty() {
            shared.notify.notified().await;
            continue;
        }

        // Armed state: poll the DelayQueue (which internally arms exactly one
        // timer at the next deadline) and simultaneously listen for schedule/
        // cancel nudges that may have moved the next deadline.
        let step = {
            let expiry = poll_fn(|cx| {
                shared.polls.fetch_add(1, Ordering::SeqCst);
                let mut q = lock(&shared.queue);
                match q.poll_expired(cx) {
                    Poll::Ready(Some(expired)) => Poll::Ready(Step::Fired(expired.into_inner())),
                    Poll::Ready(None) => Poll::Ready(Step::Empty),
                    Poll::Pending => Poll::Pending,
                }
            });
            tokio::select! {
                step = expiry => Some(step),
                _ = shared.notify.notified() => None, // re-evaluate deadlines
            }
        };

        match step {
            Some(Step::Fired(event)) => event(),
            Some(Step::Empty) | None => {} // loop re-checks emptiness / deadlines
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// With nothing scheduled the worker is parked — the expiry future is
    /// never polled over a 200 ms window (probe counter is flat).
    #[tokio::test]
    async fn wheel_fully_disarms_when_empty() {
        let wheel = TimerWheel::new();
        // Let the worker reach its parked state.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = wheel.probe_polls();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = wheel.probe_polls();
        assert_eq!(before, after, "worker polled while queue was empty");
    }

    #[tokio::test]
    async fn schedules_fire_and_wheel_reparks() {
        let wheel = TimerWheel::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        wheel.schedule(Instant::now() + Duration::from_millis(30), move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);

        // After firing, the wheel must return to the parked (disarmed) state.
        let before = wheel.probe_polls();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            wheel.probe_polls(),
            before,
            "did not re-park after draining"
        );
    }

    #[tokio::test]
    async fn cancel_prevents_firing() {
        let wheel = TimerWheel::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        let key = wheel.schedule(Instant::now() + Duration::from_millis(60), move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        assert!(wheel.cancel(key), "was pending");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 0, "cancelled event fired");
    }

    #[tokio::test]
    async fn earlier_deadline_inserted_while_armed_fires_on_time() {
        let wheel = TimerWheel::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let o1 = order.clone();
        wheel.schedule(Instant::now() + Duration::from_millis(200), move || {
            lock(&o1).push("late");
        });
        let o2 = order.clone();
        wheel.schedule(Instant::now() + Duration::from_millis(30), move || {
            lock(&o2).push("early");
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*lock(&order), vec!["early", "late"]);
    }
}
