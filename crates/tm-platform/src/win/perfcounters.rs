//! PDH (Performance Data Helper) wrappers.
//!
//! Architecture (implement.md §6.4/§13): counters are split into independent
//! query groups — [`GpuPdh`] (GPU Engine + GPU Process Memory) and
//! [`DiskPdh`] (PhysicalDisk). Each group opens its own query lazily and is
//! only kept warm while the UI's [`TelemetryDemand`] asks for it (+ a
//! keep-alive window), so the default Processes page never initializes GPU
//! wildcard counters at all.
//!
//! GPU engine identity is preserved: PDH instance strings carry pid, LUID,
//! physical adapter index, engine index and engine type, parsed into typed
//! records ([`GpuEngineRecord`]/[`GpuMemRecord`]) instead of being summed
//! into one global number.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tm_core::demand::TelemetryDemand;
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows::core::PCWSTR;

/// Keep an expensive provider warm this long after its demand disappeared,
/// so tab flips do not rebuild PDH queries constantly.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// Adapter LUID parsed out of PDH instance names / reported by DXGI.
pub type RawLuid = tm_core::model::AdapterLuid;

// ------------------------------------------------------------------ parsing

/// Typed parse of a GPU Engine PDH instance string, e.g.
/// `pid_1276_luid_0x00000000_0x0000E1A5_phys_0_eng_3_engtype_VideoDecode`.
/// Fields after `engtype_` may contain further underscores; `phys`, `eng`
/// and `engtype` are optional on some builds/instances — unknown shapes are
/// preserved as far as possible instead of dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedGpuInstance {
    pub pid: Option<u32>,
    pub luid: Option<RawLuid>,
    pub phys_index: Option<u32>,
    /// Preserved from the instance string; used by tests and future
    /// engine-level displays.
    #[allow(dead_code)]
    pub engine_index: Option<u32>,
    pub engine_type: Option<String>,
}

