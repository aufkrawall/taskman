//! Background sampling engine.
//!
//! Owns a dedicated OS thread that periodically asks the platform collector
//! for a fresh [`Snapshot`], stores it as the latest, and answers control
//! commands (interval changes, pause, shutdown) without ever blocking the UI.
//!
//! Startup architecture (see implement.md §4):
//! * The GUI never constructs a collector on the UI thread. It spawns the
//!   engine with a *lazy factory* (`spawn_lazy`); the factory runs on the
//!   engine thread when `EngineHandle::start()` is received — typically after
//!   the first frame has been presented.
//! * Every publication invokes an optional notifier callback so the UI can
//!   repaint event-driven instead of polling twice per interval.
//! * `EngineCmd::Refresh` forces one sample even while paused, without
//!   changing the paused state afterwards.

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
    /// Update the telemetry demand bitmask. Default: ignore (mock/simple
    /// collectors sample everything cheap they have anyway).
    fn set_demand(&mut self, _demand: crate::demand::TelemetryDemand) {}
}

/// Builds a collector on the engine thread. Construction can be expensive
/// (process enumeration warmup, CPU topology probing, SMBIOS parsing), so it
/// must never run on the UI thread. Factories never fail: platforms degrade
/// gracefully and surface per-feature errors through their APIs at runtime.
pub type CollectorFactory = Box<dyn FnOnce() -> Box<dyn SystemCollector> + Send>;

/// Optional wake-up hook invoked after every publication and state change
/// the UI might care about. The GUI installs a closure calling
/// `egui::Context::request_repaint`; core stays UI-agnostic.
pub type NotifyFn = Arc<dyn Fn() + Send + Sync>;

pub enum EngineCmd {
    /// Construct the collector and begin sampling (lazy start).
    Start,
    /// Update which expensive telemetry the UI currently needs.
    SetDemand(crate::demand::TelemetryDemand),
    SetInterval(Duration),
    Pause,
    Resume,
    /// Force one sample at the next opportunity without blocking the caller.
    /// Works in Running *and* Paused state; the paused/running state itself
    /// is left untouched.
    Refresh,
    /// Sample immediately and reply with the snapshot (tests/selfcheck only).
    SampleNow(Sender<Arc<Snapshot>>),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// Created but the collector factory has not run yet.
    NotStarted,
    /// Factory handed over; first sample in flight.
    Starting,
    Running,
    Paused,
    Stopped,
}

struct Shared {
    latest: RwLock<Option<Arc<Snapshot>>>,
    state: RwLock<EngineState>,
    interval: RwLock<Duration>,
    tick_count: std::sync::atomic::AtomicU64,
    notifier: Option<NotifyFn>,
}

impl Shared {
    /// Wake the UI after a publication/state change (never panics).
    fn notify(&self) {
        if let Some(n) = &self.notifier {
            n();
        }
    }
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

