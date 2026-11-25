//! Shared plumbing for the real-time ETW sessions this program hosts.
//!
//! Three features need one: per-process network bytes ([`super::net_etw`]),
//! per-process disk service time ([`super::disk_etw`]) and per-file read and
//! written bytes ([`super::file_etw`]). The lifetime rules are identical for
//! all of them and every one has already been learned the hard way, so they
//! live here once instead of being copied:
//!
//! * **Starting a session needs administrator rights** (or membership in
//!   "Performance Log Users"). When that fails the caller stays inactive and
//!   the snapshot keeps reporting `None`, which the UI renders as "—". It must
//!   NEVER fall back to reporting zero: that would be a fabricated
//!   measurement (core product invariant).
//! * **A session outlives its process if it is not stopped**, and a killed
//!   process never runs `Drop`. Session names are therefore FIXED per role
//!   (not per-PID): a stale session is found by name on the next start and
//!   reclaimed. A per-PID name cannot do that — every start picks a new name,
//!   so orphans accumulate, and once enough of them have a provider enabled
//!   Windows stops delivering events to new sessions. That failure looks
//!   exactly like "no traffic anywhere", which is how it was found.
//! * **`ProcessTrace` blocks** until the session is stopped, so it owns a
//!   dedicated thread and never runs on the sampler.

use std::sync::Arc;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FLAG,
    EVENT_TRACE_FLAG_FILE_IO, EVENT_TRACE_FLAG_FILE_IO_INIT, EVENT_TRACE_FLAG_IMAGE_LOAD,
    EVENT_TRACE_FLAG_PROCESS, EVENT_TRACE_FLAG_PROFILE, EVENT_TRACE_FLAG_THREAD,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE,
    EVENT_TRACE_SYSTEM_LOGGER_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW,
    SystemTraceControlGuid, WNODE_FLAG_TRACED_GUID,
};
use windows::core::{GUID, PCWSTR, PWSTR};

/// Informational level; the data events of every provider used here are
/// emitted at this level.
pub(crate) const TRACE_LEVEL_INFORMATION: u8 = 4;

/// Invalid real-time processing handle, as returned by `OpenTraceW` on
/// failure. `PROCESSTRACE_HANDLE` is a plain u64 in the Win32 headers.
const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;

/// Every session name this program can ever start.
///
/// Fixed and exhaustive by design (see the module docs): the ten per-feature
/// names — five features times two hosting roles — plus the one global session
/// name in [`KERNEL_LOGGER`] if this program ever starts it. Knowing them all
/// is what lets a crashed run be cleaned up without a per-process registry.
pub(crate) const SESSION_NAMES: [&str; 11] = [
    "TaskMan-Net-Service",
    "TaskMan-Net-App",
    "TaskMan-Disk-Service",
    "TaskMan-Disk-App",
    "TaskMan-File-Service",
    "TaskMan-File-App",
    "TaskMan-Profile-Service",
    "TaskMan-Profile-App",
    "TaskMan-Modules-Service",
    "TaskMan-Modules-App",
    KERNEL_LOGGER,
];

/// The single global kernel session, spelled the way the API spells it.
///
/// `KERNEL_LOGGER_NAMEW` from the SDK is the same string; a test compares the
/// two so a typo here cannot turn into a session that never starts.
pub(crate) const KERNEL_LOGGER: &str = "NT Kernel Logger";

/// How many sessions of each of [`SESSION_NAMES`] THIS process currently owns.
///
/// A COUNT, not a flag: a restarted session replaces its predecessor under the
/// same FIXED name, and the replacement is started before the old one is
/// dropped (see the maintenance thread in `core_service`). With a flag the
/// predecessor's `Drop` cleared the ownership the new session had just
/// recorded, so [`stop_live_sessions`] skipped a session this process still
/// held — for [`KERNEL_LOGGER`] that leaves the machine's kernel logger taken
/// by a dead run, and it can never be reclaimed because that name is not ours
/// to reclaim.
///
/// Lock-free and fixed-size on purpose: it is read from the panic hook, where
/// taking a lock the panicking thread might already hold would hang the
/// process instead of aborting it.
static LIVE_SESSIONS: [std::sync::atomic::AtomicUsize; SESSION_NAMES.len()] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; SESSION_NAMES.len()];

fn session_index(name: &[u16]) -> Option<usize> {
    let name = String::from_utf16_lossy(name);
    let name = name.trim_end_matches('\0');
    SESSION_NAMES.iter().position(|known| *known == name)
}

fn mark_session(name: &[u16], live: bool) {
    let Some(index) = session_index(name) else {
        return;
    };
    let owners = &LIVE_SESSIONS[index];
    if live {
        owners.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        return;
    }
    // Saturating, because a teardown that already zeroed the count (a caught
    // panic) followed by this session's own `Drop` must not wrap the count
    // into "many owners" and keep the hook stopping a session that is gone.
    //
    // A compare-exchange loop rather than `fetch_update`: that name is
    // deprecated on current nightlies in favour of `try_update`, which the
    // stable toolchain CI builds with does not have yet, so neither spelling
    // is portable across both.
    let mut held = owners.load(std::sync::atomic::Ordering::Acquire);
    loop {
        let next = held.saturating_sub(1);
        match owners.compare_exchange_weak(
            held,
            next,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => held = observed,
        }
    }
}

