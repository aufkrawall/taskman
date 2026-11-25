//! Platform action surface: services, startup apps, users, process control.
//!
//! Every method has a default "unsupported" implementation so non-Windows
//! platforms only override what they can honestly deliver. The UI disables
//! controls based on `capabilities()`.

use tm_core::error::Result;
use tm_core::model::{PriorityClass, ProcStatus, ServiceInfo, StartupItem, UserSession};

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    pub services_list: bool,
    pub services_control: bool,
    /// A native service-management UI exists (not just list/control actions).
    pub open_services_app: bool,
    pub startup_toggle: bool,
    pub end_process_tree: bool,
    pub users_sessions: bool,
    pub user_disconnect: bool,
    /// A native account-management surface exists.
    pub manage_user_accounts: bool,
    pub end_process: bool,
    pub suspend_resume: bool,
    pub set_priority: bool,
    pub set_affinity: bool,
    pub efficiency_mode: bool,
    pub run_new_task: bool,
    /// Arbitrary Run may use an elevated launch path. Linux and macOS keep
    /// this false because no safe general-purpose elevated launcher exists.
    pub run_elevated: bool,
    pub per_process_network: bool,
    pub process_modules: bool,
    pub process_threads: bool,
    pub process_connections: bool,
    pub process_file_io: bool,
    pub unload_module: bool,
    /// A backend can register/unregister TaskMan's own login-start entry.
    pub start_with_system: bool,
    /// This process's elevation state is meaningful and can be displayed.
    pub elevation_status: bool,
    /// Settings can hand the current process to an elevated replacement.
    pub relaunch_elevated: bool,
    /// A future launch can follow a persisted elevated-start policy.
    pub start_elevated: bool,
}

impl Capabilities {
    /// Elevated Run needs both the general launch surface and an explicit
    /// elevated-launch implementation. Keeping this as one predicate prevents
    /// a platform from exposing a checkbox that only turns into an error.
    pub fn supports_elevated_run(self) -> bool {
        self.run_new_task && self.run_elevated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub enum TaskManagerReplacementState {
    Unsupported,
    Disabled,
    Enabled,
    Stale(String),
    Conflict(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreServiceState {
    Unsupported,
    NotInstalled,
    Stopped,
    Starting,
    Running {
        version: String,
    },
    Degraded(String),
    /// The service is installed but refuses to run: the installed binaries do
    /// not match the pinned broker manifest (or the manifest is unreadable).
    /// No restart can help; only Repair/Install rewrites the pinned
    /// generation.
    InstallIntegrityFailed,
    /// The service is installed and healthy, but this session's executable
    /// sits outside the protected install location, so the broker's
    /// image-path authorization will keep rejecting it. "Repair" cannot fix
    /// this; the session must switch to the installed GUI.
    ForeignClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub enum UserSessionAction {
    Disconnect,
    Logoff,
}

/// Extra per-process details fetched lazily (not part of the 1 Hz snapshot).
#[derive(Debug, Clone, Default)]
pub struct ProcessExtra {
    pub status: Option<ProcStatus>,
    pub elevated: Option<bool>,
    pub wow64: Option<bool>,
}

/// One executable image mapping in a process. On Windows this is a loaded
/// module image. On Linux it is an identity-merged span of executable mappings
/// from `/proc/<pid>/maps`; a fragmented image can therefore appear as more
/// than one span. Enumeration is an explicit, on-demand diagnostic query
/// rather than part of the hot sample.
///
/// There is deliberately no "protected" marking: which module to unload is
/// the user's call. The confirmation dialog carries the crash warning, the
/// platform revalidates identity and base/path at action time, and the
/// honest [`ModuleUnloadOutcome`] says whether the module actually left.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub struct ProcessModule {
    pub name: String,
    pub path: String,
    pub base_address: u64,
    pub size_bytes: u64,
}

/// Sampled instructions whose instruction pointer landed in one mapped module
/// during the displayed measurement window.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub struct ProcessModuleCodeSamples {
    pub base_address: u64,
    pub samples: u64,
}

/// One drained sampled-code window for a single exact process.
///
/// This is execution evidence, not thread-origin evidence: a DLL hooked into
/// an existing render thread can have no threads of its own and still appear
/// here. `events_seen == 0` or `events_decoded == 0` means the trace has not
/// produced a usable measurement yet; consumers must render that as unknown,
/// never as a measured zero. ETW loss makes every count a lower bound.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub struct ProcessModuleCodeWindow {
    pub window_ms: u64,
    /// Sample-profile records handed to the decoder, across all processes.
    pub events_seen: u64,
    /// ...of which the payload decoder accepted.
    pub events_decoded: u64,
    /// Events ETW reported lost over this window. `None` means this could not
    /// be queried; it must not be interpreted as zero loss.
    pub events_lost: Option<u64>,
    /// Samples attributed to the target process during the window.
    pub target_samples: u64,
    /// Target samples that matched a loaded module at drain time.
    pub attributed_samples: u64,
    /// Target threads known to the trace. Without at least one, a zero cannot
    /// be asserted as measured, because samples may have been unattributable.
    pub tracked_threads: u32,
    pub modules: Vec<ProcessModuleCodeSamples>,
}

/// Scheduler state of one thread, as the kernel's process table reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Initialized,
    Ready,
    Running,
    /// Runnable in the kernel queue, but not necessarily executing on a CPU.
    Runnable,
    Standby,
    Terminated,
    Waiting,
    Sleeping,
    Stopped,
    Idle,
    Parked,
    Transition,
    DeferredReady,
    Other(u32),
}

impl ThreadState {
    /// `KTHREAD_STATE` as `SYSTEM_THREAD_INFORMATION.ThreadState` carries it.
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Initialized,
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Standby,
            4 => Self::Terminated,
            5 => Self::Waiting,
            6 => Self::Transition,
            7 => Self::DeferredReady,
            other => Self::Other(other),
        }
    }
}

