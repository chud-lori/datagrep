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

type TimerEvent = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug)]
pub struct TimerKey(delay_queue::Key);

struct Shared {
    queue: Mutex<DelayQueue<TimerEvent>>,
    notify: Notify,
    polls: AtomicU64,
}

pub struct TimerWheel {
    shared: Arc<Shared>,
    worker: JoinHandle<()>,
}

impl TimerWheel {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(DelayQueue::new()),
            notify: Notify::new(),
            polls: AtomicU64::new(0),
        });
        let worker = tokio::spawn(run_worker(shared.clone()));
        Self { shared, worker }
    }

    pub fn schedule(&self, deadline: Instant, event: impl FnOnce() + Send + 'static) -> TimerKey {
        let key = lock(&self.shared.queue).insert_at(Box::new(event), deadline);
        self.shared.notify.notify_one();
        TimerKey(key)
    }

    pub fn cancel(&self, key: TimerKey) -> bool {
        lock(&self.shared.queue).try_remove(&key.0).is_some()
    }

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

enum Step {
    Fired(TimerEvent),
    Empty,
}

async fn run_worker(shared: Arc<Shared>) {
    loop {
        if lock(&shared.queue).is_empty() {
            shared.notify.notified().await;
            continue;
        }

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
