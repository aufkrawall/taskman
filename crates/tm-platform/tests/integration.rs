//! Integration tests running against the REAL system.
//! These exercise the platform layer end-to-end without a GUI.

#![cfg(target_os = "windows")]

use tm_core::model::*;
use tm_platform::actions::PlatformActions;

/// Poll `probe` until it answers or the deadline passes.
///
/// A newly spawned child is not immediately visible to every query the kernel
/// answers about it, but *how long* it takes is a property of the machine's
/// load — the very load a task manager is opened for. A fixed sleep bets on
/// that number; this waits for the condition instead (AGENTS.md: no sleeps in
/// tests, poll with bounded deadlines).
// Only the Windows tests spawn children to wait for.
#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
fn live_etw_is_required() -> bool {
    matches!(
        std::env::var("TASKMAN_ETW_LIVE_REQUIRED").as_deref(),
        Ok("1")
    )
}

/// Wait until `pid` appears in a fresh process snapshot.
// Only the Windows tests spawn children to wait for.
#[cfg(target_os = "windows")]
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

/// Windows core processes must never lose their User name / Elevated values.
///
/// Their tokens refuse every interactive caller (protected processes and
/// kernel pseudo-processes do not have a readable one at all), so the answer
/// comes from identity evidence — a verified Windows image, or the kernel's
/// own bare pseudo-name — instead of from a token read. The regression this
/// pins: version metadata read from the wrong string table left the evidence
/// empty on localized Windows, which blanked the User name and Elevated
/// columns of every protected process.
#[cfg(target_os = "windows")]
#[test]
fn windows_core_processes_report_their_owning_account() {
    let mut collector = tm_platform::create_collector();
    let snapshot = collector.sample(std::time::Instant::now()).expect("tick");

    let system_owned = ["smss", "csrss", "wininit", "winlogon", "services", "lsass"];
    let mut checked = 0usize;
    for p in snapshot
        .processes
        .iter()
        .filter(|p| !p.synthetic && p.pid > 4)
    {
        let stem = p.name.to_ascii_lowercase();
        let stem = stem.strip_suffix(".exe").unwrap_or(&stem);
        let expect_system =
            system_owned.contains(&stem) || stem == "registry" || stem == "memory compression";
        let expect_dwm = stem == "dwm";
        if !expect_system && !expect_dwm {
            continue;
        }
        checked += 1;
        let user = p
            .user
            .as_deref()
            .unwrap_or_else(|| panic!("{} ({}) has no user name", p.name, p.pid));
        if expect_system {
            assert_eq!(
                user, "SYSTEM",
                "{} ({}) must run under SYSTEM",
                p.name, p.pid
            );
        } else {
            assert!(
                user.contains("DWM"),
                "dwm.exe ({}) runs the DWM session account, not {user:?}",
                p.pid
            );
        }
        assert!(
            p.elevated.is_some(),
            "{} ({}) must know its elevation",
            p.name,
            p.pid
        );
    }
    assert!(
        checked >= 4,
        "only {checked} core processes were present; the assertion proved nothing"
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

    // Identity of the processes this token cannot OPEN. Roughly half of a
    // Windows session is SYSTEM or elevated, and every handle-based query
    // (sysinfo, QueryFullProcessImageNameW, ProcessIdToSessionId) returns
    // nothing for them — which is how Process Properties came to show "—" for
    // the path and session of the very process a user was trying to identify.
    // Both now fall back to the kernel, which names every process.
    let real: Vec<_> = s2.processes.iter().filter(|p| !p.synthetic).collect();
    let with_path = real.iter().filter(|p| p.exe_path.is_some()).count();
    let with_session = real.iter().filter(|p| p.session_id.is_some()).count();
    let with_ppid = real.iter().filter(|p| p.ppid.is_some()).count();
    // Generous thresholds: a couple of kernel pseudo-processes legitimately
    // have neither, and the point is to catch a regression back to ~50 %.
    assert!(
        with_path * 10 >= real.len() * 8,
        "only {with_path}/{} processes have an image path",
        real.len()
    );
    assert!(
        with_session * 10 >= real.len() * 9,
        "only {with_session}/{} processes have a session id",
        real.len()
    );
    assert!(
        with_ppid * 10 >= real.len() * 9,
        "only {with_ppid}/{} processes have a parent PID",
        real.len()
    );

    // Same story for the Disk column. sysinfo reads I/O bytes through
    // `GetProcessIoCounters`, which needs a handle, so it reported a flat zero
    // for the whole protected half of the list — the Disk column showed
    // "0 MB/s" for a downloader moving 200 KB/s. The bytes now come from the
    // kernel process table, which answers for everything.
    let protected = real
        .iter()
        .find(|p| {
            p.name.eq_ignore_ascii_case("services.exe")
                || p.name.eq_ignore_ascii_case("wininit.exe")
        })
        .expect("no protected system process in the snapshot");
    assert!(
        protected.disk_read_total + protected.disk_write_total > 0,
        "{} reports no I/O bytes; the Disk column is back on handle-based counters",
        protected.name
    );
    assert!(
        protected.io_other_total.unwrap_or(0) > 0,
        "{} reports no other (IPC) I/O bytes; kernel table counters missed",
        protected.name
    );
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

    // Native-aligned classification invariants (would have failed under the
    // old "Microsoft-signed under %SystemRoot% => System" rule):
    // core OS images are System, while Microsoft background machinery and
    // user-launchable tools are never System unless the OS marks them
    // critical.
    for p in &snap.processes {
        if ["svchost.exe", "csrss.exe", "winlogon.exe", "services.exe"]
            .iter()
            .any(|name| p.name.eq_ignore_ascii_case(name))
        {
            assert_eq!(
                p.category,
                ProcCategory::System,
                "{} must be a Windows process",
                p.name
            );
        }
        if [
            "WmiPrvSE.exe",
            "smartscreen.exe",
            "fontdrvhost.exe",
            "TextInputHost.exe",
            "WUDFHost.exe",
            "RuntimeBroker.exe",
            "dllhost.exe",
            "taskhostw.exe",
            "sihost.exe",
            "powershell.exe",
            "cmd.exe",
        ]
        .iter()
        .any(|name| p.name.eq_ignore_ascii_case(name))
        {
            assert!(
                p.category != ProcCategory::System || p.critical == Some(true),
                "{} must stay Background/App unless critical (got {:?})",
                p.name,
                p.category
            );
        }
    }
    // The service catalog must name running service hosts, otherwise the
    // Processes page falls back to indistinguishable "svchost.exe" rows.
    assert!(
        snap.processes.iter().any(|p| p
            .service_name
            .as_deref()
            .is_some_and(|name| !name.is_empty())),
        "no process carries a hosted service name"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn parent_process_resolution_for_live_and_system_processes() {
    let mut collector = tm_platform::create_collector();
    let actions = tm_platform::create_actions();
    let snap = collector.sample(std::time::Instant::now()).unwrap();

    // 1. Own process parent info
    let own_security = actions
        .process_security_info_checked(std::process::id(), None)
        .expect("own security info");
    let own_parent = own_security.parent.expect("own parent info");
    assert!(own_parent.pid > 0);
    if own_parent.alive {
        assert!(
            own_parent.name.is_some(),
            "alive parent must report binary name"
        );
    }

    // 2. A system service (svchost) whose parent is services.exe
    if let Some(svchost) = snap
        .processes
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case("svchost.exe") && p.ppid.is_some())
    {
        let parent_info = tm_platform::win::resolve_parent_process_info(svchost.ppid.unwrap());
        if parent_info.alive {
            assert!(
                parent_info.name.is_some(),
                "alive system parent must report binary name"
            );
            assert!(
                parent_info
                    .name
                    .as_deref()
                    .unwrap()
                    .eq_ignore_ascii_case("services.exe"),
                "svchost parent should resolve to services.exe, got {:?}",
                parent_info.name
            );
        }
    }
}

#[cfg(target_os = "windows")]
#[test]
fn explorer_parent_info_matches_the_live_snapshot() {
    let mut collector = tm_platform::create_collector();
    let actions = tm_platform::create_actions();
    let snap = collector.sample(std::time::Instant::now()).unwrap();

    let explorer = snap
        .processes
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case("explorer.exe") && p.ppid.is_some())
        .expect("explorer process in snapshot");

    let sec = actions
        .process_security_info_checked(explorer.pid, explorer.start_epoch_s)
        .expect("explorer security info");
    let parent = sec.parent.expect("explorer parent info");
    assert_eq!(parent.pid, explorer.ppid.unwrap());

    // Explorer's parent is normally the already-exited `userinit.exe`, but a
    // restarted shell can legitimately be parented by a live process (e.g. a
    // Task Manager replacement that relaunched Explorer). Assert the invariant
    // that matters instead of one particular Windows genealogy: a parent that
    // is still in the same snapshot must resolve to that process's image and
    // be reported alive; a parent that is gone must be reported terminated.
    match snap.processes.iter().find(|p| p.pid == parent.pid) {
        Some(live_parent) if parent.alive => assert_eq!(
            parent.name.as_deref(),
            Some(live_parent.name.as_str()),
            "resolved parent name must match the live process in the same snapshot"
        ),
        Some(_) => {
            // The parent can exit between the snapshot and the security query.
            let resample = collector.sample(std::time::Instant::now()).unwrap();
            assert!(
                resample.processes.iter().all(|p| p.pid != parent.pid),
                "a parent reported terminated must no longer be live"
            );
        }
        None if !parent.alive => {}
        None => {
            // Conversely it can start between the two observations.
            let resample = collector.sample(std::time::Instant::now()).unwrap();
            let live = resample
                .processes
                .iter()
                .find(|p| p.pid == parent.pid)
                .expect("a parent reported alive must exist in a fresh snapshot");
            assert_eq!(parent.name.as_deref(), Some(live.name.as_str()));
        }
    }
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
fn terminated_process_is_immediately_omitted_and_evicted_from_collector() {
    let mut collector = tm_platform::create_collector();

    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    let pid = child.id();

    // Verify it is sampled while alive
    let found_alive = poll_for(
        || {
            let snap = collector.sample(std::time::Instant::now()).ok()?;
            snap.processes.iter().any(|p| p.pid == pid).then_some(())
        },
        std::time::Duration::from_secs(10),
    );
    assert!(found_alive.is_some(), "child {pid} was not sampled alive");

    // Hold an open handle to the child process with PROCESS_VM_READ | PROCESS_QUERY_INFORMATION
    // (simulating handle retention that keeps the process object alive in kernel)
    let held_handle = unsafe {
        windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION
                | windows::Win32::System::Threading::PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()
    };

    // Kill the child
    let _ = child.kill();
    let _ = child.wait();

    // Sample again: even with external handles holding the EPROCESS executive object,
    // the sampler must detect that the process terminated, omit it from the snapshot,
    // and evict it from sysinfo so it never appears as a zombie ghost process.
    let omitted = poll_for(
        || {
            let snap = collector.sample(std::time::Instant::now()).ok()?;
            if !snap.processes.iter().any(|p| p.pid == pid) {
                Some(())
            } else {
                None
            }
        },
        std::time::Duration::from_secs(5),
    );

    if let Some(h) = held_handle {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
    }

    assert!(
        omitted.is_some(),
        "terminated process {pid} lingered in snapshot despite exit"
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
fn sampled_snapshot_carries_svchost_command_lines() {
    let mut collector = tm_platform::create_collector();
    let snap = collector.sample(std::time::Instant::now()).expect("sample");
    let svchosts: Vec<_> = snap
        .processes
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case("svchost.exe"))
        .collect();
    assert!(
        !svchosts.is_empty(),
        "Windows always has running svchost processes"
    );
    let with_cmdline = svchosts
        .iter()
        .filter(|p| {
            p.command_line
                .as_deref()
                .is_some_and(|cmd| cmd.to_ascii_lowercase().contains("svchost.exe"))
        })
        .count();
    // Virtually every svchost process is a running service; at least 90% must
    // have their command line resolved even in an unelevated session.
    assert!(
        with_cmdline >= svchosts.len() * 9 / 10,
        "expected most svchosts to have command lines, got {with_cmdline}/{}",
        svchosts.len()
    );
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

    // Toggling it on must be observable. On a CHILD, not on this process:
    // `process_ops::refuse_self` rejects throttling the running copy, because
    // in the shipped binary that is the process carrying the
    // `WH_KEYBOARD_LL` hook for the whole desktop. A child also exercises the
    // cross-process path the UI actually takes. A FRESH collector is used
    // because the sampler caches per-pid attributes for ATTR_REFRESH_TTL; the
    // test asserts the query, not the cache.
    let actions = tm_platform::create_actions();
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let child_pid = child.id();
    wait_until_visible(child_pid);

    actions
        .set_efficiency_mode(child_pid, true)
        .expect("enable EcoQoS");
    let mut collector = tm_platform::create_collector();
    let snap = collector.sample(std::time::Instant::now()).expect("tick 2");
    let observed = snap
        .processes
        .iter()
        .find(|p| p.pid == child_pid)
        .expect("child")
        .power_throttled;
    actions
        .set_efficiency_mode(child_pid, false)
        .expect("disable");
    let _ = actions.kill_single(child_pid);
    let _ = child.wait();
    assert_eq!(observed, Some(true), "enabled EcoQoS must be read back");
}

/// The shipped binary routes every keystroke on the desktop through its
/// `tm-hotkey-hook` thread whenever it is the registered Task Manager
/// replacement, so the scheduling changes that could stall or throttle that
/// thread must refuse this process through the public action surface too —
/// not only through `process_ops`.
#[cfg(target_os = "windows")]
#[test]
fn scheduling_actions_aimed_at_the_running_copy_are_refused() {
    let actions = tm_platform::create_actions();
    let me = std::process::id();

    assert!(actions.suspend_process(me, true).is_err(), "suspend");
    assert!(
        actions
            .set_priority(me, tm_core::model::PriorityClass::Low)
            .is_err(),
        "priority"
    );
    assert!(actions.set_affinity_mask(me, 1).is_err(), "affinity");
    assert!(
        actions.set_efficiency_mode(me, true).is_err(),
        "efficiency mode"
    );
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

/// Pin the safety contract of module unload on the real Windows loader. A
/// user confirmation is one release request, never an attempt to drain an
/// unknown loader refcount. The child explicitly loads one DLL three times;
/// three explicit unload requests are therefore required to remove it.
#[cfg(target_os = "windows")]
#[test]
fn module_unload_releases_one_reference_per_explicit_request() {
    use tm_platform::actions::PlatformActions;

    const DLL: &str = r"C:\Windows\System32\WTSAPI32.dll";
    const DLL_NAME: &str = "WTSAPI32.DLL";
    const LOADS: u32 = 3;

    // The LOCAL action surface: this test pins the unload implementation and
    // loader semantics rather than the installed broker generation.
    let actions = tm_platform::win::WinActions;
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    wait_until_visible(pid);

    let cleanup = |child: &mut std::process::Child| {
        let _ = actions.kill_single(child.id());
        let _ = child.wait();
    };

    // Identity for the action calls, straight from the same source the UI
    // uses, so the test also pins the snapshot-epoch path.
    let mut collector = tm_platform::create_collector();
    let epoch = poll_for(
        || {
            collector
                .sample(std::time::Instant::now())
                .ok()
                .and_then(|snap| snap.process(pid).map(|p| p.start_epoch_s))
        },
        std::time::Duration::from_secs(10),
    )
    .expect("child epoch in snapshot");

    // The probe DLL must not already be mapped.
    let initial = actions
        .list_process_modules(pid, epoch)
        .expect("child module list");
    assert!(
        !initial
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(DLL_NAME)),
        "{DLL_NAME} is unexpectedly preloaded in the child"
    );

    // Locate LoadLibraryW in the child the same way the unload locates
    // FreeLibrary: local address, owning module, same-named module in the
    // target, relative offset.
    let local_modules = actions
        .list_process_modules(std::process::id(), None)
        .expect("own module list");
    let child_modules = actions
        .list_process_modules(pid, epoch)
        .expect("child modules");
    let load_library_w = unsafe {
        windows::Win32::System::LibraryLoader::GetProcAddress(
            windows::Win32::System::LibraryLoader::GetModuleHandleW(
                windows::core::PCWSTR::from_raw(
                    "kernel32.dll\0"
                        .encode_utf16()
                        .collect::<Vec<u16>>()
                        .as_ptr(),
                ),
            )
            .expect("kernel32 handle"),
            windows::core::s!("LoadLibraryW"),
        )
        .expect("LoadLibraryW address")
    } as usize;
    let local_owner = local_modules
        .iter()
        .find(|m| {
            let start = m.base_address as usize;
            (start..start.saturating_add(m.size_bytes as usize)).contains(&load_library_w)
        })
        .expect("LoadLibraryW owner")
        .clone();
    let remote_owner = child_modules
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(&local_owner.name))
        .expect("child loader runtime")
        .clone();
    let remote_proc =
        remote_owner.base_address as usize + (load_library_w - local_owner.base_address as usize);

    let child_handle = unsafe {
        windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_CREATE_THREAD
                | windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION
                | windows::Win32::System::Threading::PROCESS_VM_OPERATION
                | windows::Win32::System::Threading::PROCESS_VM_WRITE
                | windows::Win32::System::Threading::PROCESS_VM_READ,
            false,
            pid,
        )
        .expect("open child")
    };

    // Write the UTF-16 DLL path into the child.
    let path_utf16: Vec<u16> = DLL.encode_utf16().chain([0]).collect();
    let bytes_len = path_utf16.len() * 2;
    let remote_string = unsafe {
        windows::Win32::System::Memory::VirtualAllocEx(
            child_handle,
            None,
            bytes_len,
            windows::Win32::System::Memory::MEM_RESERVE
                | windows::Win32::System::Memory::MEM_COMMIT,
            windows::Win32::System::Memory::PAGE_READWRITE,
        )
    };
    assert!(!remote_string.is_null(), "could not allocate in the child");
    unsafe {
        windows::Win32::System::Diagnostics::Debug::WriteProcessMemory(
            child_handle,
            remote_string,
            path_utf16.as_ptr().cast(),
            bytes_len,
            None,
        )
        .expect("write path into child");
    }

    let start: windows::Win32::System::Threading::LPTHREAD_START_ROUTINE = Some(unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(*mut core::ffi::c_void) -> u32>(
            remote_proc,
        )
    });
    for _ in 0..LOADS {
        let thread = unsafe {
            windows::Win32::System::Threading::CreateRemoteThread(
                child_handle,
                None,
                0,
                start,
                Some(remote_string),
                0,
                None,
            )
            .expect("remote LoadLibraryW")
        };
        let waited =
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(thread, 15_000) };
        assert_eq!(
            waited,
            windows::Win32::Foundation::WAIT_OBJECT_0,
            "remote load did not finish"
        );
        let mut exit_code = 0u32;
        unsafe {
            windows::Win32::System::Threading::GetExitCodeThread(thread, &mut exit_code)
                .expect("load exit code");
        }
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(thread) };
        assert_ne!(exit_code, 0, "LoadLibraryW in the child failed");
    }
    // The remote string is child memory, not a handle: it is reclaimed when
    // the child exits.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(child_handle) };

    let mapped = actions
        .list_process_modules(pid, epoch)
        .expect("child modules after loads");
    let module = mapped
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(DLL_NAME))
        .expect("injected DLL mapped in child")
        .clone();

    // A destructive broker/local unload must never rebind a bare PID to the
    // process that happens to own it at action time.
    assert!(
        actions
            .unload_process_module(pid, None, module.base_address, &module.path)
            .is_err(),
        "PID-only module unload must fail closed"
    );

    for attempt in 1..=LOADS {
        let outcome = actions
            .unload_process_module(pid, epoch, module.base_address, &module.path)
            .expect("unload request");
        let should_remain = attempt < LOADS;
        assert_eq!(
            outcome.still_mapped,
            Some(should_remain),
            "unexpected mapping state after explicit release {attempt}"
        );
        let after = actions
            .list_process_modules(pid, epoch)
            .expect("child modules after unload request");
        assert_eq!(
            after.iter().any(|m| m.name.eq_ignore_ascii_case(DLL_NAME)),
            should_remain,
            "inventory disagrees after explicit release {attempt}"
        );
    }

    cleanup(&mut child);
}