/// `KWAIT_REASON` name, the way System Informer and WinDbg print it. These
/// are kernel identifiers, not prose, so they are deliberately not localized.
pub fn wait_reason_name(raw: u32) -> Option<&'static str> {
    const NAMES: [&str; 43] = [
        "Executive",
        "FreePage",
        "PageIn",
        "PoolAllocation",
        "DelayExecution",
        "Suspended",
        "UserRequest",
        "WrExecutive",
        "WrFreePage",
        "WrPageIn",
        "WrPoolAllocation",
        "WrDelayExecution",
        "WrSuspended",
        "WrUserRequest",
        "WrEventPair",
        "WrQueue",
        "WrLpcReceive",
        "WrLpcReply",
        "WrVirtualMemory",
        "WrPageOut",
        "WrRendezvous",
        "WrKeyedEvent",
        "WrTerminated",
        "WrProcessInSwap",
        "WrCpuRateControl",
        "WrCalloutStack",
        "WrKernel",
        "WrResource",
        "WrPushLock",
        "WrMutex",
        "WrQuantumEnd",
        "WrDispatchInt",
        "WrPreempted",
        "WrYieldExecution",
        "WrFastMutex",
        "WrGuardedMutex",
        "WrRundown",
        "WrAlertByThreadId",
        "WrDeferredPreempt",
        "WrPhysicalFault",
        "WrIoRing",
        "WrMdlCache",
        "WrRcu",
    ];
    NAMES.get(raw as usize).copied()
}

/// One thread of a process. Fetched on demand while Process Properties shows
/// its Threads page — never part of the 1 Hz snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessThread {
    pub tid: u32,
    /// The routine the thread was created to run (`Win32StartAddress`),
    /// `None` when the kernel reported none.
    pub start_address: Option<u64>,
    pub priority: i32,
    pub base_priority: i32,
    /// Linux's raw nice value. `None` where the platform has no such notion.
    pub nice: Option<i32>,
    /// Monotonic count of times the scheduler switched this thread onto a
    /// processor, including both voluntary and involuntary switches on Linux.
    /// An unchanged count between two queries proves the thread did not run in
    /// between; `None` means the counter was unavailable, never zero.
    pub context_switches: Option<u64>,
    pub kernel_time_100ns: u64,
    pub user_time_100ns: u64,
    pub created_epoch_s: Option<i64>,
    pub state: ThreadState,
    /// Only meaningful while `state` is `Waiting`. `None` when this platform
    /// does not expose a Windows-compatible wait reason.
    pub wait_reason: Option<u32>,
    /// Cumulative CPU cycles. `None` when the thread could not be opened
    /// (another user's or a protected process) — never a fabricated zero.
    pub cycles: Option<u64>,
}

