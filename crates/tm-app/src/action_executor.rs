//! Single reusable action executor thread (implement.md §11.7).
//!
//! Process/session/service control calls Windows APIs that can block for
//! tens to hundreds of milliseconds — none of them belong on the UI thread.
//! Instead of spawning an ad-hoc thread per action, everything fire-and-
//! forget goes through this bounded worker. Results come back as toasts;
//! completion wakes the UI through the shared repaint hook.

use std::sync::mpsc::Sender;
use tm_core::i18n::{self, K};

pub type Toasts = std::sync::Arc<std::sync::Mutex<Vec<crate::app::Toast>>>;

type Job = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct ActionExecutor {
    tx: Sender<Job>,
}

impl ActionExecutor {
    /// Spawn the single worker thread. Returns None only when thread spawn
    /// is impossible; callers should then run actions inline rather than
    /// silently dropping them.
    pub fn start() -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("tm-actions".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let t0 = std::time::Instant::now();
                    job();
                    let dur = t0.elapsed();
                    if dur > std::time::Duration::from_millis(200) {
                        tracing::debug!(dur_ms = dur.as_millis() as u64, "slow platform action");
                    }
                }
            });
        spawned.ok().map(|_| Self { tx })
    }

    /// Run `job` on the executor; on completion push a localized result
    /// toast into `toasts` and wake the UI.
    pub fn run(
        &self,
        toasts: Toasts,
        wake: impl Fn() + Send + 'static,
        success_msg: impl FnOnce() -> String + Send + 'static,
        job: impl FnOnce() -> Result<(), tm_core::TmError> + Send + 'static,
    ) {
        let send_result = move |res: Result<(), tm_core::TmError>| {
            let msg = match res {
                Ok(()) => success_msg(),
                Err(e) => i18n::trf(K::ErrMsg, &[&e.to_string()]),
            };
            crate::app::toast_from(&toasts, msg);
            wake();
        };
        let _ = self.tx.send(Box::new(move || send_result(job())));
    }

    /// Fire-and-forget job without a result toast (completion still wakes UI).
    pub fn run_quiet(&self, wake: impl Fn() + Send + 'static, job: impl FnOnce() + Send + 'static) {
        let _ = self.tx.send(Box::new(move || {
            job();
            wake();
        }));
    }
}
