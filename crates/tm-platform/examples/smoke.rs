fn main() {
    let mut collector = tm_platform::create_collector();
    let actions = tm_platform::create_actions();
    println!("backend: {}", collector.backend_name());
    let snap = collector.sample(std::time::Instant::now()).unwrap();
    println!(
        "cpu: {} util, {} logical, brand={}",
        snap.cpu.utilization_pct as u32,
        snap.cpu.logical_count,
        &snap.cpu.brand[..snap.cpu.brand.len().min(30)]
    );
    println!(
        "mem: {} MB / {} MB",
        snap.memory.used_bytes / 1048576,
        snap.memory.total_bytes / 1048576
    );
    println!("processes: {}", snap.processes.len());
    println!(
        "disks: {:?}",
        snap.disks
            .iter()
            .map(|d| d.mount.clone())
            .collect::<Vec<_>>()
    );
    println!("nets: {}", snap.networks.len());
    println!(
        "gpus: {:?}",
        snap.gpus
            .iter()
            .map(|g| (g.name.clone(), g.util_pct))
            .collect::<Vec<_>>()
    );
    let apps = snap
        .processes
        .iter()
        .filter(|p| p.category == tm_core::model::ProcCategory::App)
        .count();
    println!("apps w/ windows: {apps}");
    let svcs = actions.list_services().map_or(0, |v| v.len());
    println!("services: {svcs}");
    println!(
        "startup items: {}",
        actions.list_startup().map_or(0, |v| v.len())
    );
    println!(
        "sessions: {}",
        actions.list_user_sessions().map_or(0, |v| v.len())
    );
}
