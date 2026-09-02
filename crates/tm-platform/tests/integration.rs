//! Integration tests running against the REAL system.
//! These exercise the platform layer end-to-end without a GUI.

use tm_core::model::*;

/// Poll `probe` until it answers or the deadline passes.
///
/// A newly spawned child is not immediately visible to every query the kernel
/// answers about it, but *how long* it takes is a property of the machine's
/// load — the very load a task manager is opened for. A fixed sleep bets on
/// that number; this waits for the condition instead (AGENTS.md: no sleeps in
/// tests, poll with bounded deadlines).
fn poll_for<T>(mut probe: impl FnMut() -> Option<T>, timeout: std::time::Duration) -> Option<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Wait until `pid` appears in a fresh process snapshot.
fn wait_until_visible(pid: u32) {
    let found = poll_for(
        || {
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            system
                .processes()
                .keys()
                .any(|candidate| candidate.as_u32() == pid)
                .then_some(())
        },
        std::time::Duration::from_secs(10),
    );
    assert!(found.is_some(), "spawned child {pid} never became visible");
}

/// Half the process list is unopenable for an unelevated session, and
/// everything sysinfo reads through a process HANDLE fails for those: the
/// creation time came back as a fabricated `Some(0)` and the priority class
/// as `Unknown`. Both now come from the kernel process table instead, which
/// reports them for every process — including pid 4, which can never be
/// opened on any machine.
#[test]
fn unopenable_processes_still_report_an_identity_and_a_priority() {
    let mut collector = tm_platform::create_collector();
    // The very first snapshot must already be right: the UI shows it.
    let snapshot = collector.sample(std::time::Instant::now()).expect("tick");

    let system = snapshot
        .processes
        .iter()
        .find(|p| p.pid == 4)
        .expect("the System process is always present");
    assert_ne!(
        system.priority,
        PriorityClass::Unknown,
        "pid 4 must resolve through the kernel base priority"
    );
    // The ordinary path must not have regressed: our own process opens fine,
    // so its class still comes from GetPriorityClass.
    let me = std::process::id();
    let own = snapshot
        .processes
        .iter()
        .find(|p| p.pid == me)
        .expect("own process present");
    assert_ne!(own.priority, PriorityClass::Unknown);

    // A start time of 0 is not a process identity, it is a failed query
    // wearing one — no handle check can ever match it.
    for p in &snapshot.processes {
        assert_ne!(
            p.start_epoch_s,
            Some(0),
            "pid {} ({}) carries a fabricated start time",
            p.pid,
            p.name
        );
    }

    // pid 0 (the idle pseudo-process) is absent from the kernel table by
    // design and stays honestly unknown; a handful of processes may be born
    // between the two enumerations. Anything beyond that means the fallback
    // stopped working.
    let unknown: Vec<&str> = snapshot
        .processes
        .iter()
        .filter(|p| !p.synthetic && p.pid != 0 && p.priority == PriorityClass::Unknown)
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        unknown.len() <= 3,
        "{} of {} processes have no priority class: {unknown:?}",
        unknown.len(),
        snapshot.processes.len()
    );
}

#[test]
fn sample_produces_sane_snapshot() {
    let mut collector = tm_platform::create_collector();
    // Two ticks so rates/deltas have a previous point.
    let s1 = collector.sample(std::time::Instant::now()).expect("tick 1");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let s2 = collector.sample(std::time::Instant::now()).expect("tick 2");

    assert!(s2.cpu.logical_count >= 1);
    assert!(!s2.cpu.brand.is_empty());
    assert!(
        s2.memory.total_bytes > 1024 * 1024 * 1024,
        "total memory sane"
    );
    assert!(s2.processes.len() > 20, "expected many processes");
    assert!(!s2.disks.is_empty());
    for d in &s2.disks {
        assert!(d.total_bytes > 0 || d.media == MediaKind::Unknown);
        assert!(d.free_bytes <= d.total_bytes);
    }
    // Our own process must be present.
    let me = std::process::id();
    assert!(
        s2.processes.iter().any(|p| p.pid == me),
        "own pid {me} must appear in process list"
    );
    // Utilization within bounds.
    assert!((0.0..=100.0).contains(&s2.cpu.utilization_pct));
    let _ = s1;

    // Timestamps advance.
    assert!(s2.timestamp_ms >= s1.timestamp_ms);
}