/// Diagnostic (run elevated): proves the hand-written `Microsoft-Windows-
/// Kernel-Disk` payload offsets against REAL kernel events.
///
/// The synthetic unit tests in `win::disk_etw` pin the decoder against a
/// payload this repository wrote itself; only a live trace can show that the
/// layout matches what Windows actually emits. The assertion that matters is
/// that requests get ATTRIBUTED: a wrong `IssuingThreadId` offset still
/// produces events, it just sends every one of them to the unattributed
/// remainder.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "needs elevation and live disk traffic"]
fn disk_trace_attributes_real_requests_to_the_issuing_process() {
    let Some(usage) = tm_platform::win::disk_etw_test_start() else {
        eprintln!("could not start a trace (not elevated?)");
        return;
    };
    let live = tm_platform::win::live_pids_for_test();
    // Read a large, certainly-uncached file: the OS image is the one file
    // every Windows machine has and nothing else is reading right now.
    let mut victim = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    victim.push(r"System32\ntoskrnl.exe");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut window = usage.take_window(&live);
    while std::time::Instant::now() < deadline {
        let _ = std::fs::read(&victim);
        window = usage.take_window(&live);
        if !window.procs.is_empty() {
            break;
        }
    }
    let total = window.total_service_time();
    eprintln!(
        "window {} ms: {} records seen, {} decoded, {} processes,          {} service time ({} unattributed)",
        window.since_ms,
        window.events_seen,
        window.events_decoded,
        window.procs.len(),
        total,
        window.unattributed_service_time
    );
    let busiest = window
        .procs
        .iter()
        .max_by_key(|(_, d)| d.service_time)
        .map(|(pid, d)| (*pid, d.ops, d.read_bytes, d.write_bytes));
    eprintln!("busiest = {busiest:?}");
    assert!(
        window.events_seen > 0,
        "the trace received no disk records at all - wrong provider or keyword"
    );
    assert!(
        window.events_decoded > 0,
        "records arrived but none decoded - the DiskIo payload layout changed"
    );
    assert!(total > 0, "decoded records carried no service time");
    assert!(
        !window.procs.is_empty(),
        "every request went unattributed - the IssuingThreadId offset is wrong"
    );
    let attributed: u64 = window.procs.values().map(|d| d.service_time).sum();
    assert!(
        attributed * 2 >= total,
        "most disk time was unattributable: {attributed} of {total}"
    );
    // Per-disk bookkeeping: `DiskNumber` is the `PhysicalDrive<n>` number the
    // PhysicalDisk counters key on, so a misread shows up as a garbage disk.
    eprintln!("disk totals = {:?}", window.disk_totals);
    let per_disk: u64 = window.disk_totals.iter().map(|t| t.service_time).sum();
    assert_eq!(per_disk, total, "per-disk totals must cover every request");
    assert!(
        window.disk_totals.iter().all(|t| t.disk < 1024),
        "implausible disk number - the DiskNumber offset is wrong"
    );
    for disk in window.procs.values() {
        let split: u64 = disk.per_disk.iter().map(|t| t.service_time).sum();
        assert_eq!(split, disk.service_time);
    }
}

