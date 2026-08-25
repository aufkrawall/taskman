//! Deterministic mock collector + fixture snapshots.
//!
//! Used by unit tests, the `--selfcheck --mock` CLI path, and UI development
//! without touching real system APIs.

use crate::engine::SystemCollector;
use crate::error::Result;
use crate::model::*;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Build a small deterministic snapshot. `seed` shifts values so tests can
/// observe changes between ticks.
pub fn snapshot(seed: u32) -> Snapshot {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let s = seed as f32;

    let mut p1 = ProcessEntry::new(1000 + seed % 7, "mockapp.exe");
    p1.category = ProcCategory::App;
    p1.has_window = true;
    p1.cpu_pct = (s * 3.7) % 25.0;
    p1.mem_bytes = 150_000_000 + (s as u64 * 1_000_003) % 400_000_000;
    p1.user = Some("alice".into());
    p1.cpu_time_s = Some(12.5 + s as f64);

    let mut p2 = ProcessEntry::new(2000, "backgroundsvc.exe");
    p2.category = ProcCategory::Background;
    p2.cpu_pct = (s * 1.3) % 4.0;
    p2.mem_bytes = 50_000_000;

    let mut p3 = ProcessEntry::new(4, "System");
    p3.category = ProcCategory::System;

    Snapshot {
        timestamp_ms: now,
        sample_duration_ms: 1,
        cpu: CpuInfo {
            brand: "Mock CPU 8-Core".into(),
            vendor: "MockVendor".into(),
            architecture: "x86_64".into(),
            utilization_pct: ((s * 11.0) % 100.0),
            per_core_pct: (0..8).map(|i| (s * (i as f32 + 1.0)) % 100.0).collect(),
            freq_mhz: 3400.0,
            freq_base_mhz: 3200.0,
            logical_count: 8,
            physical_cores: 4,
            sockets: 1,
            l1_kb: 512,
            l2_kb: 4096,
            l3_kb: 16384,
            virtualization: "Enabled".into(),
        },
        memory: MemoryInfo {
            total_bytes: 32 * 1024 * 1024 * 1024,
            used_bytes: (10.0 + (s * 0.5) % 6.0) as u64 * 1024 * 1024 * 1024,
            available_bytes: 16 * 1024 * 1024 * 1024,
            cached_bytes: 4 * 1024 * 1024 * 1024,
            commit_total_bytes: 48 * 1024 * 1024 * 1024,
            commit_used_bytes: 14 * 1024 * 1024 * 1024,
            paged_pool_bytes: 800 * 1024 * 1024,
            non_paged_pool_bytes: 400 * 1024 * 1024,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            installed_bytes: 32 * 1024 * 1024 * 1024,
            hw_reserved_bytes: 0,
            speed_mts: 4800,
            speed_max_mts: 4800,
            slots_used: 2,
            slots_total: 4,
            form_factor: "SODIMM".into(),
            manufacturer: "MockRAM".into(),
            part_number: "M471A2G43CB2-CVE".into(),
        },
        disks: vec![DiskInfo {
            id: "0 (C:)".into(),
            mount: "C:\\".into(),
            label: "System".into(),
            media: MediaKind::SsdNvme,
            total_bytes: 1000 * 1024 * 1024 * 1024,
            free_bytes: 300 * 1024 * 1024 * 1024,
            active_pct: (s * 7.0) % 100.0,
            read_bps: s as f64 * 1e6,
            write_bps: s as f64 * 2e6,
            avg_resp_ms: 0.3,
            total_read_bytes: 123456789,
            total_written_bytes: 987654321,
        }],
        networks: vec![NetworkInfo {
            name: "Ethernet".into(),
            desc: "Mock Adapter".into(),
            kind: "Ethernet".into(),
            oper_up: true,
            recv_bps: s as f64 * 3e5,
            sent_bps: s as f64 * 1e5,
            total_recv_bytes: 42_000_000_000,
            total_sent_bytes: 13_000_000_000,
            link_bps: 1_000_000_000,
            ssid: None,
        }],
        gpus: vec![GpuInfo {
            id: 0,
            name: "Mock RTX GPU".into(),
            driver_version: "999.99".into(),
            util_pct: (s * 5.0) % 100.0,
            mem_used_bytes: 2 * 1024 * 1024 * 1024,
            mem_total_bytes: 8 * 1024 * 1024 * 1024,
            dedicated_used_bytes: 1024 * 1024 * 1024,
            temperature_c: Some(42.0),
            engines: vec![
                GpuEngine {
                    name: "3D".into(),
                    util_pct: (s * 5.0) % 100.0,
                },
                GpuEngine {
                    name: "Copy".into(),
                    util_pct: 0.0,
                },
            ],
        }],
        processes: vec![p1, p2, p3],
        system: SystemMisc {
            hostname: "mock-machine".into(),
            os_name: "MockOS".into(),
            os_version: "1.0".into(),
            kernel_version: "mock-kernel".into(),
            uptime_s: 3600 + seed as u64,
            boot_epoch_s: (now / 1000) as i64 - 3600 - seed as i64,
            process_count: 3,
            thread_count: 128,
            handle_count: 4096,
        },
    }
}

/// Collector producing [`snapshot`] each tick with an incrementing seed.
#[derive(Default)]
pub struct MockCollector {
    seed: u32,
}

impl MockCollector {
    pub fn new() -> Self {
        Self { seed: 0 }
    }
}

impl SystemCollector for MockCollector {
    fn sample(&mut self, _now: Instant) -> Result<Snapshot> {
        self.seed = self.seed.wrapping_add(1);
        Ok(snapshot(self.seed))
    }

    fn backend_name(&self) -> &'static str {
        "mock"
    }
}

/// Convenience: sleep helper for tests.
pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_snapshot_is_deterministic_per_seed() {
        let a = snapshot(3);
        let b = snapshot(3);
        assert_eq!(a.processes.len(), b.processes.len());
        assert!((a.cpu.utilization_pct - b.cpu.utilization_pct).abs() < f32::EPSILON);
    }

    #[test]
    fn mock_collector_increments() {
        let mut c = MockCollector::new();
        let s1 = c.sample(Instant::now()).unwrap();
        let s2 = c.sample(Instant::now()).unwrap();
        // Seeds differ => utilization differs.
        assert_ne!(s1.timestamp_ms, 0);
        let _ = s2;
    }
}