/// Stop every session this process still owns, by name.
///
/// An ETW session is a KERNEL object: it outlives the process that started it
/// and keeps filling buffers for every disk and network operation on the
/// machine until something stops it or the user reboots. `Drop` handles the
/// ordinary exit, but release builds are `panic = "abort"` and a killed
/// process runs no destructor either, so this is registered as the panic
/// teardown the first time a session comes up.
///
/// Only sessions THIS process started are touched. A crashing GUI must not
/// stop the service's sessions: that is a different, still-healthy process.
///
/// Panic-hook safe: no locks, no allocation past one stack buffer, no
/// `unwrap`. A session that is already gone just reports an error nobody
/// reads.
pub(crate) fn stop_live_sessions() {
    // `EVENT_TRACE_PROPERTIES` plus the longest name; see `properties_buffer`
    // for why this is `u64`-aligned.
    const WORDS: usize = 64;
    for (index, name) in SESSION_NAMES.iter().enumerate() {
        // Zeroed before the stop: a session name has one owner per running
        // session, and this runs once for the whole process.
        if LIVE_SESSIONS[index].swap(0, std::sync::atomic::Ordering::AcqRel) == 0 {
            continue;
        }
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let header = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let total = header + wide.len() * 2;
        if total > WORDS * std::mem::size_of::<u64>() {
            continue;
        }
        let mut buffer = [0u64; WORDS];
        let properties = buffer.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            (*properties).Wnode.BufferSize = total as u32;
            (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*properties).LoggerNameOffset = header as u32;
            let _ = ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(wide.as_ptr()),
                properties.cast(),
                EVENT_TRACE_CONTROL_STOP,
            );
        }
    }
}

/// Which component hosts a trace. Each role owns one fixed session name per
/// feature so it can reclaim its own orphan without ever stopping the other's
/// session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRole {
    /// The protected LocalSystem service (the normal host).
    Service,
    /// The GUI's own token, used only when there is no service and the GUI
    /// happens to be elevated.
    App,
}

/// One provider to enable on a session.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Provider {
    pub guid: GUID,
    /// `0` means "every event of this provider"; the record callback does the
    /// real filtering.
    pub match_any_keyword: u64,
}

/// Which kind of session to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoggerKind {
    /// An ordinary session for manifest providers.
    Manifest,
    /// A system logger, the only kind that can carry the kernel's own
    /// providers (`SystemIoProviderGuid` and friends). Their events arrive as
    /// classic MOF records — identified by the MOF class GUID in
    /// `EventHeader.ProviderId` and an opcode, not by an event id.
    System,
    /// A system logger used for a one-shot image rundown. The initial
    /// `DCStart` sequence covers every loaded image on the machine and can
    /// arrive as a large burst, so it gets the same larger buffers as the
    /// sampled-profile session.
    SystemLoader,
    /// The system logger shape used by sampled CPU profiling. The provider
    /// can emit around one sample per logical processor per millisecond, so
    /// it gets larger buffers than the sparse disk/thread session.
    SystemProfile,
    /// A private system logger that carries the kernel's per-file records
    /// (the classic `FileIo` class) — the per-file trace's PREFERRED session.
    ///
    /// Its subscription is its `EnableFlags`: `EVENT_TRACE_FLAG_FILE_IO_INIT`
    /// (Create, Cleanup, Close, Read, Write and the rest of the request
    /// family, opcodes 64 and up) plus `EVENT_TRACE_FLAG_FILE_IO`, which adds
    /// the `OpEnd` completion (opcode 76) carrying the bytes a request really
    /// transferred, plus `EVENT_TRACE_FLAG_PROCESS` for process exits (one
    /// record per process start/exit) so a long-running session forgets an
    /// exited process at once. The file flags were measured live on Windows
    /// 11 build 26300: the `FILE_IO` flag ALONE delivers nothing but `OpEnd`,
    /// and the SystemIo
    /// provider's `SYSTEM_IO_KW_FILE` keyword delivers the same `OpEnd`-only
    /// stream — which is what earlier builds subscribed with, and why they never
    /// saw a single Read or Write. No provider is enabled on this kind.
    ///
    /// A private session rather than [`KERNEL_LOGGER`]: the kernel logger is a
    /// machine-wide singleton other tools own, while Windows multiplexes the
    /// SystemTraceProvider over several private system loggers, so this one
    /// coexists with them. Like the loader and profile kinds this is the
    /// highest-rate stream in the program, so it asks for the larger buffer
    /// set.
    SystemFile,
    /// The single global kernel session — [`KERNEL_LOGGER`] — with the same
    /// file flags as [`LoggerKind::SystemFile`]: the per-file trace's
    /// FALLBACK, for when no private system logger could be started.
    ///
    /// Two things about it are unlike every kind above:
    ///
    /// * **It is identified by `SystemTraceControlGuid`**, so `Wnode.Guid`
    ///   carries it the way the SDK's kernel-logger example sets it. The named
    ///   kinds above are identified by their session name alone.
    /// * **It is EXCLUSIVE, and it is usually someone else's.**
    ///   `EVENT_TRACE_SYSTEM_LOGGER_MODE` is deliberately NOT set: the kernel
    ///   logger IS the system logger, and that bit marks a PRIVATE
    ///   SystemTraceProvider session — which is what the kinds above are. There
    ///   is exactly one of these on the machine, so when another application
    ///   owns it `StartTraceW` returns `ERROR_ALREADY_EXISTS` and this kind
    ///   must NOT reclaim the name the way a per-role session does: stopping it
    ///   would tear down that application's trace. The failure is surfaced to
    ///   the caller ([`StartError::AlreadyExists`]) instead.
    KernelFile,
}