/// Diagnostic (run with elevation or Performance Log Users): proves that the
/// privileged loader rundown can enumerate a protected process that ToolHelp
/// cannot open. Synthetic payload tests cannot show whether Windows emits the
/// supported image-record layouts to this session or whether `csrss` is
/// included.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "needs elevation or Performance Log Users and live loader events"]
fn protected_process_modules_include_csrss_images() {
    use tm_platform::actions::PlatformActions;

    let mut collector = tm_platform::create_collector();
    let snapshot = collector
        .sample(std::time::Instant::now())
        .expect("live process snapshot");
    let (pid, start_epoch_s) = snapshot
        .processes
        .iter()
        .find_map(|process| {
            process
                .name
                .eq_ignore_ascii_case("csrss.exe")
                .then_some(process.start_epoch_s.map(|start| (process.pid, start)))
                .flatten()
        })
        .expect("csrss.exe must have a kernel-reported start time");

    let actions = tm_platform::win::WinActions;
    let first = actions.list_process_modules(pid, Some(start_epoch_s));
    eprintln!("first csrss module request: {first:?}");
    if !tm_platform::win::module_etw_test_running() {
        assert!(
            !live_etw_is_required(),
            "required loader trace could not start"
        );
        eprintln!("could not start the loader trace (not elevated or Performance Log Users?)");
        tm_platform::win::stop_live_etw_sessions();
        return;
    }
    let modules = poll_for(
        || actions.list_process_modules(pid, Some(start_epoch_s)).ok(),
        std::time::Duration::from_secs(10),
    );
    tm_platform::win::stop_live_etw_sessions();

    let modules = modules.expect("privileged loader rundown never completed");
    eprintln!(
        "csrss pid={pid} modules={} first={:?}",
        modules.len(),
        modules.first()
    );
    assert!(
        modules
            .iter()
            .any(|module| module.name.eq_ignore_ascii_case("csrss.exe")),
        "the rundown omitted the csrss executable"
    );
    assert!(
        modules
            .iter()
            .any(|module| module.name.eq_ignore_ascii_case("ntdll.dll")),
        "the rundown omitted ntdll"
    );
    assert!(
        modules.iter().all(|module| {
            module.base_address != 0 && module.size_bytes != 0 && !module.path.is_empty()
        }),
        "the loader rundown produced an incomplete module record"
    );
}

