//! Live probe for the time-based CPU accountant: samples the system every
//! 500 ms for 10 s and prints global/per-core load plus the top processes.
//!
//! Run: cargo run -p tm-platform --example cpu_probe

use std::time::Instant;

use tm_core::model::Snapshot;

fn main() {
    let mut collector = tm_platform::create_collector();
    println!("backend: {}", collector.backend_name());

    // Warm-up tick (no reference window yet; load values are zeros).
    let _ = collector.sample(Instant::now()).unwrap();

    let t0 = Instant::now();
    while t0.elapsed().as_secs() < 10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Ok(snap) = collector.sample(Instant::now()) {
            print_tick(&snap, &t0);
        }
    }
}

fn print_tick(last: &Snapshot, t0: &Instant) {
    let cores: String = last
        .cpu
        .per_core_pct
        .iter()
        .map(|c| format!("{:>3}", *c as u32))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "t={:>5}ms  global {:>5.1}%  cores[{cores}]",
        t0.elapsed().as_millis(),
        last.cpu.utilization_pct
    );

    let mut top: Vec<_> = last.processes.iter().filter(|p| p.cpu_pct > 0.05).collect();
    top.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap());
    for p in top.iter().take(5) {
        println!(
            "    pid {:>6}  {:>5.1}%  cpu_time {:>8.1}s  {}",
            p.pid,
            p.cpu_pct,
            p.cpu_time_s.unwrap_or(0.0),
            &p.name[..p.name.len().min(32)]
        );
    }
    if top.is_empty() {
        println!("    (no process above 0.1%)");
    }
}