impl LoggerKind {
    /// Whether a session of this kind may reclaim a same-named session it did
    /// not start.
    ///
    /// The per-role kinds may: their names are ours by construction (see
    /// [`TraceRole`]), so finding one already running means a previous run of
    /// OURS crashed without stopping it. The kernel logger may NOT: its name is
    /// a machine-wide singleton any tracing tool can own, so a session already
    /// running under it belongs to somebody else and stopping it would end
    /// their trace. [`StartError::AlreadyExists`] reports that instead.
    pub(crate) fn reclaims_a_stale_session(self) -> bool {
        self != Self::KernelFile
    }
}

/// Why a session could not be started.
///
/// The distinction exists because the two failures need different words on
/// screen: "no rights" is the ordinary unelevated case, while "already exists"
/// means another application owns the single global session and the user has
/// to close it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartError {
    /// `StartTraceW` reported `ERROR_ALREADY_EXISTS` for a kind that must not
    /// reclaim (only the kernel logger): another process owns that session.
    AlreadyExists,
    /// Anything else: no rights, no free logger slot, a refused provider
    /// enable, a consumer that could not be opened.
    Failed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => formatter.write_str("the session is owned by another process"),
            Self::Failed(error) => formatter.write_str(error),
        }
    }
}

impl From<StartError> for String {
    fn from(error: StartError) -> Self {
        error.to_string()
    }
}

/// The [`StartError`] for one `StartTraceW` status.
fn start_error(status: WIN32_ERROR) -> StartError {
    if status == ERROR_ALREADY_EXISTS {
        return StartError::AlreadyExists;
    }
    StartError::Failed(format!("StartTrace failed (Win32 {})", status.0))
}

/// State shared between an ETW callback thread and the sampler.
pub(crate) trait TraceContext: Send + Sync + 'static {
    /// Called before the session is torn down so a late callback cannot touch
    /// state the owner has already stopped reading.
    fn stop(&self);
}

/// A running real-time trace: the session, its consumer, and the thread that
/// pumps it.
pub(crate) struct Session<T: TraceContext> {
    shared: Arc<T>,
    kind: LoggerKind,
    /// Raw pointer handed to the ETW callback; reclaimed on teardown.
    context: *const T,
    session: CONTROLTRACE_HANDLE,
    trace: PROCESSTRACE_HANDLE,
    name: Vec<u16>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Set by the consumer thread when `ProcessTrace` returns. Before `Drop`
    /// that means the session was stopped underneath us — by `logman`, or by
    /// another instance reclaiming the fixed session name — and the counters
    /// have frozen while still looking live.
    ended: Arc<std::sync::atomic::AtomicBool>,
}

// The raw context pointer is only dereferenced by the ETW callback, which is
// alive exactly between `start` and `Drop`; nothing else touches it.
unsafe impl<T: TraceContext> Send for Session<T> {}

impl<T: TraceContext> Session<T> {
    /// Start a real-time session called `name` with every provider enabled.
    ///
    /// Returns `None` when ETW is unavailable to this token (the common
    /// unelevated case) or when any provider cannot be enabled, in which case
    /// the half-built session is torn down first.
    pub(crate) fn start(
        name: &str,
        kind: LoggerKind,
        providers: &[Provider],
        shared: Arc<T>,
        callback: unsafe extern "system" fn(*mut EVENT_RECORD),
        thread_name: &str,
    ) -> Option<Self> {
        match Self::try_start(name, kind, providers, shared, callback, thread_name) {
            Ok(session) => Some(session),
            Err(error) => {
                tracing::debug!(%error, thread_name, "real-time ETW session unavailable");
                None
            }
        }
    }

