//! Snapshot data model. One `Snapshot` is a fully consistent view of the
//! system produced by a single sampling tick; the UI never mutates it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single consistent sample of the whole system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Wall-clock timestamp of this snapshot (ms since UNIX epoch).
    pub timestamp_ms: u64,
    /// How long the sampling pass took (ms) — useful for diagnostics.
    pub sample_duration_ms: u64,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub disks: Vec<DiskInfo>,
    pub networks: Vec<NetworkInfo>,
    pub gpus: Vec<GpuInfo>,
    pub processes: Vec<ProcessEntry>,
    pub system: SystemMisc,
}

impl Snapshot {
    /// Look up a process by pid (linear scan is fine for a few hundred entries).
    pub fn process(&self, pid: u32) -> Option<&ProcessEntry> {
        self.processes.iter().find(|p| p.pid == pid)
    }

    /// Total system-wide network receive rate in bytes/sec.
    pub fn net_recv_bps(&self) -> f64 {
        self.networks.iter().map(|n| n.recv_bps).sum()
    }

    /// Total system-wide network send rate in bytes/sec.
    pub fn net_sent_bps(&self) -> f64 {
        self.networks.iter().map(|n| n.sent_bps).sum()
    }
}

// ---------------------------------------------------------------- CPU

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CpuInfo {
    pub brand: String,
    pub vendor: String,
    pub architecture: String,
    /// Aggregate utilization 0..=100 (TM-style, all cores averaged).
    pub utilization_pct: f32,
    /// Utilization per logical processor 0..=100.
    pub per_core_pct: Vec<f32>,
    /// Kernel-mode share per logical processor 0..=100 (Windows; empty when
    /// the platform does not split user/kernel time).
    pub per_core_kernel_pct: Vec<f32>,
    /// Aggregate kernel-mode share 0..=100 (0 = unknown/unavailable).
    pub kernel_pct: f32,
    /// Current average frequency across cores, MHz.
    pub freq_mhz: f32,
    /// Advertised base frequency, MHz (0 = unknown).
    pub freq_base_mhz: f32,
    pub logical_count: usize,
    /// Physical core count across all sockets (0 = unknown).
    pub physical_cores: usize,
    /// Processor packages/sockets (0 = unknown).
    pub sockets: usize,
    /// Total L1 data+inst cache across all cores, KB (0 = unknown).
    pub l1_kb: u64,
    /// Total L2 cache, KB (0 = unknown).
    pub l2_kb: u64,
    /// Total shared L3 cache, KB (0 = unknown).
    pub l3_kb: u64,
    /// "Enabled" / "Disabled" / "Unknown" — hardware virtualization state.
    pub virtualization: String,
}

// ---------------------------------------------------------------- memory

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    /// OS file cache standing on top of used memory (Windows standby / Linux cached).
    pub cached_bytes: u64,
    /// Committed (pagefile-backed + RAM-backed) charge.
    pub commit_total_bytes: u64,
    pub commit_used_bytes: u64,
    pub paged_pool_bytes: u64,
    pub non_paged_pool_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    /// Physically installed RAM (SMBIOS), before hardware reservations.
    pub installed_bytes: u64,
    /// Memory reserved by hardware/firmware (installed - usable).
    pub hw_reserved_bytes: u64,
    /// Configured DRAM clock of the fastest populated module, MT/s (0 = unknown).
    pub speed_mts: u32,
    /// Maximum supported module speed, MT/s (0 = unknown).
    pub speed_max_mts: u32,
    /// Populated memory slots / total slots (0 = unknown).
    pub slots_used: u32,
    pub slots_total: u32,
    /// "DIMM" / "SODIMM" / ... of the first populated module (empty = unknown).
    pub form_factor: String,
    /// Module manufacturer of the first populated module (empty = unknown).
    pub manufacturer: String,
    /// Module part number of the first populated module (empty = unknown).
    pub part_number: String,
}

impl MemoryInfo {
    pub fn used_pct(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.used_bytes as f32 / self.total_bytes as f32 * 100.0
        }
    }

    pub fn commit_pct(&self) -> f32 {
        if self.commit_total_bytes == 0 {
            0.0
        } else {
            self.commit_used_bytes as f32 / self.commit_total_bytes as f32 * 100.0
        }
    }
}