/// Every thread of one exact process. Platforms with atomic process/thread
/// snapshots use that snapshot; procfs backends bracket individual records
/// with the process/TID creation identity so recycled entries are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessThreads {
    /// The process's base priority, which the threads' symbolic priority
    /// ("Normal", "Highest", ...) is relative to.
    pub process_base_priority: i32,
    pub threads: Vec<ProcessThread>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionProtocol {
    Tcp,
    Tcp6,
    Udp,
    Udp6,
}

/// `MIB_TCP_STATE`. UDP endpoints have no state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    DeleteTcb,
    Other(u32),
}

impl TcpState {
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            // Windows 11 returns dwState = 0 for TCP listener sockets in
            // GetExtendedTcpTable (TCP_TABLE_OWNER_PID_ALL).
            0 | 2 => Self::Listen,
            1 => Self::Closed,
            3 => Self::SynSent,
            4 => Self::SynReceived,
            5 => Self::Established,
            6 => Self::FinWait1,
            7 => Self::FinWait2,
            8 => Self::CloseWait,
            9 => Self::Closing,
            10 => Self::LastAck,
            11 => Self::TimeWait,
            12 => Self::DeleteTcb,
            other => Self::Other(other),
        }
    }
}

/// One TCP connection/listener or UDP endpoint owned by a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessConnection {
    pub protocol: ConnectionProtocol,
    pub local: std::net::SocketAddr,
    /// `None` for listeners and UDP endpoints.
    pub remote: Option<std::net::SocketAddr>,
    pub state: Option<TcpState>,
}

/// One file a process has an open handle to and/or has read or written since
/// per-file accounting began.
///
/// The byte and operation counts are MEASURED request totals, never inferred
/// from a handle's file pointer or a file's size (neither says how much a
/// process really transferred: rewrites, repeated reads, positional I/O,
/// short-lived handles and mapped views all defeat them). `None` therefore
/// means "not measured" and must render as unavailable, never as zero; `Some(0)`
/// is a measured zero.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub struct ProcessFileIo {
    pub path: String,
    /// Bytes this process read from the file since accounting began.
    pub read_bytes: Option<u64>,
    /// Bytes this process wrote to the file since accounting began.
    pub write_bytes: Option<u64>,
    pub read_ops: Option<u64>,
    pub write_ops: Option<u64>,
    /// Current size, known only while a handle to the file is open.
    pub file_size: Option<u64>,
    /// Current file pointer offset of open handle(s), if known.
    pub offset: Option<u64>,
    /// Access rights of the currently open handles; both `false` once the
    /// file is closed.
    #[cfg_attr(target_os = "windows", serde(default))]
    pub access_known: bool,
    pub is_read: bool,
    pub is_write: bool,
    pub open_handles: u32,
}

/// Whether, and how far, per-file byte accounting could be trusted for one
/// answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub enum FileIoAccountingState {
    /// No per-file accounting is running (unsupported platform, or neither
    /// the privileged service nor an elevated session can trace file I/O).
    /// The rows carry no byte counts.
    #[default]
    Unavailable,
    /// A trace is running but has not decoded a single completed read or write
    /// anywhere on the machine yet. A silent window cannot be told apart from a
    /// broken subscription, so it is treated as unknown rather than as zero
    /// activity. (Once the session has proven it delivers transfers, a process
    /// with none is [`Self::Measured`] at zero.)
    Undelivered,
    /// The target issued file requests but none of the records could be
    /// decoded: the payload layout is not what this build expects.
    Undecodable,
    /// No trace could be started: the private file session was refused, and
    /// the fallback — the single global kernel session — is owned by ANOTHER
    /// application right now (`ERROR_ALREADY_EXISTS` from `StartTraceW`).
    ///
    /// Distinct from [`Self::Unavailable`] because it is not a privilege
    /// problem and the user can act on it: some other tracing tool (a
    /// Process Hacker-style monitor, an installer's logger, `wpr`, a
    /// performance-recording app) holds the kernel logger, and closing it
    /// releases the session. Distinct from [`Self::Undelivered`] because
    /// nothing was started at all — there is no silent window to wait out, and
    /// reporting it as one would suggest the subscription is broken. The rows
    /// carry no byte counts.
    KernelLoggerInUse,
    /// Counts are measured: completed logical reads and writes since the trace
    /// started. A row the trace knows the process has open reads zero when it
    /// saw no transfer; a row it knows nothing about stays unavailable.
    Measured,
}