    /// Start a session while preserving the exact kind of failure for a
    /// diagnostic caller. Ordinary callers use [`Self::start`] and render
    /// unavailability as unknown.
    pub(crate) fn try_start(
        name: &str,
        kind: LoggerKind,
        providers: &[Provider],
        shared: Arc<T>,
        callback: unsafe extern "system" fn(*mut EVENT_RECORD),
        thread_name: &str,
    ) -> std::result::Result<Self, StartError> {
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        if kind == LoggerKind::SystemProfile {
            enable_system_profile_privilege();
        }
        let (session, mut properties) = start_session(&name, kind).map_err(start_error)?;

        for provider in providers {
            let enable = unsafe {
                EnableTraceEx2(
                    session,
                    &provider.guid,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                    TRACE_LEVEL_INFORMATION,
                    provider.match_any_keyword,
                    0,
                    0,
                    None,
                )
            };
            if enable != ERROR_SUCCESS {
                stop_session(session, &name, &mut properties);
                return Err(StartError::Failed(format!(
                    "EnableTraceEx2 failed for {:?} (Win32 {})",
                    provider.guid, enable.0
                )));
            }
        }

        // The callback owns a strong reference for as long as the trace runs.
        let context = Arc::into_raw(Arc::clone(&shared));
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_ptr() as *mut u16),
            Anonymous1: windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_LOGFILEW_0 {
                ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_LOGFILEW_1 {
                EventRecordCallback: Some(callback),
            },
            Context: context as *mut core::ffi::c_void,
            ..Default::default()
        };
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == INVALID_PROCESSTRACE_HANDLE {
            let error = unsafe { GetLastError() };
            stop_session(session, &name, &mut properties);
            // Reclaim the reference the callback would have owned.
            drop(unsafe { Arc::from_raw(context) });
            return Err(StartError::Failed(format!(
                "OpenTraceW failed (Win32 {})",
                error.0
            )));
        }

        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ended_by_consumer = Arc::clone(&ended);
        let worker = match std::thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                // Blocks until the session is stopped; the return code is not
                // actionable (a stopped session reports "cancelled").
                let _ = unsafe { ProcessTrace(&[trace], None, None) };
                ended_by_consumer.store(true, std::sync::atomic::Ordering::Release);
            }) {
            Ok(worker) => worker,
            Err(error) => {
                stop_session(session, &name, &mut properties);
                let _ = unsafe { CloseTrace(trace) };
                drop(unsafe { Arc::from_raw(context) });
                return Err(StartError::Failed(format!(
                    "ProcessTrace worker spawn failed: {error}"
                )));
            }
        };

        // A session now exists that the process dying will NOT clean up.
        tm_core::logging::set_panic_cleanup(stop_live_sessions);
        mark_session(&name, true);

        Ok(Self {
            shared,
            kind,
            context,
            session,
            trace,
            name,
            worker: Some(worker),
            ended,
        })
    }

    pub(crate) fn shared(&self) -> &Arc<T> {
        &self.shared
    }

    /// Whether the consumer is still receiving events. Callers must treat a
    /// session that is no longer running as unknown, never as idle.
    pub(crate) fn is_running(&self) -> bool {
        !self.ended.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Cumulative events ETW reported lost for this session, or `None` when
    /// the live logger properties could not be queried.
    pub(crate) fn events_lost(&self) -> Option<u64> {
        let mut properties = properties_buffer(&self.name, self.kind);
        let status = unsafe {
            ControlTraceW(
                self.session,
                PCWSTR(self.name.as_ptr()),
                properties.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let properties = properties.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        Some(unsafe { (*properties).EventsLost.into() })
    }
}

/// Enable `SeSystemProfilePrivilege` before the profile provider is enabled.
///
/// `StartTrace` succeeds without it, but `EnableTraceEx2` for
/// `SystemProfileProviderGuid` returns `ERROR_PRIVILEGE_NOT_HELD` when the
/// token merely holds the privilege disabled. Elevated administrators and
/// members of Performance Log Users commonly start with it disabled.
fn enable_system_profile_privilege() {
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::w;

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return;
        }
        let mut luid = Default::default();
        if LookupPrivilegeValueW(PCWSTR::null(), w!("SeSystemProfilePrivilege"), &mut luid).is_ok()
        {
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let _ = AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);
        }
        let _ = CloseHandle(token);
    }
}

