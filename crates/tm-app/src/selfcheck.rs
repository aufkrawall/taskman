//! Headless self-check: exercises the full sampling stack without a GUI.
//! Prints one JSON summary line and exits 0 on success, 1 on failure.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use tm_platform::actions::{FileIoAccounting, FileIoAccountingState, ProcessFileIoSnapshot};

/// Ticks to wait for before summarizing.
///
/// The PDH groups (GPU engines, GPU memory, disk rates, CPU speed) are
/// two-sample counters: the first collection only establishes a baseline, and
/// the group is opened lazily on the tick its demand first arrives. Four ticks
/// leaves room for open + baseline + a real reading, so the summary reports
/// what those providers actually measured instead of their warm-up state.
const REQUIRED_TICKS: u64 = 4;

/// How long the self-check waits for the per-file trace to deliver a record of
/// its own I/O.
///
/// A session delivers nothing that happened before it started, and the records
/// reach the consumer asynchronously, so a sample taken a few milliseconds
/// after the start can only say `undelivered` — the state means "no transfer of
/// this process yet", not "no trace". The check gives the delivery three
/// seconds and reports the wait beside the state, so a verdict that was taken
/// too early is recognisable instead of final.
const FILE_IO_DELIVERY_BUDGET: Duration = Duration::from_secs(3);

/// Interval between two samples of the trace while waiting for that delivery.
const FILE_IO_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bytes the check writes and reads back to give the trace a request of its
/// own: the trace issues one record per request, so one flushed write and one
/// read are enough for a state that requires a decoded transfer.
const OWN_IO_BYTES: usize = 64 * 1024;

/// Byte count in GB, rounded to the one decimal the summary has always shown.
fn gb(bytes: u64) -> f64 {
    (bytes as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0
}

/// Used and total memory in GB, or a pair of `None`s while the platform reports
/// no memory at all.
///
/// A headless run reads this line to answer "did the memory provider work?".
/// Publishing `0.0` for an unreported total made an unmeasured machine
/// indistinguishable from an idle one; the neighbouring CPU and disk figures
/// already answer `null` for exactly this reason, and the UI renders the same
/// state as "\u{2014}".
fn memory_gb(mem: &tm_core::model::MemoryInfo) -> (Option<f64>, Option<f64>) {
    if mem.total_bytes == 0 {
        return (None, None);
    }
    (Some(gb(mem.used_bytes)), Some(gb(mem.total_bytes)))
}

/// Wire name of one per-file accounting state.
fn file_io_state_name(state: FileIoAccountingState) -> &'static str {
    match state {
        FileIoAccountingState::Unavailable => "unavailable",
        FileIoAccountingState::Undelivered => "undelivered",
        FileIoAccountingState::Undecodable => "undecodable",
        FileIoAccountingState::KernelLoggerInUse => "kernel_logger_in_use",
        FileIoAccountingState::Measured => "measured",
    }
}