/// Diagnostic (run with elevation or Performance Log Users): proves the
/// sampled-profile payload layout, thread ownership seeding, and module
/// attribution against REAL Windows events rather than a synthetic buffer.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "needs elevation or Performance Log Users and live profile events"]
fn sampled_profile_attributes_code_to_this_process() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tm_platform::actions::PlatformActions;

    let mut collector = tm_platform::create_collector();
    let snapshot = collector
        .sample(std::time::Instant::now())
        .expect("live process snapshot");
    let pid = std::process::id();
    let start_epoch_s = snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.start_epoch_s)
        .expect("the test process must have a kernel-reported start time");

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let worker = std::thread::Builder::new()
        .name("tm-profile-probe".into())
        .spawn(move || {
            let mut value = 0u64;
            while !stop_worker.load(Ordering::Relaxed) {
                for _ in 0..100_000 {
                    value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                std::hint::black_box(value);
            }
        })
        .expect("start profile workload");

    let actions = tm_platform::win::WinActions;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut last = None;
    let mut last_error = None;
    while std::time::Instant::now() < deadline {
        match actions.process_module_code_samples(pid, Some(start_epoch_s)) {
            Ok(Some(window)) => {
                let done = window.events_decoded > 0 && window.target_samples > 0;
                last = Some(window);
                if done {
                    break;
                }
            }
            Ok(None) => {
                last_error = Some("sampled-profile session was unavailable".to_string());
                assert!(
                    !live_etw_is_required(),
                    "required sampled-profile session was unavailable"
                );
                break;
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    tm_platform::win::stop_live_etw_sessions();

    let Some(window) = last else {
        assert!(
            !live_etw_is_required(),
            "required sampled-profile probe failed"
        );
        eprintln!(
            "the sampled-profile trace produced no window: {}",
            last_error.as_deref().unwrap_or("no attempt completed")
        );
        return;
    };
    eprintln!(
        "window={} ms seen={} decoded={} lost={:?} target={} attributed={} threads={} modules={}",
        window.window_ms,
        window.events_seen,
        window.events_decoded,
        window.events_lost,
        window.target_samples,
        window.attributed_samples,
        window.tracked_threads,
        window.modules.len()
    );
    assert!(
        window.events_seen > 0,
        "the session received no SampledProfile records - wrong provider, keyword, or system-logger mode"
    );
    assert!(
        window.events_decoded > 0,
        "SampledProfile records arrived but none decoded - payload layout changed"
    );
    assert!(
        window.tracked_threads > 0,
        "no target thread was known despite the trace starting after the workload"
    );
    assert!(
        window.target_samples > 0,
        "no sampled instruction pointer was attributed to this process"
    );
    assert!(
        window.attributed_samples > 0,
        "target samples did not match any current module"
    );
}

/// Diagnostic (run elevated): WHICH session carries the per-file `FileIo` class
/// on this machine, and does it deliver anything?
///
/// Three outcomes are all real and must not be confused, so this reports the one
/// it got instead of assuming:
///
/// * the global `NT Kernel Logger` started (the preferred mechanism);
/// * it was already owned by another application, so the private SystemIo
///   subscription started instead — the fallback;
/// * neither started, and the refusal names why (no rights, or
///   `KernelLoggerInUse`).
///
/// What only a live range can prove is that Windows ACCEPTS one of these shapes
/// on this build and emits the classic records through it. The synthetic unit
/// tests pin the decoder; this pins the subscription hypothesis, which is the
/// part a previous release got wrong while every test passed.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "starts a machine-wide ETW session; needs elevation and live file traffic"]
fn the_file_trace_measures_exact_bytes_per_file() {
    use std::io::{Read, Seek, Write};
    use tm_platform::actions::FileIoAccountingState;

    let pid = std::process::id();
    let dir = std::env::temp_dir();
    // Opened BEFORE the session: the trace sees no Create for it, so its name
    // has to come from the handle table.
    let early_path = dir.join(format!("tm_file_etw_early_{pid}.bin"));
    let mut early = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&early_path)
        .expect("open the early file");
    let (trace, session) = match tm_platform::win::file_etw_test_start() {
        Ok(started) => started,
        Err(refusal) => {
            let _ = std::fs::remove_file(&early_path);
            // Still an answer: say it rather than pass silently.
            eprintln!("no per-file session was started: {refusal:?}");
            return;
        }
    };
    eprintln!("per-file trace session = {session}");
    let epoch = tm_platform::win::own_start_epoch_for_test();
    assert!(epoch > 0, "this process's creation time is readable");
    // The first sample names every file handle this process holds.
    let _ = trace.sample(pid, epoch, &tm_platform::win::live_pids_for_test());

    // Exact traffic: 3 x 8 KiB written and 4 KiB read through the early
    // handle; 2 x 64 KiB written and the whole file read back (one request
    // for more than the file holds) through a new one. The flushes add paging
    // writes the trace must NOT count.
    for _ in 0..3 {
        early.write_all(&[1u8; 8_192]).expect("early write");
    }
    early.sync_all().expect("early flush");
    early.seek(std::io::SeekFrom::Start(0)).expect("seek");
    let mut buffer = vec![0u8; 4_096];
    early.read_exact(&mut buffer).expect("early read");
    let late_path = dir.join(format!("tm_file_etw_late_{pid}.bin"));
    {
        let mut late = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&late_path)
            .expect("open the late file");
        late.write_all(&[7u8; 65_536]).expect("late write");
        late.write_all(&[7u8; 65_536]).expect("late write");
        late.sync_all().expect("late flush");
        late.seek(std::io::SeekFrom::Start(0)).expect("seek");
        let mut whole = vec![0u8; 200_000];
        let mut got = 0;
        loop {
            let read = late.read(&mut whole[got..]).expect("late read");
            if read == 0 {
                break;
            }
            got += read;
        }
        assert_eq!(got, 131_072);
    }

    let row = |sample: &tm_platform::win::FileIoSampleForTest, path: &std::path::Path| {
        let wanted = path.to_string_lossy().to_lowercase();
        sample
            .files
            .iter()
            .filter(|(candidate, _)| candidate.to_lowercase() == wanted)
            .fold((0u64, 0u64), |(read, write), (_, bytes)| {
                (read + bytes.read_bytes, write + bytes.write_bytes)
            })
    };
    let expected_early = (4_096, 3 * 8_192);
    let expected_late = (131_072, 131_072);
    // Delivery is asynchronous (1 s flush timer); poll against a deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut sample = trace.sample(pid, epoch, &tm_platform::win::live_pids_for_test());
    while std::time::Instant::now() < deadline
        && (row(&sample, &early_path) != expected_early
            || row(&sample, &late_path) != expected_late)
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
        sample = trace.sample(pid, epoch, &tm_platform::win::live_pids_for_test());
    }
    drop(early);
    let _ = std::fs::remove_file(&early_path);
    let _ = std::fs::remove_file(&late_path);
    eprintln!(
        "state={:?} seen={} decoded={} transfers={} early={:?} late={:?} unnamed={:?} lost={:?} since={} ms",
        sample.state(),
        sample.events_seen,
        sample.events_decoded,
        sample.session_transfers,
        row(&sample, &early_path),
        row(&sample, &late_path),
        sample.unnamed,
        sample.events_lost,
        sample.since_ms
    );
    assert!(
        sample.events_seen > 0,
        "session {session} delivered no FileIo record at all - the subscription shape is wrong"
    );
    assert!(
        sample.events_decoded > 0,
        "records arrived and none decoded - the payload layout changed"
    );
    assert_eq!(sample.state(), FileIoAccountingState::Measured);
    assert_eq!(
        row(&sample, &late_path),
        expected_late,
        "a file opened under the trace: (read, written) exactly, paging flushes excluded"
    );
    assert_eq!(
        row(&sample, &early_path),
        expected_early,
        "a handle opened before the trace is named from the handle table"
    );
}