impl<T: TraceContext> Drop for Session<T> {
    fn drop(&mut self) {
        // Order matters: stop the session so `ProcessTrace` returns, then
        // close the consumer and join before the context is reclaimed.
        // A consumer that already ended means the session was stopped by
        // someone else, and the fixed name (possibly even the logger handle)
        // may by now belong to a session another process started; stopping
        // it would tear THAT one down. A leftover of ours is reclaimed by
        // name on the next start anyway.
        let ended_underneath = !self.is_running();
        self.shared.stop();
        mark_session(&self.name, false);
        if !ended_underneath {
            let mut properties = properties_buffer(&self.name, self.kind);
            stop_session(self.session, &self.name, &mut properties);
        }
        let _ = unsafe { CloseTrace(self.trace) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        drop(unsafe { Arc::from_raw(self.context) });
    }
}

/// The context pointer an ETW record carries, as `&T`.
///
/// # Safety
/// Only valid inside a record callback of a [`Session<T>`] that is still
/// running; the pointer is the `Arc<T>` that session leaked into ETW.
pub(crate) unsafe fn context_of<T: TraceContext>(record: &EVENT_RECORD) -> Option<&T> {
    let shared = record.UserContext as *const T;
    (!shared.is_null()).then(|| unsafe { &*shared })
}

/// The record's payload as a byte slice, or `None` when there is none.
pub(crate) unsafe fn payload_of(record: &EVENT_RECORD) -> Option<&[u8]> {
    if record.UserData.is_null() || record.UserDataLength == 0 {
        return None;
    }
    Some(unsafe {
        std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize)
    })
}

/// `EVENT_TRACE_PROPERTIES` plus room for the trailing session name, which
/// the API copies in at `LoggerNameOffset`.
///
/// Allocated as `u64` words, not `u8`: the struct embeds 64-bit
/// `LARGE_INTEGER` fields and requires 8-byte alignment, which `Vec<u8>` does
/// not guarantee. `Wnode.BufferSize` carries the byte length; the allocation
/// is rounded up to a whole word.
fn properties_buffer(name: &[u16], kind: LoggerKind) -> Vec<u64> {
    let header = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = header + name.len() * 2;
    let mut buffer = vec![0u64; total.div_ceil(std::mem::size_of::<u64>())];
    // SAFETY: `buffer` is `u64`-aligned and at least `header` bytes long, so
    // the cast is aligned and in bounds. All writes are inside the struct.
    let properties = buffer.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
    unsafe {
        (*properties).Wnode.BufferSize = total as u32;
        (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        // QPC timestamps: cheapest clock, and we only need ordering.
        (*properties).Wnode.ClientContext = 1;
        // The kernel logger is identified by the system trace control GUID, the
        // way the SDK's own kernel-logger example sets it; every other kind is
        // a named private session and is identified by its name alone.
        if kind == LoggerKind::KernelFile {
            (*properties).Wnode.Guid = SystemTraceControlGuid;
        }
        (*properties).LogFileMode = EVENT_TRACE_REAL_TIME_MODE
            | match kind {
                LoggerKind::Manifest => 0,
                // The only way to subscribe to the kernel's own providers
                // without taking over the single global "NT Kernel Logger".
                // Windows allows several such sessions, which is why the disk,
                // loader, profile and file traces can each host their own.
                LoggerKind::System
                | LoggerKind::SystemLoader
                | LoggerKind::SystemProfile
                | LoggerKind::SystemFile => EVENT_TRACE_SYSTEM_LOGGER_MODE,
                // Deliberately absent for the kernel logger: it IS the system
                // logger, and this bit is what marks a PRIVATE
                // SystemTraceProvider session. Setting it on the kernel logger
                // asks `StartTraceW` for a shape that does not exist.
                LoggerKind::KernelFile => 0,
            };
        // EnableTraceEx2 can enable these providers after the session starts,
        // but the initial DCStart rundown is emitted while StartTrace creates
        // the logger. Request the corresponding legacy flags up front so a
        // loader/sample session reliably receives that initial state.
        // Both file kinds subscribe through these flags and nothing else: the
        // request family (`FILE_IO_INIT`), its completions (`FILE_IO`), and
        // process start/exit (`PROCESS`, one record per process) so a long
        // session drops an exited process's accounting at once; see
        // [`LoggerKind::SystemFile`].
        (*properties).EnableFlags = match kind {
            LoggerKind::Manifest | LoggerKind::System => EVENT_TRACE_FLAG::default(),
            LoggerKind::SystemLoader => EVENT_TRACE_FLAG_IMAGE_LOAD,
            LoggerKind::SystemProfile => EVENT_TRACE_FLAG_PROFILE | EVENT_TRACE_FLAG_THREAD,
            LoggerKind::SystemFile | LoggerKind::KernelFile => FILE_SESSION_FLAGS,
        };
        (*properties).LoggerNameOffset = header as u32;
        // A small buffer set with a 1 s flush keeps latency at roughly one
        // sampling tick without reserving much non-paged memory. The profile
        // stream, the initial loader rundown and the file I/O stream have much
        // higher event rates, so they ask for a larger set — the file stream on
        // either session shape, since both carry every request and completion
        // on the machine.
        let (buffer_kb, minimum, maximum) = match kind {
            LoggerKind::Manifest | LoggerKind::System => (64, 4, 16),
            LoggerKind::SystemLoader
            | LoggerKind::SystemProfile
            | LoggerKind::SystemFile
            | LoggerKind::KernelFile => (256, 8, 32),
        };
        (*properties).BufferSize = buffer_kb;
        (*properties).MinimumBuffers = minimum;
        (*properties).MaximumBuffers = maximum;
        (*properties).FlushTimer = 1;
    }
    buffer
}

/// What both per-file session kinds subscribe to; see [`LoggerKind::SystemFile`].
pub(crate) const FILE_SESSION_FLAGS: EVENT_TRACE_FLAG = EVENT_TRACE_FLAG(
    EVENT_TRACE_FLAG_FILE_IO_INIT.0 | EVENT_TRACE_FLAG_FILE_IO.0 | EVENT_TRACE_FLAG_PROCESS.0,
);

/// Start the session, retrying once after clearing a stale session of the
/// same name (left behind by a crash).
///
/// Only for a kind whose name is OURS — [`LoggerKind::reclaims_a_stale_session`].
/// The kernel logger's name is a machine-wide singleton, so an
/// `ERROR_ALREADY_EXISTS` there is reported to the caller instead of being
/// reclaimed: stopping a session we did not start would end another
/// application's trace.
fn start_session(
    name: &[u16],
    kind: LoggerKind,
) -> std::result::Result<(CONTROLTRACE_HANDLE, Vec<u64>), WIN32_ERROR> {
    let mut properties = properties_buffer(name, kind);
    let mut handle = CONTROLTRACE_HANDLE::default();
    let mut status = unsafe {
        StartTraceW(
            &mut handle,
            PCWSTR(name.as_ptr()),
            properties.as_mut_ptr().cast(),
        )
    };
    if status == ERROR_ALREADY_EXISTS && kind.reclaims_a_stale_session() {
        let mut stale = properties_buffer(name, kind);
        let _ = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(name.as_ptr()),
                stale.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        properties = properties_buffer(name, kind);
        status = unsafe {
            StartTraceW(
                &mut handle,
                PCWSTR(name.as_ptr()),
                properties.as_mut_ptr().cast(),
            )
        };
    }
    if status != ERROR_SUCCESS {
        return Err(status);
    }
    Ok((handle, properties))
}

fn stop_session(handle: CONTROLTRACE_HANDLE, name: &[u16], properties: &mut [u64]) {
    let status: WIN32_ERROR = unsafe {
        ControlTraceW(
            handle,
            PCWSTR(name.as_ptr()),
            properties.as_mut_ptr().cast(),
            EVENT_TRACE_CONTROL_STOP,
        )
    };
    if status != ERROR_SUCCESS {
        tracing::debug!(error = status.0, "stopping a real-time trace failed");
    }
}

/// Pointer width of the process the record came from.
///
/// ETW flags 32-bit records explicitly, and the pointers embedded in a payload
/// occupy that many bytes. Getting this wrong does not fail loudly: it shifts
/// every following field by four bytes and charges whatever number lands
/// there. Callers validate the fields they read so a misread becomes
/// "unknown" instead of a confident lie.
///
/// This answers for a payload a USER-MODE process wrote. A provider whose
/// payload is written by the kernel must pin native width instead, whatever
/// the calling process's flag says — see [`super::file_etw`].
pub(crate) fn pointer_bytes(record: &EVENT_RECORD) -> usize {
    const EVENT_HEADER_FLAG_32_BIT_HEADER: u16 = 0x0020;
    if record.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
        4
    } else {
        8
    }
}