#[cfg(target_os = "windows")]
#[test]
fn windows_extras() {
    use tm_core::model::ProcCategory;
    let mut collector = tm_platform::create_collector();
    let actions = tm_platform::create_actions();
    let snap = collector.sample(std::time::Instant::now()).unwrap();

    // Services enumerate in bulk.
    let services = actions.list_services().expect("services");
    assert!(services.len() > 50);
    assert!(services.iter().any(|s| !s.name.is_empty()));
    assert!(
        services
            .iter()
            .any(|s| matches!(s.status, ServiceStatus::Running | ServiceStatus::Stopped))
    );

    // Startup items parse.
    let startup = actions.list_startup().expect("startup");
    for item in &startup {
        assert!(!item.name.is_empty());
        assert!(!item.location.is_empty());
    }

    // User sessions exist (at least our own).
    let sessions = actions.list_user_sessions().expect("sessions");
    assert!(!sessions.is_empty());

    // Process classification sanity.
    assert!(
        snap.processes
            .iter()
            .any(|p| p.category == ProcCategory::System)
    );
}

#[cfg(target_os = "windows")]
#[test]
fn kill_spawned_child() {
    let actions = tm_platform::create_actions();

    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    wait_until_visible(pid);

    actions.kill_single(pid).expect("kill child");
    let exited = poll_for(
        || match child.try_wait() {
            Ok(status) => status.map(Some),
            Err(_) => Some(None),
        },
        std::time::Duration::from_secs(10),
    );
    assert!(
        matches!(exited, Some(Some(_))),
        "child did not exit after TerminateProcess"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn suspend_resume_own_child() {
    let actions = tm_platform::create_actions();
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "10", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let pid = child.id();
    wait_until_visible(pid);

    actions.suspend_process(pid, true).expect("suspend");
    actions.suspend_process(pid, false).expect("resume");
    let _ = actions.kill_single(pid);
    let _ = child.wait();
}

#[cfg(target_os = "windows")]
#[test]
fn sampled_snapshot_carries_command_lines() {
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    wait_until_visible(pid);

    let mut collector = tm_platform::create_collector();
    let snap = collector.sample(std::time::Instant::now()).expect("sample");
    let entry = snap
        .processes
        .iter()
        .find(|p| p.pid == pid)
        .expect("child in snapshot");
    let cmdline = entry.command_line.as_deref().expect("command line set");
    assert!(
        cmdline.to_ascii_lowercase().contains("ping -n 30"),
        "unexpected command line: {cmdline}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "windows")]
#[test]
fn efficiency_mode_state_is_known_for_own_process() {
    // Regression: `GetProcessInformation(ProcessPowerThrottling)` needs
    // `Version` set on INPUT. With a zeroed struct it failed with
    // ERROR_INVALID_PARAMETER for every pid, so the snapshot reported
    // "unknown" everywhere and the Processes tab never drew the efficiency
    // leaf. Our own process is always openable, so its state must be known.
    let mut collector = tm_platform::create_collector();
    let snap = collector.sample(std::time::Instant::now()).expect("tick");
    let me = std::process::id();
    let own = snap
        .processes
        .iter()
        .find(|p| p.pid == me)
        .expect("own process in snapshot");
    assert_eq!(
        own.power_throttled,
        Some(false),
        "own process must report a KNOWN, not-throttled efficiency state"
    );

    // Toggling it on must be observable. A FRESH collector is used because
    // the sampler caches per-pid attributes for ATTR_REFRESH_TTL; the test
    // asserts the query, not the cache.
    let actions = tm_platform::create_actions();
    actions
        .set_efficiency_mode(me, true)
        .expect("enable EcoQoS");
    let mut collector = tm_platform::create_collector();
    let snap = collector.sample(std::time::Instant::now()).expect("tick 2");
    let observed = snap
        .processes
        .iter()
        .find(|p| p.pid == me)
        .expect("own")
        .power_throttled;
    actions.set_efficiency_mode(me, false).expect("disable");
    assert_eq!(observed, Some(true), "enabled EcoQoS must be read back");
}

#[cfg(target_os = "windows")]
#[test]
fn per_process_network_is_unknown_or_measured_never_fabricated() {
    // The ETW session needs administrator rights. Whichever way this test
    // runs, the invariant is the same: either EVERY real process carries a
    // network reading, or NONE does. A mix would mean we invented zeros for
    // the processes the trace happened not to see.
    let mut collector = tm_platform::create_collector();
    collector.set_demand(
        tm_core::demand::TelemetryDemand::core()
            .union(tm_core::demand::TelemetryDemand::PROCESS_NET),
    );
    let _ = collector.sample(std::time::Instant::now()).expect("tick 1");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let snap = collector.sample(std::time::Instant::now()).expect("tick 2");

    let real: Vec<_> = snap.processes.iter().filter(|p| !p.synthetic).collect();
    assert!(!real.is_empty());
    let measured = real.iter().filter(|p| p.net_recv_bps.is_some()).count();
    assert!(
        measured == 0 || measured == real.len(),
        "network telemetry must be all-or-nothing, got {measured}/{}",
        real.len()
    );
    if measured == 0 {
        eprintln!("per-process network unavailable (not elevated) - reported as unknown");
        return;
    }
    // When it IS available the numbers must be sane: no negative rates, and
    // totals must be consistent with the reported direction.
    for p in &real {
        assert!(p.net_recv_bps.unwrap() >= 0.0, "{} negative recv", p.name);
        assert!(p.net_sent_bps.unwrap() >= 0.0, "{} negative sent", p.name);
        assert!(p.net_recv_total.is_some() && p.net_sent_total.is_some());
    }
    // Synthetic CPU pseudo-rows never get a fabricated reading.
    for p in snap.processes.iter().filter(|p| p.synthetic) {
        assert!(p.net_recv_bps.is_none(), "pseudo-row must stay unknown");
    }
}

/// Diagnostic (run elevated): prints the busiest processes by network rate so
/// the ETW attribution can be eyeballed against Task Manager. Ignored by
/// default because it needs administrator rights AND live traffic.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "needs elevation and live network traffic"]
fn show_top_network_processes() {
    use tm_core::demand::TelemetryDemand;
    let mut collector = tm_platform::create_collector();
    collector.set_demand(TelemetryDemand::core().union(TelemetryDemand::PROCESS_NET));
    let mut top = Vec::new();
    for _ in 0..6 {
        let snap = collector.sample(std::time::Instant::now()).expect("tick");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        top = snap
            .processes
            .iter()
            .filter_map(|p| {
                let rate = p.net_recv_bps? + p.net_sent_bps?;
                Some((
                    rate,
                    p.name.clone(),
                    p.pid,
                    p.net_recv_total?,
                    p.net_sent_total?,
                ))
            })
            .collect::<Vec<_>>();
    }
    top.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    eprintln!("--- top network processes (bytes/s, cumulative recv/sent) ---");
    for (rate, name, pid, recv, sent) in top.iter().take(10) {
        eprintln!("{name:<28} pid={pid:<7} {rate:>12.0} B/s   recv={recv:<12} sent={sent}");
    }
    assert!(!top.is_empty(), "no process carried a network reading");
}

/// Reproduces the GUI's exact plumbing: a lazily spawned engine, demand sent
/// through the command channel, snapshots read from the handle. Run elevated;
/// ignored by default because the ETW session needs administrator rights.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "needs elevation"]
fn engine_delivers_process_network_when_demanded() {
    use tm_core::demand::TelemetryDemand;
    let factory: tm_core::engine::CollectorFactory = Box::new(tm_platform::create_collector);
    let (engine, _join) =
        tm_core::engine::spawn_lazy(factory, std::time::Duration::from_millis(400), None)
            .expect("engine");
    engine.start();
    // Exactly what update_demand() ships when the Processes page is visible.
    engine.set_demand(TelemetryDemand::core().union(TelemetryDemand::PROCESS_NET));

    let mut with_values = 0usize;
    let mut total = 0usize;
    for _ in 0..12 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Some(snap) = engine.latest() {
            let real: Vec<_> = snap.processes.iter().filter(|p| !p.synthetic).collect();
            total = real.len();
            with_values = real.iter().filter(|p| p.net_recv_bps.is_some()).count();
            if with_values > 0 {
                break;
            }
        }
    }
    eprintln!("processes with a network reading: {with_values}/{total}");
    assert!(total > 0, "engine never published a snapshot");
    assert!(
        with_values == total,
        "demand did not reach the collector: {with_values}/{total}"
    );
}

/// Diagnostic (run elevated): separates "the trace saw no events" from "the
/// counters were pruned away", which look identical from the GUI.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "needs elevation and live network traffic"]
fn network_trace_raw_vs_pruned() {
    let Some(usage) = tm_platform::win::net_etw_test_start() else {
        eprintln!("could not start a trace (not elevated?)");
        return;
    };
    let mut child = std::process::Command::new("curl.exe")
        .args([
            "-s",
            "-o",
            "NUL",
            "https://speed.cloudflare.com/__down?bytes=50000000",
        ])
        .spawn()
        .expect("spawn curl");
    std::thread::sleep(std::time::Duration::from_secs(6));
    let raw = usage.totals();
    let live = tm_platform::win::live_pids_for_test();
    let pruned = usage.totals_pruned(&live);
    let _ = child.kill();
    let _ = child.wait();
    eprintln!(
        "raw entries={}  live pids={}  pruned entries={}",
        raw.len(),
        live.len(),
        pruned.len()
    );
    let busiest = raw
        .iter()
        .max_by_key(|(_, b)| b.received + b.sent)
        .map(|(pid, b)| (*pid, b.received, b.sent));
    eprintln!("busiest raw = {busiest:?}");
    assert!(!raw.is_empty(), "the trace received no events at all");
    assert!(!live.is_empty(), "process enumeration returned nothing");
    assert!(!pruned.is_empty(), "pruning removed every counter");
}