// ---------------------------------------------------------------- disks

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MediaKind {
    #[default]
    Unknown,
    SsdNvme,
    SsdSata,
    /// Generic SSD when the bus type is not distinguishable.
    Ssd,
    Hdd,
    Usb,
    Network,
    Optical,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Unknown => "",
            MediaKind::SsdNvme => "SSD (NVMe)",
            MediaKind::SsdSata => "SSD (SATA)",
            MediaKind::Ssd => "SSD",
            MediaKind::Hdd => "HDD",
            MediaKind::Usb => "USB",
            MediaKind::Network => "Network drive",
            MediaKind::Optical => "Optical",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Stable per-boot identifier, e.g. Windows "0 (C:)", Linux "nvme0n1", macOS "disk0".
    pub id: String,
    /// Primary mount point / volume letter path.
    pub mount: String,
    /// Volume label if any.
    pub label: String,
    pub media: MediaKind,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Time spent servicing I/O as fraction of elapsed (0..=100).
    pub active_pct: f32,
    pub read_bps: f64,
    pub write_bps: f64,
    /// Average time requests spend being serviced, ms (0 = unknown).
    pub avg_resp_ms: f32,
    pub total_read_bytes: u64,
    pub total_written_bytes: u64,
}

impl DiskInfo {
    pub fn used_pct(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.total_bytes - self.free_bytes) as f32 / self.total_bytes as f32 * 100.0
        }
    }
}

// ---------------------------------------------------------------- network

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub name: String,
    /// Human description, e.g. adapter model.
    pub desc: String,
    /// Adapter type hint: "Ethernet", "Wi-Fi", "Loopback", "".
    pub kind: String,
    pub oper_up: bool,
    pub recv_bps: f64,
    pub sent_bps: f64,
    pub total_recv_bytes: u64,
    pub total_sent_bytes: u64,
    /// Negotiated link speed in bits/s (0 = unknown).
    pub link_bps: u64,
    /// SSID for Wi-Fi adapters when discoverable.
    #[serde(default)]
    pub ssid: Option<String>,
    /// Preferred unicast addresses for the adapter when discoverable.
    #[serde(default)]
    pub ipv4: Option<String>,
    #[serde(default)]
    pub ipv6: Option<String>,
    /// Native WLAN signal quality in percent (0–100).
    #[serde(default)]
    pub signal_quality_pct: Option<u32>,
}

// ---------------------------------------------------------------- GPU

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuEngine {
    /// Engine name, e.g. "3D", "Copy", "VideoDecode".
    pub name: String,
    pub util_pct: f32,
}

/// Adapter LUID as exposed by DXGI/PDH — the join key between static adapter
/// discovery and per-engine counter instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdapterLuid {
    pub high: i32,
    pub low: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuInfo {
    /// Zero-based adapter index.
    pub id: usize,
    pub name: String,
    pub driver_version: String,
    pub util_pct: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Dedicated (VRAM) usage where distinguishable.
    pub dedicated_used_bytes: u64,
    /// Shared/system-memory usage where distinguishable (0 = unknown).
    pub shared_used_bytes: u64,
    pub temperature_c: Option<f32>,
    /// LUID of this adapter when known; PDH engine records join on this.
    pub luid: Option<AdapterLuid>,
    pub engines: Vec<GpuEngine>,
}

// ---------------------------------------------------------------- processes

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProcCategory {
    #[default]
    App,
    Background,
    System,
}

impl ProcCategory {
    pub fn group_label(self) -> &'static str {
        match self {
            ProcCategory::App => "Apps",
            ProcCategory::Background => "Background",
            ProcCategory::System => "System",
        }
    }

    /// Sort order of the groups inside the Processes tab (Apps first like Win11 TM).
    pub fn group_rank(self) -> u8 {
        match self {
            ProcCategory::App => 0,
            ProcCategory::Background => 1,
            ProcCategory::System => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum ProcStatus {
    #[default]
    Running,
    Suspended,
    /// Not responding (only detectable on some platforms).
    NotResponding,
}

/// Base priority classes as exposed by Windows; other platforms map onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum PriorityClass {
    Realtime,
    High,
    AboveNormal,
    #[default]
    Normal,
    BelowNormal,
    Low,
    Unknown,
}

/// UAC virtualization state from the process token (Windows); never inferred
/// or fabricated — `None` means the token could not be queried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UacVirtualization {
    Enabled,
    Disabled,
    NotAllowed,
    #[default]
    Unknown,
}