    /// Begin sampling: runs the collector factory on the engine thread.
    /// Idempotent; a no-op once started/stopped.
    pub fn start(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Start);
    }

    /// Tell the collector what telemetry the visible UI currently needs;
    /// expensive providers warm up / sleep accordingly.
    pub fn set_demand(&self, demand: crate::demand::TelemetryDemand) {
        let _ = self.cmd_tx.send(EngineCmd::SetDemand(demand));
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

    /// Request an out-of-band refresh; never blocks the caller. Forces one
    /// sample even when the engine is paused (state stays paused).
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

/// Spawn the engine with an already-built collector and begin sampling
/// immediately (headless tools/tests).
pub fn spawn(
    collector: Box<dyn SystemCollector>,
    interval: Duration,
) -> std::io::Result<(EngineHandle, std::thread::JoinHandle<()>)> {
    spawn_inner(Box::new(move || collector), interval, None, true)
}

/// Like [`spawn`] but with a UI wake callback invoked after publications.
pub fn spawn_with_notifier(
    collector: Box<dyn SystemCollector>,
    interval: Duration,
    notifier: Option<NotifyFn>,
) -> std::io::Result<(EngineHandle, std::thread::JoinHandle<()>)> {
    spawn_inner(Box::new(move || collector), interval, notifier, true)
}

/// Spawn a *parked* engine: no collector exists until [`EngineHandle::start`]
/// is received, at which point `factory` runs **on the engine thread**. This
/// keeps heavy platform construction off both the process-startup path and
/// the UI thread.
pub fn spawn_lazy(
    factory: CollectorFactory,
    interval: Duration,
    notifier: Option<NotifyFn>,
) -> std::io::Result<(EngineHandle, std::thread::JoinHandle<()>)> {
    spawn_inner(factory, interval, notifier, false)
}

fn spawn_inner(
    factory: CollectorFactory,
    interval: Duration,
    notifier: Option<NotifyFn>,
    start_immediately: bool,
) -> std::io::Result<(EngineHandle, std::thread::JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<EngineCmd>();
    let initial_state = if start_immediately {
        EngineState::Starting
    } else {
        EngineState::NotStarted
    };
    let shared = Arc::new(Shared {
        latest: RwLock::new(None),
        state: RwLock::new(initial_state),
        interval: RwLock::new(interval),
        tick_count: Default::default(),
        notifier,
    });

    tracing::info!(interval_ms = interval.as_millis() as u64, "engine spawned");

    let sh = shared.clone();
    let join = std::thread::Builder::new()
        .name("tm-engine".into())
        .spawn(move || run_loop(factory, start_immediately, &sh, cmd_rx))?;

    Ok((EngineHandle { shared, cmd_tx }, join))
}

/// Sample once, publish it, bump the tick counter, notify the UI.
///
/// Cadence ticks, forced refreshes and `SampleNow` all publish into the same
/// slot and all advance `tick_count`: the counter is the visible generation
/// that UI caches invalidate on, so an out-of-band sample that did not bump it
/// would leave those caches serving the previous snapshot.
fn sample_and_publish(
    collector: &mut Box<dyn SystemCollector>,
    shared: &Shared,
) -> Result<Arc<Snapshot>> {
    let started = Instant::now();
    let snap = collector.sample(started)?;
    let dur = started.elapsed();
    let arc = Arc::new(snap);
    *sync::write(&shared.latest) = Some(arc.clone());
    shared
        .tick_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared.notify();
    if dur > Duration::from_millis(250) {
        tracing::warn!(dur_ms = dur.as_millis() as u64, "slow sampling tick");
    } else {
        tracing::trace!(dur_ms = dur.as_millis() as u64, "tick done");
    }
    Ok(arc)
}

fn run_loop(
    factory: CollectorFactory,
    start_immediately: bool,
    shared: &Shared,
    cmd_rx: Receiver<EngineCmd>,
) {
    // The collector only exists after Start; before that the thread parks
    // here without touching any platform APIs.
    //
    // Configuration that arrives while parked MUST be remembered rather than
    // dropped. The UI computes its telemetry demand on its first frame, which
    // is deliberately BEFORE the engine is started, and it only re-sends when
    // the value changes — so a demand discarded here is lost for the rest of
    // the session. That is how per-process network stayed dark on the default
    // start page: `PROCESS_NET` was requested exactly once, into the void.
    let mut pending_demand: Option<crate::demand::TelemetryDemand> = None;
    let mut collector: Box<dyn SystemCollector> = if start_immediately {
        factory()
    } else {
        loop {
            match cmd_rx.recv() {
                Ok(EngineCmd::Start) => break factory(),
                Ok(EngineCmd::SetDemand(d)) => pending_demand = Some(d),
                // The sampling loop reads the interval from shared state, so
                // applying it now is exactly what the running loop would do.
                Ok(EngineCmd::SetInterval(i)) => *sync::write(&shared.interval) = i,
                Ok(EngineCmd::Shutdown) => {
                    *sync::write(&shared.state) = EngineState::Stopped;
                    shared.notify();
                    return;
                }
                // Pause/Resume/Refresh are transitions of a running engine;
                // Start is what brings it up, so they stay meaningless here.
                Ok(_) => {}
                Err(std::sync::mpsc::RecvError) => {
                    *sync::write(&shared.state) = EngineState::Stopped;
                    return;
                }
            }
        }
    };
    if let Some(demand) = pending_demand.take() {
        collector.set_demand(demand);
    }
    *sync::write(&shared.state) = EngineState::Running;
    shared.notify();
    tracing::info!(backend = collector.backend_name(), "engine running");

    // A collector that keeps failing must not do so silently: the UI would
    // just keep showing the last good snapshot with no clue why it froze.
    // Rate-limited so a persistently failing provider cannot flood the log.
    let mut failures: u64 = 0;
    let mut last_report: Option<Instant> = None;
    let report_failure = |error: &TmError, failures: u64, last: &mut Option<Instant>| {
        let due = last.is_none_or(|at| at.elapsed() >= Duration::from_secs(30));
        if due {
            *last = Some(Instant::now());
            tracing::warn!(%error, failures, "sampling tick failed; keeping the previous snapshot");
        }
    };

    loop {
        let started = Instant::now();

        // Take the sample unless paused.
        if *sync::read(&shared.state) == EngineState::Running {
            match sample_and_publish(&mut collector, shared) {
                Ok(_) => {
                    failures = 0;
                    last_report = None;
                }
                Err(e @ TmError::Unsupported(_)) => {
                    // Collector lacks this platform's support entirely; park.
                    tracing::error!(error = %e, "collector unsupported; pausing engine");
                    *sync::write(&shared.state) = EngineState::Paused;
                    shared.notify();
                }
                Err(e) => {
                    failures += 1;
                    report_failure(&e, failures, &mut last_report);
                }
            }
        }

        // Wait out the rest of the interval while staying responsive to cmds.
        let elapsed = started.elapsed();
        let interval = *sync::read(&shared.interval);
        let wait = if *sync::read(&shared.state) == EngineState::Running {
            interval
                .saturating_sub(elapsed)
                .max(Duration::from_millis(5))
        } else {
            // Paused: park until a command arrives (no busy polling).
            Duration::from_secs(3600)
        };

        match cmd_rx.recv_timeout(wait) {
            Ok(EngineCmd::SetInterval(i)) => {
                tracing::info!(interval_ms = i.as_millis() as u64, "interval changed");
                *sync::write(&shared.interval) = i;
            }
            Ok(EngineCmd::SetDemand(d)) => collector.set_demand(d),
            Ok(EngineCmd::Pause) => {
                tracing::info!("engine paused");
                *sync::write(&shared.state) = EngineState::Paused;
                shared.notify();
            }
            Ok(EngineCmd::Resume) => {
                tracing::info!("engine resumed");
                *sync::write(&shared.state) = EngineState::Running;
                shared.notify();
            }
            Ok(EngineCmd::Start) => {} // already running; idempotent
            Ok(EngineCmd::Refresh) => {
                // Force exactly one sample regardless of paused state; the
                // pause/running mode itself must not change (F5 semantics).
                match sample_and_publish(&mut collector, shared) {
                    Ok(_) => {
                        failures = 0;
                        last_report = None;
                    }
                    Err(e @ TmError::Unsupported(_)) => {
                        tracing::error!(error = %e, "refresh failed; pausing engine");
                        *sync::write(&shared.state) = EngineState::Paused;
                        shared.notify();
                    }
                    Err(e) => {
                        failures += 1;
                        report_failure(&e, failures, &mut last_report);
                    }
                }
            }
            Ok(EngineCmd::SampleNow(reply)) => match sample_and_publish(&mut collector, shared) {
                Ok(arc) => {
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
    shared.notify();
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

    /// Regression: telemetry demand sent while the lazy engine is still
    /// parked must reach the collector once it is built. The UI ships its
    /// demand on the first frame — before `start()` — and only re-sends on
    /// change, so dropping it here disables that provider for the session.
    #[test]
    fn demand_sent_before_start_reaches_the_collector() {
        #[derive(Clone, Default)]
        struct Seen(Arc<std::sync::Mutex<Vec<u64>>>);
        struct DemandCollector(Seen);
        impl SystemCollector for DemandCollector {
            fn sample(&mut self, _now: Instant) -> Result<Snapshot> {
                Ok(tiny_snapshot())
            }
            fn backend_name(&self) -> &'static str {
                "demand-mock"
            }
            fn set_demand(&mut self, demand: crate::demand::TelemetryDemand) {
                self.0.0.lock().unwrap().push(demand.bits());
            }
        }

        let seen = Seen::default();
        let factory_seen = seen.clone();
        let (h, join) = spawn_lazy(
            Box::new(move || Box::new(DemandCollector(factory_seen))),
            Duration::from_millis(15),
            None,
        )
        .unwrap();

        // Exactly the UI's order: demand first, engine start second.
        let wanted = crate::demand::TelemetryDemand::core()
            .union(crate::demand::TelemetryDemand::PROCESS_NET);
        h.set_demand(wanted);
        h.start();

        assert!(
            wait_for(|| seen.0.lock().unwrap().contains(&wanted.bits()), 5000),
            "collector never received the pre-start demand: {:?}",
            seen.0.lock().unwrap()
        );
        h.shutdown();
        join.join().unwrap();
    }

    /// An interval set before start must also survive, for the same reason.
    #[test]
    fn interval_sent_before_start_is_honored() {
        let (h, join) = spawn_lazy(
            Box::new(|| Box::new(TinyCollector)),
            Duration::from_secs(600),
            None,
        )
        .unwrap();
        h.set_interval(Duration::from_millis(15));
        h.start();
        assert!(
            wait_for(|| h.tick_count() >= 3, 5000),
            "engine kept the stale 10-minute interval"
        );
        h.shutdown();
        join.join().unwrap();
    }

    #[test]
    fn publishes_latest_and_shuts_down() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_millis(15)).unwrap();
        assert!(wait_for(|| h.tick_count() >= 2, 5000));
        assert!(h.latest().is_some());
        assert_eq!(h.state(), EngineState::Running);

        h.shutdown();
        join.join().unwrap();
        assert_eq!(h.state(), EngineState::Stopped);
    }

    #[test]
    fn lazy_engine_does_not_construct_until_start() {
        static CONSTRUCTED: AtomicU32 = AtomicU32::new(0);
        struct Counting;
        impl SystemCollector for Counting {
            fn sample(&mut self, _now: Instant) -> Result<Snapshot> {
                Ok(tiny_snapshot())
            }
            fn backend_name(&self) -> &'static str {
                "counting"
            }
        }
        let (h, join) = spawn_lazy(
            Box::new(|| {
                CONSTRUCTED.fetch_add(1, Ordering::SeqCst);
                Box::new(Counting) as Box<dyn SystemCollector>
            }),
            Duration::from_millis(10),
            None,
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            CONSTRUCTED.load(Ordering::SeqCst),
            0,
            "factory must stay parked"
        );
        assert_eq!(h.state(), EngineState::NotStarted);
        assert_eq!(h.tick_count(), 0);

        h.start();
        assert!(wait_for(|| h.tick_count() >= 2, 5000));
        assert_eq!(
            CONSTRUCTED.load(Ordering::SeqCst),
            1,
            "factory ran exactly once"
        );
        assert_eq!(h.state(), EngineState::Running);
        h.shutdown();
        join.join().unwrap();
    }

    #[test]
    fn pause_freezes_updates_resume_continues() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_millis(15)).unwrap();
        h.start(); // spawn() auto-starts via factory; explicit start harmless
        assert!(wait_for(|| h.tick_count() >= 2, 5000));
        h.pause();
        // Give the engine a moment to apply pause.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(h.state(), EngineState::Paused);
        let frozen_ticks = h.tick_count();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(h.tick_count(), frozen_ticks);

        h.resume();
        assert!(wait_for(|| h.tick_count() > frozen_ticks, 5000));
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
        assert!(wait_for(|| h.tick_count() > before, 2000));
        h.shutdown();
        join.join().unwrap();
    }

    /// F5 while paused must produce exactly one fresh sample and keep the
    /// engine paused afterwards (implement.md §7.1).
    #[test]
    fn engine_refresh_while_paused() {
        let (h, join) = spawn(Box::new(TinyCollector), Duration::from_millis(20)).unwrap();
        assert!(wait_for(|| h.tick_count() >= 3, 5000));

        h.pause();
        assert!(wait_for(|| h.state() == EngineState::Paused, 2000));
        let before = h.tick_count();
        let ts_before = h.latest().map(|s| s.timestamp_ms).unwrap_or(0);

        h.request_refresh();
        assert!(
            wait_for(
                || h.tick_count() > before && h.latest().map(|s| s.timestamp_ms) != Some(ts_before),
                2000
            ),
            "refresh must sample while paused"
        );
        // Still paused, and no further ticks arrive.
        assert_eq!(h.state(), EngineState::Paused);
        let after_refresh = h.tick_count();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            h.tick_count(),
            after_refresh,
            "paused engine must not keep ticking"
        );

        h.shutdown();
        join.join().unwrap();
    }

    /// Every publication fires the installed notifier (event-driven UI).
    #[test]
    fn publication_notifies_ui_wake() {
        static WAKES: AtomicU32 = AtomicU32::new(0);
        let notifier: NotifyFn = Arc::new(|| {
            WAKES.fetch_add(1, Ordering::SeqCst);
        });
        let (h, join) = spawn_with_notifier(
            Box::new(TinyCollector),
            Duration::from_millis(15),
            Some(notifier),
        )
        .unwrap();
        assert!(wait_for(|| WAKES.load(Ordering::SeqCst) >= 2, 5000));
        h.shutdown();
        join.join().unwrap();
    }
}
