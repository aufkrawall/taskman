//! Integration tests running against the REAL system.
//! These exercise the platform layer end-to-end without a GUI.

use tm_core::engine::SystemCollector;
use tm_core::model::*;

#[test]
fn sample_produces_sane_snapshot() {
    let (mut collector, _actions) = tm_platform::create_stack();
    // Two ticks so rates/deltas have a previous point.
    let s1 = collector.sample(std::time::Instant::now()).expect("tick 1");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let s2 = collector.sample(std::time::Instant::now()).expect("tick 2");

    assert!(s2.cpu.logical_count >= 1);
    assert!(!s2.cpu.brand.is_empty());
    assert!(s2.memory.total_bytes > 1024 * 1024 * 1024, "total memory sane");
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
    let (mut collector, actions) = tm_platform::create_stack();
    let snap = collector.sample(std::time::Instant::now()).unwrap();

    // Services enumerate in bulk.
    let services = actions.list_services().expect("services");
    assert!(services.len() > 50);
    assert!(services.iter().any(|s| !s.name.is_empty()));
    assert!(services
        .iter()
        .any(|s| matches!(s.status, ServiceStatus::Running | ServiceStatus::Stopped)));

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
    assert!(snap.processes.iter().any(|p| p.category == ProcCategory::System));
}

#[cfg(target_os = "windows")]
#[test]
fn kill_spawned_child() {
    let (_c, actions) = tm_platform::create_stack();

    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    let pid = child.id();
    std::thread::sleep(std::time::Duration::from_millis(200));

    actions.kill_single(pid).expect("kill child");
    // Give it a moment; then verify exit.
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    panic!("child did not exit after TerminateProcess");
}

#[cfg(target_os = "windows")]
#[test]
fn suspend_resume_own_child() {
    let (_c, actions) = tm_platform::create_stack();
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "ping", "-n", "10", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let pid = child.id();
    std::thread::sleep(std::time::Duration::from_millis(150));

    actions.suspend_process(pid, true).expect("suspend");
    actions.suspend_process(pid, false).expect("resume");
    let _ = actions.kill_single(pid);
    let _ = child.wait();
}
