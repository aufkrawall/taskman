//! Headless self-check: exercises the full sampling stack without a GUI.
//! Prints one JSON summary line and exits 0 on success, 1 on failure.

use std::time::{Duration, Instant};

/// Ticks to wait for before summarizing.
///
/// The PDH groups (GPU engines, GPU memory, disk rates, CPU speed) are
/// two-sample counters: the first collection only establishes a baseline, and
/// the group is opened lazily on the tick its demand first arrives. Four ticks
/// leaves room for open + baseline + a real reading, so the summary reports
/// what those providers actually measured instead of their warm-up state.
const REQUIRED_TICKS: u64 = 4;

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
    let summary = serde_json::json!({
        "ok": true,
        "backend": if mock { "mock".to_string() } else { "platform".to_string() },
        "startup_to_snapshot_ms": t0.elapsed().as_millis() as u64,
        "ticks": handle.tick_count(),
        "sample_duration_ms": snap.sample_duration_ms,
        "cpu": {
            "util_pct": snap.cpu.utilization_pct,
            "logical": snap.cpu.logical_count,
            "brand": snap.cpu.brand,
            // Measured current frequency; only the CPU_SPEED provider fills it.
            "freq_mhz": snap.cpu.freq_mhz,
        },
        "memory_used_gb": (snap.memory.used_bytes as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        "memory_total_gb": (snap.memory.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
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
        "gpus": snap.gpus.iter().map(|g| g.name.clone()).collect::<Vec<_>>(),
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
        "uptime_s": snap.system.uptime_s,
    });
    println!("{summary}");

    handle.shutdown();
    let _ = join.join();

    // Basic sanity gates.
    if !mock && (snap.processes.len() < 5 || snap.cpu.logical_count == 0) {
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
        .and_then(|process| process.start_epoch_s)
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
