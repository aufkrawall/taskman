//! Reusable bounded action executor (implement.md §11.7).
//!
//! Process/session/service control calls Windows APIs that can block for
//! tens to hundreds of milliseconds — none of them belong on the UI thread.
//! Instead of spawning an ad-hoc thread per action, everything fire-and-
//! forget goes through two independent bounded lanes. Results come back as
//! toasts; completion wakes the UI through the shared repaint hook.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use tm_core::i18n::{self, K};

pub type Toasts = std::sync::Arc<std::sync::Mutex<Vec<crate::app::Toast>>>;

type Job = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct ActionExecutor {
    inner: Arc<ExecutorInner>,
}

struct ExecutorInner {
    senders: Vec<SyncSender<Job>>,
    next_lane: AtomicUsize,
}

enum QueueError {
    Full,
    Disconnected(Job),
}

impl ActionExecutor {
    /// Spawn up to two worker lanes. Returns None only when the first thread
    /// cannot be created; callers can then keep the UI alive while reporting
    /// that actions are unavailable.
    pub fn start() -> Option<Self> {
        // Two independent lanes leave one control path available if an OS API
        // wedges a worker. The total queue remains bounded at 64 jobs.
        let mut senders = Vec::with_capacity(2);
        for lane in 0..2 {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(32);
            let spawned = std::thread::Builder::new()
                .name(format!("tm-actions-{lane}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        let t0 = std::time::Instant::now();
                        job();
                        let dur = t0.elapsed();
                        if dur > std::time::Duration::from_millis(200) {
                            tracing::debug!(
                                dur_ms = dur.as_millis() as u64,
                                "slow platform action"
                            );
                        }
                    }
                });
            if spawned.is_err() {
                break;
            }
            senders.push(tx);
        }
        (!senders.is_empty()).then(|| Self {
            inner: Arc::new(ExecutorInner {
                senders,
                next_lane: AtomicUsize::new(0),
            }),
        })
    }

    fn try_queue(&self, job: Job) -> Result<(), QueueError> {
        let lane_count = self.inner.senders.len();
        let first = self.inner.next_lane.fetch_add(1, Ordering::Relaxed) % lane_count;
        let mut pending = Some(job);
        let mut disconnected = 0;
        for offset in 0..lane_count {
            let lane = (first + offset) % lane_count;
            let job = pending.take().expect("queued job remains owned");
            match self.inner.senders[lane].try_send(job) {
                Ok(()) => return Ok(()),
                Err(std::sync::mpsc::TrySendError::Full(job)) => pending = Some(job),
                Err(std::sync::mpsc::TrySendError::Disconnected(job)) => {
                    disconnected += 1;
                    pending = Some(job);
                }
            }
        }
        let job = pending.expect("unsent job remains owned");
        if disconnected == lane_count {
            Err(QueueError::Disconnected(job))
        } else {
            drop(job);
            Err(QueueError::Full)
        }
    }

    /// Run `job` on the executor; on completion push a localized result
    /// toast into `toasts` and wake the UI.
    pub fn run(
        &self,
        toasts: Toasts,
        wake: impl Fn() + Send + Sync + 'static,
        success_msg: impl FnOnce() -> String + Send + 'static,
        job: impl FnOnce() -> Result<(), tm_core::TmError> + Send + 'static,
    ) -> bool {
        let overload_toasts = toasts.clone();
        let wake = Arc::new(wake);
        let completion_wake = wake.clone();
        let send_result = move |res: Result<(), tm_core::TmError>| {
            let msg = match res {
                Ok(()) => success_msg(),
                Err(e) => i18n::trf(K::ErrMsg, &[&e.to_string()]),
            };
            crate::app::toast_from(&toasts, msg);
            completion_wake();
        };
        let queued: Job = Box::new(move || send_result(job()));
        match self.try_queue(queued) {
            Ok(()) => true,
            Err(QueueError::Full) => {
                crate::app::toast_from(&overload_toasts, i18n::tr(K::ActionQueueFull));
                wake();
                false
            }
            Err(QueueError::Disconnected(job)) => {
                drop(job);
                crate::app::toast_from(&overload_toasts, i18n::tr(K::ActionFailed));
                wake();
                false
            }
        }
    }

    /// Fire-and-forget job without a result toast (completion still wakes UI).
    /// Returns false when bounded backpressure rejected the job.
    pub fn run_quiet(
        &self,
        wake: impl Fn() + Send + 'static,
        job: impl FnOnce() + Send + 'static,
    ) -> bool {
        let queued: Job = Box::new(move || {
            job();
            wake();
        });
        match self.try_queue(queued) {
            Ok(()) => true,
            Err(QueueError::Full) => false,
            Err(QueueError::Disconnected(job)) => {
                drop(job);
                false
            }
        }
    }
}
