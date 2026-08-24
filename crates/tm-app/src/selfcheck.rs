//! Headless self-check: exercises the full sampling stack without a GUI.
//! Prints one JSON summary line and exits 0 on success, 1 on failure.

use std::time::{Duration, Instant};

pub fn run(mock: bool) -> i32 {
    let t0 = Instant::now();
    let interval = Duration::from_millis(250);

    let collector: Box<dyn tm_core::engine::SystemCollector> = if mock {
        Box::new(tm_core::mock::MockCollector::new())
    } else {
        let (c, _) = tm_platform::create_stack();
        c
    };

    let (handle, join) = match tm_core::engine::spawn(collector, interval) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(r#"{{"ok":false,"error":"engine spawn failed: {e}"}}"#);
            return 1;
        }
    };

    // Wait for at least 3 ticks.
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.tick_count() < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }

    let Some(snap) = handle.latest() else {
        eprintln!(r#"{{"ok":false,"error":"no snapshot produced in 10s"}}"#);
        handle.shutdown();
        let _ = join.join();
        return 1;
    };

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
        },
        "memory_used_gb": (snap.memory.used_bytes as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        "memory_total_gb": (snap.memory.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
        "processes": snap.processes.len(),
        "disks": snap.disks.len(),
        "networks": snap.networks.len(),
        "gpus": snap.gpus.iter().map(|g| g.name.clone()).collect::<Vec<_>>(),
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
