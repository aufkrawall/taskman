//! PDH (Performance Data Helper) wrappers: GPU engine/process counters and
//! physical disk activity, plus misc system-wide stats.

use std::collections::HashMap;
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows::core::PCWSTR;

/// Per-disk performance sample.
#[derive(Debug, Clone, Default)]
pub struct DiskPerf {
    /// Raw instance name, e.g. "0 C:".
    pub instance: String,
    pub active_pct: f32,
    pub read_bps: f64,
    pub write_bps: f64,
    pub avg_resp_ms: f32,
}

impl DiskPerf {
    pub fn matches_mount(&self, mount: &str) -> bool {
        let m = mount.trim_end_matches(['\\', '/']).to_uppercase();
        if m.is_empty() {
            return false;
        }
        // instance "0 C:" ends with the drive letter
        self.instance.to_uppercase().ends_with(&m)
    }

    #[allow(dead_code)]
    pub fn matches_index_prefix(&self, _id: &str) -> bool {
        false
    }
}

/// Per-process GPU sample.
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuProc {
    pub util_pct: f32,
    pub mem_bytes: u64,
}

struct Counter {
    handle: PDH_HCOUNTER,
    kind: CounterKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CounterKind {
    GpuEngine,
    GpuProcMem,
    DiskIdle,
    DiskRead,
    DiskWrite,
    DiskSec,
}

/// Persistent PDH query with wildcard counters added once.
pub struct Pdh {
    query: Option<PDH_HQUERY>,
    counters: Vec<Counter>,
    gpu_available: bool,
    disk_available: bool,
    /// True after two collections — required for rate-type counters.
    warm: bool,
    collections_done: u32,
    /// Reused transfer buffer for `PdhGetFormattedCounterArrayW`.
    scratch: Vec<u8>,
}

// SAFETY: PDH handles are process-global and only used from the engine thread
// (guarded by the Mutex around `Pdh`); the raw pointers are opaque integers.
unsafe impl Send for Pdh {}

impl Default for Pdh {
    fn default() -> Self {
        Self::new()
    }
}

impl Pdh {
    /// Cheap constructor — the actual query is opened lazily on first use so
    /// process startup does not pay the counter-registration cost.
    pub fn new() -> Self {
        Self {
            warm: false,
            query: None,
            counters: Vec::new(),
            gpu_available: true,
            disk_available: true,
            collections_done: 0,
            scratch: Vec::new(),
        }
    }

    fn try_open(&mut self) {
        if self.query.is_some() {
            return;
        }
        unsafe {
            let mut q = PDH_HQUERY::default();
            let status = PdhOpenQueryW(PCWSTR::null(), 0, &mut q);
            if status != 0 {
                tracing::debug!(status, "PdhOpenQueryW failed; perf counters unavailable");
                return;
            }
            let mut counters = Vec::new();
            for (path, kind) in [
                (
                    "\\GPU Engine(*)\\Utilization Percentage",
                    CounterKind::GpuEngine,
                ),
                (
                    "\\GPU Process Memory(*)\\Local Usage",
                    CounterKind::GpuProcMem,
                ),
                ("\\PhysicalDisk(*)\\% Idle Time", CounterKind::DiskIdle),
                (
                    "\\PhysicalDisk(*)\\Disk Read Bytes/sec",
                    CounterKind::DiskRead,
                ),
                (
                    "\\PhysicalDisk(*)\\Disk Write Bytes/sec",
                    CounterKind::DiskWrite,
                ),
                (
                    "\\PhysicalDisk(*)\\Avg. Disk sec/Transfer",
                    CounterKind::DiskSec,
                ),
            ] {
                let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
                let mut hcounter = PDH_HCOUNTER::default();
                let status =
                    PdhAddEnglishCounterW(q, PCWSTR::from_raw(wide.as_ptr()), 0, &mut hcounter);
                if status != 0 {
                    match kind {
                        CounterKind::GpuEngine | CounterKind::GpuProcMem => {
                            self.gpu_available = false;
                        }
                        _ => self.disk_available = false,
                    }
                    tracing::debug!(status, path, "PdhAddEnglishCounterW failed");
                    continue;
                }
                counters.push(Counter {
                    handle: hcounter,
                    kind,
                });
            }
            tracing::info!(
                counters = counters.len(),
                gpu = self.gpu_available,
                disks = self.disk_available,
                "PDH query opened"
            );
            self.query = Some(q);
            self.counters = counters;
            self.warm = false;
            self.collections_done = 0;
        }
    }