/// A process started after the trace has its whole life covered: the
/// production request path reports `traced_since_process_start`, its file
/// write on a row, and the process counters over the same window — which
/// also hold what it wrote to a device (`NUL`), which no file row carries.
#[test]
#[ignore = "starts a machine-wide ETW session; needs elevation"]
fn the_file_trace_covers_a_process_from_its_start() {
    use tm_platform::actions::PlatformActions;
    let actions = tm_platform::win::WinActions;
    let own = std::process::id();
    // Starts this process's own trace (no service is asked on this path).
    let _ = actions.list_process_file_io(own, Some(tm_platform::win::own_start_epoch_for_test()));
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tm_cov_{own}.txt"));
    let script = format!(
        "echo 0123456789abcdef> \"{}\" & type \"{}\" > nul & ping -n 4 127.0.0.1 > nul",
        path.display(),
        path.display()
    );
    // `raw_arg`: cmd parses its own command line, and the quoting `args`
    // applies for ordinary programs breaks it.
    let mut child = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd.exe")
            .raw_arg(format!("/d /c \"{script}\""))
            .spawn()
            .expect("spawn")
    };
    let pid = child.id();
    std::thread::sleep(std::time::Duration::from_millis(2_000));
    let epoch = tm_platform::win::process_start_epoch_for_test(pid);
    let snapshot = actions.list_process_file_io(pid, Some(epoch));
    let _ = child.wait();
    let length = std::fs::metadata(&path)
        .map(|meta| meta.len())
        .expect("written");
    let _ = std::fs::remove_file(&path);
    let snapshot = snapshot.expect("answered");
    let accounting = &snapshot.accounting;
    eprintln!(
        "state={:?} since_start={} file r/w={}/{} counter window r/w={:?}/{:?}",
        accounting.state,
        accounting.traced_since_process_start,
        accounting.read_bytes,
        accounting.write_bytes,
        accounting.counter_window_read_bytes,
        accounting.counter_window_write_bytes
    );
    for file in &snapshot.files {
        eprintln!(
            "  {} handles={} read={:?} written={:?}",
            file.path, file.open_handles, file.read_bytes, file.write_bytes
        );
    }
    assert_eq!(
        accounting.state,
        tm_platform::actions::FileIoAccountingState::Measured
    );
    assert!(accounting.traced_since_process_start);
    let wanted = path.to_string_lossy().to_lowercase();
    let row = snapshot
        .files
        .iter()
        .find(|file| file.path.to_lowercase() == wanted)
        .expect("the file the child wrote has a row");
    assert_eq!(row.write_bytes, Some(length), "what echo wrote");
    assert_eq!(row.read_bytes, Some(length), "what type read back");
    // `type ... > nul` wrote the same bytes again, to a device: the process
    // counters hold both, the file row only the first.
    let window = accounting
        .counter_window_write_bytes
        .expect("a counter window");
    assert_eq!(window, 2 * length);
    assert_eq!(accounting.counter_window_read_bytes, Some(length));
}