/// `since` in whole milliseconds, saturating instead of wrapping.
fn elapsed_ms(since: Instant) -> u64 {
    since.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// One sample of the per-file trace, and how long the check waited for it.
#[derive(Debug)]
struct FileIoWait {
    /// The sample the wait settled on: the first one that was not
    /// `undelivered`, the failure that ended the wait, or the last one before
    /// the deadline.
    answer: Result<ProcessFileIoSnapshot, String>,
    /// The wait that produced `answer`, measured rather than assumed. A sample
    /// taken four milliseconds after the session started could not have seen
    /// an event, and this is the number that says which kind of sample it was.
    waited_ms: u64,
}

/// The `file_io` facts of one run: which process was sampled, what waiting for
/// it settled on, and whether the check could issue file I/O of its own.
#[derive(Debug)]
struct FileIoProbe {
    /// The sampled process, or `None` when no question was asked at all.
    pid: Option<u32>,
    waited: FileIoWait,
    /// Why this process's own file I/O did not happen. Non-null means an
    /// `undelivered` answer is a statement about this check rather than about
    /// the trace: it had nothing of its own to deliver.
    own_io_error: Option<String>,
}

/// The accounting an answer carries, or `None` when the question itself failed.
fn answered_accounting(wait: &FileIoWait) -> Option<&FileIoAccounting> {
    wait.answer
        .as_ref()
        .ok()
        .map(|snapshot| &snapshot.accounting)
}

/// The `file_io` block of the summary: whether per-file accounting has a live
/// source, how long the check waited for it to speak, and how much that source
/// has lost or capped.
///
/// The I/O page reports the same thing as one line above the file table. A
/// headless run has no table, so without this block a trace that starts and
/// delivers nothing is indistinguishable from one that measured nothing —
/// which is the failure the page's state line exists for.
///
/// `state` is `null` when the question could not be asked at all (no verified
/// process identity, or a platform refusing the request), and that stays
/// distinct from `"unavailable"` — no source — exactly as the page keeps them
/// apart. A counter that could not be queried is `null`, never a fabricated
/// zero.
///
/// `waited_ms` is what qualifies the state: `undelivered` after three seconds
/// means the trace had its chance and delivered nothing, while the same word
/// after four milliseconds means only that the check did not look again. And
/// `decoded_bytes` reports what the trace decoded for this process, read plus
/// write, which exists only for a `measured` sample — an unmeasured one keeps
/// `null` rather than a zero that reads as "measured nothing".
fn file_io_summary(probe: &FileIoProbe) -> serde_json::Value {
    let accounting = answered_accounting(&probe.waited);
    serde_json::json!({
        "pid": probe.pid,
        "state": accounting.map(|accounting| file_io_state_name(accounting.state)),
        "since_ms": accounting.map(|accounting| accounting.since_ms),
        "waited_ms": probe.waited.waited_ms,
        "decoded_bytes": accounting
            .filter(|accounting| accounting.state == FileIoAccountingState::Measured)
            .map(|accounting| accounting.read_bytes.saturating_add(accounting.write_bytes)),
        "events_lost": accounting.and_then(|accounting| accounting.events_lost),
        "unnamed_creates": accounting.map(|accounting| accounting.unnamed_creates),
        "dropped_paths": accounting.map(|accounting| accounting.dropped_paths),
        "cleared_file_objects": accounting.map(|accounting| accounting.cleared_file_objects),
        "own_io_error": probe.own_io_error,
        "error": probe.waited.answer.as_ref().err(),
    })
}

/// Write, flush, read back and delete a temporary file of this process's own,
/// so a running per-file trace has something of its own to deliver.
///
/// [`FileIoAccountingState::Measured`] requires one read or write record
/// decoded for THIS process, so a check that only sampled a freshly started
/// session could never see one: nothing had been issued yet. The file is
/// created, written, flushed, read back and removed here, in the data directory
/// rather than the system temp directory, because `TASKMAN_DATA_DIR` is the
/// isolation knob a diagnostic run sets.
///
/// Returns the bytes written and read back. The file is removed whatever
/// happens, including a failed read-back, so a rerun cannot mistake the
/// leftovers of this one for its own window.
fn exercise_own_file_io_in(dir: &Path) -> Result<u64, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let path = dir.join(format!(
        "selfcheck-file-io-{}-{nanos}.tmp",
        std::process::id()
    ));
    let payload = vec![0x5a_u8; OWN_IO_BYTES];
    let result = write_and_read_back(&path, &payload);
    let _ = std::fs::remove_file(&path);
    result
        .map(|()| (payload.len() as u64).saturating_mul(2))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// One create/write/flush/read cycle on `path`, verified against `payload`.
fn write_and_read_back(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(payload)?;
    // Without the flush the bytes may still sit in a cache buffer rather than
    // in a completed request, which is what the trace records.
    file.sync_all()?;
    drop(file);
    let mut read_back = Vec::with_capacity(payload.len());
    std::fs::File::open(path)?.read_to_end(&mut read_back)?;
    if read_back != payload {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "read back a different payload",
        ));
    }
    Ok(())
}