/// What per-file accounting covers, and how it reconciles with the kernel's
/// per-process I/O counters (the numbers the graphs plot).
///
/// `read_bytes`/`write_bytes` are everything the trace attributed to the
/// process, whether or not it ended up on a row; the rows plus the three
/// remainders below add up to them exactly. Process I/O through an object that
/// cannot have a row — a pipe, a device, a console, a name that did not
/// resolve — is therefore INSIDE these totals, in the unnamed remainder: the
/// handle walk names disk files only, while the session delivers every file
/// object. They are not the kernel counters' quantity either: the counters are
/// process-lifetime totals and these cover the trace's lifetime, so the two
/// differ by the age of the process as well as by every event ETW dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub struct FileIoAccounting {
    pub state: FileIoAccountingState,
    /// How long accounting has been running, in milliseconds. Only I/O after
    /// that point is counted.
    pub since_ms: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    /// Part of the totals whose file name could not be resolved (no row).
    pub unnamed_read_bytes: u64,
    pub unnamed_write_bytes: u64,
    /// Part of the totals dropped from `files` by a capacity limit.
    pub omitted_read_bytes: u64,
    pub omitted_write_bytes: u64,
    /// Kernel per-process read/write byte counters: TOTALS SINCE THE PROCESS
    /// STARTED, not growth over the span `since_ms` measures. They are carried
    /// alongside the measured bytes as a separate, coarser fact — and are the
    /// only per-process source on platforms without a per-file trace.
    /// `None` when the counters could not be read.
    pub counter_read_bytes: Option<u64>,
    pub counter_write_bytes: Option<u64>,
    /// Events the trace session reports as lost; counts are lower bounds when
    /// this is non-zero. `None` when it could not be queried.
    pub events_lost: Option<u64>,
    /// Entries discarded from the trace's `FileObject -> name` map when it hit
    /// its cap, over the trace's lifetime. Their later transfers are in
    /// `unnamed_*`, which is why a row can read unavailable while
    /// [`Self::state`] is still [`FileIoAccountingState::Measured`].
    pub cleared_file_objects: u64,
    /// File opens the trace refused to name (a network share, a `\SystemRoot\`
    /// name, a raw device), over the trace's lifetime. Their transfers are in
    /// `unnamed_*`.
    pub unnamed_creates: u64,
    /// Paths never tracked because a capacity limit (per process or for the
    /// whole session) was full, over the trace's lifetime. Their transfers are
    /// in `omitted_*`.
    pub dropped_paths: u64,
    /// Whether the open-handle enumeration (names, sizes, access) completed.
    ///
    /// This says the handle TABLE was covered, not that every handle in it could
    /// be read: a process always holds protected handles that cannot be
    /// duplicated, and files opened without query rights, so those are skipped
    /// without clearing the flag (see `win::process_file_io`). It is false when
    /// the snapshot was truncated, when the process could not be opened, or when
    /// a caller dropped entries from `files`.
    pub handles_complete: bool,
    /// Whether the per-file trace has seen this process's WHOLE life: it was
    /// created after the trace started. Otherwise the per-file counts cover
    /// only the time since the trace started (`since_ms`).
    pub traced_since_process_start: bool,
    /// The process's own kernel `IO_COUNTERS` read/write bytes over exactly the
    /// window the per-file trace covers (since process start, or since the
    /// trace started). Those counters count EVERY read and write call —
    /// files, pipes, devices, consoles — so this minus `read_bytes` /
    /// `write_bytes` is what went anywhere but a file. `None` when the counter
    /// at the start of the window is unknown (the trace restarted, or the
    /// process could not be read then), never a guessed zero.
    pub counter_window_read_bytes: Option<u64>,
    pub counter_window_write_bytes: Option<u64>,
}