/// What the always-on per-file trace costs, in CPU cycles of this process
/// rather than wall time, so background load on the machine does not enter
/// the numbers:
///
/// * the workload thread's own kernel+user time for a small-file storm with
///   and without the session — ETW records are written in the issuing
///   thread's context, so that is where the per-request cost lands;
/// * the rest of this process's CPU while the session runs — the consumer
///   thread decoding and accounting every file record on the machine — both
///   under the storm and while the machine idles.
///
/// Prints numbers; asserts only that a session ran.
#[test]
#[ignore = "starts a machine-wide ETW session; needs elevation; a measurement"]
fn the_file_trace_costs_little() {
    use std::io::{Read, Write};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
    use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

    /// CPU cycles of this process and of the calling thread: exact, unlike
    /// the 15.6 ms ticks `GetThreadTimes` reports, and blind to every other
    /// process's load.
    fn cycles() -> (u64, u64) {
        let (mut process, mut thread) = (0u64, 0u64);
        unsafe {
            let _ = QueryProcessCycleTime(GetCurrentProcess(), &mut process);
            let _ = QueryThreadCycleTime(GetCurrentThread(), &mut thread);
        }
        (process, thread)
    }
    /// The storm; returns (thread cycles, rest-of-process cycles during it).
    fn workload(dir: &std::path::Path, files: usize) -> (u64, u64) {
        let (process_before, thread_before) = cycles();
        let payload = [0x42u8; 4_096];
        let mut back = Vec::with_capacity(4_096);
        for index in 0..files {
            let path = dir.join(format!("w{index}.bin"));
            std::fs::File::create(&path)
                .and_then(|mut file| file.write_all(&payload))
                .expect("write");
            back.clear();
            std::fs::File::open(&path)
                .and_then(|mut file| file.read_to_end(&mut back))
                .expect("read");
            std::fs::remove_file(&path).expect("remove");
        }
        let (process_after, thread_after) = cycles();
        let thread = thread_after - thread_before;
        (
            thread,
            (process_after - process_before).saturating_sub(thread),
        )
    }
    fn median(mut values: Vec<u64>) -> u64 {
        values.sort_unstable();
        values[values.len() / 2]
    }
    let dir = std::env::temp_dir().join(format!("tm_file_cost_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    const FILES: usize = 4_000;
    let _ = workload(&dir, 500);
    let (mut off, mut on, mut consumer_busy) = (Vec::new(), Vec::new(), Vec::new());
    let (mut consumer_idle_cycles, mut idle_events) = (0u64, 0u64);
    for _ in 0..6 {
        off.push(workload(&dir, FILES).0);
        let (trace, _) = match tm_platform::win::file_etw_test_start() {
            Ok(started) => started,
            Err(refusal) => {
                eprintln!("no session: {refusal:?}");
                return;
            }
        };
        let pid = std::process::id();
        let epoch = tm_platform::win::own_start_epoch_for_test();
        let live = tm_platform::win::live_pids_for_test();
        let _ = trace.sample(pid, epoch, &live);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (process_before, _) = cycles();
        let (thread, _) = workload(&dir, FILES);
        on.push(thread);
        // The consumer drains the storm's records within its flush window;
        // everything this process spent besides the storm thread is its cost.
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        consumer_busy.push((cycles().0 - process_before).saturating_sub(thread));
        let (idle_before, _) = cycles();
        let seen_before = trace.sample(pid, epoch, &live).events_seen;
        std::thread::sleep(std::time::Duration::from_secs(5));
        let seen_after = trace.sample(pid, epoch, &live).events_seen;
        consumer_idle_cycles += cycles().0 - idle_before;
        idle_events += seen_after - seen_before;
    }
    let _ = std::fs::remove_dir_all(&dir);
    let (off_cycles, on_cycles) = (median(off.clone()), median(on.clone()));
    eprintln!("workload thread CPU (cycles) off {off:?} on {on:?}");
    eprintln!(
        "storm of {FILES} create/write/read/delete: thread CPU median off {off_cycles} cycles, \
         on {on_cycles} ({:+.1}%, {:+.0} cycles per file)",
        (on_cycles as f64 / off_cycles as f64 - 1.0) * 100.0,
        (on_cycles as f64 - off_cycles as f64) / FILES as f64
    );
    eprintln!(
        "consumer CPU during the storm (cycles): {consumer_busy:?}, median {} = {:.0} cycles per file",
        median(consumer_busy.clone()),
        median(consumer_busy.clone()) as f64 / FILES as f64
    );
    eprintln!(
        "idle: {idle_events} file records in 30 s ({:.0}/s machine-wide), consumer {} Mcycles \
         ({:.0} cycles per record; {:.2}% of one 4 GHz core)",
        idle_events as f64 / 30.0,
        consumer_idle_cycles / 1_000_000,
        consumer_idle_cycles as f64 / idle_events.max(1) as f64,
        consumer_idle_cycles as f64 / (30.0 * 4.0e9) * 100.0
    );
}

/// Follow a live process through the production trace for 15 s and print
/// what each step saw: the handle-table names, then the sample. Set
/// `TASKMAN_FILE_TRACE_PID`; skipped without it.
#[test]
#[ignore = "starts a machine-wide ETW session; needs elevation and TASKMAN_FILE_TRACE_PID"]
fn the_file_trace_follows_a_live_process() {
    let Some(pid) = std::env::var("TASKMAN_FILE_TRACE_PID")
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        eprintln!("TASKMAN_FILE_TRACE_PID is not set");
        return;
    };
    let epoch = tm_platform::win::process_start_epoch_for_test(pid);
    eprintln!("pid {pid} epoch {epoch}");
    let names = tm_platform::win::name_file_objects_for_test(pid, epoch);
    eprintln!("handle table named {} objects", names.len());
    for (object, path) in &names {
        eprintln!("  {object:#x} {path}");
    }
    let (trace, session) = match tm_platform::win::file_etw_test_start() {
        Ok(started) => started,
        Err(refusal) => {
            eprintln!("no per-file session was started: {refusal:?}");
            return;
        }
    };
    eprintln!("session {session}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut sample = trace.sample(pid, epoch, &tm_platform::win::live_pids_for_test());
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(1));
        sample = trace.sample(pid, epoch, &tm_platform::win::live_pids_for_test());
    }
    eprintln!(
        "state={:?} transfers r/w={}/{} unnamed={:?} omitted={:?}",
        sample.state(),
        sample.pid_read_transfers,
        sample.pid_write_transfers,
        sample.unnamed,
        sample.omitted
    );
    for (path, bytes) in &sample.files {
        eprintln!(
            "  {path} read={} written={}",
            bytes.read_bytes, bytes.write_bytes
        );
    }
    drop(trace);
    // The request path the GUI uses: the broker when it answers, this
    // process's own trace otherwise.
    use tm_platform::actions::PlatformActions;
    let actions = tm_platform::win::WinActions;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        last = Some(actions.list_process_file_io(pid, Some(epoch)));
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    match last.expect("asked at least once") {
        Ok(snapshot) => {
            let accounting = &snapshot.accounting;
            eprintln!(
                "request path: state={:?} since={} ms r/w={}/{} bytes r/w={}/{} unnamed r/w={}/{} \
                 since_start={} counter window r/w={:?}/{:?}",
                accounting.state,
                accounting.since_ms,
                accounting.read_ops,
                accounting.write_ops,
                accounting.read_bytes,
                accounting.write_bytes,
                accounting.unnamed_read_bytes,
                accounting.unnamed_write_bytes,
                accounting.traced_since_process_start,
                accounting.counter_window_read_bytes,
                accounting.counter_window_write_bytes
            );
            for file in &snapshot.files {
                eprintln!(
                    "  {} handles={} read={:?} written={:?}",
                    file.path, file.open_handles, file.read_bytes, file.write_bytes
                );
            }
        }
        Err(error) => eprintln!("request path failed: {error}"),
    }
}