/// Sample the per-file trace until it reports something other than
/// `undelivered`, or `budget` runs out.
///
/// `undelivered` is the one state a later sample can still change: it says the
/// session delivered no transfer of this process yet, and delivery is
/// asynchronous. Every other state is an answer — `unavailable` means there is
/// no session to wait for, `undecodable` means records arrived and the layout
/// did not match, `measured` means a transfer was decoded — and so is a failed
/// request, which no amount of waiting makes askable.
///
/// The loop polls against a deadline instead of sleeping for it: it samples at
/// once, then at most [`FILE_IO_POLL_INTERVAL`] apart, and returns the sample
/// that is at or past the deadline rather than assuming one arrived.
fn await_file_io_delivery(
    budget: Duration,
    mut sample: impl FnMut() -> Result<ProcessFileIoSnapshot, String>,
) -> FileIoWait {
    let started = Instant::now();
    loop {
        let answer = sample();
        let waited_ms = elapsed_ms(started);
        let undelivered = matches!(
            &answer,
            Ok(snapshot) if snapshot.accounting.state == FileIoAccountingState::Undelivered
        );
        if !undelivered {
            return FileIoWait { answer, waited_ms };
        }
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return FileIoWait { answer, waited_ms };
        }
        std::thread::sleep(FILE_IO_POLL_INTERVAL.min(remaining));
    }
}

/// Measure the per-file accounting of this process, giving the trace I/O of its
/// own to deliver and time to deliver it.
///
/// The GUI asks the same question about the process its dialog shows. A
/// headless run can only be sure of its own identity, and the accounting state
/// is a fact about the file trace session rather than about one target, so this
/// process is the sampled one. `--mock` has no trace at all and reports no
/// answer instead of an invented state.
///
/// The session starts on the first sample and delivers nothing from before it,
/// so that sample is taken on its own: the check then issues its own file I/O
/// ([`exercise_own_file_io_in`]) and waits, bounded, for the trace to report it.
/// Sampling once and printing the state was the defect — four milliseconds into
/// a session, no event could have arrived.
fn probe_file_io(mock: bool, snapshot: &tm_core::model::Snapshot) -> FileIoProbe {
    let unasked = |pid: Option<u32>, error: &str| FileIoProbe {
        pid,
        waited: FileIoWait {
            answer: Err(error.to_string()),
            waited_ms: 0,
        },
        own_io_error: None,
    };
    if mock {
        return unasked(None, "mock collector: no per-file trace");
    }
    let pid = std::process::id();
    let Some(start_epoch_s) = snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.creation_identity())
        .filter(|start| *start > 0)
    else {
        return unasked(Some(pid), "own process creation time is unavailable");
    };
    let actions = tm_platform::create_actions();
    let sample = || {
        actions
            .list_process_file_io(pid, Some(start_epoch_s))
            .map_err(|error| error.to_string())
    };
    let started = sample();
    let own_io_error = match &started {
        // Only a session has anything to deliver; with no source there is no
        // trace to feed, and touching the disk for it would measure nothing.
        Ok(ready) if ready.accounting.state != FileIoAccountingState::Unavailable => {
            exercise_own_file_io_in(&tm_core::settings::taskman_data_dir()).err()
        }
        _ => None,
    };
    let waited = match started {
        Ok(ready) if ready.accounting.state == FileIoAccountingState::Undelivered => {
            await_file_io_delivery(FILE_IO_DELIVERY_BUDGET, sample)
        }
        answer => FileIoWait {
            answer,
            waited_ms: 0,
        },
    };
    FileIoProbe {
        pid: Some(pid),
        waited,
        own_io_error,
    }
}