/// Files of one process plus the accounting scope they were measured under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(target_os = "windows", derive(serde::Serialize, serde::Deserialize))]
pub struct ProcessFileIoSnapshot {
    pub files: Vec<ProcessFileIo>,
    pub accounting: FileIoAccounting,
}

/// Wire/request capability marker appended to module paths by current GUIs.
///
/// Windows file names cannot contain NUL, so a stale implementation cannot
/// mistake the marked string for a real ToolHelp module path: exact base/path
/// revalidation fails before any legacy multi-call `FreeLibrary` loop. Protocol
/// v3 now also rejects older brokers at the handshake, but the marker remains a
/// defense-in-depth invariant for direct/local and mixed-binary execution.
pub const MODULE_UNLOAD_SINGLE_RELEASE_MARKER: &str = "\0taskman-single-release-v1";

/// What could be verified after one remote `FreeLibrary` request completed.
///
/// TaskMan deliberately does not try to guess or drain the loader's private
/// reference count. A confirmation performs one release request, then the
/// target's module list is re-enumerated. `None` means `FreeLibrary` returned
/// success but the post-action inventory could not be obtained reliably; the
/// request must not be reported as either mapped or unmapped in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleUnloadOutcome {
    /// `Some(false)` when the exact base/path disappeared, `Some(true)` when
    /// it remained mapped, and `None` when post-action verification failed.
    pub still_mapped: Option<bool>,
}

pub trait PlatformActions: Send + Sync {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    // ------------------------------------------------ services
    fn list_services(&self) -> Result<Vec<ServiceInfo>> {
        Err(tm_core::TmError::Unsupported("services"))
    }
    fn control_service(&self, _name: &str, _action: ServiceAction) -> Result<()> {
        Err(tm_core::TmError::Unsupported("service control"))
    }
    /// Open the platform's native service-management surface. Platforms
    /// without one leave `Capabilities::open_services_app` false.
    fn open_services_app(&self) -> Result<()> {
        Err(tm_core::TmError::Unsupported("Services app"))
    }

