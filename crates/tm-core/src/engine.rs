//! Background sampling engine.
//!
//! Owns a dedicated OS thread that periodically asks the platform collector
//! for a fresh [`Snapshot`], stores it as the latest, and answers control
//! commands (interval changes, pause, shutdown) without ever blocking the UI.
//!
//! Uses `std::sync::mpsc` + `recv_timeout` (no third-party channel crate):
//! the loop sleeps for the remainder of the interval but wakes instantly when
//! a command arrives, which is exactly the semantics we need.

use crate::error::{Result, TmError};
use crate::model::Snapshot;
use crate::sync;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Platform-specific sampler. Implementations keep internal state between
/// ticks (previous counters) so they can compute rates/deltas.
pub trait SystemCollector: Send {
    fn sample(&mut self, now: Instant) -> Result<Snapshot>;
    /// Human-readable name of the backend (for logs/selfcheck).
    fn backend_name(&self) -> &'static str;
}

pub enum EngineCmd {
    SetInterval(Duration),
    Pause,
    Resume,
    /// Sample at the next opportunity without blocking the caller (F5).
    Refresh,
    /// Sample immediately and reply with the snapshot (tests/selfcheck only).
    SampleNow(Sender<Arc<Snapshot>>),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Running,
    Paused,
    Stopped,
}

struct Shared {
    latest: RwLock<Option<Arc<Snapshot>>>,
    state: RwLock<EngineState>,
    interval: RwLock<Duration>,
    tick_count: std::sync::atomic::AtomicU64,
}

/// Cloneable handle to the running engine.
#[derive(Clone)]
pub struct EngineHandle {
    shared: Arc<Shared>,
    cmd_tx: Sender<EngineCmd>,
}

impl EngineHandle {
    pub fn latest(&self) -> Option<Arc<Snapshot>> {
        sync::read(&self.shared.latest).clone()
    }

    pub fn state(&self) -> EngineState {
        *sync::read(&self.shared.state)
    }

    pub fn interval(&self) -> Duration {
        *sync::read(&self.shared.interval)
    }

    pub fn tick_count(&self) -> u64 {
        self.shared
            .tick_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_interval(&self, interval: Duration) {
        *sync::write(&self.shared.interval) = interval;
        let _ = self.cmd_tx.send(EngineCmd::SetInterval(interval));
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Pause);
    }

    pub fn resume(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Resume);
    }

    /// Request an out-of-band refresh; never blocks the caller.
    pub fn request_refresh(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Refresh);
    }

    pub fn set_speed_paused(&self, paused: bool) {
        if paused {
            self.pause();
        } else {
            self.resume();
        }
    }

    /// Request an out-of-band refresh and block until it completes.
    /// Only for tests / headless tools — the GUI uses [`request_refresh`].
    pub fn sample_now(&self) -> Option<Arc<Snapshot>> {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.cmd_tx.send(EngineCmd::SampleNow(tx)).is_err() {
            return None;
        }
        rx.recv_timeout(Duration::from_secs(10)).ok()
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Shutdown);
    }
}

/// Spawn the engine on a background thread. The returned guard shuts the
/// engine down when dropped (or earlier via [`EngineHandle::shutdown`]).
pub fn spawn(
    mut collector: Box<dyn SystemCollector>,
    interval: Duration,
) -> std::io::Result<(EngineHandle, std::thread::JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<EngineCmd>();
    let shared = Arc::new(Shared {
        latest: RwLock::new(None),
        state: RwLock::new(EngineState::Running),
        interval: RwLock::new(interval),
        tick_count: Default::default(),
    });

    tracing::info!(
        backend = collector.backend_name(),
        interval_ms = interval.as_millis() as u64,
        "engine starting"
    );

    let sh = shared.clone();
    let join = std::thread::Builder::new()
        .name("tm-engine".into())
        .spawn(move || run_loop(&mut collector, &sh, cmd_rx))?;

    Ok((EngineHandle { shared, cmd_tx }, join))
}