pub fn run(mock: bool) -> i32 {
    let t0 = Instant::now();
    let interval = Duration::from_millis(250);

    let mut collector: Box<dyn tm_core::engine::SystemCollector> = if mock {
        Box::new(tm_core::mock::MockCollector::new())
    } else {
        tm_platform::create_collector()
    };
    // A diagnostic run must exercise every provider, not just the ones the
    // default Processes page keeps warm. Sampling at `core()` left the GPU,
    // disk-rate, per-process-network and CPU-speed paths completely untouched
    // and printed `"gpus":[]` on machines that have a GPU — a smoke test that
    // silently skips the most fragile collectors is not a smoke test.
    collector.set_demand(tm_core::demand::TelemetryDemand::all());

    let (handle, join) = match tm_core::engine::spawn(collector, interval) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(r#"{{"ok":false,"error":"engine spawn failed: {e}"}}"#);
            return 1;
        }
    };

    // Wait for enough ticks that the two-sample providers have real readings.
    let deadline = Instant::now() + Duration::from_secs(15);
    while handle.tick_count() < REQUIRED_TICKS && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }

    let Some(snap) = handle.latest() else {
        eprintln!(r#"{{"ok":false,"error":"no snapshot produced in 15s"}}"#);
        handle.shutdown();
        let _ = join.join();
        return 1;
    };

    // Unknown telemetry is reported as null, never as a fabricated zero — the
    // same invariant the UI renders as "—".
    let session_capability = if mock {
        false
    } else {
        tm_platform::create_actions().capabilities().users_sessions
    };
    let (memory_used_gb, memory_total_gb) = memory_gb(&snap.memory);
    let file_io = probe_file_io(mock, &snap);
    let summary = serde_json::json!({
        "ok": true,
        "backend": if mock { "mock".to_string() } else { "platform".to_string() },
        "startup_to_snapshot_ms": t0.elapsed().as_millis() as u64,
        "ticks": handle.tick_count(),
        "sample_duration_ms": snap.sample_duration_ms,
        "cpu": {
            "util_pct": snap
                .cpu
                .utilization_pct_available
                .then_some(snap.cpu.utilization_pct),
            "logical": snap.cpu.logical_count,
            "brand": snap.cpu.brand,
            "kernel_pct": snap.cpu.kernel_pct,
            "per_core_count": snap.cpu.per_core_pct.len(),
            // Measured current frequency; only the CPU_SPEED provider fills it.
            "freq_mhz": snap.cpu.freq_mhz,
        },
        "memory_used_gb": memory_used_gb,
        "memory_total_gb": memory_total_gb,
        "processes": snap.processes.len(),
        "disks": snap.disks.len(),
        // The busiest disk's active time — the machine total both disk
        // columns are read against. `null` means the PhysicalDisk counters
        // reported nothing, which used to be indistinguishable from an idle
        // machine because it was published as a flat 0 %.
        "disk_busiest_active_pct": snap
            .disks
            .iter()
            .filter_map(|d| d.active_pct)
            .reduce(f32::max),
        "networks": snap.networks.len(),
        "network_metadata": {
            "ipv4": snap.networks.iter().filter(|n| n.ipv4.is_some()).count(),
            "ipv6": snap.networks.iter().filter(|n| n.ipv6.is_some()).count(),
            "described": snap.networks.iter().filter(|n| !n.desc.is_empty()).count(),
        },
        "gpus": snap.gpus.iter().map(|g| serde_json::json!({
            "name": g.name.clone(),
            "util_available": g.util_pct_available,
            "dedicated_available": g.dedicated_mem_available,
            "shared_available": g.shared_mem_available,
            "engine_count": g.engines.len(),
        })).collect::<Vec<_>>(),
        "users_sessions_capable": session_capability,
        // How many rows carry a measured CPU share, and how many real
        // processes show the honest dash. A healthy tick measures every
        // process except one it has only seen once, so `readings: 0` with
        // `cpu.utilization_pct_available` true is the regression signature:
        // the accountant stopped reporting while the header stayed honest.
        "process_cpu_readings": snap
            .processes
            .iter()
            .filter(|p| p.cpu_pct.is_some())
            .count(),
        "process_cpu_unknown": snap
            .processes
            .iter()
            .filter(|p| !p.synthetic && p.cpu_pct.is_none())
            .count(),
        // How many processes carry a per-process network reading. `0` with a
        // running trace is a real answer; the provider is simply unavailable
        // without administrator rights.
        "process_net_readings": snap
            .processes
            .iter()
            .filter(|p| p.net_recv_bps.is_some())
            .count(),
        // Same idea for the disk trace: how many processes carry a measured
        // disk active time, and what the busiest one is. That value is a share
        // of `disk_busiest_active_pct`, so it can never exceed it — a larger
        // one would mean the scaling regressed. A run with readings but a flat
        // zero everywhere means the events arrived and none of them could be
        // attributed.
        "process_disk_readings": snap
            .processes
            .iter()
            .filter(|p| p.disk_active_pct.is_some())
            .count(),
        "process_disk_busiest_pct": snap
            .processes
            .iter()
            .filter_map(|p| p.disk_active_pct)
            .fold(f32::NAN, f32::max),
        "process_disk_unknown": snap
            .processes
            .iter()
            .filter(|p| !p.synthetic && p.disk_active_pct.is_none())
            .count(),
        // How many processes report a non-zero Disk column. sysinfo's
        // handle-based counters left this at roughly half the list.
        "process_disk_bytes_readings": snap
            .processes
            .iter()
            .filter(|p| !p.synthetic && p.disk_read_total + p.disk_write_total > 0)
            .count(),
        "process_io_ops_readings": snap
            .processes
            .iter()
            .filter(|p| p.io_ops_per_s.is_some())
            .count(),
        // Whether per-file byte accounting has a live source for the sampled
        // process, and what that source delivered and lost: the headless answer
        // to "is the file trace alive", which the I/O page only answers in a
        // GUI. Read it with `waited_ms`: the same state means different things
        // four milliseconds and three seconds into a session.
        "file_io": file_io_summary(&file_io),
        "uptime_s": snap.system.uptime_s,
    });
    println!("{summary}");

    handle.shutdown();
    let _ = join.join();

    // Basic sanity gates.
    if !mock
        && (snap.processes.len() < 5
            || snap.cpu.logical_count == 0
            || !snap.cpu.utilization_pct_available)
    {
        eprintln!("selfcheck: implausible snapshot; failing");
        return 1;
    }
    0
}