impl UacVirtualization {
    pub fn label(self) -> &'static str {
        match self {
            // Localized via i18n at the call site; these are stable keys.
            UacVirtualization::Enabled => "Enabled",
            UacVirtualization::Disabled => "Disabled",
            UacVirtualization::NotAllowed => "Not allowed",
            UacVirtualization::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: Option<u32>,
    /// Executable base name ("explorer.exe").
    pub name: String,
    /// Friendly display name (description/title when known), falls back to `name`.
    pub display: String,
    /// Publisher/company string when known.
    pub company: Option<String>,
    /// Description metadata when known.
    pub description: Option<String>,
    pub category: ProcCategory,
    pub status: ProcStatus,
    pub user: Option<String>,
    pub session_id: Option<u32>,

    /// CPU utilization normalized to whole-system capacity (TM-style, max 100).
    pub cpu_pct: f32,
    /// Resident memory attributed to the process.
    pub mem_bytes: u64,
    /// Private commit charge when available.
    pub commit_bytes: Option<u64>,
    /// Peak resident memory when available.
    pub peak_mem_bytes: Option<u64>,

    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub disk_read_total: u64,
    pub disk_write_total: u64,

    /// Per-process network rates; None where the platform cannot measure them.
    pub net_recv_bps: Option<f64>,
    pub net_sent_bps: Option<f64>,
    pub net_recv_total: Option<u64>,
    pub net_sent_total: Option<u64>,

    pub gpu_util_pct: Option<f32>,
    pub gpu_mem_bytes: Option<u64>,
    /// Dedicated GPU memory used by this process where measurable.
    pub gpu_dedicated_bytes: Option<u64>,
    /// Shared GPU memory used by this process where measurable.
    pub gpu_shared_bytes: Option<u64>,
    /// Dominant GPU engine label, e.g. "GPU 0 - 3D" (never a percentage).
    pub gpu_engine_label: Option<String>,

    pub handles: Option<u32>,
    pub threads: Option<u32>,
    /// Page faults delta per second where available.
    pub page_faults_per_s: Option<u32>,

    /// Process start time (seconds since UNIX epoch).
    pub start_epoch_s: Option<i64>,
    /// Accumulated CPU time in seconds.
    pub cpu_time_s: Option<f64>,
    /// Base priority class.
    pub priority: PriorityClass,

    pub exe_path: Option<PathBuf>,
    pub command_line: Option<String>,
    pub has_window: bool,
    /// Topmost process of an app subtree (Task Manager's "Apps" root rows).
    pub app_root: bool,
    /// Runs elevated (admin/root) when determinable.
    pub elevated: Option<bool>,
    /// 32-bit process on a 64-bit OS (Windows WOW64).
    pub wow64: Option<bool>,
    /// True when the process hosts exactly one service (Windows services host).
    pub service_name: Option<String>,
    /// Token virtualization state (TokenVirtualizationAllowed/Enabled).
    /// None = could not be queried — never inferred from user name or pid.
    pub uac_virtualization: Option<UacVirtualization>,
    /// Windows EcoQoS / power throttling state (None = unknown).
    pub power_throttled: Option<bool>,
    /// Pseudo-row that does not correspond to an OS process (CPU
    /// attribution helpers like "System interrupts" / "Terminated
    /// processes"). Must never receive destructive process actions.
    pub synthetic: bool,
}

impl ProcessEntry {
    pub fn new(pid: u32, name: impl Into<String>) -> Self {
        let name: String = name.into();
        Self {
            pid,
            display: name.clone(),
            name,
            ..Self::default_entry()
        }
    }