#[test]
fn list_process_file_io_for_system_process_succeeds_without_error() {
    let actions = tm_platform::win::WinActions;
    let snapshot = actions
        .list_process_file_io(4, None)
        .expect("list_process_file_io for pid 4 (System) must succeed");
    assert!(
        snapshot.files.is_empty(),
        "system process has no accessible user-mode duplicate handles"
    );
    assert!(
        !snapshot.accounting.handles_complete,
        "handles must be marked incomplete"
    );
    assert!(
        snapshot.accounting.counter_read_bytes.is_some(),
        "system process must report kernel read transfer counter"
    );
}

#[test]
fn list_process_file_io_for_current_process_has_complete_handles() {
    let actions = tm_platform::win::WinActions;
    let pid = std::process::id();
    let snapshot = actions
        .list_process_file_io(pid, None)
        .expect("own process file io");
    assert!(snapshot.accounting.handles_complete);
}

/// The direct path attaches its OWN per-file trace sample, and it only does so
/// when the caller passed a verified creation time — which every other test
/// here omits. Without this one the attach step never runs end to end, and a
/// wiring break (wrong state, unmeasured rows reported as zero, a total that
/// does not add up) would go unnoticed.
///
/// This test process is normally unelevated, so `Unavailable` is the expected
/// answer and nothing is asserted about bytes existing. What IS asserted is the
/// accounting: either the trace measured and rows plus both remainders are the
/// process total, or there is no source and every per-file number stayed empty.
#[cfg(target_os = "windows")]
#[test]
fn list_process_file_io_with_a_creation_time_keeps_the_accounting_consistent() {
    use tm_platform::actions::FileIoAccountingState;

    /// Stops every ETW session this process started, however this test leaves.
    ///
    /// A session is a kernel object and process termination runs no
    /// destructor, so an elevated run — the one where the attach step really
    /// starts a per-file trace — would leave that session filling buffers on
    /// the machine until the next reboot as soon as an assertion failed. It is
    /// created before anything can start a session and dropped last, which is
    /// what makes it cover the panic paths in between.
    struct EtwTeardown;
    impl Drop for EtwTeardown {
        fn drop(&mut self) {
            tm_platform::win::stop_live_etw_sessions();
        }
    }

    let _etw_teardown = EtwTeardown;

    let mut collector = tm_platform::create_collector();
    let snapshot = collector
        .sample(std::time::Instant::now())
        .expect("live process snapshot");
    let pid = std::process::id();
    let start_epoch_s = snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.start_epoch_s)
        .expect("the test process must have a kernel-reported start time");

    let actions = tm_platform::win::WinActions;
    let file_io = actions
        .list_process_file_io(pid, Some(start_epoch_s))
        .expect("own process file io with a verified creation time");
    assert!(
        file_io.accounting.handles_complete,
        "the handle walk must still report this process's files"
    );

    let rows_read: u64 = file_io
        .files
        .iter()
        .filter_map(|file| file.read_bytes)
        .sum();
    let rows_written: u64 = file_io
        .files
        .iter()
        .filter_map(|file| file.write_bytes)
        .sum();
    let accounting = &file_io.accounting;
    if accounting.state == FileIoAccountingState::Measured {
        assert!(
            accounting.since_ms > 0 || !file_io.files.is_empty(),
            "a measured snapshot says how long it has been accounting"
        );
        assert_eq!(
            rows_read + accounting.unnamed_read_bytes + accounting.omitted_read_bytes,
            accounting.read_bytes,
            "rows plus the remainders are the whole attribution"
        );
        assert_eq!(
            rows_written + accounting.unnamed_write_bytes + accounting.omitted_write_bytes,
            accounting.write_bytes,
            "rows plus the remainders are the whole attribution"
        );
        eprintln!(
            "per-file trace measured {} rows in {} ms",
            file_io.files.len(),
            accounting.since_ms
        );
    } else {
        // Two states mean "no delivered accounting": `Unavailable` when the host
        // has no source at all (an unelevated session), and `Undelivered` when a
        // session exists but has delivered nothing yet. A CI runner is elevated,
        // so the session starts there and its kernel provider may stay silent;
        // that is a missing source, not a measured zero. Only `Measured` may
        // carry bytes, which the assertions below pin either way.
        assert!(
            matches!(
                accounting.state,
                FileIoAccountingState::Unavailable | FileIoAccountingState::Undelivered
            ),
            "a host without delivered accounting reports Unavailable or Undelivered, not {:?}",
            accounting.state
        );
        assert!(
            file_io
                .files
                .iter()
                .all(|file| file.read_bytes.is_none() && file.write_bytes.is_none()),
            "without a source no row may carry a per-file count"
        );
        assert_eq!(accounting.read_bytes, 0);
        assert_eq!(accounting.write_bytes, 0);
        // `since_ms` is the age of the trace session, so a running session that
        // has not delivered a transfer reports how long it has been waiting
        // rather than zero. Only a host with no session has no window at all.
        if accounting.state == FileIoAccountingState::Unavailable {
            assert_eq!(
                accounting.since_ms, 0,
                "a host with no session reports no accounting window"
            );
        }
        eprintln!(
            "no per-file accounting delivered here ({:?}); accounting stayed empty",
            accounting.state
        );
    }
}