/// Exercise the installed GUI → LocalSystem broker → system-profile ETW path.
///
/// This intentionally targets the GUI's own process: the broker must accept
/// the installed client image, start a profile trace as LocalSystem, decode
/// sampled instruction pointers, and attribute at least one page to a module.
pub fn run_code_samples() -> i32 {
    let mut collector = tm_platform::create_collector();
    let Ok(snapshot) = collector.sample(Instant::now()) else {
        tracing::error!("code-sample selfcheck: process snapshot failed");
        eprintln!(
            "{}",
            serde_json::json!({"ok": false, "error": "process snapshot failed"})
        );
        return 1;
    };
    let pid = std::process::id();
    let Some(start_epoch_s) = snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.creation_identity())
        .filter(|start| *start > 0)
    else {
        tracing::error!(
            pid,
            "code-sample selfcheck: own process identity unavailable"
        );
        eprintln!(
            "{}",
            serde_json::json!({"ok": false, "error": "own process identity is unavailable"})
        );
        return 1;
    };

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_worker = stop.clone();
    let worker = std::thread::Builder::new()
        .name("tm-code-sample-selfcheck".into())
        .spawn(move || {
            let mut value = 0u64;
            while !stop_worker.load(std::sync::atomic::Ordering::Relaxed) {
                for _ in 0..100_000 {
                    value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                std::hint::black_box(value);
            }
        });
    let Ok(worker) = worker else {
        tracing::error!("code-sample selfcheck: workload thread failed");
        eprintln!(
            "{}",
            serde_json::json!({"ok": false, "error": "workload thread failed"})
        );
        return 1;
    };

    let actions = tm_platform::create_actions();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut best = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match actions.process_module_code_samples(pid, Some(start_epoch_s)) {
            Ok(Some(window)) => {
                let complete = window.events_decoded > 0
                    && window.target_samples > 0
                    && window.attributed_samples > 0;
                best = Some(window);
                if complete {
                    break;
                }
            }
            Ok(None) => {
                tracing::error!("code-sample selfcheck: sampled-profile session unavailable");
                last_error = Some("sampled-profile session unavailable".to_string());
                break;
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = worker.join();

    let Some(window) = best else {
        tracing::error!(
            error = last_error.as_deref().unwrap_or("no sample window returned"),
            "code-sample selfcheck failed"
        );
        let summary = serde_json::json!({
            "ok": false,
            "error": last_error.unwrap_or_else(|| "no sample window returned".to_string()),
        });
        println!("{summary}");
        return 1;
    };
    let ok =
        window.events_decoded > 0 && window.target_samples > 0 && window.attributed_samples > 0;
    let summary = serde_json::json!({
        "ok": ok,
        "pid": pid,
        "window_ms": window.window_ms,
        "events_seen": window.events_seen,
        "events_decoded": window.events_decoded,
        "events_lost": window.events_lost,
        "target_samples": window.target_samples,
        "attributed_samples": window.attributed_samples,
        "tracked_threads": window.tracked_threads,
        "modules": window.modules.len(),
        "error": if ok { None } else { last_error },
    });
    if ok {
        tracing::info!(
            pid,
            window_ms = window.window_ms,
            events_decoded = window.events_decoded,
            target_samples = window.target_samples,
            attributed_samples = window.attributed_samples,
            "code-sample selfcheck passed"
        );
    } else {
        tracing::error!(
            pid,
            window_ms = window.window_ms,
            events_decoded = window.events_decoded,
            target_samples = window.target_samples,
            attributed_samples = window.attributed_samples,
            "code-sample selfcheck produced no attributed sample"
        );
    }
    println!("{summary}");
    if ok { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: an unreported memory figure was published as `0.0`, so a
    /// headless run could not tell "no memory telemetry" from "idle machine".
    /// The CPU and disk figures in the same summary already answer `null`.
    #[test]
    fn unreported_memory_publishes_null_not_zero() {
        let unmeasured = tm_core::model::MemoryInfo::default();
        assert_eq!(memory_gb(&unmeasured), (None, None));

        let measured = tm_core::model::MemoryInfo {
            total_bytes: 16 * 1024 * 1024 * 1024,
            used_bytes: 3 * 1024 * 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(memory_gb(&measured), (Some(3.0), Some(16.0)));
    }

    /// One answer of the sampled process in `state`.
    fn snapshot_in(state: FileIoAccountingState) -> ProcessFileIoSnapshot {
        ProcessFileIoSnapshot {
            files: Vec::new(),
            accounting: FileIoAccounting {
                state,
                ..Default::default()
            },
        }
    }

    /// A probe whose wait settled on `answer` after `waited_ms`.
    fn probe_with(answer: Result<ProcessFileIoSnapshot, String>, waited_ms: u64) -> FileIoProbe {
        FileIoProbe {
            pid: Some(4242),
            waited: FileIoWait { answer, waited_ms },
            own_io_error: None,
        }
    }

    /// A path in the system temp directory that no other run shares.
    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        std::env::temp_dir().join(format!("tm-selfcheck-{name}-{nanos}"))
    }

    /// The summary states the per-file accounting of the sampled process, and
    /// every state keeps its own name instead of collapsing into "unavailable".
    ///
    /// Regression: the summary carried no per-file accounting at all, so a run
    /// without a GUI could not tell a trace that started and delivered nothing
    /// from one that measured nothing — the same blindness the I/O page's state
    /// line fixes in the dialog.
    #[test]
    fn the_summary_states_the_per_file_accounting() {
        for (state, name) in [
            (FileIoAccountingState::Unavailable, "unavailable"),
            (FileIoAccountingState::Undelivered, "undelivered"),
            (FileIoAccountingState::Undecodable, "undecodable"),
            (
                FileIoAccountingState::KernelLoggerInUse,
                "kernel_logger_in_use",
            ),
            (FileIoAccountingState::Measured, "measured"),
        ] {
            let summary = file_io_summary(&probe_with(Ok(snapshot_in(state)), 0));
            assert_eq!(summary["pid"], serde_json::json!(4242));
            assert_eq!(summary["state"], serde_json::json!(name), "{state:?}");
        }

        // The counters a lossy or capped session is recognised by, plus the
        // span they cover and the wait that produced them.
        let accounting = FileIoAccounting {
            state: FileIoAccountingState::Undelivered,
            since_ms: 1500,
            events_lost: Some(12),
            unnamed_creates: 17,
            dropped_paths: 3,
            cleared_file_objects: 5,
            ..Default::default()
        };
        let summary = file_io_summary(&probe_with(
            Ok(ProcessFileIoSnapshot {
                files: Vec::new(),
                accounting,
            }),
            3000,
        ));
        assert_eq!(summary["since_ms"], serde_json::json!(1500));
        assert_eq!(summary["waited_ms"], serde_json::json!(3000));
        assert_eq!(summary["events_lost"], serde_json::json!(12));
        assert_eq!(summary["unnamed_creates"], serde_json::json!(17));
        assert_eq!(summary["dropped_paths"], serde_json::json!(3));
        assert_eq!(summary["cleared_file_objects"], serde_json::json!(5));
        assert_eq!(summary["error"], serde_json::Value::Null);

        // The block goes into the summary under `file_io`; its own field set is
        // pinned here, so dropping or renaming one fails rather than silently
        // shrinking what a headless run can read.
        let mut fields: Vec<&str> = summary
            .as_object()
            .expect("the file_io block is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "cleared_file_objects",
                "decoded_bytes",
                "dropped_paths",
                "error",
                "events_lost",
                "own_io_error",
                "pid",
                "since_ms",
                "state",
                "unnamed_creates",
                "waited_ms",
            ]
        );
    }

    /// A trace that delivers on a later sample is reported as measured, and the
    /// wait that found it is visible in `waited_ms`.
    ///
    /// Regression: the check sampled once, four milliseconds into the session,
    /// when no event could have arrived — so the verdict was `undelivered` by
    /// construction and said nothing about the trace.
    #[test]
    fn a_later_sample_that_delivers_is_measured_after_a_wait() {
        let mut samples = 0;
        let waited = await_file_io_delivery(FILE_IO_DELIVERY_BUDGET, || {
            samples += 1;
            Ok(if samples == 1 {
                snapshot_in(FileIoAccountingState::Undelivered)
            } else {
                ProcessFileIoSnapshot {
                    files: Vec::new(),
                    accounting: FileIoAccounting {
                        state: FileIoAccountingState::Measured,
                        since_ms: 120,
                        read_bytes: 4096,
                        write_bytes: 2048,
                        ..Default::default()
                    },
                }
            })
        });
        assert_eq!(samples, 2, "the wait keeps sampling until it delivers");
        let summary = file_io_summary(&FileIoProbe {
            pid: Some(4242),
            waited,
            own_io_error: None,
        });
        assert_eq!(summary["state"], serde_json::json!("measured"));
        assert!(
            summary["waited_ms"].as_u64().expect("a number") > 0,
            "{summary}"
        );
        assert_eq!(summary["decoded_bytes"], serde_json::json!(6144));
    }

    /// A trace that never delivers keeps saying so — after the whole deadline —
    /// and publishes no byte total for the silence.
    #[test]
    fn a_trace_that_never_delivers_stays_undelivered_after_the_deadline() {
        let budget = Duration::from_millis(40);
        let mut samples = 0;
        let waited = await_file_io_delivery(budget, || {
            samples += 1;
            Ok(snapshot_in(FileIoAccountingState::Undelivered))
        });
        assert!(samples >= 2, "{samples} samples before the deadline");
        let summary = file_io_summary(&FileIoProbe {
            pid: Some(4242),
            waited,
            own_io_error: None,
        });
        assert_eq!(summary["state"], serde_json::json!("undelivered"));
        assert!(
            summary["waited_ms"].as_u64().expect("a number") >= 40,
            "{summary}"
        );
        // An unmeasured sample has no byte total at all: a zero here would read
        // as "measured nothing", which is the fabrication this block avoids.
        assert_eq!(summary["decoded_bytes"], serde_json::Value::Null);
    }

    /// Only `undelivered` is a verdict a later sample can change: a missing
    /// source, a layout that did not decode, a measurement and a refused
    /// question are all answers on the first sample.
    #[test]
    fn an_answer_that_cannot_change_is_not_waited_on() {
        let one_poll = FILE_IO_POLL_INTERVAL.as_millis() as u64;
        for state in [
            FileIoAccountingState::Unavailable,
            FileIoAccountingState::Undecodable,
            FileIoAccountingState::Measured,
        ] {
            let mut samples = 0;
            let waited = await_file_io_delivery(FILE_IO_DELIVERY_BUDGET, || {
                samples += 1;
                Ok(snapshot_in(state))
            });
            assert_eq!(samples, 1, "{state:?}");
            assert!(waited.waited_ms < one_poll, "{state:?}: {waited:?}");
        }

        let mut samples = 0;
        let waited = await_file_io_delivery(FILE_IO_DELIVERY_BUDGET, || {
            samples += 1;
            Err("the trace refused the request".to_string())
        });
        assert_eq!(samples, 1);
        assert!(waited.waited_ms < one_poll, "{}", waited.waited_ms);
        let summary = file_io_summary(&probe_with(
            Err("the trace refused the request".to_string()),
            waited.waited_ms,
        ));
        assert_eq!(summary["state"], serde_json::Value::Null);
        assert_eq!(
            summary["error"],
            serde_json::json!("the trace refused the request")
        );
    }

    /// The check's own file I/O is real file I/O — created, written, flushed,
    /// read back and deleted — and it leaves nothing behind for a rerun to
    /// mistake for its own window.
    #[test]
    fn the_own_io_writes_reads_back_and_cleans_up() {
        let dir = temp_path("own-io");
        let bytes = exercise_own_file_io_in(&dir).expect("own file I/O");
        assert_eq!(bytes, (OWN_IO_BYTES as u64) * 2);
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("the data directory exists")
            .map(|entry| entry.expect("an entry").path())
            .collect();
        entries.sort();
        assert!(entries.is_empty(), "{entries:?}");
        let _ = std::fs::remove_dir(&dir);
    }

    /// A check that cannot issue its own I/O reports that instead of blaming
    /// the trace for the silence it created.
    #[test]
    fn an_unusable_data_directory_is_reported_not_swallowed() {
        let blocked = temp_path("own-io-blocked");
        std::fs::write(&blocked, b"not a directory").expect("blocker file");
        let result = exercise_own_file_io_in(&blocked);
        let _ = std::fs::remove_file(&blocked);
        assert!(result.is_err(), "{result:?}");

        let mut probe = probe_with(Ok(snapshot_in(FileIoAccountingState::Undelivered)), 3000);
        probe.own_io_error = Some("data directory: access denied".to_string());
        let summary = file_io_summary(&probe);
        assert_eq!(
            summary["own_io_error"],
            serde_json::json!("data directory: access denied")
        );
        // The state is still reported as sampled — qualified by the reason it
        // cannot be read as a verdict about the trace.
        assert_eq!(summary["state"], serde_json::json!("undelivered"));
    }

    /// A question that could not be asked reports no state, not a fabricated
    /// one: an unlost and an unasked session must not read alike.
    #[test]
    fn an_unasked_file_io_reports_null_rather_than_a_state() {
        let summary = file_io_summary(&probe_with(
            Err("mock collector: no per-file trace".to_string()),
            0,
        ));
        assert_eq!(summary["pid"], serde_json::json!(4242));
        assert_eq!(summary["state"], serde_json::Value::Null);
        assert_eq!(summary["waited_ms"], serde_json::json!(0));
        assert_eq!(summary["events_lost"], serde_json::Value::Null);
        assert_eq!(summary["decoded_bytes"], serde_json::Value::Null);
        assert_eq!(summary["unnamed_creates"], serde_json::Value::Null);
        assert_eq!(summary["dropped_paths"], serde_json::Value::Null);
        assert_eq!(summary["cleared_file_objects"], serde_json::Value::Null);
        assert_eq!(
            summary["error"],
            serde_json::json!("mock collector: no per-file trace")
        );

        // An unqueried loss count inside a real answer stays unknown as well: a
        // `null` here means "could not be read", never "nothing was lost".
        let accounting = FileIoAccounting {
            events_lost: None,
            ..Default::default()
        };
        let summary = file_io_summary(&probe_with(
            Ok(ProcessFileIoSnapshot {
                files: Vec::new(),
                accounting,
            }),
            0,
        ));
        assert_eq!(summary["events_lost"], serde_json::Value::Null);
        assert_eq!(summary["state"], serde_json::json!("unavailable"));
    }

    /// A mock run has no trace, so it reports that it could not ask instead of
    /// a state the run never observed — and it does not wait for one either.
    #[test]
    fn a_mock_run_reports_no_file_io_answer() {
        let probe = probe_file_io(true, &tm_core::model::Snapshot::default());
        let summary = file_io_summary(&probe);
        assert_eq!(summary["pid"], serde_json::Value::Null);
        assert_eq!(summary["state"], serde_json::Value::Null);
        assert_eq!(summary["waited_ms"], serde_json::json!(0));
        assert_eq!(summary["own_io_error"], serde_json::Value::Null);
        assert!(probe.waited.answer.is_err());
    }
}
