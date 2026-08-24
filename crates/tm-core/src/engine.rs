//! Background sampling engine.
//!
//! Owns a dedicated OS thread that periodically asks the platform collector
//! for a fresh [`Snapshot`], stores it as the latest, and answers control
//! commands (interval changes, pause, shutdown) without ever blocking the UI.

use crate::error::{Result, TmError};
use crate::model::Snapshot;
use crossbeam_channel::{Receiver, Sender, bounded, select};
use parking_lot::RwLock;
use std::sync::Arc;
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
    /// Sample immediately and reply with the snapshot.
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
        self.shared.latest.read().clone()
    }

    pub fn state(&self) -> EngineState {
        *self.shared.state.read()
    }

    pub fn interval(&self) -> Duration {
        *self.shared.interval.read()
    }

    pub fn tick_count(&self) -> u64 {
        self.shared
            .tick_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_interval(&self, interval: Duration) {
        *self.shared.interval.write() = interval;
        let _ = self.cmd_tx.send(EngineCmd::SetInterval(interval));
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Pause);
    }

    pub fn resume(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Resume);
    }

    pub fn set_speed_paused(&self, paused: bool) {
        if paused {
            self.pause();
        } else {
            self.resume();
        }
    }

    /// Request an out-of-band refresh; blocks until it completes.
    pub fn sample_now(&self) -> Option<Arc<Snapshot>> {
        let (tx, rx) = bounded(1);
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
    let (cmd_tx, cmd_rx) = bounded::<EngineCmd>(32);
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
    let ctrlc_done = std::sync::atomic::AtomicBool::new(false);
    let _ = ctrlc_done; // reserved for future graceful-exit signaling

    loop {
        let started = Instant::now();

        // Take the sample unless paused.
        if *shared.state.read() == EngineState::Running {
            match collector.sample(started) {
                Ok(snap) => {
                    let dur = started.elapsed();
                    let arc = Arc::new(snap);
                    *shared.latest.write() = Some(arc);
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
                    *shared.state.write() = EngineState::Paused;
                }
                Err(e) => {
                    tracing::error!(error = %e, "sampling failed");
                }
            }
        }

        // Wait out the rest of the interval while staying responsive to cmds.
        let elapsed = started.elapsed();
        let interval = *shared.interval.read();
        let wait = interval
            .saturating_sub(elapsed)
            .max(Duration::from_millis(5));

        select! {
            recv(cmd_rx) -> msg => match msg {
                Ok(EngineCmd::SetInterval(i)) => {
                    tracing::info!(interval_ms = i.as_millis() as u64, "interval changed");
                    *shared.interval.write() = i;
                }
                Ok(EngineCmd::Pause) => {
                    tracing::info!("engine paused");
                    *shared.state.write() = EngineState::Paused;
                }
                Ok(EngineCmd::Resume) => {
                    tracing::info!("engine resumed");
                    *shared.state.write() = EngineState::Running;
                }
                Ok(EngineCmd::SampleNow(reply)) => {
                    match collector.sample(Instant::now()) {
                        Ok(snap) => {
                            let arc = Arc::new(snap);
                            *shared.latest.write() = Some(arc.clone());
                            let _ = reply.send_timeout(arc, Duration::from_secs(2));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "sample_now failed");
                        }
                    }
                }
                Ok(EngineCmd::Shutdown) | Err(_) => break,
            },
            recv(crossbeam_channel::after(wait)) -> _ => {}
        }
    }

    *shared.state.write() = EngineState::Stopped;
    tracing::info!("engine stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_ID: AtomicU32 = AtomicU32::new(1);

    fn tiny_snapshot() -> Snapshot {
        crate::mock::snapshot(NEXT_ID.fetch_add(1, Ordering::Relaxed))
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
}