    fn default_entry() -> Self {
        Self {
            pid: 0,
            ppid: None,
            name: String::new(),
            display: String::new(),
            company: None,
            description: None,
            category: ProcCategory::Background,
            status: ProcStatus::Running,
            user: None,
            session_id: None,
            cpu_pct: 0.0,
            mem_bytes: 0,
            commit_bytes: None,
            peak_mem_bytes: None,
            disk_read_bps: 0.0,
            disk_write_bps: 0.0,
            disk_read_total: 0,
            disk_write_total: 0,
            net_recv_bps: None,
            net_sent_bps: None,
            net_recv_total: None,
            net_sent_total: None,
            gpu_util_pct: None,
            gpu_mem_bytes: None,
            gpu_dedicated_bytes: None,
            gpu_shared_bytes: None,
            gpu_engine_label: None,
            handles: None,
            threads: None,
            page_faults_per_s: None,
            start_epoch_s: None,
            cpu_time_s: None,
            priority: PriorityClass::Unknown,
            exe_path: None,
            command_line: None,
            has_window: false,
            app_root: false,
            elevated: None,
            wow64: None,
            service_name: None,
            uac_virtualization: None,
            power_throttled: None,
            synthetic: false,
        }
    }

    /// Name shown in tables.
    pub fn shown_name(&self) -> &str {
        if self.display.is_empty() {
            &self.name
        } else {
            &self.display
        }
    }

    pub fn uptime_s(&self, now_epoch_s: i64) -> Option<i64> {
        self.start_epoch_s.map(|s| (now_epoch_s - s).max(0))
    }
}

// ---------------------------------------------------------------- misc system

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemMisc {
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    /// Kernel version where applicable.
    pub kernel_version: String,
    /// Seconds since boot.
    pub uptime_s: u64,
    pub boot_epoch_s: i64,
    pub process_count: usize,
    pub thread_count: usize,
    pub handle_count: usize,
}

// ---------------------------------------------------------------- platform services

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ServiceStatus {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    #[default]
    Unknown,
}

impl ServiceStatus {
    pub fn label(self) -> &'static str {
        match self {
            ServiceStatus::Stopped => "Stopped",
            ServiceStatus::StartPending => "Starting",
            ServiceStatus::StopPending => "Stopping",
            ServiceStatus::Running => "Running",
            ServiceStatus::ContinuePending => "Resuming",
            ServiceStatus::PausePending => "Pausing",
            ServiceStatus::Paused => "Paused",
            ServiceStatus::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub pid: Option<u32>,
    pub status: ServiceStatus,
    pub group: String,
    pub startup_type: String,
    pub account: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StartupImpact {
    #[default]
    Unknown,
    None,
    Low,
    Medium,
    High,
}

impl StartupImpact {
    pub fn label(self) -> &'static str {
        match self {
            StartupImpact::Unknown => "Not measured",
            StartupImpact::None => "None",
            StartupImpact::Low => "Low",
            StartupImpact::Medium => "Medium",
            StartupImpact::High => "High",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartupItem {
    /// Stable id within its location.
    pub id: String,
    pub name: String,
    pub command: String,
    /// Where it comes from, e.g. `HKCU\...\Run`, "Startup folder", autostart dir.
    pub location: String,
    pub publisher: Option<String>,
    pub enabled: bool,
    pub impact: StartupImpact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UserSessionState {
    Active,
    Connected,
    ConnectQuery,
    Shadowing,
    Disconnected,
    Idle,
    Listen,
    Reset,
    Down,
    Init,
    #[default]
    Unknown,
}

impl UserSessionState {
    pub fn label(self) -> &'static str {
        match self {
            UserSessionState::Active => "Active",
            UserSessionState::Connected => "Connected",
            UserSessionState::ConnectQuery => "Connecting",
            UserSessionState::Shadowing => "Shadowing",
            UserSessionState::Disconnected => "Disconnected",
            UserSessionState::Idle => "Idle",
            UserSessionState::Listen => "Listening",
            UserSessionState::Reset => "Resetting",
            UserSessionState::Down => "Down",
            UserSessionState::Init => "Initializing",
            UserSessionState::Unknown => "",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserSession {
    /// Platform session id.
    pub id: u32,
    pub user: String,
    pub domain: Option<String>,
    pub state: UserSessionState,
    pub logon_epoch_s: Option<i64>,
    /// Aggregated stats filled from the latest snapshot.
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub process_count: usize,
}