/// Windows allocates process and thread ids out of the same handle table, so
/// every real one is a non-zero multiple of four.
///
/// The ETW payloads here are addressed by hand-written offsets. A wrong offset
/// would not fail loudly — it would quietly charge disk time to whatever
/// number happened to sit there. This check throws out the overwhelming
/// majority of such reads, leaving "unknown" rather than a confident lie.
pub(crate) fn plausible_client_id(id: u32) -> bool {
    id != 0 && id.is_multiple_of(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_buffer_reserves_room_for_the_session_name() {
        let name: Vec<u16> = "TaskMan-Test\0".encode_utf16().collect();
        let total = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2;
        let buffer = properties_buffer(&name, LoggerKind::Manifest);
        // The allocation is word-rounded, but must cover the requested bytes.
        assert!(buffer.len() * std::mem::size_of::<u64>() >= total);
        assert_eq!(buffer.as_ptr().align_offset(8), 0, "8-byte aligned");
        let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            assert_eq!((*properties).Wnode.BufferSize as usize, total);
            assert_eq!(
                (*properties).LoggerNameOffset as usize,
                std::mem::size_of::<EVENT_TRACE_PROPERTIES>()
            );
            assert_eq!((*properties).LogFileMode, EVENT_TRACE_REAL_TIME_MODE);
        }
    }

    /// A kernel provider is only reachable from a system logger; without the
    /// mode flag `EnableTraceEx2` accepts the call and then delivers nothing.
    #[test]
    fn a_system_logger_asks_for_the_system_logger_mode() {
        let name: Vec<u16> = "TaskMan-Test\0".encode_utf16().collect();
        let buffer = properties_buffer(&name, LoggerKind::System);
        let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            assert_eq!(
                (*properties).LogFileMode,
                EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE
            );
        }
    }

    #[test]
    fn profile_logger_has_the_system_mode_and_a_larger_buffer_set() {
        for kind in [LoggerKind::SystemLoader, LoggerKind::SystemProfile] {
            let name: Vec<u16> = "TaskMan-Test\0".encode_utf16().collect();
            let buffer = properties_buffer(&name, kind);
            let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            unsafe {
                assert_eq!(
                    (*properties).LogFileMode,
                    EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE
                );
                assert_eq!((*properties).BufferSize, 256);
                assert!((*properties).MaximumBuffers >= 32);
                let expected_flags = match kind {
                    LoggerKind::SystemLoader => EVENT_TRACE_FLAG_IMAGE_LOAD,
                    LoggerKind::SystemProfile => EVENT_TRACE_FLAG_PROFILE | EVENT_TRACE_FLAG_THREAD,
                    _ => unreachable!(),
                };
                assert_eq!((*properties).EnableFlags, expected_flags);
            }
        }
    }

    /// The per-file session subscribes with the request flag AND the
    /// completion flag, on a private system logger.
    ///
    /// Regression: earlier builds asked for `EVENT_TRACE_FLAG_FILE_IO` alone
    /// (or the SystemIo `FILE` keyword, which is the same stream). A live
    /// elevated probe showed that this delivers only `OpEnd` completions —
    /// 22 000 of them in four seconds and not one Create, Read or Write — so
    /// the trace reported "no file event yet" forever. `FILE_IO_INIT` is what
    /// carries the requests.
    #[test]
    fn the_file_logger_asks_for_requests_and_their_completions() {
        let name: Vec<u16> = "TaskMan-Test\0".encode_utf16().collect();
        let buffer = properties_buffer(&name, LoggerKind::SystemFile);
        let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            assert_eq!(
                (*properties).LogFileMode,
                EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE
            );
            assert_eq!((*properties).EnableFlags, FILE_SESSION_FLAGS);
            assert!(
                (*properties)
                    .EnableFlags
                    .contains(EVENT_TRACE_FLAG_FILE_IO_INIT | EVENT_TRACE_FLAG_FILE_IO),
                "requests and their completions"
            );
            assert!(
                (*properties).EnableFlags.contains(EVENT_TRACE_FLAG_PROCESS),
                "process exits end a process's accounting"
            );
            // Every request and completion on the machine.
            assert_eq!((*properties).BufferSize, 256);
            assert!((*properties).MaximumBuffers >= 32);
        }
    }

    /// The disk, loader and profile sessions must keep exactly the flags they
    /// had: adding the file flags to one of them would multiply its event rate
    /// and change what its windows measure. The file trace is a session of its
    /// own.
    #[test]
    fn no_other_system_logger_asks_for_the_file_flags() {
        for kind in [
            LoggerKind::System,
            LoggerKind::SystemLoader,
            LoggerKind::SystemProfile,
        ] {
            let name: Vec<u16> = "TaskMan-Test\0".encode_utf16().collect();
            let buffer = properties_buffer(&name, kind);
            let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            unsafe {
                assert!(
                    !(*properties).EnableFlags.contains(EVENT_TRACE_FLAG_FILE_IO),
                    "{kind:?} must not subscribe to file completions"
                );
                assert!(
                    !(*properties)
                        .EnableFlags
                        .contains(EVENT_TRACE_FLAG_FILE_IO_INIT),
                    "{kind:?} must not subscribe to file requests"
                );
            }
        }
    }

    /// The kernel logger fallback asks for the same file flags as the private
    /// session, on the system trace control GUID, real-time only.
    #[test]
    fn the_kernel_logger_carries_the_file_flags_on_the_system_trace_control_guid() {
        let name: Vec<u16> = KERNEL_LOGGER
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let buffer = properties_buffer(&name, LoggerKind::KernelFile);
        let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            assert_eq!((*properties).Wnode.Guid, SystemTraceControlGuid);
            // Only the file class: the kernel logger is shared, so asking for
            // more would multiply the rate for every other consumer's session
            // as well as ours.
            assert_eq!((*properties).EnableFlags, FILE_SESSION_FLAGS);
            assert!(
                (*properties)
                    .EnableFlags
                    .contains(EVENT_TRACE_FLAG_FILE_IO_INIT | EVENT_TRACE_FLAG_FILE_IO),
                "requests and their completions"
            );
            assert!(
                (*properties).EnableFlags.contains(EVENT_TRACE_FLAG_PROCESS),
                "process exits end a process's accounting"
            );
            assert_eq!(
                (*properties).LogFileMode,
                EVENT_TRACE_REAL_TIME_MODE,
                "the kernel logger IS the system logger; the mode bit marks a PRIVATE one"
            );
            assert_eq!((*properties).Wnode.Flags, WNODE_FLAG_TRACED_GUID);
            assert_eq!((*properties).BufferSize, 256);
            assert_eq!((*properties).FlushTimer, 1);
        }
        // A private session does NOT get the system trace control GUID: it is
        // identified by its name, and the GUID would make `StartTraceW` try to
        // start the global session instead.
        let buffer = properties_buffer(&name, LoggerKind::SystemFile);
        let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            assert_ne!((*properties).Wnode.Guid, SystemTraceControlGuid);
        }
    }

    /// The literal has to be the string the API expects, or the session never
    /// starts and the failure reads as "no rights".
    #[test]
    fn the_kernel_logger_name_is_the_one_the_api_defines() {
        let sdk = windows::Win32::System::Diagnostics::Etw::KERNEL_LOGGER_NAMEW;
        let mut wide = Vec::new();
        for index in 0..64 {
            // SAFETY: the SDK constant is a static NUL-terminated wide string;
            // the bound keeps this from running past a missing terminator.
            let unit = unsafe { *sdk.0.add(index) };
            if unit == 0 {
                break;
            }
            wide.push(unit);
        }
        assert_eq!(
            String::from_utf16_lossy(&wide),
            KERNEL_LOGGER,
            "KERNEL_LOGGER must spell KERNEL_LOGGER_NAMEW"
        );
        // The crash teardown stops sessions BY NAME from this list, so the
        // global session has to be in it too: a run that owned it and aborted
        // would otherwise leave the machine's kernel logger taken.
        assert!(SESSION_NAMES.contains(&KERNEL_LOGGER));
    }

    /// Only a session whose name is ours may be reclaimed. Reclaiming the
    /// kernel logger would stop somebody else's trace, which is the failure
    /// this whole distinction exists to prevent.
    #[test]
    fn only_our_own_session_names_are_reclaimed() {
        assert!(LoggerKind::Manifest.reclaims_a_stale_session());
        assert!(LoggerKind::System.reclaims_a_stale_session());
        assert!(LoggerKind::SystemLoader.reclaims_a_stale_session());
        assert!(LoggerKind::SystemProfile.reclaims_a_stale_session());
        assert!(LoggerKind::SystemFile.reclaims_a_stale_session());
        assert!(
            !LoggerKind::KernelFile.reclaims_a_stale_session(),
            "the global session belongs to whoever started it"
        );
    }

    /// A restarted session replaces its predecessor under the same FIXED name,
    /// and the replacement is started BEFORE the old one is dropped. The name
    /// therefore has to stay owned while the new session holds it: with a flag,
    /// the old session's `Drop` cleared the ownership the new one had just
    /// recorded, and the crash teardown then skipped a session this process
    /// still owned — for the global kernel logger, leaving it taken by a run
    /// that is already gone, under a name that must never be reclaimed.
    #[test]
    fn a_replaced_session_keeps_its_name_owned_until_its_last_holder_drops() {
        use std::sync::atomic::Ordering;

        // A name no other test of this module marks.
        let name = SESSION_NAMES[8];
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let owners =
            || session_index(&wide).map(|index| LIVE_SESSIONS[index].load(Ordering::Acquire));
        assert_eq!(owners(), Some(0), "nothing owns it yet");

        mark_session(&wide, true);
        mark_session(&wide, true);
        assert_eq!(
            owners(),
            Some(2),
            "a restart overlaps the session it replaces"
        );

        mark_session(&wide, false);
        assert_eq!(
            owners(),
            Some(1),
            "the predecessor's teardown must not clear the replacement's ownership"
        );
        mark_session(&wide, false);
        assert_eq!(owners(), Some(0));
        // An unbalanced release cannot wrap the count into "many owners", which
        // would keep the panic teardown stopping a session that is long gone.
        mark_session(&wide, false);
        assert_eq!(owners(), Some(0));
    }

    /// `ERROR_ALREADY_EXISTS` has to survive as its own answer: it is the
    /// difference between "no rights" and "another application owns the single
    /// kernel session", and the UI says different things for each.
    #[test]
    fn a_start_that_reports_already_exists_is_its_own_failure() {
        assert_eq!(start_error(ERROR_ALREADY_EXISTS), StartError::AlreadyExists);
        assert_eq!(
            start_error(windows::Win32::Foundation::ERROR_ACCESS_DENIED),
            StartError::Failed("StartTrace failed (Win32 5)".to_string())
        );
        // The message a log line prints stays readable.
        assert_eq!(
            StartError::AlreadyExists.to_string(),
            "the session is owned by another process"
        );
        assert_eq!(
            String::from(start_error(
                windows::Win32::Foundation::ERROR_INVALID_PARAMETER
            )),
            "StartTrace failed (Win32 87)"
        );
    }

    #[test]
    fn only_real_client_ids_are_accepted() {
        assert!(plausible_client_id(4));
        assert!(plausible_client_id(13_372));
        // Zero is the idle process, never an issuer; anything not aligned to
        // four did not come out of the handle table.
        assert!(!plausible_client_id(0));
        assert!(!plausible_client_id(1));
        assert!(!plausible_client_id(4321));
    }
}