fn run_loop(
    collector: &mut Box<dyn SystemCollector>,
    shared: &Shared,
    cmd_rx: Receiver<EngineCmd>,
) {
    loop {
        let started = Instant::now();

        // Take the sample unless paused.
        if *sync::read(&shared.state) == EngineState::Running {
            match collector.sample(started) {
                Ok(snap) => {
                    let dur = started.elapsed();
                    let arc = Arc::new(snap);
                    *sync::write(&shared.latest) = Some(arc);
                    shared
                        .tick_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if dur > Duration::from_millis(250) {
                        tracing::warn!(dur_ms = dur.as_millis() as u64, "slow sampling tick");
                    } else {
                        tracing::trace!(dur_ms = dur.as_millis() as u64, "tick done");
                    }
                }
                Err(e @ TmError::Unsupported(_)) => {
                    // Collector lacks this platform's support entirely; park.
                    tracing::error!(error = %e, "collector unsupported; pausing engine");
                    *sync::write(&shared.state) = EngineState::Paused;
                }
                Err(e) => {
                    tracing::error!(error = %e, "sampling failed");
                }
            }
        }

        // Wait out the rest of the interval while staying responsive to cmds.
        let elapsed = started.elapsed();
        let interval = *sync::read(&shared.interval);
        let wait = interval
            .saturating_sub(elapsed)
            .max(Duration::from_millis(5));

        match cmd_rx.recv_timeout(wait) {
            Ok(EngineCmd::SetInterval(i)) => {
                tracing::info!(interval_ms = i.as_millis() as u64, "interval changed");
                *sync::write(&shared.interval) = i;
            }
            Ok(EngineCmd::Pause) => {
                tracing::info!("engine paused");
                *sync::write(&shared.state) = EngineState::Paused;
            }
            Ok(EngineCmd::Resume) => {
                tracing::info!("engine resumed");
                *sync::write(&shared.state) = EngineState::Running;
            }
            Ok(EngineCmd::Refresh) => {
                // Loop immediately; the top of the loop takes a fresh sample
                // (unless paused). No reply, no UI blocking.
                continue;
            }
            Ok(EngineCmd::SampleNow(reply)) => match collector.sample(Instant::now()) {
                Ok(snap) => {
                    let arc = Arc::new(snap);
                    *sync::write(&shared.latest) = Some(arc.clone());
                    let _ = reply.send(arc);
                }
                Err(e) => {
                    tracing::error!(error = %e, "sample_now failed");
                }
            },
            Ok(EngineCmd::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    *sync::write(&shared.state) = EngineState::Stopped;
    tracing::info!("engine stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_ID: AtomicU32 = AtomicU32::new(1);

    fn tiny_snapshot() -> Snapshot {
        mock::snapshot(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    struct TinyCollector;
    impl SystemCollector for TinyCollector {
        fn sample(&mut self, _now: Instant) -> Result<Snapshot> {
            Ok(tiny_snapshot())
        }
        fn backend_name(&self) -> &'static str {
            "tiny-mock"
        }
    }

    #[test]
    fn publishes_latest_and_shuts_down() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_millis(15)).unwrap();
        // Wait until at least two ticks happened.
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.tick_count() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.tick_count() >= 2);
        assert!(h.latest().is_some());
        assert_eq!(h.state(), EngineState::Running);

        h.shutdown();
        join.join().unwrap();
        assert_eq!(h.state(), EngineState::Stopped);
    }

    #[test]
    fn pause_freezes_updates_resume_continues() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_millis(15)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.tick_count() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        h.pause();
        // Give the engine a moment to apply pause.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(h.state(), EngineState::Paused);
        let frozen_ticks = h.tick_count();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(h.tick_count(), frozen_ticks);

        h.resume();
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.tick_count() <= frozen_ticks && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.tick_count() > frozen_ticks);
        h.shutdown();
        join.join().unwrap();
    }

    #[test]
    fn sample_now_returns_fresh_snapshot() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_millis(500)).unwrap();
        let snap = h.sample_now().expect("sample_now should answer");
        assert!(snap.timestamp_ms > 0);
        h.shutdown();
        join.join().unwrap();
    }

    #[test]
    fn refresh_does_not_block_and_produces_a_tick() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_secs(3600)).unwrap();
        let before = h.tick_count();
        h.request_refresh(); // must not block even with a 1 h interval
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.tick_count() <= before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.tick_count() > before);
        h.shutdown();
        join.join().unwrap();
    }
}