    /// Collect exactly once per sampling tick; all subsequent reads in that
    /// tick reuse the collected data. Returns false while unavailable/warming.
    pub fn collect_once(&mut self) -> bool {
        if !self.gpu_available && !self.disk_available {
            return false;
        }
        self.try_open();
        let Some(q) = self.query else { return false };
        unsafe {
            if PdhCollectQueryData(q) != 0 {
                return false;
            }
        }
        self.collections_done = self.collections_done.saturating_add(1);
        if !self.warm {
            // Rate counters need two samples before values are valid.
            if self.collections_done >= 2 {
                self.warm = true;
            } else {
                return false;
            }
        }
        true
    }

    fn read_pairs(
        &mut self,
        counter_handle: windows::Win32::System::Performance::PDH_HCOUNTER,
    ) -> Vec<(String, f64)> {
        let mut out = Vec::new();
        unsafe {
            let mut size: u32 = 0;
            let mut count: u32 = 0;
            let _status = PdhGetFormattedCounterArrayW(
                counter_handle,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                None,
            );
            if size == 0 {
                return out;
            }
            // Reuse the scratch buffer across counter reads (same tick and
            // across ticks) to avoid a heap allocation per counter.
            self.scratch.clear();
            self.scratch.resize(size as usize, 0);
            let buf = &mut self.scratch;
            let mut count2: u32 = 0;
            let status = PdhGetFormattedCounterArrayW(
                counter_handle,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count2,
                Some(buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
            );
            if status != 0 {
                return out;
            }
            let items = std::slice::from_raw_parts(
                buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                count2 as usize,
            );
            for item in items {
                let name = item.szName.to_string().unwrap_or_default();
                let val = item.FmtValue.Anonymous.doubleValue;
                out.push((name, val));
            }
        }
        out
    }

    /// Snapshot of `(handle, kind)` pairs so counter reads can borrow `self`
    /// mutably (scratch buffer) without conflicting with the counters list.
    fn counter_list(&self) -> Vec<(PDH_HCOUNTER, CounterKind)> {
        self.counters.iter().map(|c| (c.handle, c.kind)).collect()
    }

    /// Per-process GPU stats from data already collected this tick via
    /// [`collect_once`]. Returns None when GPU counters are unavailable.
    pub fn read_gpu_process_stats(&mut self) -> Option<HashMap<u32, GpuProc>> {
        if !self.gpu_available || !self.warm {
            return None;
        }
        let mut map: HashMap<u32, GpuProc> = HashMap::new();
        for (handle, kind) in self.counter_list() {
            match kind {
                CounterKind::GpuEngine => {
                    for (name, v) in self.read_pairs(handle) {
                        if let Some(pid) = parse_instance_pid(&name) {
                            let e = map.entry(pid).or_default();
                            e.util_pct += v as f32;
                        }
                    }
                }
                CounterKind::GpuProcMem => {
                    for (name, v) in self.read_pairs(handle) {
                        if let Some(pid) = parse_instance_pid(&name) {
                            let e = map.entry(pid).or_default();
                            e.mem_bytes += v.max(0.0) as u64;
                        }
                    }
                }
                _ => {}
            }
        }
        for g in map.values_mut() {
            g.util_pct = g.util_pct.clamp(0.0, 100.0);
        }
        Some(map)
    }

    /// Engine utilization aggregated by engine type across all GPUs.
    pub fn read_engine_stats(&mut self) -> Vec<tm_core::model::GpuEngine> {
        if !self.gpu_available || !self.warm {
            return Vec::new();
        }
        let mut totals: HashMap<String, f32> = HashMap::new();
        for (handle, kind) in self.counter_list() {
            if kind != CounterKind::GpuEngine {
                continue;
            }
            for (name, v) in self.read_pairs(handle) {
                let eng = parse_engtype(&name).unwrap_or_else(|| "Other".into());
                *totals.entry(eng).or_insert(0.0) += v as f32;
            }
        }
        let mut out: Vec<tm_core::model::GpuEngine> = totals
            .into_iter()
            .map(|(name, util)| tm_core::model::GpuEngine {
                name,
                util_pct: util.clamp(0.0, 100.0),
            })
            .collect();
        out.sort_by(|a, b| {
            b.util_pct
                .partial_cmp(&a.util_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(6);
        out
    }

    /// Disk performance samples from data already collected this tick.
    pub fn read_disks(&mut self) -> Vec<DiskPerf> {
        if !self.disk_available || !self.warm {
            return Vec::new();
        }
        let mut idle: HashMap<String, f64> = HashMap::new();
        let mut read: HashMap<String, f64> = HashMap::new();
        let mut write: HashMap<String, f64> = HashMap::new();
        let mut sec: HashMap<String, f64> = HashMap::new();
        for (handle, kind) in self.counter_list() {
            match kind {
                CounterKind::DiskIdle => {
                    for (n, v) in self.read_pairs(handle) {
                        idle.insert(n, v);
                    }
                }
                CounterKind::DiskRead => {
                    for (n, v) in self.read_pairs(handle) {
                        read.insert(n, v);
                    }
                }
                CounterKind::DiskWrite => {
                    for (n, v) in self.read_pairs(handle) {
                        write.insert(n, v);
                    }
                }
                CounterKind::DiskSec => {
                    for (n, v) in self.read_pairs(handle) {
                        sec.insert(n, v);
                    }
                }
                _ => {}
            }
        }
        let mut instances: Vec<String> = idle.keys().cloned().collect();
        instances.sort();
        instances
            .into_iter()
            .map(|inst| {
                let idle_pct = idle.get(&inst).copied().unwrap_or(100.0);
                DiskPerf {
                    active_pct: (100.0 - idle_pct).clamp(0.0, 100.0) as f32,
                    read_bps: read.get(&inst).copied().unwrap_or(0.0).max(0.0),
                    write_bps: write.get(&inst).copied().unwrap_or(0.0).max(0.0),
                    avg_resp_ms: (sec.get(&inst).copied().unwrap_or(0.0).max(0.0) * 1000.0) as f32,
                    instance: inst,
                }
            })
            .collect()
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        if let Some(q) = self.query {
            unsafe {
                let _ = PdhCloseQuery(q);
            }
        }
    }
}

/// Parse "pid_<pid>_luid_..." from a GPU counter instance name.
fn parse_instance_pid(instance: &str) -> Option<u32> {
    let rest = instance.strip_prefix("pid_")?;
    let pid_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    pid_str.parse().ok()
}

fn parse_engtype(instance: &str) -> Option<String> {
    let idx = instance.find("engtype_")?;
    Some(instance[idx + "engtype_".len()..].to_string())
}

// ------------------------------------------------------------------ free helpers

/// Memory details via GlobalMemoryStatusEx + GetPerformanceInfo.
pub struct WinMemory {
    pub cached: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub paged_pool: u64,
    pub non_paged_pool: u64,
}

pub fn query_windows_memory() -> WinMemory {
    let mut perf = PERFORMANCE_INFORMATION::default();
    unsafe {
        if GetPerformanceInfo(
            &mut perf,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
        .is_err()
        {
            return WinMemory {
                cached: 0,
                commit_total: 0,
                commit_limit: 0,
                paged_pool: 0,
                non_paged_pool: 0,
            };
        }
        let page = perf.PageSize as u64;
        WinMemory {
            cached: perf.SystemCache as u64 * page,
            commit_total: perf.CommitTotal as u64 * page,
            commit_limit: perf.CommitLimit as u64 * page,
            paged_pool: perf.KernelPaged as u64 * page,
            non_paged_pool: perf.KernelNonpaged as u64 * page,
        }
    }
}

/// System-wide handle/thread counts from GetPerformanceInfo.
pub fn global_handle_thread_count() -> (usize, usize) {
    let mut perf = PERFORMANCE_INFORMATION::default();
    unsafe {
        if GetPerformanceInfo(
            &mut perf,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
        .is_err()
        {
            return (0, 0);
        }
        (perf.HandleCount as usize, perf.ThreadCount as usize)
    }
}
