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

/// Apply `attempt` once per target inside a batch action job.
///
/// Counts successes into `completed` (read afterwards by the job's toast
/// message) and fails only when NOTHING succeeded, carrying the first error —
/// a partially refused batch still refreshes and reports its real count,
/// while an all-failed batch must never toast success. `targets` must be
/// non-empty; callers filter empty selections before submitting.
pub(crate) fn apply_batch<T>(
    targets: Vec<T>,
    completed: &AtomicUsize,
    mut attempt: impl FnMut(T) -> Result<(), tm_core::TmError>,
) -> Result<(), tm_core::TmError> {
    let mut first_error = None;
    for target in targets {
        match attempt(target) {
            Ok(()) => {
                completed.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if completed.load(Ordering::Relaxed) == 0 {
        Err(first_error.expect("non-empty batch must produce a result"))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------- core service
// Install/repair/switch orchestration for the Advanced settings block. It
// lives here (not in the chrome file) because it is action-lane dispatch —
// inflight bookkeeping plus one executor job — not drawing.

#[cfg(target_os = "windows")]
use crate::app::TaskManApp;
#[cfg(target_os = "windows")]
use eframe::egui;

/// Dispatch the core-service install/remove change on an action lane. The
/// inflight flag disables the buttons until the operation completes.
#[cfg(target_os = "windows")]
pub(crate) fn dispatch_core_service_change(
    app: &mut TaskManApp,
    ctx: &egui::Context,
    install: bool,
) {
    let actions = app.actions.clone();
    let inflight = app.core_service_change_inflight.clone();
    inflight.store(true, std::sync::atomic::Ordering::Release);
    let completion = inflight.clone();
    let dispatched = app.run_action(
        ctx,
        move || {
            i18n::tr(if install {
                K::CoreServiceInstallRequested
            } else {
                K::CoreServiceRemoveRequested
            })
            .to_string()
        },
        move || {
            let outcome = actions.set_core_service_installed(install);
            completion.store(false, std::sync::atomic::Ordering::Release);
            outcome
        },
    );
    if !dispatched {
        inflight.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Dispatch a repair from a foreign session: install this build as the
/// protected generation, then hand the session over to the installed copy —
/// the running session's image path stays rejected until it switches, so a
/// bare repair would leave the user in the same "not the installed client"
/// state they tried to leave.
#[cfg(target_os = "windows")]
pub(crate) fn dispatch_core_service_repair_and_switch(app: &mut TaskManApp, ctx: &egui::Context) {
    let actions = app.actions.clone();
    let inflight = app.core_service_change_inflight.clone();
    inflight.store(true, std::sync::atomic::Ordering::Release);
    let completion = inflight.clone();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let close_ctx = ctx.clone();
    let dispatched = app.run_action(
        ctx,
        || i18n::tr(K::CoreServiceRepairSwitchRequested).to_string(),
        move || {
            actions.set_core_service_installed(true)?;
            if actions.switch_to_installed_gui(&args)? {
                crate::request_programmatic_exit();
                close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            completion.store(false, std::sync::atomic::Ordering::Release);
            Ok(())
        },
    );
    if !dispatched {
        inflight.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Dispatch the handover to the protected installed GUI. Shutting down
/// gracefully lets on_exit flush settings and history while the installed
/// replacement waits on the single-instance handoff.
#[cfg(target_os = "windows")]
pub(crate) fn dispatch_core_service_switch(app: &mut TaskManApp, ctx: &egui::Context) {
    let actions = app.actions.clone();
    let inflight = app.core_service_change_inflight.clone();
    inflight.store(true, std::sync::atomic::Ordering::Release);
    let completion = inflight.clone();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let close_ctx = ctx.clone();
    let dispatched = app.run_action(
        ctx,
        || i18n::tr(K::CoreServiceSwitchRequested).to_string(),
        move || {
            let switched = actions.switch_to_installed_gui(&args)?;
            completion.store(false, std::sync::atomic::Ordering::Release);
            if switched {
                crate::request_programmatic_exit();
                close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Ok(())
        },
    );
    if !dispatched {
        inflight.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Bounded deadline poll — no fixed sleeps (AGENTS.md).
    fn wait_for(cond: impl Fn() -> bool, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn apply_batch_counts_completions_and_succeeds_when_any_target_did() {
        let completed = AtomicUsize::new(0);
        let result = apply_batch(vec![1, 2, 3, 4], &completed, |n| {
            if n % 2 == 0 {
                Ok(())
            } else {
                Err(tm_core::TmError::Unsupported("odd"))
            }
        });
        assert!(result.is_ok());
        assert_eq!(completed.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn apply_batch_fails_with_the_first_error_when_everything_failed() {
        // Regression: the efficiency-mode batch used to return Ok(())
        // unconditionally and toast a success for a batch where every call
        // was refused (targets already exited, access denied, ...).
        let completed = AtomicUsize::new(0);
        let result: Result<(), _> = apply_batch(vec![10, 20], &completed, |pid| {
            Err(tm_core::TmError::ProcessNotFound { pid })
        });
        assert!(matches!(
            result,
            Err(tm_core::TmError::ProcessNotFound { pid: 10 })
        ));
        assert_eq!(completed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn run_toasts_success_and_wakes_on_completion() {
        let executor = ActionExecutor::start().expect("executor starts");
        let toasts: Toasts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake_flag = woken.clone();
        let ok = executor.run(
            toasts.clone(),
            move || wake_flag.store(true, Ordering::Relaxed),
            || "did it".to_string(),
            || Ok(()),
        );
        assert!(ok);
        assert!(
            wait_for(
                || { woken.load(Ordering::Relaxed) && !toasts.lock().unwrap().is_empty() },
                5000
            ),
            "job completed and toasted"
        );
        assert_eq!(toasts.lock().unwrap()[0].msg, "did it");
    }

    #[test]
    fn run_reports_failure_when_the_job_errors() {
        let executor = ActionExecutor::start().expect("executor starts");
        let toasts: Toasts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ok = executor.run(
            toasts.clone(),
            || {},
            || unreachable!("success message must not be built"),
            || Err(tm_core::TmError::ChannelClosed),
        );
        assert!(ok);
        assert!(
            wait_for(|| !toasts.lock().unwrap().is_empty(), 5000),
            "error toast appears"
        );
    }

    #[test]
    fn a_saturated_queue_rejects_new_jobs_with_a_toast() {
        // Both lanes blocked by a running job, both 32-slot buffers full:
        // the next submission must be refused, not queued and not silent.
        let executor = ActionExecutor::start().expect("executor starts");
        let toasts: Toasts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));

        // Two blockers: one per lane (round-robin assigns them to lanes 0,1
        // and the workers must have dequeued them so both buffers are empty).
        for _ in 0..2 {
            let started = started_tx.clone();
            let release = release_rx.clone();
            assert!(executor.run_quiet(
                || {},
                move || {
                    let _ = started.send(());
                    let _ = release.lock().unwrap().recv();
                }
            ));
        }
        drop(started_tx);
        let mut dequeue_confirmed = 0;
        while started_rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            dequeue_confirmed += 1;
        }
        assert_eq!(dequeue_confirmed, 2, "both workers dequeued their blocker");

        // Fill both lane buffers (32 each, round-robin => 64 jobs).
        for _ in 0..64 {
            assert!(executor.run_quiet(|| {}, || {}));
        }
        // 65th: both lanes Full.
        let accepted = executor.run(
            toasts.clone(),
            || {},
            || unreachable!("rejected job must not toast success"),
            || Ok(()),
        );
        assert!(!accepted, "saturated queue refuses the job");
        assert!(
            wait_for(|| !toasts.lock().unwrap().is_empty(), 1000),
            "overload toast is shown"
        );

        // Unblock the workers so the executor can shut down cleanly.
        for _ in 0..2 {
            let _ = release_tx.send(());
        }
    }
}