pub fn parse_gpu_instance(instance: &str) -> ParsedGpuInstance {
    let mut out = ParsedGpuInstance::default();
    let parts: Vec<&str> = instance.split('_').collect();
    let mut i = 0;
    while i < parts.len() {
        match parts[i] {
            "pid" if i + 1 < parts.len() => {
                out.pid = parts[i + 1].parse().ok();
            }
            "luid" if i + 2 < parts.len() => {
                // Two hex chunks: high part first ("0x00000000"), then low.
                let hi = u32::from_str_radix(parts[i + 1].trim_start_matches("0x"), 16).ok();
                let lo = u32::from_str_radix(parts[i + 2].trim_start_matches("0x"), 16).ok();
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    // DXGI LUID HighPart is an i32 bit pattern.
                    out.luid = Some(RawLuid {
                        high: hi as i32,
                        low: lo,
                    });
                } else {
                    // Non-hex LUID chunks: skip the second chunk so we do not
                    // misparse it as another field.
                    i += 1;
                }
            }
            "phys" if i + 1 < parts.len() => {
                out.phys_index = parts[i + 1].parse().ok();
            }
            "eng" if i + 1 < parts.len() => {
                out.engine_index = parts[i + 1].parse().ok();
            }
            "engtype" => {
                // Everything until the end belongs to the type name.
                out.engine_type = Some(parts[i + 1..].join("_"));
                break;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Utilization of one engine instance of one process on one adapter.
#[derive(Debug, Clone)]
pub struct GpuEngineRecord {
    pub luid: RawLuid,
    pub pid: Option<u32>,
    pub phys_index: Option<u32>,
    /// Preserved from the instance string; used by tests and future
    /// engine-level displays.
    #[allow(dead_code)]
    pub engine_index: Option<u32>,
    /// e.g. "3D", "Copy", "VideoDecode"; unknown types are preserved.
    pub engine_type: String,
    pub utilization_pct: f32,
}

/// Memory usage of one process on one adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuMemRecord {
    pub luid: Option<RawLuid>,
    pub pid: Option<u32>,
    pub dedicated_bytes: u64,
    pub shared_bytes: u64,
}

// ------------------------------------------------------------------ disk

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
}

// ------------------------------------------------------------------ plumbing

struct Counter {
    handle: PDH_HCOUNTER,
    path: &'static str,
}

/// A single lazily-opened PDH query group with rate-counter warm-up.
struct QueryGroup {
    query: Option<PDH_HQUERY>,
    counters: Vec<Counter>,
    collections_done: u32,
    warm: bool,
    scratch: Vec<u8>,
    last_needed: Instant,
}

impl QueryGroup {
    fn new() -> Self {
        Self {
            query: None,
            counters: Vec::new(),
            collections_done: 0,
            warm: false,
            scratch: Vec::new(),
            last_needed: Instant::now(),
        }
    }

    fn open(&mut self, paths: &[&'static str]) {
        if self.query.is_some() || paths.is_empty() {
            return;
        }
        unsafe {
            let mut q = PDH_HQUERY::default();
            let status = PdhOpenQueryW(PCWSTR::null(), 0, &mut q);
            if status != 0 {
                tracing::debug!(
                    status,
                    "PdhOpenQueryW failed; perf counter group unavailable"
                );
                return;
            }
            for path in paths {
                let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
                let mut hcounter = PDH_HCOUNTER::default();
                let status =
                    PdhAddEnglishCounterW(q, PCWSTR::from_raw(wide.as_ptr()), 0, &mut hcounter);
                if status != 0 {
                    tracing::debug!(status, path, "PdhAddEnglishCounterW failed");
                    continue;
                }
                self.counters.push(Counter {
                    handle: hcounter,
                    path,
                });
            }
            tracing::info!(counters = self.counters.len(), "PDH query group opened");
            self.query = Some(q);
            self.collections_done = 0;
            self.warm = false;
        }
    }

    fn close(&mut self) {
        if let Some(q) = self.query.take() {
            unsafe {
                let _ = PdhCloseQuery(q);
            }
        }
        self.counters.clear();
        self.warm = false;
        self.collections_done = 0;
    }

    fn is_open(&self) -> bool {
        self.query.is_some()
    }

    /// Collect exactly once per tick; returns false while unavailable or
    /// still warming (rate counters need two samples).
    fn collect(&mut self) -> bool {
        if !self.is_open() {
            return false;
        }
        self.last_needed = Instant::now();
        let q = self.query.expect("checked");
        unsafe {
            if PdhCollectQueryData(q) != 0 {
                return false;
            }
        }
        self.collections_done = self.collections_done.saturating_add(1);
        if !self.warm {
            if self.collections_done >= 2 {
                self.warm = true;
            } else {
                return false;
            }
        }
        true
    }

    /// Tear down when demand has been gone for longer than the keep-alive.
    fn maybe_sleep(&mut self, needed: bool) {
        if needed {
            self.last_needed = Instant::now();
        } else if self.is_open() && self.last_needed.elapsed() > KEEPALIVE {
            tracing::debug!("PDH query group going to sleep (no demand)");
            self.close();
        }
    }

    fn read_pairs(&mut self, handle: PDH_HCOUNTER) -> Vec<(String, f64)> {
        let mut out = Vec::new();
        unsafe {
            let mut size: u32 = 0;
            let mut count: u32 = 0;
            let _status =
                PdhGetFormattedCounterArrayW(handle, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if size == 0 {
                return out;
            }
            self.scratch.clear();
            self.scratch.resize(size as usize, 0);
            let buf = &mut self.scratch;
            let mut count2: u32 = 0;
            let status = PdhGetFormattedCounterArrayW(
                handle,
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
}

// SAFETY: PDH handles are process-global and only used from the engine thread
// (guarded by the Mutex around `PdhCounters`); the raw pointers are opaque.
unsafe impl Send for QueryGroup {}

const GPU_ENGINE_PATH: &str = "\\GPU Engine(*)\\Utilization Percentage";
const GPU_MEM_DEDICATED: &str = "\\GPU Process Memory(*)\\Dedicated Usage";
const GPU_MEM_SHARED: &str = "\\GPU Process Memory(*)\\Shared Usage";
const DISK_IDLE: &str = "\\PhysicalDisk(*)\\% Idle Time";
const DISK_READ: &str = "\\PhysicalDisk(*)\\Disk Read Bytes/sec";
const DISK_WRITE: &str = "\\PhysicalDisk(*)\\Disk Write Bytes/sec";
const DISK_SEC: &str = "\\PhysicalDisk(*)\\Avg. Disk sec/Transfer";
/// Task-Manager-style current speed source: base clock × this percentage.
/// sysinfo's frequency (CallNtPowerInformation CurrentMhz) reports the fixed
/// nominal clock on modern Windows, so it cannot show real-time speed.
const CPU_PERF_PCT: &str = "\\Processor Information(_Total)\\% Processor Performance";
/// Interrupt/DPC servicing share. Not chargeable to any process, so without
/// it heavy driver work would look like unexplained "ghost" CPU on the
/// process pages (native TM shows it as the "System interrupts" row).
const CPU_INTERRUPT_PCT: &str = "\\Processor Information(_Total)\\% Interrupt Time";

/// Split PDH state: each expensive provider warms up only on demand.
pub struct PdhCounters {
    gpu: Option<QueryGroup>,
    disk: Option<QueryGroup>,
    /// Single-instance CPU speed counter (cheap, but demand-gated like the
    /// rest so the sampling tick stays free of unneeded collections).
    cpu: Option<QueryGroup>,
    /// Interrupt-time counter, kept warm whenever the process tables are
    /// shown so the CPU residual can be split into interrupts vs. rest.
    interrupt: Option<QueryGroup>,
    gpu_failed: bool,
    disk_failed: bool,
    cpu_failed: bool,
    interrupt_failed: bool,
}

impl Default for PdhCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl PdhCounters {
    pub fn new() -> Self {
        Self {
            gpu: None,
            disk: None,
            cpu: None,
            interrupt: None,
            gpu_failed: false,
            disk_failed: false,
            cpu_failed: false,
            interrupt_failed: false,
        }
    }

    /// Collect all *open* groups once per tick. Opening happens here based on
    /// demand; closing (after keep-alive) also happens here.
    pub fn tick(&mut self, demand: TelemetryDemand) {
        // --- GPU group ---
        let want_gpu = demand.any_gpu() && !self.gpu_failed;
        if want_gpu && self.gpu.is_none() {
            let mut g = QueryGroup::new();
            g.open(&[GPU_ENGINE_PATH, GPU_MEM_DEDICATED, GPU_MEM_SHARED]);
            if !g.is_open() {
                // Query itself failed → don't retry forever this session.
                self.gpu_failed = true;
            }
            self.gpu = Some(g);
        }
        if let Some(g) = &mut self.gpu {
            if g.is_open() {
                g.collect();
            }
            g.maybe_sleep(want_gpu);
        }

        // --- Disk group ---
        let want_disk = demand.wants(TelemetryDemand::DISK_RATE) && !self.disk_failed;
        if want_disk && self.disk.is_none() {
            let mut d = QueryGroup::new();
            d.open(&[DISK_IDLE, DISK_READ, DISK_WRITE, DISK_SEC]);
            if !d.is_open() {
                self.disk_failed = true;
            }
            self.disk = Some(d);
        }
        if let Some(d) = &mut self.disk {
            if d.is_open() {
                d.collect();
            }
            d.maybe_sleep(want_disk);
        }

        // --- CPU speed group ---
        let want_cpu = demand.wants(TelemetryDemand::CPU_SPEED) && !self.cpu_failed;
        if want_cpu && self.cpu.is_none() {
            let mut c = QueryGroup::new();
            c.open(&[CPU_PERF_PCT]);
            if !c.is_open() || c.counters.is_empty() {
                // Query itself failed OR the counter path does not exist on
                // this system → fall back to the nominal frequency instead
                // of rendering "—" forever.
                self.cpu_failed = true;
            }
            self.cpu = Some(c);
        }
        if let Some(c) = &mut self.cpu {
            if c.is_open() {
                c.collect();
            }
            c.maybe_sleep(want_cpu);
        }

        // --- Interrupt time group (core demand: needed to explain the CPU
        // residual on the Processes page, not only the Performance page) ---
        let want_interrupt = demand.wants(TelemetryDemand::CORE_PROCESS) && !self.interrupt_failed;
        if want_interrupt && self.interrupt.is_none() {
            let mut g = QueryGroup::new();
            g.open(&[CPU_INTERRUPT_PCT]);
            if !g.is_open() || g.counters.is_empty() {
                self.interrupt_failed = true;
            }
            self.interrupt = Some(g);
        }
        if let Some(g) = &mut self.interrupt {
            if g.is_open() {
                g.collect();
            }
            g.maybe_sleep(want_interrupt);
        }
    }

    fn counters_snapshot(g: &mut QueryGroup) -> Vec<(PDH_HCOUNTER, &'static str)> {
        g.counters.iter().map(|c| (c.handle, c.path)).collect()
    }

    /// LUID-preserving GPU engine samples from data already collected this
    /// tick via [`tick`]. Returns None while GPU counters unavailable/warming.
    pub fn read_gpu_engines(&mut self) -> Option<Vec<GpuEngineRecord>> {
        let g = self.gpu.as_mut()?;
        if !g.is_open() || !g.warm {
            return None;
        }
        let mut out = Vec::new();
        for (handle, path) in Self::counters_snapshot(g) {
            if path != GPU_ENGINE_PATH {
                continue;
            }
            for (instance, v) in g.read_pairs(handle) {
                if v < 0.0 {
                    continue;
                }
                let p = parse_gpu_instance(&instance);
                let Some(luid) = p.luid else { continue };
                out.push(GpuEngineRecord {
                    luid,
                    pid: p.pid,
                    phys_index: p.phys_index,
                    engine_index: p.engine_index,
                    engine_type: p.engine_type.unwrap_or_else(|| "Unknown".into()),
                    utilization_pct: v.clamp(0.0, 100.0) as f32,
                });
            }
        }
        Some(out)
    }

    /// Per-process GPU memory records (dedicated/shared per LUID+PID).
    pub fn read_gpu_memory(&mut self) -> Option<Vec<GpuMemRecord>> {
        let g = self.gpu.as_mut()?;
        if !g.is_open() || !g.warm {
            return None;
        }
        // key: (luid bits, pid)
        let key = |l: Option<RawLuid>, pid: Option<u32>| {
            (
                l.map(|x| ((x.high as i64) << 32) | x.low as i64)
                    .unwrap_or(-1),
                pid.unwrap_or(u32::MAX),
            )
        };
        let mut ded: HashMap<(i64, u32), u64> = HashMap::new();
        let mut shared: HashMap<(i64, u32), u64> = HashMap::new();
        let mut meta: HashMap<(i64, u32), (Option<RawLuid>, Option<u32>)> = HashMap::new();

        for (handle, path) in Self::counters_snapshot(g) {
            match path {
                GPU_MEM_DEDICATED | GPU_MEM_SHARED => {}
                _ => continue,
            }
            let dedicated = path == GPU_MEM_DEDICATED;
            for (instance, v) in g.read_pairs(handle) {
                let p = parse_gpu_instance(&instance);
                let k = key(p.luid, p.pid);
                let slot = if dedicated { &mut ded } else { &mut shared };
                *slot.entry(k).or_insert(0) += v.max(0.0) as u64;
                meta.insert(k, (p.luid, p.pid));
            }
        }
        let mut out: Vec<GpuMemRecord> = meta
            .into_iter()
            .map(|(k, (luid, pid))| GpuMemRecord {
                luid,
                pid,
                dedicated_bytes: ded.get(&k).copied().unwrap_or(0),
                shared_bytes: shared.get(&k).copied().unwrap_or(0),
            })
            .collect();
        out.sort_by_key(|r| (r.luid.map(|l| l.low).unwrap_or(0), r.pid.unwrap_or(0)));
        Some(out)
    }

    /// Average `% Processor Performance` (percent of nominal frequency) from
    /// data already collected this tick. None while the counter is warming
    /// up, sleeping, or unavailable on this system; non-positive raw values
    /// are treated as unusable rather than reported.
    pub fn read_cpu_perf_pct(&mut self) -> Option<f32> {
        let g = self.cpu.as_mut()?;
        if !g.is_open() || !g.warm {
            return None;
        }
        for (handle, path) in Self::counters_snapshot(g) {
            if path != CPU_PERF_PCT {
                continue;
            }
            let pairs = g.read_pairs(handle);
            if pairs.is_empty() {
                continue;
            }
            let usable: Vec<f64> = pairs
                .into_iter()
                .map(|(_, v)| v)
                .filter(|v| *v > 0.0)
                .collect();
            if !usable.is_empty() {
                let avg = usable.iter().sum::<f64>() / usable.len() as f64;
                return Some(avg as f32);
            }
        }
        None
    }

    /// True when the CPU speed counter could not be opened at all this
    /// session (system without the counter) — callers fall back to the
    /// nominal frequency instead of showing "—" forever.
    pub fn cpu_counter_failed(&self) -> bool {
        self.cpu_failed
    }

    /// Interrupt/DPC servicing share (percent of total capacity) from data
    /// already collected this tick. None while unavailable or warming up;
    /// callers must treat that as unknown and never as zero.
    pub fn read_interrupt_pct(&mut self) -> Option<f32> {
        let g = self.interrupt.as_mut()?;
        if !g.is_open() || !g.warm {
            return None;
        }
        for (handle, path) in Self::counters_snapshot(g) {
            if path != CPU_INTERRUPT_PCT {
                continue;
            }
            let pairs = g.read_pairs(handle);
            if pairs.is_empty() {
                continue;
            }
            let usable: Vec<f64> = pairs
                .into_iter()
                .map(|(_, v)| v)
                .filter(|v| *v >= 0.0)
                .collect();
            if !usable.is_empty() {
                let avg = usable.iter().sum::<f64>() / usable.len() as f64;
                return Some((avg as f32).clamp(0.0, 100.0));
            }
        }
        None
    }

    /// Disk performance samples from data already collected this tick.
    pub fn read_disks(&mut self) -> Vec<DiskPerf> {
        let Some(d) = self.disk.as_mut() else {
            return Vec::new();
        };
        if !d.is_open() || !d.warm {
            return Vec::new();
        }
        let mut idle: HashMap<String, f64> = HashMap::new();
        let mut read: HashMap<String, f64> = HashMap::new();
        let mut write: HashMap<String, f64> = HashMap::new();
        let mut sec: HashMap<String, f64> = HashMap::new();
        for (handle, path) in Self::counters_snapshot(d) {
            match path {
                DISK_IDLE => {
                    for (n, v) in d.read_pairs(handle) {
                        idle.insert(n, v);
                    }
                }
                DISK_READ => {
                    for (n, v) in d.read_pairs(handle) {
                        read.insert(n, v);
                    }
                }
                DISK_WRITE => {
                    for (n, v) in d.read_pairs(handle) {
                        write.insert(n, v);
                    }
                }
                DISK_SEC => {
                    for (n, v) in d.read_pairs(handle) {
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

impl Drop for PdhCounters {
    fn drop(&mut self) {
        if let Some(mut g) = self.gpu.take() {
            g.close();
        }
        if let Some(mut d) = self.disk.take() {
            d.close();
        }
        if let Some(mut c) = self.cpu.take() {
            c.close();
        }
        if let Some(mut g) = self.interrupt.take() {
            g.close();
        }
    }
}

impl Drop for QueryGroup {
    fn drop(&mut self) {
        self.close();
    }
}

// ------------------------------------------------------------------ misc

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_pdh_instance_parser_extracts_pid_luid_engine() {
        let p = parse_gpu_instance(
            "pid_1276_luid_0x00000000_0x0000E1A5_phys_0_eng_3_engtype_VideoDecode",
        );
        assert_eq!(p.pid, Some(1276));
        assert_eq!(
            p.luid,
            Some(RawLuid {
                high: 0,
                low: 0xE1A5
            })
        );
        assert_eq!(p.phys_index, Some(0));
        assert_eq!(p.engine_index, Some(3));
        assert_eq!(p.engine_type.as_deref(), Some("VideoDecode"));

        // Real-world shape with a nonzero high part.
        let p2 = parse_gpu_instance("pid_4_luid_0x000012AB_0x0000DEAD_phys_1_eng_0_engtype_3D");
        assert_eq!(
            p2.luid,
            Some(RawLuid {
                high: 0x12AB,
                low: 0xDEAD
            })
        );
        assert_eq!(p2.phys_index, Some(1));
        assert_eq!(p2.engine_type.as_deref(), Some("3D"));

        // Unknown engine types are preserved verbatim (not dropped).
        let p3 = parse_gpu_instance(
            "pid_99_luid_0x00000000_0x00000001_phys_0_eng_7_engtype_Something_New",
        );
        assert_eq!(p3.engine_type.as_deref(), Some("Something_New"));

        // Degraded shapes still yield what they can.
        let p4 = parse_gpu_instance("pid_42_luid_0x00000000_0x00000002");
        assert_eq!(p4.pid, Some(42));
        assert_eq!(p4.engine_type, None);
        assert_eq!(p4.phys_index, None);
    }

    // Live counter: needs two collections before a format succeeds, then
    // reports a sane percentage of the nominal frequency (real system, fast).
    #[test]
    fn cpu_perf_counter_warms_and_reads_sane_percentage() {
        let mut pdh = PdhCounters::new();
        let demand = TelemetryDemand::core().union(TelemetryDemand::CPU_SPEED);
        pdh.tick(demand);
        assert!(pdh.read_cpu_perf_pct().is_none(), "must be warming");
        if pdh.cpu_counter_failed() {
            // Exotic system without Processor Information counters.
            return;
        }
        // The counter is usable after the second collection; poll with a
        // bounded deadline instead of assuming one more tick always suffices.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let pct = loop {
            pdh.tick(demand);
            if let Some(pct) = pdh.read_cpu_perf_pct() {
                break pct;
            }
            assert!(std::time::Instant::now() < deadline, "counter never warmed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(pct > 0.0 && pct < 10_000.0, "implausible {pct}%");
    }
}
