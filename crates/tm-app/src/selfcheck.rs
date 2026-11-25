//! Headless self-check: exercises the full sampling stack without a GUI.
//! Prints one JSON summary line and exits 0 on success, 1 on failure.

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
        FileIoAccountingState::Measured => "measured",
    }
}

/// The `file_io` block of the summary: whether per-file accounting has a live
/// source, and how much that source has lost or capped.
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
fn file_io_summary(
    pid: Option<u32>,
    accounting: Option<&FileIoAccounting>,
    error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "pid": pid,
        "state": accounting.map(|accounting| file_io_state_name(accounting.state)),
        "since_ms": accounting.map(|accounting| accounting.since_ms),
        "events_lost": accounting.and_then(|accounting| accounting.events_lost),
        "unnamed_creates": accounting.map(|accounting| accounting.unnamed_creates),
        "dropped_paths": accounting.map(|accounting| accounting.dropped_paths),
        "cleared_file_objects": accounting.map(|accounting| accounting.cleared_file_objects),
        "error": error,
    })
}

/// Ask the platform for per-file accounting of this process.
///
/// The GUI asks the same question about the process its dialog shows. A
/// headless run can only be sure of its own identity, and the accounting state
/// is a fact about the file trace session rather than about one target, so this
/// process is the sampled one. `--mock` has no trace at all and reports no
/// answer instead of an invented state.
fn probe_file_io(
    mock: bool,
    snapshot: &tm_core::model::Snapshot,
) -> (Option<u32>, Result<ProcessFileIoSnapshot, String>) {
    if mock {
        return (None, Err("mock collector: no per-file trace".to_string()));
    }
    let pid = std::process::id();
    let Some(start_epoch_s) = snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.creation_identity())
        .filter(|start| *start > 0)
    else {
        return (
            Some(pid),
            Err("own process creation time is unavailable".to_string()),
        );
    };
    let actions = tm_platform::create_actions();
    (
        Some(pid),
        actions
            .list_process_file_io(pid, Some(start_epoch_s))
            .map_err(|error| error.to_string()),
    )
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
    let (file_io_pid, file_io_answer) = probe_file_io(mock, &snap);
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
        // process, and what that source lost: the headless answer to "is the
        // file trace alive", which the I/O page only answers in a GUI.
        "file_io": file_io_summary(
            file_io_pid,
            file_io_answer
                .as_ref()
                .ok()
                .map(|snapshot: &ProcessFileIoSnapshot| &snapshot.accounting),
            file_io_answer.as_ref().err().map(String::as_str),
        ),
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
            (FileIoAccountingState::Measured, "measured"),
        ] {
            let accounting = FileIoAccounting {
                state,
                ..Default::default()
            };
            let summary = file_io_summary(Some(4242), Some(&accounting), None);
            assert_eq!(summary["pid"], serde_json::json!(4242));
            assert_eq!(summary["state"], serde_json::json!(name), "{state:?}");
        }

        // The counters a lossy or capped session is recognised by, plus the
        // span they cover.
        let accounting = FileIoAccounting {
            state: FileIoAccountingState::Undelivered,
            since_ms: 1500,
            events_lost: Some(12),
            unnamed_creates: 17,
            dropped_paths: 3,
            cleared_file_objects: 5,
            ..Default::default()
        };
        let summary = file_io_summary(Some(4242), Some(&accounting), None);
        assert_eq!(summary["since_ms"], serde_json::json!(1500));
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
                "dropped_paths",
                "error",
                "events_lost",
                "pid",
                "since_ms",
                "state",
                "unnamed_creates",
            ]
        );
    }

    /// A question that could not be asked reports no state, not a fabricated
    /// one: an unlost and an unasked session must not read alike.
    #[test]
    fn an_unasked_file_io_reports_null_rather_than_a_state() {
        let summary = file_io_summary(None, None, Some("mock collector: no per-file trace"));
        assert_eq!(summary["pid"], serde_json::Value::Null);
        assert_eq!(summary["state"], serde_json::Value::Null);
        assert_eq!(summary["events_lost"], serde_json::Value::Null);
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
        let summary = file_io_summary(Some(4242), Some(&accounting), None);
        assert_eq!(summary["events_lost"], serde_json::Value::Null);
        assert_eq!(summary["state"], serde_json::json!("unavailable"));
    }

    /// A mock run has no trace, so it reports that it could not ask instead of
    /// a state the run never observed.
    #[test]
    fn a_mock_run_reports_no_file_io_answer() {
        let (pid, answer) = probe_file_io(true, &tm_core::model::Snapshot::default());
        assert_eq!(pid, None);
        assert!(answer.is_err());
    }
}