/// Regression: the pre-copy `GetFileType` short-circuit resolved the child's
/// handle VALUES in this process's handle table, so a foreign process's files
/// were dropped unless a local handle happened to share the value and be a
/// disk file. Only a child process can show the end-to-end effect.
///
/// `type` opens the file inside `cmd.exe` and blocks writing it into a stdout
/// pipe nobody drains, which is what keeps the handle open: no sleep, and the
/// open handle is the child's own, not one this process could have made.
#[cfg(target_os = "windows")]
#[test]
fn files_of_a_child_process_survive_handle_type_probing() {
    /// Kills and reaps the holder however this test leaves.
    struct ChildHolder(std::process::Child);
    impl Drop for ChildHolder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let path = std::env::temp_dir().join(format!("tm_child_file_io_{}.bin", std::process::id()));
    // Larger than any anonymous pipe buffer, so the child cannot finish the
    // copy and close the file before it is enumerated.
    std::fs::write(&path, vec![0u8; 1 << 20]).expect("write the held file");

    let child = std::process::Command::new("cmd")
        .args(["/C", "type"])
        .arg(&path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let holder = match child {
        Ok(child) => ChildHolder(child),
        Err(error) => {
            eprintln!("cannot spawn a child to hold a file open: {error}");
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let pid = holder.0.id();
    // The kernel reports the file's long, fully qualified path, while
    // `temp_dir()` can hand back the 8.3 short form (`C:\Users\RUNNER~1\...`) on
    // CI runners, so both sides are resolved before they are compared.
    // `canonicalize` re-adds the verbatim prefix the collector strips.
    let expected = std::fs::canonicalize(&path)
        .map(|resolved| {
            let text = resolved.to_string_lossy().into_owned();
            text.strip_prefix(r"\\?\")
                .map(str::to_string)
                .unwrap_or(text)
        })
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
        .to_ascii_lowercase();

    let actions = tm_platform::win::WinActions;
    // What the last walk reported or why it failed, so a timeout can name the
    // case it hit instead of reading as "a complete walk lacked the file".
    let mut last_walk: Option<Result<(bool, bool), String>> = None;
    let snapshot = poll_for(
        || {
            let snapshot = match actions.list_process_file_io(pid, None) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    last_walk = Some(Err(error.to_string()));
                    return None;
                }
            };
            let holds_file = snapshot
                .files
                .iter()
                .any(|file| file.path.to_ascii_lowercase() == expected);
            last_walk = Some(Ok((holds_file, snapshot.accounting.handles_complete)));
            // A walk can legitimately report itself incomplete: the child opens
            // and closes handles of its own while it is being read, a protected
            // handle cannot be duplicated, and a disk handle whose path,
            // position or access cannot be read leaves the flag down. The
            // regression this pins is that the child's file is reported at all,
            // so completeness is recorded for the failure message rather than
            // demanded here.
            holds_file.then(|| snapshot.clone())
        },
        std::time::Duration::from_secs(10),
    );

    drop(holder);
    let _ = std::fs::remove_file(&path);

    let snapshot = match snapshot {
        Some(snapshot) => snapshot,
        None => {
            let observed = match last_walk {
                Some(Ok((holds_file, complete))) => format!(
                    "the last walk read the handle table \
                     (holds_file={holds_file}, handles_complete={complete})"
                ),
                Some(Err(error)) => format!("the last walk failed: {error}"),
                None => "no walk ran".to_string(),
            };
            panic!("child {pid} never reported the held file: {observed}");
        }
    };
    let row = snapshot
        .files
        .iter()
        .find(|file| file.path.to_ascii_lowercase() == expected)
        .expect("the found file is still there");
    assert!(row.is_read, "`type` opened the file for reading: {row:?}");
    // A live child always holds handles this process cannot duplicate. That must
    // not clear the enumeration flag: the I/O page renders the list only when the
    // flag is set, so treating an un-inspectable handle as a gap blanked the page
    // for every process.
    assert!(
        snapshot.accounting.handles_complete,
        "an un-inspectable handle must not clear the enumeration flag"
    );
}

/// The class procedure for the throwaway window below. The presentation gate's
/// subclass runs ahead of it, so this only has to be a valid class procedure.
unsafe extern "system" fn present_gate_test_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

/// Drain this thread's queue, which is what actually runs the gate's subclass.
/// `ShowWindowAsync` only posts, so nothing happens until somebody pumps.
fn pump_window_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// The presentation gate must keep a window the desktop is showing off the
/// compositor, and give its client area the app's colour in the meantime.
///
/// A window of its own class, because the gate sets `GCLP_HBRBACKGROUND` on
/// whatever class the window belongs to: the predefined classes are shared with
/// the rest of the process and with user32, and mutating `STATIC`'s background
/// would be a side effect on somebody else's window. The window is a tool
/// window with no activation so it never takes focus or a taskbar button, and it
/// lives for the length of this one test.
#[test]
fn the_presentation_gate_keeps_a_shown_window_cloaked_until_release() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CW_USEDEFAULT, CreateWindowExW, DestroyWindow, GCLP_HBRBACKGROUND, GetClassLongPtrW,
        IsIconic, IsWindowVisible, RegisterClassW, SW_MINIMIZE, SW_RESTORE, SW_SHOW,
        ShowWindowAsync, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None) }.expect("own module").0);
    let class_name = windows::core::w!("TaskManPresentGateIntegrationTest");
    let class = WNDCLASSW {
        lpfnWndProc: Some(present_gate_test_proc),
        hInstance: hinstance,
        lpszClassName: class_name,
        ..Default::default()
    };
    assert_ne!(
        unsafe { RegisterClassW(&class) },
        0,
        "throwaway window class must register"
    );
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            windows::core::w!("TaskMan presentation gate test"),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            200,
            150,
            None,
            None,
            Some(hinstance),
            None,
        )
    }
    .expect("throwaway window");
    let raw = hwnd.0 as isize;

    let wakes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&wakes);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        counted.fetch_add(1, Ordering::Relaxed);
    });

    assert!(
        !tm_platform::is_window_cloaked(raw),
        "the fixture window starts uncloaked"
    );
    assert_eq!(
        unsafe { GetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND) },
        0,
        "the fixture class starts with no background brush"
    );

    tm_platform::install_present_gate(raw, [0x19, 0x1a, 0x1b], wake);
    assert!(tm_platform::present_gate_held(), "install holds the window");
    assert!(
        tm_platform::is_window_cloaked(raw),
        "install must cloak before the window can ever be composed"
    );
    assert_ne!(
        unsafe { GetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND) },
        0,
        "the class must get the app's brush, so no erase can be the system white"
    );

    // Release the install hold BEFORE the show, so the cloak asserted after it
    // can only be the subclass's. This is the case that matters: a window that
    // becomes visible while the gate holds nothing (a tray show, a cold start
    // after the first release) has nothing but the WndProc between it and the
    // compositor.
    assert!(
        tm_platform::present_gate_release(raw),
        "the install hold must be releasable"
    );
    assert!(
        !tm_platform::is_window_cloaked(raw),
        "the release must uncloak the window"
    );

    // A show request is not allowed to reach the compositor.
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_SHOW);
    }
    let shown = poll_for(
        || {
            pump_window_messages();
            unsafe { IsWindowVisible(hwnd) }.as_bool().then_some(())
        },
        std::time::Duration::from_secs(5),
    );
    assert!(shown.is_some(), "the window never became visible");
    pump_window_messages();
    assert!(
        tm_platform::is_window_cloaked(raw),
        "WM_SHOWWINDOW must re-cloak before Windows composes the window"
    );
    assert!(
        tm_platform::present_gate_needs_frame(),
        "the transition must leave the app owing a frame"
    );
    assert!(
        wakes.load(Ordering::Relaxed) > 0,
        "the transition must wake the app so a frame follows"
    );
    assert!(
        tm_platform::present_gate_take_transition(),
        "the transition is reported to the app exactly once"
    );
    assert!(!tm_platform::present_gate_needs_frame());

    assert!(
        tm_platform::present_gate_release(raw),
        "a hold must be released"
    );
    assert!(!tm_platform::present_gate_held());
    assert!(
        !tm_platform::is_window_cloaked(raw),
        "release must uncloak the window"
    );
    assert!(
        !tm_platform::present_gate_release(raw),
        "release must not act without a hold"
    );

    // The taskbar path: minimize, then restore while the gate holds.
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_MINIMIZE);
    }
    let minimized = poll_for(
        || {
            pump_window_messages();
            unsafe { IsIconic(hwnd) }.as_bool().then_some(())
        },
        std::time::Duration::from_secs(5),
    );
    assert!(minimized.is_some(), "the window never minimized");
    tm_platform::present_gate_hold(raw);
    assert!(tm_platform::present_gate_held());
    unsafe {
        let _ = ShowWindowAsync(hwnd, SW_RESTORE);
    }
    let restored = poll_for(
        || {
            pump_window_messages();
            let iconic = unsafe { IsIconic(hwnd) }.as_bool();
            (!iconic).then_some(())
        },
        std::time::Duration::from_secs(5),
    );
    assert!(restored.is_some(), "the window never restored");
    pump_window_messages();
    assert!(
        tm_platform::is_window_cloaked(raw),
        "a restore must stay cloaked until the app has a frame"
    );
    assert!(
        tm_platform::present_gate_take_transition(),
        "the restore must ask for a frame"
    );
    assert!(tm_platform::present_gate_release(raw));

    tm_platform::remove_present_gate();
    assert!(!tm_platform::present_gate_held());
    assert_eq!(
        unsafe { GetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND) },
        0,
        "removal must hand the class its old background back"
    );
    tm_platform::remove_present_gate();
    assert!(!tm_platform::present_gate_held(), "removal is idempotent");

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}