    // ------------------------------------------------ startup
    fn list_startup(&self) -> Result<Vec<StartupItem>> {
        Err(tm_core::TmError::Unsupported("startup apps"))
    }
    fn set_startup_enabled(&self, _item_id: &str, _location: &str, _enabled: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("startup toggle"))
    }

    // ------------------------------------------------ users
    fn list_user_sessions(&self) -> Result<Vec<UserSession>> {
        Err(tm_core::TmError::Unsupported("user sessions"))
    }
    fn control_user_session(&self, _session_id: u32, _action: UserSessionAction) -> Result<()> {
        Err(tm_core::TmError::Unsupported("session control"))
    }
    /// Open the platform's native account-management surface. This is separate
    /// from session listing/control, which several Unix flavors implement.
    fn manage_user_accounts(&self) -> Result<()> {
        Err(tm_core::TmError::Unsupported("user account management"))
    }

    // ------------------------------------------------ process control
    /// Terminate a process. Backends with tree support also kill its
    /// descendants when requested. The expected creation token comes from
    /// ProcessEntry::creation_identity and is verified before acting.
    fn kill_process(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _tree: bool,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("end task"))
    }
    fn kill_single(&self, pid: u32) -> Result<()> {
        self.kill_process(pid, None, false)
    }
    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        self.suspend_process_checked(pid, None, suspend)
    }
    fn suspend_process_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _suspend: bool,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("suspend/resume"))
    }
    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        self.set_priority_checked(pid, None, priority)
    }
    fn set_priority_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _priority: PriorityClass,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("set priority"))
    }
    fn get_affinity_mask(&self, pid: u32) -> Result<u64> {
        self.get_affinity_mask_checked(pid, None)
    }
    fn get_affinity_mask_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<u64> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn system_affinity_mask(&self) -> Result<u64> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn set_affinity_mask(&self, pid: u32, mask: u64) -> Result<()> {
        self.set_affinity_mask_checked(pid, None, mask)
    }
    fn set_affinity_mask_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _mask: u64,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("affinity"))
    }
    fn set_efficiency_mode(&self, pid: u32, on: bool) -> Result<()> {
        self.set_efficiency_mode_checked(pid, None, on)
    }
    fn set_efficiency_mode_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _on: bool,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("efficiency mode"))
    }
    fn set_uac_virtualization_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _enabled: bool,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("UAC virtualization"))
    }
    /// Lazily inspect security hardening for one exact Windows process.
    /// This lives on the action surface so an unelevated GUI can use the
    /// authenticated LocalSystem broker for service/SYSTEM targets.
    #[cfg(target_os = "windows")]
    fn process_security_info_checked(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<crate::win::ProcessSecurityInfo> {
        Err(tm_core::TmError::Unsupported(
            "process security information",
        ))
    }
    fn list_process_modules(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<Vec<ProcessModule>> {
        Err(tm_core::TmError::Unsupported("process modules"))
    }
    /// Drain one sampled-code window for the exact process shown by a Modules
    /// page. `None` means privileged sampling is unavailable; an explicit
    /// trace rejection remains an error.
    fn process_module_code_samples(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<Option<ProcessModuleCodeWindow>> {
        Ok(None)
    }
    /// Every thread of one exact process. `expected_start_epoch_s` is checked
    /// against the SAME kernel snapshot the threads come from, so a recycled
    /// PID can never hand back another process's threads.
    fn list_process_threads(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<ProcessThreads> {
        Err(tm_core::TmError::Unsupported("process threads"))
    }
    /// TCP connections/listeners and UDP endpoints the system attributes to
    /// `pid`. The connection tables carry no process creation time; callers
    /// re-check the identity against a fresh snapshot.
    fn list_process_connections(&self, _pid: u32) -> Result<Vec<ProcessConnection>> {
        Err(tm_core::TmError::Unsupported("process connections"))
    }
    /// Files `pid` has open or has touched since accounting began, with
    /// measured per-file read/write totals where the platform can measure
    /// them (see [`ProcessFileIoSnapshot`]).
    fn list_process_file_io(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
    ) -> Result<ProcessFileIoSnapshot> {
        Err(tm_core::TmError::Unsupported("process file I/O"))
    }
    /// Ask the target process to perform one `FreeLibrary` call for one exact
    /// mapped module. This is a diagnostic escape hatch: callers must
    /// confirmation-gate it and provide a sampled process identity, while the
    /// platform revalidates process identity plus module base/path at action
    /// time and verifies the module list again afterwards.
    fn unload_process_module(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _base_address: u64,
        _expected_path: &str,
    ) -> Result<ModuleUnloadOutcome> {
        Err(tm_core::TmError::Unsupported("unload module"))
    }

    // ------------------------------------------------ launching / elevation
    fn is_elevated(&self) -> bool {
        false
    }
    /// Tri-state current-process elevation. Backends that can fail a token
    /// query return `None` instead of turning unknown into unelevated.
    fn elevation_status(&self) -> Option<bool> {
        Some(self.is_elevated())
    }
    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        let _ = (command_line, elevate);
        Err(tm_core::TmError::Unsupported("run new task"))
    }
    /// Report a launch outcome where the platform supports that probe.
    /// Linux deliberately leaves this non-launching and unimplemented.
    fn run_new_task_probe(&self, command_line: &str, elevate: bool) -> Result<()> {
        let _ = (command_line, elevate);
        Err(tm_core::TmError::Unsupported("run new task"))
    }
    fn relaunch_elevated(&self) -> Result<()> {
        Err(tm_core::TmError::Unsupported("elevation"))
    }

    // ------------------------------------------------ shell integration
    /// Start the built-in OS task manager even when this application owns a
    /// Task Manager replacement/interception registration.
    fn launch_native_task_manager(&self) -> Result<()> {
        Err(tm_core::TmError::Unsupported("Windows Task Manager"))
    }
    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {
        TaskManagerReplacementState::Unsupported
    }
    fn set_task_manager_replacement(&self, _enabled: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("Task Manager replacement"))
    }
    /// Re-point an own, but broken, registration at this installation
    /// WITHOUT asking the user for anything. `Ok(false)` means the repair
    /// needs a consent prompt and was therefore not attempted; the caller
    /// then has to ask instead of silently elevating.
    fn repair_task_manager_replacement(&self) -> Result<bool> {
        Ok(false)
    }
    /// Enable or disable the per-user OS startup registration.
    ///
    /// `start_minimized` is only a request for a genuinely supported hidden
    /// startup path; callers must pass `false` where the platform cannot honor
    /// it.
    fn set_start_with_system(&self, _enabled: bool, _start_minimized: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("start with system"))
    }
    #[deprecated(note = "use set_start_with_system")]
    fn set_start_with_windows(&self, enabled: bool, start_minimized: bool) -> Result<()> {
        self.set_start_with_system(enabled, start_minimized)
    }
    /// Remove only TaskMan's own login-start registration. The default action
    /// delegates to the same capability-owning implementation as the setting,
    /// so Reset cannot drift from the platform's ownership checks.
    fn clear_self_autostart(&self) -> Result<()> {
        self.set_start_with_system(false, true)
    }
    fn core_service_state(&self) -> CoreServiceState {
        CoreServiceState::Unsupported
    }
    /// Start whatever accounting must run for the GUI's whole lifetime rather
    /// than only while a page asks — today the per-file I/O trace, on a
    /// Windows GUI that is elevated and has no service to host it. Called
    /// once, off the UI thread, after start-up. A no-op where nothing applies.
    fn start_background_accounting(&self) {}
    fn set_core_service_installed(&self, _installed: bool) -> Result<()> {
        Err(tm_core::TmError::Unsupported("core service"))
    }
    /// Hand the current session over to the protected installed GUI. Returns
    /// `Ok(false)` when there is no installed generation to switch into.
    fn switch_to_installed_gui(&self, _args: &[String]) -> Result<bool> {
        Ok(false)
    }

    /// Create a user-mode dump. `expected_start_epoch_s` has the same
    /// PID-reuse protection contract as [`Self::kill_process`].
    fn create_dump_file(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        path: &std::path::Path,
        dump_type: tm_core::model::DumpType,
    ) -> Result<()> {
        self.create_dump_file_with_progress(pid, expected_start_epoch_s, path, dump_type, None)
    }

    /// Create a user-mode dump with progress tracking and cooperative cancellation.
    fn create_dump_file_with_progress(
        &self,
        _pid: u32,
        _expected_start_epoch_s: Option<i64>,
        _path: &std::path::Path,
        _dump_type: tm_core::model::DumpType,
        _progress: Option<std::sync::Arc<tm_core::model::DumpProgressTracker>>,
    ) -> Result<()> {
        Err(tm_core::TmError::Unsupported("create dump"))
    }
    fn open_file_location(&self, _path: &str) -> Result<()> {
        Err(tm_core::TmError::Unsupported("open file location"))
    }
    fn open_properties(&self, _path: &str) -> Result<()> {
        Err(tm_core::TmError::Unsupported("properties"))
    }
    fn open_url(&self, _url: &str) -> Result<()> {
        Err(tm_core::TmError::Unsupported("open url"))
    }

    fn last_bios_time_ms(&self) -> Option<u64> {
        None
    }

    fn process_icon_rgba(&self, _exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        None
    }

    /// The OS's default application icon — what the platform's shell shows for
    /// an application that has no icon of its own — as straight-alpha RGBA.
    ///
    /// The UI paints this wherever a row has no icon it can resolve (a kernel
    /// pseudo-process without an image path, an app without a themed icon,
    /// a failed extraction), so the lists keep the OS icon theme instead of a
    /// drawn placeholder. Backends without a shell icon answer `None`.
    fn default_app_icon_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }

    fn backend_name(&self) -> &'static str {
        "unknown"
    }
}

/// Inert action backend for `--mock` sessions so UI testing can never mutate or
/// terminate real host processes, services, or registry state.
pub struct MockActions;

impl PlatformActions for MockActions {
    fn backend_name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_run_requires_both_capabilities() {
        assert!(!Capabilities::default().supports_elevated_run());
        assert!(
            !Capabilities {
                run_new_task: true,
                ..Default::default()
            }
            .supports_elevated_run()
        );
        assert!(
            !Capabilities {
                run_elevated: true,
                ..Default::default()
            }
            .supports_elevated_run()
        );
        assert!(
            Capabilities {
                run_new_task: true,
                run_elevated: true,
                ..Default::default()
            }
            .supports_elevated_run()
        );
    }
}
