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
