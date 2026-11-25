//! Hardened Windows service transport and installation boundary.
//!
//! The broker deliberately exposes semantic process/service operations only.
//! It never accepts an arbitrary command line or privileged output path.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tm_core::error::{Result, TmError};
use tm_core::model::PriorityClass;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetTokenInformation, LookupAccountSidW,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SID_NAME_USE, SetFileSecurityW, SidTypeUser, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ, GetFileInformationByHandle,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::actions::{
    Capabilities, CoreServiceState, ModuleUnloadOutcome, PlatformActions, ProcessModule,
    ProcessModuleCodeWindow, ServiceAction, TaskManagerReplacementState, UserSessionAction,
};

pub const SERVICE_NAME: &str = "TaskmanCore";
pub const SERVICE_DISPLAY_NAME: &str = "TaskMan Core Service";
pub const SERVICE_EXE_NAME: &str = "taskman-service.exe";
pub const GUI_EXE_NAME: &str = "taskman.exe";
pub const SERVICE_LOG_FILE_PREFIX: &str = "taskman-service.log";
// v2 added `ProcessNetworkCounters`; v3 adds identity-bound module
// inventory so an unelevated GUI can inspect SYSTEM/service processes through
// its authenticated LocalSystem broker; v4 adds `ProcessDiskCounters`, the
// same read-only shape for disk service time. Version bumps are deliberate: an
// old service fails the handshake as unavailable, allowing the GUI's safe local
// fallback instead of misclassifying an unknown request as a rejection. v5
// carries the disk decoder's event counts, which separate an idle disk from a
// decoder that could not read a single record. v6 adds `ProcessIdentity`, the
// identity-bound token read that fills the User/Elevated columns of the
// SYSTEM/service processes an unelevated GUI cannot open at all. v7 splits
// the disk service time by physical disk (per entry and per-disk totals), so
// each process's share is taken of the disk that served it. v8 adds bounded,
// identity-bound thread start-address and cycle evidence so an unelevated GUI
// can fill those fields for elevated targets such as Windows Task Manager. v9
// adds drained, identity-bound sampled instruction-pointer attribution for
// the Modules page. v10 adds bounded, identity-bound process file I/O
// enumeration (ProcessFileIo). v11 adds identity-bound parent process
// evidence (ProcessParentInfo) in ProcessSecurityInfo. v13 adds the per-file
// trace's coverage to `FileIoAccounting`: whether it saw the process's whole
// life, and the process counters over the same window.
pub const PROTOCOL_VERSION: u16 = 13;

const PIPE_NAME: &str = r"\\.\pipe\Taskman.Core.v1";
const FRAME_MAGIC: [u8; 4] = *b"TMB1";
const FRAME_REQUEST: u16 = 1;
const FRAME_RESPONSE: u16 = 2;
const FRAME_HEADER_LEN: usize = 12;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
// Module inventories contain paths and can legitimately exceed 64 KiB. Keep
// responses bounded, but give the authenticated read-only inventory enough
// room for normal large processes without silently truncating it.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_MODULES_PER_RESPONSE: usize = 1024;
const MAX_THREAD_DETAILS_PER_RESPONSE: usize = 4096;
const MAX_FILES_PER_RESPONSE: usize = 1024;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const WORKER_COUNT: usize = 2;
const WORK_QUEUE_CAP: usize = 16;
// Two active workers + the full queue + the instance the accept thread is
// still resolving + the fresh listener it re-arms before doing so.
const PIPE_INSTANCE_CAP: u32 = (WORKER_COUNT + WORK_QUEUE_CAP + 2) as u32;
/// Consecutive accept-path failures tolerated before the broker gives up and
/// lets SCM recovery restart it from a clean image. Individual failures are
/// per-connection and must not take the control plane down, but a listener
/// that can never be armed again is a real fault and has to stay visible.
const MAX_CONSECUTIVE_ACCEPT_FAILURES: u32 = 16;
/// How long the accept thread waits before trying again to arm a listener
/// while every pipe instance is in use. npfs gives the server no event for an
/// instance being released (a rejected client frees its instance only when
/// it closes its own end), so this is a bounded poll on that resource.
const PIPE_REARM_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);
/// How long a client keeps retrying a busy pipe before reporting the service
/// as unavailable, and the per-attempt `WaitNamedPipeW` bound inside that.
const PIPE_BUSY_RETRY: std::time::Duration = std::time::Duration::from_millis(1000);
const PIPE_BUSY_WAIT_MS: u32 = 250;
const MANIFEST_SCHEMA: u32 = 1;
const CLIENT_REGISTRY_KEY: &str = r"Software\TaskMan";
const CLIENT_REGISTRY_VALUE: &str = "CoreServiceGui";

/// The pipe access the installing user's ACE grants and the client requests:
/// FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE.
/// npfs silently requires FILE_READ_ATTRIBUTES beyond the requested data
/// rights when opening a pipe client end, so an ACE granting only the data
/// bits (plus SYNCHRONIZE) denies every client that relies on it — i.e.
/// every non-elevated GUI, which has no generic/administrator ACE to fall
/// back on. The user ACE and the client's desired access must both include
/// the attribute right, and requesting generic rights would additionally
/// pull in FILE_CREATE_PIPE_INSTANCE, which the user ACE must not grant.
const USER_PIPE_ACCESS: u32 = 0x0010_0083;
const INSTALL_SDDL: &str = "O:BAG:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)";
const SERVICE_DATA_SDDL: &str = "O:BAG:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerManifest {
    schema_version: u32,
    protocol_version: u16,
    authorized_user_sid: String,
    gui_path: PathBuf,
    gui_sha256: String,
    service_path: PathBuf,
    service_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
enum BrokerRequest {
    Ping,
    KillProcess {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        tree: bool,
    },
    SuspendProcess {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        suspend: bool,
    },
    SetPriority {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        priority: PriorityClass,
    },
    GetAffinity {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    SetAffinity {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        mask: u64,
    },
    SetEfficiencyMode {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        enabled: bool,
    },
    SetUacVirtualization {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        enabled: bool,
    },
    /// Read-only, identity-bound hardening metadata used by Process Properties.
    ProcessSecurityInfo {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    /// Read-only, identity-bound token identity (account, SID, elevation, UAC
    /// virtualization). Brokered because the interactive GUI's own token
    /// cannot open the token of SYSTEM/service processes — exactly the rows
    /// whose User/Elevated columns would otherwise stay blank.
    ProcessIdentity {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    /// Read-only, identity-bound module inventory. This is brokered because
    /// ordinary GUI tokens cannot snapshot many SYSTEM/service processes.
    ProcessModules {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    /// Drain one sampled-code window for the exact process whose Modules page
    /// is visible. The service owns the privileged system-profile session.
    ProcessModuleCodeSamples {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    /// Read-only thread evidence the interactive GUI cannot obtain from its
    /// own token. Bound to the exact process creation time and to each
    /// thread's creation time before any value is returned.
    ProcessThreadDetails {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    /// Read-only, identity-bound open file handles and I/O accounting.
    ProcessFileIo {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    },
    UnloadModule {
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        base_address: u64,
        expected_path: String,
    },
    ControlService {
        name: String,
        action: ServiceAction,
    },
    ControlUserSession {
        session_id: u32,
        action: UserSessionAction,
    },
    TaskManagerReplacementState,
    SetTaskManagerReplacement {
        enabled: bool,
    },
    /// Read-only per-process network counters (see [`ProcessNetworkSample`]).
    ProcessNetworkCounters,
    /// Read-only per-process disk activity (see [`ProcessDiskSample`]).
    ProcessDiskCounters,
}

/// Cumulative per-process network bytes as answered by the broker.
///
/// This is the protocol's network telemetry response, and it is deliberately
/// shaped to stay small and unambiguous:
/// * `active` distinguishes "the trace is running, so a process missing from
///   `entries` moved zero bytes" from "no trace, the answer is unknown". The
///   product invariant is that unknown must never render as zero.
/// * only processes with non-zero counters are listed; on a normal desktop
///   that is a few dozen entries, far inside the 512 KiB response cap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessNetworkSample {
    pub active: bool,
    pub entries: Vec<ProcessNetworkEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessNetworkEntry {
    pub pid: u32,
    pub received: u64,
    pub sent: u64,
}

/// Per-process disk activity for ONE measurement window, as answered by the
/// broker. Same shape and the same rules as [`ProcessNetworkSample`]:
/// * `active` separates "the trace runs, so a missing process touched no disk"
///   from "no trace, the answer is unknown" — unknown must never render as
///   zero.
/// * the window is DRAINED by each answer, so `window_ms` states what the
///   numbers cover instead of leaving the caller to guess at the boundaries.
/// * `service_time` is in the disk provider's own units and is only ever used
///   as a ratio against `total_service_time`; see `win::disk_etw`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDiskSample {
    pub active: bool,
    /// Length of the drained window in milliseconds.
    pub window_ms: u64,
    /// Service time observed in the window INCLUDING the part that could not
    /// be charged to a live process. The denominator of every share.
    pub total_service_time: u64,
    /// Disk records the decoder was handed, and how many it could read. A
    /// window with records but no reads is NOT a measurement: see
    /// `win::disk_etw::DiskWindow::decoded`.
    pub events_seen: u64,
    pub events_decoded: u64,
    /// Every disk's service time in the window, charged or not: the
    /// denominator of the per-disk shares.
    pub disk_totals: Vec<ProcessDiskTime>,
    pub entries: Vec<ProcessDiskEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDiskEntry {
    pub pid: u32,
    pub service_time: u64,
    /// `service_time` split by the physical disk that served it.
    pub disks: Vec<ProcessDiskTime>,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub ops: u64,
}

/// Service time on one physical disk (`PhysicalDrive<disk>`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDiskTime {
    pub disk: u32,
    pub service_time: u64,
}

fn module_unload_verified_default() -> bool {
    true
}

fn module_unload_legacy_call_default() -> u32 {
    1
}

/// The two per-thread values that require a thread handle. The rest of the
/// thread list comes from the handle-free kernel snapshot in the GUI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessThreadDetail {
    tid: u32,
    created_epoch_s: Option<i64>,
    start_address: Option<u64>,
    cycles: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum BrokerValue {
    Pong {
        version: String,
    },
    Unit,
    AffinityMask(u64),
    TaskManagerReplacementState(TaskManagerReplacementState),
    ProcessNetwork(ProcessNetworkSample),
    ProcessDisk(ProcessDiskSample),
    ProcessSecurity(super::ProcessSecurityInfo),
    ProcessIdentity(super::process_ops::ProcessTokenIdentity),
    ProcessModules(Vec<ProcessModule>),
    ProcessModuleCodeSamples(Option<ProcessModuleCodeWindow>),
    ProcessThreadDetails(Vec<ProcessThreadDetail>),
    ProcessFileIo(crate::actions::ProcessFileIoSnapshot),
    /// Result of `UnloadModule`. `released` is retained only for compatibility
    /// with the earlier v2 wire shape; it now means successful remote
    /// FreeLibrary calls, not an observable loader-reference count. `verified`
    /// is additive and defaults to true so both pre-count and counted v2
    /// services remain decodable by a newer GUI.
    ModuleUnload {
        still_mapped: bool,
        #[serde(default = "module_unload_verified_default")]
        verified: bool,
        #[serde(default = "module_unload_legacy_call_default")]
        released: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerResponse {
    protocol_version: u16,
    value: std::result::Result<BrokerValue, String>,
}

#[derive(Debug)]
enum BrokerCallError {
    /// Nothing was tried, or the service is not there to try.
    Unavailable(String),
    /// The request or the answer was lost in transport, or the two sides do not
    /// agree on the frame. The service never said no, so this is an unknown
    /// rather than a decision: a read-only request may answer from the local
    /// arm (the same treatment [`Self::Unavailable`] gets), while an action has
    /// to report that its outcome is unknown.
    NoAnswer(String),
    /// The service answered and declined. Never worked around locally.
    Rejected(String),
}

impl std::fmt::Display for BrokerCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) | Self::NoAnswer(detail) => write!(formatter, "{detail}"),
            Self::Rejected(detail) => write!(formatter, "broker rejected request: {detail}"),
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn path_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn normalized_path(path: &Path) -> std::io::Result<String> {
    let canonical = std::fs::canonicalize(path)?;
    let value = canonical.to_string_lossy();
    Ok(value
        .strip_prefix(r"\\?\")
        .unwrap_or(&value)
        .replace('/', "\\")
        .to_lowercase())
}

fn paths_match(left: &Path, right: &Path) -> bool {
    normalized_path(left)
        .and_then(|left| normalized_path(right).map(|right| left == right))
        .unwrap_or(false)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    sha256_reader(&mut file)
}

fn sha256_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn process_image_path(pid: u32) -> std::result::Result<PathBuf, BrokerCallError> {
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|error| {
            BrokerCallError::Rejected(format!("cannot open client process: {error}"))
        })?;
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    result.map_err(|error| {
        BrokerCallError::Rejected(format!("cannot resolve client process image: {error}"))
    })?;
    buffer.truncate(length as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

fn write_frame(
    writer: &mut impl Write,
    kind: u16,
    payload: &[u8],
    maximum: usize,
) -> std::io::Result<()> {
    if payload.len() > maximum || payload.len() > u32::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "broker frame exceeds limit",
        ));
    }
    let mut header = [0u8; FRAME_HEADER_LEN];
    header[..4].copy_from_slice(&FRAME_MAGIC);
    header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&kind.to_le_bytes());
    header[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

fn read_frame(
    reader: &mut impl Read,
    expected_kind: u16,
    maximum: usize,
) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    reader.read_exact(&mut header)?;
    if header[..4] != FRAME_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid broker frame magic",
        ));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != PROTOCOL_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported broker protocol version",
        ));
    }
    let kind = u16::from_le_bytes([header[6], header[7]]);
    if kind != expected_kind {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected broker frame kind",
        ));
    }
    let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "broker frame exceeds limit",
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

pub(super) struct SecurityDescriptor(pub(super) PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
}

pub(super) fn security_descriptor(sddl: &str) -> Result<SecurityDescriptor> {
    let encoded = wide(sddl);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(encoded.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|error| TmError::platform("parse security descriptor", error.to_string()))?;
    }
    Ok(SecurityDescriptor(descriptor))
}

fn pipe_sddl(authorized_sid: &str) -> Result<String> {
    if !valid_sid_text(authorized_sid) {
        return Err(TmError::platform(
            "broker manifest",
            "authorized user SID is malformed",
        ));
    }
    // The user ACE deliberately grants only synchronous read/write data plus
    // the attribute right the client end requires, not
    // FILE_CREATE_PIPE_INSTANCE (which generic write would also grant).
    Ok(format!(
        "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;{USER_PIPE_ACCESS:#08x};;;{authorized_sid})"
    ))
}

fn valid_sid_text(value: &str) -> bool {
    if value.len() > 184 {
        return false;
    }
    let Some(parts) = value.strip_prefix("S-") else {
        return false;
    };
    let mut parts = parts.split('-');
    parts.next() == Some("1")
        && parts.clone().count() >= 1
        && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether `sid_text` names a real user account rather than a group, alias or
/// well-known SID.
///
/// [`valid_sid_text`] only proves the *shape* of a SID. The pipe ACE is the
/// authorization boundary, so accepting any well-formed SID would let a
/// crafted elevated helper invocation (`taskman.exe --core-service=install
/// --core-service-user=S-1-5-32-545`) authorize every member of that group.
/// The GUI always passes its own token's SID, so requiring a user account
/// costs nothing legitimate and closes that widening path. Resolution is
/// local: the account is logged on and its SID is already known to LSA.
fn sid_is_user_account(sid_text: &str) -> bool {
    let encoded = wide(sid_text);
    let mut sid = windows::Win32::Security::PSID::default();
    if unsafe { ConvertStringSidToSidW(PCWSTR(encoded.as_ptr()), &mut sid) }.is_err() {
        return false;
    }
    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut kind = SID_NAME_USE(0);
    // First call only sizes both output buffers; it always fails.
    let _ = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            None,
            &mut name_len,
            None,
            &mut domain_len,
            &mut kind,
        )
    };
    let mut name = vec![0u16; name_len.max(1) as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    let resolved = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        )
    };
    // SAFETY: `sid` was allocated by the successful `ConvertStringSidToSidW`
    // above and is freed on THIS path unconditionally; the buffer handed to
    // `LookupAccountSidW` is only read while `sid` is alive.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid.0)));
    }
    resolved.is_ok() && kind == SidTypeUser
}

/// Context shared by every error that means "the installed generation does
/// not match its pinned manifest". The whole class maps to the dedicated SCM
/// exit code [`SERVICE_EXIT_INSTALL_INTEGRITY`] so the GUI can tell "broken
/// install, run Repair" apart from an ordinary broker fault.
const MANIFEST_CONTEXT: &str = "broker manifest";

/// Broker exit code for an ordinary runtime failure (the historical
/// catch-all). Windows renders service-specific codes in event 7024 as
/// meaningless Win32 error text, so these numbers are a machine-readable
/// channel the GUI reads back through SCM — not something to expect a
/// readable rendering of in the event log.
pub const SERVICE_EXIT_FAILURE: u32 = 1;

/// Broker exit code for install-integrity failures: the installation does not
/// match `manifest.json`, so no restart can help — only Repair/Install
/// rewrites manifest and binaries.
pub const SERVICE_EXIT_INSTALL_INTEGRITY: u32 = 2;

/// True when the broker refused to run over an install-integrity problem.
pub fn is_install_integrity_failure(error: &TmError) -> bool {
    matches!(error, TmError::Platform { context, .. } if *context == MANIFEST_CONTEXT)
}

/// The service-specific exit code the broker reports to SCM for `error`.
pub fn service_exit_code(error: &TmError) -> u32 {
    if is_install_integrity_failure(error) {
        SERVICE_EXIT_INSTALL_INTEGRITY
    } else {
        SERVICE_EXIT_FAILURE
    }
}

/// First 12 hex chars of a hash: enough for an operator to compare two
/// values by eye without flooding the log line.
fn hash_fingerprint(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// Compares one installed binary against its manifest pin. A mismatch is an
/// install-integrity failure, and its detail names the drifted binary plus
/// both fingerprints — the 2026-09-26 crash loop hid exactly this behind 853
/// opaque "service-specific error 1" events. Hashes of the installed binaries
/// are not secrets; they are pins the manifest already stores.
fn verify_pinned_hash(label: &str, expected: &str, actual: &str) -> Result<()> {
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(TmError::platform(
        MANIFEST_CONTEXT,
        format!(
            "installed {label} binary does not match the manifest \
             (expected sha256 {}…, found {}…); Repair/Install reinstalls the pinned generation",
            hash_fingerprint(expected),
            hash_fingerprint(actual),
        ),
    ))
}

fn validate_manifest(manifest: &BrokerManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA
        || manifest.protocol_version != PROTOCOL_VERSION
        || !valid_sid_text(&manifest.authorized_user_sid)
    {
        return Err(TmError::platform(
            MANIFEST_CONTEXT,
            "unsupported schema/protocol or invalid SID",
        ));
    }
    let own_exe = std::env::current_exe()?;
    if !paths_match(&own_exe, &manifest.service_path) {
        return Err(TmError::platform(
            MANIFEST_CONTEXT,
            "service is not running from its protected installed path",
        ));
    }
    if !paths_match(&expected_installed_gui_path()?, &manifest.gui_path)
        || !paths_match(&expected_installed_service_path()?, &manifest.service_path)
    {
        return Err(TmError::platform(
            MANIFEST_CONTEXT,
            "binary paths do not match the protected install location",
        ));
    }
    let service_hash = sha256_file(&manifest.service_path)?;
    verify_pinned_hash("service", &manifest.service_sha256, &service_hash)?;
    let gui_hash = sha256_file(&manifest.gui_path)?;
    verify_pinned_hash("GUI", &manifest.gui_sha256, &gui_hash)?;
    Ok(())
}

fn load_manifest() -> Result<BrokerManifest> {
    let path = manifest_path()?;
    let mut file = pinned_source(&path)?;
    let length = file.metadata()?.len();
    if length > 64 * 1024 {
        return Err(TmError::platform(MANIFEST_CONTEXT, "manifest is too large"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)?;
    // A corrupt manifest is an install-integrity problem like a hash
    // mismatch: Repair is the remedy, so it belongs to the same class.
    let manifest: BrokerManifest = serde_json::from_slice(&bytes).map_err(|error| {
        TmError::platform(MANIFEST_CONTEXT, format!("manifest is unreadable: {error}"))
    })?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

#[derive(Default)]
struct BrokerClient;

impl BrokerClient {
    fn call(&self, request: BrokerRequest) -> std::result::Result<BrokerValue, BrokerCallError> {
        let mut pipe = open_client_pipe()?;
        let server_pid = server_pid(&pipe)?;
        let expected = expected_installed_service_path().map_err(|error| {
            BrokerCallError::Unavailable(format!("cannot resolve service path: {error}"))
        })?;
        if !verify_pipe_server(server_pid, &expected)? {
            return Err(BrokerCallError::Rejected(
                "named-pipe server is not the protected TaskMan service".into(),
            ));
        }
        let payload = serde_json::to_vec(&request)
            .map_err(|error| BrokerCallError::NoAnswer(error.to_string()))?;
        write_frame(&mut pipe, FRAME_REQUEST, &payload, MAX_REQUEST_BYTES).map_err(|error| {
            BrokerCallError::NoAnswer(format!(
                "request delivery failed; action state is unknown: {error}"
            ))
        })?;
        let response =
            read_frame(&mut pipe, FRAME_RESPONSE, MAX_RESPONSE_BYTES).map_err(|error| {
                BrokerCallError::NoAnswer(format!(
                    "service response was lost; action state is unknown: {error}"
                ))
            })?;
        let response: BrokerResponse = serde_json::from_slice(&response)
            .map_err(|error| BrokerCallError::NoAnswer(error.to_string()))?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(BrokerCallError::NoAnswer(
                "service protocol version changed during request".into(),
            ));
        }
        response.value.map_err(BrokerCallError::Rejected)
    }
}

/// One bounded lane for sampler telemetry. A service that accepted a pipe but
/// stopped answering must not park the sampling engine forever. A timed-out
/// call may leave this one worker blocked, but no new workers or requests are
/// created until it completes.
type TelemetryReply = std::result::Result<BrokerValue, BrokerCallError>;
type TelemetryJob = (BrokerRequest, std::sync::mpsc::Sender<TelemetryReply>);

struct TelemetryBroker {
    tx: std::sync::mpsc::SyncSender<TelemetryJob>,
    busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TelemetryBroker {
    fn start_with(
        handler: impl Fn(BrokerRequest) -> std::result::Result<BrokerValue, BrokerCallError>
        + Send
        + 'static,
    ) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let (tx, rx) = std::sync::mpsc::sync_channel::<TelemetryJob>(1);
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_busy = busy.clone();
        std::thread::Builder::new()
            .name("tm-broker-telemetry".into())
            .spawn(move || {
                while let Ok((request, reply)) = rx.recv() {
                    let result = handler(request);
                    worker_busy.store(false, Ordering::Release);
                    let _ = reply.send(result);
                }
            })
            .ok()?;
        Some(Self { tx, busy })
    }

    fn call_with_timeout(
        &self,
        request: BrokerRequest,
        timeout: std::time::Duration,
    ) -> std::result::Result<BrokerValue, BrokerCallError> {
        use std::sync::atomic::Ordering;
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BrokerCallError::Unavailable(
                "telemetry request still in flight".into(),
            ));
        }
        let (reply, response) = std::sync::mpsc::channel();
        if self.tx.try_send((request, reply)).is_err() {
            self.busy.store(false, Ordering::Release);
            return Err(BrokerCallError::Unavailable(
                "telemetry worker stopped".into(),
            ));
        }
        match response.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if broker_warning_allowed() {
                    tracing::warn!("core-service telemetry response timed out");
                }
                Err(BrokerCallError::Unavailable(
                    "telemetry response timed out".into(),
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(
                BrokerCallError::Unavailable("telemetry worker stopped".into()),
            ),
        }
    }
}

fn telemetry_broker_call(
    request: BrokerRequest,
) -> std::result::Result<BrokerValue, BrokerCallError> {
    static WORKER: std::sync::OnceLock<Option<TelemetryBroker>> = std::sync::OnceLock::new();
    let Some(worker) =
        WORKER.get_or_init(|| TelemetryBroker::start_with(|request| BrokerClient.call(request)))
    else {
        return Err(BrokerCallError::Unavailable(
            "telemetry worker could not start".into(),
        ));
    };
    worker.call_with_timeout(request, std::time::Duration::from_secs(2))
}

/// Outcome of asking the broker for per-process network counters.
///
/// `Unavailable` and `Rejected` are kept apart on purpose: the first means
/// "no service, or one too old to know this request" — or an answer that was
/// lost in transport, which is what a telemetry timeout already reads as —
/// where falling back to our own token is correct; the second is the service
/// declining, which must never be worked around locally.
pub(crate) enum BrokeredNetwork {
    Sample(ProcessNetworkSample),
    Unavailable,
    Rejected(String),
}

/// Ask the protected service for per-process network counters.
pub(crate) fn brokered_process_network() -> BrokeredNetwork {
    match telemetry_broker_call(BrokerRequest::ProcessNetworkCounters) {
        Ok(BrokerValue::ProcessNetwork(sample)) => BrokeredNetwork::Sample(sample),
        Ok(_) => BrokeredNetwork::Rejected("unexpected response type".into()),
        Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => {
            BrokeredNetwork::Unavailable
        }
        Err(BrokerCallError::Rejected(detail)) => BrokeredNetwork::Rejected(detail),
    }
}

/// Outcome of asking the broker for per-process disk counters. The three
/// cases mean exactly what [`BrokeredNetwork`]'s do.
pub(crate) enum BrokeredDisk {
    Sample(ProcessDiskSample),
    Unavailable,
    Rejected(String),
}

/// Ask the protected service for per-process disk activity.
pub(crate) fn brokered_process_disk() -> BrokeredDisk {
    match telemetry_broker_call(BrokerRequest::ProcessDiskCounters) {
        Ok(BrokerValue::ProcessDisk(sample)) => BrokeredDisk::Sample(sample),
        Ok(_) => BrokeredDisk::Rejected("unexpected response type".into()),
        Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => {
            BrokeredDisk::Unavailable
        }
        Err(BrokerCallError::Rejected(detail)) => BrokeredDisk::Rejected(detail),
    }
}

/// Outcome of asking the broker for one process's token identity. The three
/// cases mean exactly what [`BrokeredNetwork`]'s do.
pub(crate) enum BrokeredIdentity {
    Token(super::process_ops::ProcessTokenIdentity),
    Unavailable,
    Rejected(String),
}

/// Ask the protected service for the token identity of one exact process.
///
/// This is the only way an unelevated GUI learns who owns the SYSTEM and
/// service processes: their processes and tokens both refuse our own token,
/// while LocalSystem opens the unprotected ones. Routed through the bounded
/// telemetry lane like the counter queries so a wedged service can stall one
/// worker for its deadline, never the sampling engine.
pub(crate) fn brokered_process_identity(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> BrokeredIdentity {
    match telemetry_broker_call(BrokerRequest::ProcessIdentity {
        pid,
        expected_start_epoch_s,
    }) {
        Ok(BrokerValue::ProcessIdentity(identity)) => BrokeredIdentity::Token(identity),
        Ok(_) => BrokeredIdentity::Rejected("unexpected response type".into()),
        Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => {
            BrokeredIdentity::Unavailable
        }
        Err(BrokerCallError::Rejected(detail)) => BrokeredIdentity::Rejected(detail),
    }
}

/// Windows action surface that prefers the authenticated service and falls
/// back to direct user-token operations only when the service is unavailable.
/// An explicit service rejection never falls back, because doing so could turn
/// a security policy decision into a confused-deputy bypass.
pub struct BrokeredActions {
    local: super::WinActions,
    client: BrokerClient,
}

impl Default for BrokeredActions {
    fn default() -> Self {
        Self {
            local: super::WinActions,
            client: BrokerClient,
        }
    }
}

/// Mark `pid`'s cached slow-changing attributes stale after a successful
/// control action. The broker path bypasses `process_ops` entirely, so
/// without this the sampler would keep serving the pre-change priority /
/// efficiency / virtualization state for the rest of its attribute TTL
/// exactly when the privileged path is the one that succeeded.
fn noting_attrs(pid: u32, result: Result<()>) -> Result<()> {
    if result.is_ok() {
        super::process_ops::note_attrs_changed(pid);
    }
    result
}

impl BrokeredActions {
    fn unit_or_local(
        &self,
        request: BrokerRequest,
        local: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        match self.client.call(request) {
            Ok(BrokerValue::Unit) => Ok(()),
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected response type",
            )),
            Err(BrokerCallError::Unavailable(_)) => local(),
            // A lost answer is not a decision, but an ACTION whose answer was
            // lost has an unknown outcome: reporting it is the only honest
            // answer, and retrying it on the local token would repeat its side
            // effect.
            Err(BrokerCallError::NoAnswer(detail)) => {
                Err(TmError::platform("core service", detail))
            }
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }

    /// Run `request` and decode its answer, falling back to `local` when the
    /// service is absent — and, when `lost_answer_falls_back` is set, when the
    /// answer never arrived either.
    ///
    /// A lost answer is an unknown rather than a decision, so a caller that only
    /// reads may take the local arm; a caller that changes state must pass
    /// `false`, because repeating its side effect locally after an
    /// unacknowledged attempt is how one reference becomes two.
    fn value_or_local<T>(
        &self,
        request: BrokerRequest,
        lost_answer_falls_back: bool,
        decode: impl FnOnce(BrokerValue) -> Option<T>,
        local: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        match self.client.call(request) {
            Ok(value) => decode(value)
                .ok_or_else(|| TmError::platform("core service", "unexpected response type")),
            Err(BrokerCallError::Unavailable(_)) => local(),
            Err(BrokerCallError::NoAnswer(_)) if lost_answer_falls_back => local(),
            Err(BrokerCallError::NoAnswer(detail)) => {
                Err(TmError::platform("core service", detail))
            }
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }
}

/// Fill only privileged per-thread evidence. A TID is mergeable only when the
/// local list and the broker read agree on the thread creation time; a recycled
/// TID must never inherit another thread's start address or cycles.
fn merge_process_thread_details(
    threads: &mut crate::actions::ProcessThreads,
    details: &[ProcessThreadDetail],
) -> usize {
    let mut filled = 0usize;
    for thread in &mut threads.threads {
        if thread.start_address.is_some() && thread.cycles.is_some() {
            continue;
        }
        let Some(created_epoch_s) = thread.created_epoch_s else {
            continue;
        };
        let Some(detail) = details.iter().find(|detail| {
            detail.tid == thread.tid && detail.created_epoch_s == Some(created_epoch_s)
        }) else {
            continue;
        };
        let before = (thread.start_address, thread.cycles);
        if thread.start_address.is_none() {
            thread.start_address = detail.start_address;
        }
        if thread.cycles.is_none() {
            thread.cycles = detail.cycles;
        }
        if (thread.start_address, thread.cycles) != before {
            filled += 1;
        }
    }
    filled
}

impl PlatformActions for BrokeredActions {
    fn capabilities(&self) -> Capabilities {
        self.local.capabilities()
    }

    fn list_services(&self) -> Result<Vec<tm_core::model::ServiceInfo>> {
        self.local.list_services()
    }

    fn control_service(&self, name: &str, action: ServiceAction) -> Result<()> {
        self.unit_or_local(
            BrokerRequest::ControlService {
                name: name.to_string(),
                action,
            },
            || self.local.control_service(name, action),
        )
    }
    fn open_services_app(&self) -> Result<()> {
        self.local.open_services_app()
    }

    fn list_startup(&self) -> Result<Vec<tm_core::model::StartupItem>> {
        self.local.list_startup()
    }

    fn set_startup_enabled(&self, item_id: &str, location: &str, enabled: bool) -> Result<()> {
        self.local.set_startup_enabled(item_id, location, enabled)
    }

    fn list_user_sessions(&self) -> Result<Vec<tm_core::model::UserSession>> {
        self.local.list_user_sessions()
    }

    fn control_user_session(&self, session_id: u32, action: UserSessionAction) -> Result<()> {
        self.unit_or_local(
            BrokerRequest::ControlUserSession { session_id, action },
            || self.local.control_user_session(session_id, action),
        )
    }
    fn manage_user_accounts(&self) -> Result<()> {
        self.local.manage_user_accounts()
    }

    fn kill_process(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        tree: bool,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            // PID-only compatibility calls cannot safely cross a privilege
            // boundary because the PID may have been recycled. Keep them
            // constrained to the interactive user's token.
            return self.local.kill_process(pid, None, tree);
        };
        self.unit_or_local(
            BrokerRequest::KillProcess {
                pid,
                expected_start_epoch_s: Some(start_epoch_s),
                tree,
            },
            || self.local.kill_process(pid, Some(start_epoch_s), tree),
        )
    }

    fn kill_single(&self, pid: u32) -> Result<()> {
        self.kill_process(pid, None, false)
    }

    fn suspend_process(&self, pid: u32, suspend: bool) -> Result<()> {
        self.suspend_process_checked(pid, None, suspend)
    }

    fn suspend_process_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        suspend: bool,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self.local.suspend_process_checked(pid, None, suspend);
        };
        self.unit_or_local(
            BrokerRequest::SuspendProcess {
                pid,
                expected_start_epoch_s: Some(start_epoch_s),
                suspend,
            },
            || {
                self.local
                    .suspend_process_checked(pid, Some(start_epoch_s), suspend)
            },
        )
    }

    fn set_priority(&self, pid: u32, priority: PriorityClass) -> Result<()> {
        self.set_priority_checked(pid, None, priority)
    }

    fn set_priority_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        priority: PriorityClass,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self.local.set_priority_checked(pid, None, priority);
        };
        noting_attrs(
            pid,
            self.unit_or_local(
                BrokerRequest::SetPriority {
                    pid,
                    expected_start_epoch_s: Some(start_epoch_s),
                    priority,
                },
                || {
                    self.local
                        .set_priority_checked(pid, Some(start_epoch_s), priority)
                },
            ),
        )
    }

    fn get_affinity_mask(&self, pid: u32) -> Result<u64> {
        self.get_affinity_mask_checked(pid, None)
    }

    fn get_affinity_mask_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<u64> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self.local.get_affinity_mask_checked(pid, None);
        };
        self.value_or_local(
            BrokerRequest::GetAffinity {
                pid,
                expected_start_epoch_s: Some(start_epoch_s),
            },
            // A read: the local arm reads the same mask, so a lost answer may
            // take it.
            true,
            |value| match value {
                BrokerValue::AffinityMask(mask) => Some(mask),
                _ => None,
            },
            || {
                self.local
                    .get_affinity_mask_checked(pid, Some(start_epoch_s))
            },
        )
    }

    fn system_affinity_mask(&self) -> Result<u64> {
        self.local.system_affinity_mask()
    }

    fn set_affinity_mask(&self, pid: u32, mask: u64) -> Result<()> {
        self.set_affinity_mask_checked(pid, None, mask)
    }

    fn set_affinity_mask_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        mask: u64,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self.local.set_affinity_mask_checked(pid, None, mask);
        };
        self.unit_or_local(
            BrokerRequest::SetAffinity {
                pid,
                expected_start_epoch_s: Some(start_epoch_s),
                mask,
            },
            || {
                self.local
                    .set_affinity_mask_checked(pid, Some(start_epoch_s), mask)
            },
        )
    }

    fn set_efficiency_mode(&self, pid: u32, on: bool) -> Result<()> {
        self.set_efficiency_mode_checked(pid, None, on)
    }

    fn set_efficiency_mode_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        on: bool,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self.local.set_efficiency_mode_checked(pid, None, on);
        };
        noting_attrs(
            pid,
            self.unit_or_local(
                BrokerRequest::SetEfficiencyMode {
                    pid,
                    expected_start_epoch_s: Some(start_epoch_s),
                    enabled: on,
                },
                || {
                    self.local
                        .set_efficiency_mode_checked(pid, Some(start_epoch_s), on)
                },
            ),
        )
    }

    fn set_uac_virtualization_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        enabled: bool,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self
                .local
                .set_uac_virtualization_checked(pid, None, enabled);
        };
        noting_attrs(
            pid,
            self.unit_or_local(
                BrokerRequest::SetUacVirtualization {
                    pid,
                    expected_start_epoch_s: Some(start_epoch_s),
                    enabled,
                },
                || {
                    self.local
                        .set_uac_virtualization_checked(pid, Some(start_epoch_s), enabled)
                },
            ),
        )
    }

    fn process_security_info_checked(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<super::ProcessSecurityInfo> {
        let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
            return self
                .local
                .process_security_info_checked(pid, expected_start_epoch_s);
        };
        let local = || {
            self.local
                .process_security_info_checked(pid, Some(start_epoch_s))
        };
        match self.client.call(BrokerRequest::ProcessSecurityInfo {
            pid,
            expected_start_epoch_s: Some(start_epoch_s),
        }) {
            Ok(BrokerValue::ProcessSecurity(info)) => Ok(info),
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected process-security response type",
            )),
            Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => local(),
            Err(BrokerCallError::Rejected(detail)) => {
                local().map_err(|_| TmError::platform("core service", detail))
            }
        }
    }

    fn list_process_modules(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<Vec<ProcessModule>> {
        let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
            // The local path still performs a handle-before/after creation-time
            // check around ToolHelp enumeration, so processes without sampled
            // creation metadata retain the pre-broker behavior safely.
            return self.local.list_process_modules(pid, expected_start_epoch_s);
        };
        let local = || self.local.list_process_modules(pid, Some(start_epoch_s));
        match self.client.call(BrokerRequest::ProcessModules {
            pid,
            expected_start_epoch_s: Some(start_epoch_s),
        }) {
            Ok(BrokerValue::ProcessModules(modules)) => Ok(modules),
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected process-modules response type",
            )),
            Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => local(),
            Err(BrokerCallError::Rejected(detail)) => {
                local().map_err(|_| TmError::platform("core service", detail))
            }
        }
    }

    fn process_module_code_samples(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<Option<ProcessModuleCodeWindow>> {
        let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
            return Err(TmError::platform(
                "core service",
                "a valid sampled process creation time is required",
            ));
        };
        let local = || {
            self.local
                .process_module_code_samples(pid, Some(start_epoch_s))
        };
        match self.client.call(BrokerRequest::ProcessModuleCodeSamples {
            pid,
            expected_start_epoch_s: Some(start_epoch_s),
        }) {
            Ok(BrokerValue::ProcessModuleCodeSamples(window)) => Ok(window),
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected module code-sample response type",
            )),
            Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => local(),
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }

    fn list_process_threads(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<crate::actions::ProcessThreads> {
        let mut threads = self
            .local
            .list_process_threads(pid, expected_start_epoch_s)?;
        let missing = threads
            .threads
            .iter()
            .filter(|thread| thread.start_address.is_none() || thread.cycles.is_none())
            .count();
        if missing == 0 {
            return Ok(threads);
        }
        let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
            return Ok(threads);
        };
        match self.client.call(BrokerRequest::ProcessThreadDetails {
            pid,
            expected_start_epoch_s: Some(start_epoch_s),
        }) {
            Ok(BrokerValue::ProcessThreadDetails(details)) => {
                merge_process_thread_details(&mut threads, &details);
                Ok(threads)
            }
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected thread-details response type",
            )),
            // The handle-free list remains trustworthy without privileged
            // evidence. Keep showing it with honest "—" values when the
            // service is old, absent, or refuses the read.
            Err(BrokerCallError::Unavailable(detail)) => {
                tracing::debug!(pid, missing, %detail, "broker thread details unavailable");
                Ok(threads)
            }
            // A lost answer is no better evidence than an absent service: the
            // handle-free list still stands and the privileged columns stay "—".
            Err(BrokerCallError::NoAnswer(detail)) => {
                tracing::debug!(pid, missing, %detail, "broker thread details lost");
                Ok(threads)
            }
            Err(BrokerCallError::Rejected(detail)) => {
                tracing::debug!(pid, missing, %detail, "broker refused thread details");
                Ok(threads)
            }
        }
    }

    // This is a handle-free IP Helper read that already sees every process
    // from the user's token; it never crosses the broker.
    fn list_process_connections(&self, pid: u32) -> Result<Vec<crate::actions::ProcessConnection>> {
        self.local.list_process_connections(pid)
    }

    fn start_background_accounting(&self) {
        self.local.start_background_accounting();
    }

    fn list_process_file_io(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<crate::actions::ProcessFileIoSnapshot> {
        let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
            return self.local.list_process_file_io(pid, expected_start_epoch_s);
        };
        let local = || self.local.list_process_file_io(pid, Some(start_epoch_s));
        match self.client.call(BrokerRequest::ProcessFileIo {
            pid,
            expected_start_epoch_s: Some(start_epoch_s),
        }) {
            Ok(BrokerValue::ProcessFileIo(snapshot)) => Ok(snapshot),
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected process file I/O response type",
            )),
            // Transport loss is a legitimate unavailability: the local arm makes
            // the same read with our own token. A service that answered and
            // declined is a decision and still propagates below.
            Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_)) => local(),
            // The service declining is a decision, and the local arm is no
            // longer a plain read: it starts a per-file session under this
            // process's own token, machine-wide, whenever that token is
            // elevated. Working a policy decision around by hosting the same
            // measurement locally is exactly what every other rejection arm in
            // this file refuses to do, so this one propagates the reason.
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }

    fn unload_process_module(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        base_address: u64,
        expected_path: &str,
    ) -> Result<ModuleUnloadOutcome> {
        let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
            return Err(TmError::platform(
                "unload module",
                "a valid sampled process creation time is required",
            ));
        };
        self.value_or_local(
            BrokerRequest::UnloadModule {
                pid,
                expected_start_epoch_s: Some(start_epoch_s),
                base_address,
                expected_path: expected_path.to_string(),
            },
            // A state change: repeating a FreeLibrary locally after an
            // unacknowledged attempt could release a second reference.
            false,
            |value| match value {
                BrokerValue::ModuleUnload {
                    still_mapped,
                    verified,
                    ..
                } => Some(ModuleUnloadOutcome {
                    still_mapped: verified.then_some(still_mapped),
                }),
                _ => None,
            },
            || {
                self.local.unload_process_module(
                    pid,
                    Some(start_epoch_s),
                    base_address,
                    expected_path,
                )
            },
        )
    }

    fn is_elevated(&self) -> bool {
        self.local.is_elevated()
    }
    fn elevation_status(&self) -> Option<bool> {
        self.local.elevation_status()
    }

    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        self.local.run_new_task(command_line, elevate)
    }

    fn run_new_task_probe(&self, command_line: &str, elevate: bool) -> Result<()> {
        self.local.run_new_task_probe(command_line, elevate)
    }

    fn relaunch_elevated(&self) -> Result<()> {
        self.local.relaunch_elevated()
    }

    fn launch_native_task_manager(&self) -> Result<()> {
        self.local.launch_native_task_manager()
    }

    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {
        match self.client.call(BrokerRequest::TaskManagerReplacementState) {
            Ok(BrokerValue::TaskManagerReplacementState(state)) => state,
            _ => self.local.task_manager_replacement_state(),
        }
    }

    fn set_task_manager_replacement(&self, enabled: bool) -> Result<()> {
        self.unit_or_local(BrokerRequest::SetTaskManagerReplacement { enabled }, || {
            self.local.set_task_manager_replacement(enabled)
        })
    }

    fn repair_task_manager_replacement(&self) -> Result<bool> {
        // The broker owns an unprompted HKLM write, and it always re-points
        // at the protected GUI it verified — exactly the executable that is
        // guaranteed to still be there.
        match self
            .client
            .call(BrokerRequest::SetTaskManagerReplacement { enabled: true })
        {
            Ok(BrokerValue::Unit) => Ok(true),
            Ok(_) => Err(TmError::platform(
                "core service",
                "unexpected response type",
            )),
            Err(BrokerCallError::Unavailable(_)) => self.local.repair_task_manager_replacement(),
            // The broker was asked to write the key and its answer was lost, so
            // whether it happened is unknown. Repeating the write locally would
            // claim a repair this process never got an acknowledgement for.
            Err(BrokerCallError::NoAnswer(detail)) => {
                Err(TmError::platform("core service", detail))
            }
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }

    fn set_start_with_system(&self, enabled: bool, start_minimized: bool) -> Result<()> {
        self.local.set_start_with_system(enabled, start_minimized)
    }

    fn core_service_state(&self) -> CoreServiceState {
        service_state(&self.client)
    }

    fn set_core_service_installed(&self, installed: bool) -> Result<()> {
        launch_service_helper(installed)?;
        if installed {
            let installed_gui = expected_installed_gui_path()?;
            set_client_redirect_marker(Some(&installed_gui))?;
            if let Err(error) = super::autostart::retarget_if_registered(&installed_gui) {
                // Autostart is independent of the privileged boundary. A
                // malformed/stale Run value must not make a completed service
                // installation look as though it failed.
                tracing::warn!(%error, "could not retarget existing TaskMan autostart");
            }
            Ok(())
        } else {
            set_client_redirect_marker(None)
        }
    }

    fn switch_to_installed_gui(&self, args: &[String]) -> Result<bool> {
        relaunch_into_installed_gui(args)
    }

    fn create_dump_file(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        path: &Path,
        dump_type: tm_core::model::DumpType,
    ) -> Result<()> {
        // A path must never become a SYSTEM write primitive. The GUI's user
        // token owns this operation until handle transfer is implemented.
        self.local
            .create_dump_file(pid, expected_start_epoch_s, path, dump_type)
    }

    fn create_dump_file_with_progress(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        path: &Path,
        dump_type: tm_core::model::DumpType,
        progress: Option<std::sync::Arc<tm_core::model::DumpProgressTracker>>,
    ) -> Result<()> {
        self.local.create_dump_file_with_progress(
            pid,
            expected_start_epoch_s,
            path,
            dump_type,
            progress,
        )
    }

    fn open_file_location(&self, path: &str) -> Result<()> {
        self.local.open_file_location(path)
    }

    fn open_properties(&self, path: &str) -> Result<()> {
        self.local.open_properties(path)
    }

    fn open_url(&self, url: &str) -> Result<()> {
        self.local.open_url(url)
    }

    fn last_bios_time_ms(&self) -> Option<u64> {
        self.local.last_bios_time_ms()
    }

    fn process_icon_rgba(&self, exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
        self.local.process_icon_rgba(exe_path)
    }

    fn default_app_icon_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.local.default_app_icon_rgba()
    }

    fn backend_name(&self) -> &'static str {
        "win32+core-service"
    }
}

fn open_client_pipe() -> std::result::Result<File, BrokerCallError> {
    let name = wide(PIPE_NAME);
    let open = || unsafe {
        // Exact rights match the user ACE (data read/write, attributes, and
        // synchronize) and avoid requesting FILE_CREATE_PIPE_INSTANCE through
        // GENERIC_WRITE. See USER_PIPE_ACCESS for why the attribute right is
        // part of the request.
        CreateFileW(
            PCWSTR(name.as_ptr()),
            USER_PIPE_ACCESS,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    // The broker accepts on a single thread, so two GUI threads calling at the
    // same instant routinely collide and one of them sees ERROR_PIPE_BUSY.
    // WaitNamedPipeW only reports that AN instance became free - another
    // client can still take it first - so a single retry loses that race often
    // enough to report a healthy service as missing. Retry until a deadline
    // instead; this is an eventful wait on instance availability, not a sleep.
    let deadline = std::time::Instant::now() + PIPE_BUSY_RETRY;
    loop {
        let error = match open() {
            // SAFETY: `handle` comes from a successful `CreateFileW` on this
            // same pipe (raw HANDLE, not previously wrapped); transferring it
            // into `File` gives it exactly one owner, so the retry loop above
            // never wraps — and thus never closes — the same handle twice.
            Ok(handle) => return Ok(unsafe { File::from_raw_handle(handle.0) }),
            Err(error) => error,
        };
        if error.code() != ERROR_PIPE_BUSY.to_hresult() {
            return Err(BrokerCallError::Unavailable(error.to_string()));
        }
        if std::time::Instant::now() >= deadline {
            return Err(BrokerCallError::Unavailable(
                "core service is busy or stopped".into(),
            ));
        }
        // A timeout here is not conclusive: the deadline above owns the
        // decision, and another attempt may still find a free instance.
        let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), PIPE_BUSY_WAIT_MS) };
    }
}

fn server_pid(pipe: &File) -> std::result::Result<u32, BrokerCallError> {
    let mut pid = 0;
    unsafe {
        GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)
            .map_err(|error| BrokerCallError::Rejected(error.to_string()))?;
    }
    if pid == 0 {
        return Err(BrokerCallError::Rejected(
            "pipe server did not report a process identity".into(),
        ));
    }
    Ok(pid)
}

/// Verify the connected pipe server is the protected service process.
///
/// The direct image-path check opens the LocalSystem service process with
/// PROCESS_QUERY_LIMITED_INFORMATION, which a non-elevated GUI token is
/// denied — that must degrade, not fail the handshake. On denial the SCM
/// view is used instead: the pipe server PID must be the PID the SCM
/// reports for our service, and the service's configured image must be the
/// protected install path. Both are readable by ordinary users, and
/// registering a service under our name requires administrator rights, so
/// an impostor cannot satisfy the pairing without already being admin.
fn verify_pipe_server(
    server_pid: u32,
    expected: &Path,
) -> std::result::Result<bool, BrokerCallError> {
    match process_image_path(server_pid) {
        Ok(server_path) => return Ok(paths_match(&server_path, expected)),
        Err(BrokerCallError::Rejected(detail)) => {
            tracing::debug!(
                server_pid,
                %detail,
                "pipe server image check unavailable; verifying through the SCM"
            );
        }
        Err(error) => return Err(error),
    }
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| {
            BrokerCallError::Unavailable(format!("cannot open service manager: {error}"))
        })?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        )
        .map_err(|error| {
            BrokerCallError::Unavailable(format!(
                "cannot open the core service for identity verification: {error}"
            ))
        })?;
    let status = service.query_status().map_err(|error| {
        BrokerCallError::Unavailable(format!("cannot query core service status: {error}"))
    })?;
    if status.process_id != Some(server_pid) {
        return Ok(false);
    }
    let config = service.query_config().map_err(|error| {
        BrokerCallError::Unavailable(format!("cannot query core service config: {error}"))
    })?;
    // The SCM reports the image path as configured, typically quoted because
    // it contains spaces. windows-service keeps the quotes; strip them before
    // comparing paths.
    let configured = config.executable_path.to_string_lossy();
    let configured = configured.trim_matches('"');
    Ok(paths_match(Path::new(configured), expected))
}

fn client_pid(pipe: &File) -> Result<u32> {
    let mut pid = 0;
    unsafe {
        GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)
            .map_err(|error| TmError::platform("named-pipe client PID", error.to_string()))?;
    }
    if pid == 0 {
        return Err(TmError::platform(
            "named-pipe client PID",
            "client reported PID zero",
        ));
    }
    Ok(pid)
}

fn create_pipe_instance(pipe_name: &str, sddl: &str, first: bool) -> Result<File> {
    let descriptor = security_descriptor(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    let name = wide(pipe_name);
    let mut open_mode = PIPE_ACCESS_DUPLEX.0;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE.0;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(open_mode),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_INSTANCE_CAP,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            250,
            Some(&attributes),
        )
    };
    if handle.is_invalid() {
        let error = std::io::Error::last_os_error();
        // Kept as the raw OS error so the accept loop can tell back-pressure
        // (`is_pipe_busy`) from a real fault.
        if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) {
            return Err(TmError::Io(error));
        }
        return Err(TmError::platform("CreateNamedPipeW", error.to_string()));
    }
    // SAFETY: `handle` is the fresh, validity-checked result of
    // `CreateNamedPipeW` above (the invalid case returned early); wrapping it
    // once in `File` makes the kernel pipe instance and its ACL subject to
    // ordinary Rust drop — there is no second owner.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// Every one of the pipe's `PIPE_INSTANCE_CAP` instances is in use.
fn is_pipe_busy(error: &TmError) -> bool {
    matches!(error, TmError::Io(io) if io.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32))
}

/// Outcome of waiting for a client on one listening pipe instance.
enum AcceptOutcome {
    /// A client is connected on this instance.
    Connected,
    /// The client closed its handle before the server accepted it. The
    /// instance is spent, but nothing failed.
    ClientVanished,
    /// The instance itself could not be used.
    Failed(TmError),
}

fn connect_pipe(pipe: &File) -> AcceptOutcome {
    match unsafe { ConnectNamedPipe(HANDLE(pipe.as_raw_handle()), None) } {
        Ok(()) => AcceptOutcome::Connected,
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => AcceptOutcome::Connected,
        // ERROR_NO_DATA means the client end was already closed when we got
        // here: npfs connects an opener to a listening instance before the
        // server calls ConnectNamedPipe, so any client that opens and drops
        // the handle lands in this state. That is ordinary client behaviour -
        // a call abandoned during identity verification, a GUI shutting down,
        // the stop thread's own synthetic wake - and it must never be able to
        // terminate the privileged control plane.
        Err(error) if error.code() == ERROR_NO_DATA.to_hresult() => AcceptOutcome::ClientVanished,
        Err(error) => {
            AcceptOutcome::Failed(TmError::platform("ConnectNamedPipe", error.to_string()))
        }
    }
}

struct ClientWork {
    pipe: File,
    client_pid: u32,
}

fn broker_warning_allowed() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    static LAST_MS: AtomicU64 = AtomicU64::new(0);

    let now = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX - 1)) as u64
        + 1;
    let mut previous = LAST_MS.load(Ordering::Relaxed);
    loop {
        if previous != 0 && now.saturating_sub(previous) < 5_000 {
            return false;
        }
        match LAST_MS.compare_exchange_weak(previous, now, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => previous = actual,
        }
    }
}

fn handle_client(work: ClientWork, actions: &super::WinActions) {
    let ClientWork {
        mut pipe,
        client_pid,
    } = work;
    let result = (|| -> Result<()> {
        let payload = read_frame(&mut pipe, FRAME_REQUEST, MAX_REQUEST_BYTES)?;
        let request: BrokerRequest = serde_json::from_slice(&payload)?;
        let value = dispatch(actions, request, client_pid).map_err(|error| error.to_string());
        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value,
        };
        let payload = serde_json::to_vec(&response)?;
        write_frame(&mut pipe, FRAME_RESPONSE, &payload, MAX_RESPONSE_BYTES)?;
        Ok(())
    })();
    if let Err(error) = result
        && broker_warning_allowed()
    {
        tracing::warn!(%error, "broker client request failed");
    }
    // Deliberately no DisconnectNamedPipe here: it discards response bytes the
    // client has not read yet, so a racing client lost its response and saw
    // "action state is unknown". Dropping the handle lets the client drain
    // the response first; the pipe instance is released when the client's end
    // closes.
}

fn reject_client(mut pipe: File, detail: impl Into<String>) {
    let response = BrokerResponse {
        protocol_version: PROTOCOL_VERSION,
        value: Err(detail.into()),
    };
    if let Ok(payload) = serde_json::to_vec(&response) {
        let _ = write_frame(&mut pipe, FRAME_RESPONSE, &payload, MAX_RESPONSE_BYTES);
    }
    // Drop without DisconnectNamedPipe so the rejection text survives the
    // teardown race; see the note in handle_client.
}

fn checked_target(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
    requesting_gui_pid: u32,
) -> Result<()> {
    let Some(created) = expected_start_epoch_s.filter(|created| *created > 0) else {
        return Err(TmError::platform(
            "broker target",
            "a valid sampled process creation time is required",
        ));
    };
    if pid <= 4 || pid == std::process::id() || pid == requesting_gui_pid {
        return Err(TmError::platform(
            "broker target",
            "system, broker, and requesting GUI processes are protected",
        ));
    }
    // Keep the value consumed here rather than allowing a future refactor to
    // silently reintroduce action-time PID rebinding.
    let _ = created;
    super::process_ops::refuse_critical_process(pid)
}

fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b' '))
}

/// How long the broker keeps the network trace alive after the last request.
/// The GUI polls once per sampling tick while a page that shows the column is
/// visible, so a few missed ticks (paused sampling, a slow frame) must not
/// tear the session down and lose the counters.
const NET_TRACE_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Hard cap on entries in one response, sized so even worst-case counters fit
/// `MAX_RESPONSE_BYTES`. A desktop produces a few dozen entries. Above the
/// cap the entire answer is unknown; omitting active processes and rendering
/// them as measured zero would violate the telemetry contract. Pinned by
/// `a_capped_network_response_fits_the_frame_limit`.
const NET_MAX_ENTRIES: usize = 5_000;

/// The broker-hosted ETW trace. Started on the first request and stopped by a
/// watchdog once the GUI stops asking, so the service is not tracing whenever
/// nobody is looking at the column.
struct NetTrace {
    usage: super::net_etw::NetworkUsage,
    last_request: std::time::Instant,
}

fn net_trace() -> &'static std::sync::Mutex<Option<NetTrace>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<NetTrace>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Answer one `ProcessNetworkCounters` request, starting the trace if needed.
///
/// Counters are pruned to processes that still exist. The service, not the
/// caller, decides what is live: it is the one holding the cumulative map, and
/// trusting a pid list from the client would let the client shape the answer.
fn process_network_sample() -> ProcessNetworkSample {
    let mut slot = match net_trace().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.as_ref().is_some_and(|trace| !trace.usage.is_running()) {
        // Stopped underneath the broker: its totals are frozen. Replace the
        // session in place so the slot's one watchdog keeps covering it.
        tracing::warn!("broker network trace ended unexpectedly; restarting it");
        match super::net_etw::NetworkUsage::start(super::net_etw::TraceRole::Service) {
            Some(usage) => {
                if let Some(trace) = slot.as_mut() {
                    trace.usage = usage;
                    // The replacement is timed from this request, exactly as
                    // the disk sibling times its own: the idle watchdog
                    // measures `last_request`, and a wake that is already due
                    // would otherwise judge the old session's age against the
                    // one that just started and retire it on the way past.
                    trace.last_request = std::time::Instant::now();
                }
                // A replacement's totals start again at zero, and an empty
                // total is indistinguishable from a measured "no traffic".
                // Answering this request from it would report 0 B/s and a
                // zeroed byte total for every process on the tick the counters
                // restarted — the fabricated zero the disk sibling avoids by
                // answering unknown here. The next request measures it.
                return ProcessNetworkSample::default();
            }
            None => return ProcessNetworkSample::default(),
        }
    }
    if slot.is_none() {
        match super::net_etw::NetworkUsage::start(super::net_etw::TraceRole::Service) {
            Some(usage) => {
                *slot = Some(NetTrace {
                    usage,
                    last_request: std::time::Instant::now(),
                });
                if !spawn_net_trace_watchdog() {
                    // Nothing else stops this session. Keeping it would trace
                    // every process's network traffic on the machine for the
                    // life of the service, so it is dropped and the answer
                    // stays unknown.
                    tracing::error!(
                        "could not start the network-trace watchdog; the session was not kept"
                    );
                    *slot = None;
                    return ProcessNetworkSample::default();
                }
                // A session started here has delivered nothing yet, so it
                // cannot speak for any process either; see the restart above.
                return ProcessNetworkSample::default();
            }
            None => {
                // LocalSystem should always be able to start a session; if it
                // cannot, report unknown rather than a fabricated zero.
                tracing::warn!("broker could not start the per-process network trace");
                return ProcessNetworkSample::default();
            }
        }
    }
    let Some(trace) = slot.as_mut() else {
        return ProcessNetworkSample::default();
    };
    trace.last_request = std::time::Instant::now();
    let live = live_pids();
    let totals = trace.usage.totals_pruned(&live);
    let entries: Vec<ProcessNetworkEntry> = totals
        .into_iter()
        .filter(|(_, bytes)| bytes.received != 0 || bytes.sent != 0)
        .map(|(pid, bytes)| ProcessNetworkEntry {
            pid,
            received: bytes.received,
            sent: bytes.sent,
        })
        .collect();
    if entries.len() > NET_MAX_ENTRIES {
        if broker_warning_allowed() {
            tracing::warn!(
                count = entries.len(),
                "network response exceeds the frame budget"
            );
        }
        return ProcessNetworkSample::default();
    }
    ProcessNetworkSample {
        active: true,
        entries,
    }
}

/// Hard caps on one disk response: entries, and per-disk items across the
/// entries and the totals. Worst-case JSON stays below the 512 KiB response
/// frame. Overflow is unavailable rather than a false zero on omitted
/// processes, and the window it refused is held for the next request instead of
/// being dropped (see [`process_disk_sample`]). Pinned by
/// `a_capped_disk_response_fits_the_frame_limit`.
const DISK_MAX_ENTRIES: usize = 1_800;
const DISK_MAX_DISK_TIMES: usize = 3_600;

/// Whether one drained window encodes within the per-response caps.
///
/// The counts are exactly the ones the response carries: a row for every
/// process that completed at least one request, that row's per-disk split, and
/// the window's own per-disk totals.
fn disk_window_fits(window: &super::disk_etw::DiskWindow) -> bool {
    let mut entries = 0usize;
    let mut disk_times = window.disk_totals.len();
    for row in window.procs.values().filter(|row| row.ops != 0) {
        entries += 1;
        disk_times += row.per_disk.len();
    }
    entries <= DISK_MAX_ENTRIES && disk_times <= DISK_MAX_DISK_TIMES
}

/// Fold a window a previous response could not carry into the one that follows
/// it.
///
/// The two cover consecutive spans, so the merged window's `since_ms` is their
/// sum and every counter adds: nothing is charged to a guess and no record is
/// dropped. A merged window is measured as one longer interval, so the shares
/// derived from it stay ratios of the same denominator.
fn merge_disk_windows(into: &mut super::disk_etw::DiskWindow, later: super::disk_etw::DiskWindow) {
    into.since_ms = into.since_ms.saturating_add(later.since_ms);
    into.events_seen = into.events_seen.saturating_add(later.events_seen);
    into.events_decoded = into.events_decoded.saturating_add(later.events_decoded);
    into.unattributed_service_time = into
        .unattributed_service_time
        .saturating_add(later.unattributed_service_time);
    for total in later.disk_totals {
        super::disk_etw::add_disk_time(&mut into.disk_totals, total.disk, total.service_time);
    }
    for (pid, disk) in later.procs {
        let row = into.procs.entry(pid).or_default();
        row.service_time = row.service_time.saturating_add(disk.service_time);
        row.read_bytes = row.read_bytes.saturating_add(disk.read_bytes);
        row.write_bytes = row.write_bytes.saturating_add(disk.write_bytes);
        row.ops = row.ops.saturating_add(disk.ops);
        for part in disk.per_disk {
            super::disk_etw::add_disk_time(&mut row.per_disk, part.disk, part.service_time);
        }
    }
}

/// Drop rows for processes that no longer exist from a window held across
/// requests, charging their service time to the unattributed remainder.
///
/// `take_window` prunes the window it drains, but a held window was drained by
/// an earlier request and its rows are never re-checked: without this, a
/// process that exits during the hold keeps its row for as long as the hold
/// lasts, and every row is one more entry the response caps count — so the one
/// thing that could make an oversized window publishable again would never
/// happen. The charging mirrors the drain exactly, including its empty-`live`
/// case: an empty set means "the caller could not enumerate processes", never
/// "nothing is alive".
fn prune_held_disk_window(
    window: &mut super::disk_etw::DiskWindow,
    live: &std::collections::HashSet<u32>,
) {
    if live.is_empty() {
        return;
    }
    let mut orphaned = 0u64;
    window.procs.retain(|pid, row| {
        let alive = live.contains(pid);
        if !alive {
            orphaned = orphaned.saturating_add(row.service_time);
        }
        alive
    });
    window.unattributed_service_time = window.unattributed_service_time.saturating_add(orphaned);
}

/// The broker-hosted per-process disk trace, with the same idle watchdog as
/// the network one.
struct DiskTrace {
    usage: super::disk_etw::DiskUsage,
    last_request: std::time::Instant,
    /// A drained window the previous response could not carry because it did
    /// not fit the caps; measured together with the next one. One slot, not a
    /// list: the windows a single machine produces over consecutive polls cover
    /// the same processes, so holding more than one would add span, not rows.
    pending: Option<super::disk_etw::DiskWindow>,
}

fn disk_trace() -> &'static std::sync::Mutex<Option<DiskTrace>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<DiskTrace>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Answer one `ProcessDiskCounters` request, starting the trace if needed.
///
/// The answer DRAINS the window, so the numbers describe exactly the interval
/// since the previous request. As with the network counters, the service — not
/// the caller — decides which processes are live.
fn process_disk_sample() -> ProcessDiskSample {
    let mut slot = match disk_trace().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.as_ref().is_some_and(|trace| !trace.usage.is_running()) {
        // Same as the network trace; the fresh session's first window is too
        // short to divide by, so this tick stays unknown either way.
        tracing::warn!("broker disk trace ended unexpectedly; restarting it");
        let now = std::time::Instant::now();
        if let Some(usage) = super::disk_etw::DiskUsage::start(super::etw::TraceRole::Service)
            && let Some(trace) = slot.as_mut()
        {
            trace.usage = usage;
            // The replacement is timed from this request. `last_request` still
            // names the last poll before the old session died, so an idle
            // watchdog whose wake is due would measure that age against the
            // session that just started and retire it on the way past.
            trace.last_request = now;
        }
        return ProcessDiskSample::default();
    }
    if slot.is_none() {
        match super::disk_etw::DiskUsage::start(super::etw::TraceRole::Service) {
            Some(usage) => {
                *slot = Some(DiskTrace {
                    usage,
                    last_request: std::time::Instant::now(),
                    pending: None,
                });
                if !spawn_disk_trace_watchdog() {
                    // Same rule as the network trace: the watchdog is the only
                    // thing that ever stops this session, so a session without
                    // one would trace disk I/O for the life of the service.
                    tracing::error!(
                        "could not start the disk-trace watchdog; the session was not kept"
                    );
                    *slot = None;
                    return ProcessDiskSample::default();
                }
                // The first answer would cover the microseconds since the
                // session started, which is not a window anyone can divide by.
                return ProcessDiskSample::default();
            }
            None => {
                // LocalSystem should always be able to start a session; if it
                // cannot, report unknown rather than a fabricated zero.
                tracing::warn!("broker could not start the per-process disk trace");
                return ProcessDiskSample::default();
            }
        }
    }
    let Some(trace) = slot.as_mut() else {
        return ProcessDiskSample::default();
    };
    trace.last_request = std::time::Instant::now();
    let live = live_pids();
    let mut window = trace.usage.take_window(&live);
    let mut resumed_hold = false;
    if let Some(held) = trace.pending.take() {
        // A window an earlier response could not carry; see below. It is folded
        // in FRONT of this one, so the merged window reads in the order it
        // covered the machine, and its rows are checked against the live set
        // again — the drain that pruned them was an earlier request's.
        let mut merged = held;
        merge_disk_windows(&mut merged, window);
        prune_held_disk_window(&mut merged, &live);
        window = merged;
        resumed_hold = true;
    }
    // The caps bound the RESPONSE, not the measurement, and the drained window
    // is checked before anything is taken out of it: a window that cannot be
    // encoded within them is held for the next request, so its records are
    // measured one request later. This tick answers unknown, which is what a
    // source that cannot be delivered has to say — publishing the rows that
    // happen to fit would charge every other process a fabricated zero for the
    // same window.
    if !disk_window_fits(&window) {
        if resumed_hold {
            // The hold did not make it publishable, and waiting cannot: a
            // window over the caps is over them because of the processes that
            // are still running, and the hold has already had its chance to
            // shrink it (exited rows are pruned above). Holding it again would
            // pin this column unknown for as long as the page polls, so it is
            // dropped and the source reports unavailable until a window fits.
            if broker_warning_allowed() {
                tracing::error!(
                    entries = window.procs.len(),
                    "disk window cannot fit one response even when held; it was dropped"
                );
            }
            return ProcessDiskSample::default();
        }
        if broker_warning_allowed() {
            tracing::warn!(
                entries = window.procs.len(),
                "disk response exceeds the frame budget; holding the window"
            );
        }
        trace.pending = Some(window);
        return ProcessDiskSample::default();
    }
    let total_service_time = window.total_service_time();
    let to_wire = |times: &[super::disk_etw::DiskTime]| -> Vec<ProcessDiskTime> {
        times
            .iter()
            .map(|time| ProcessDiskTime {
                disk: time.disk,
                service_time: time.service_time,
            })
            .collect()
    };
    let disk_totals = to_wire(&window.disk_totals);
    let entries: Vec<ProcessDiskEntry> = window
        .procs
        .into_iter()
        .filter(|(_, disk)| disk.ops != 0)
        .map(|(pid, disk)| ProcessDiskEntry {
            pid,
            service_time: disk.service_time,
            disks: to_wire(&disk.per_disk),
            read_bytes: disk.read_bytes,
            write_bytes: disk.write_bytes,
            ops: disk.ops,
        })
        .collect();
    ProcessDiskSample {
        active: true,
        window_ms: window.since_ms,
        total_service_time,
        events_seen: window.events_seen,
        events_decoded: window.events_decoded,
        disk_totals,
        entries,
    }
}

/// Stop a broker-hosted trace once the GUI stops asking for it.
///
/// The thread lives exactly as long as the trace does: it is spawned with the
/// session, sleeps straight to the earliest moment the session COULD be idle,
/// and returns once it has torn it down. Both earlier watchdogs instead polled
/// every five seconds for the lifetime of the service — so a machine whose
/// user had opened the Processes page once at login kept waking a LocalSystem
/// process twelve times a minute for the rest of the day, long after the last
/// trace was gone. Nothing observes the timer, so there is nothing to poll
/// for; the deadline is known exactly.
///
/// Both transitions of the slot happen under its lock — the caller starts a
/// trace and spawns this while holding it, and this clears it while holding it
/// — so there is exactly one watchdog per live session and never a second one
/// racing it.
///
/// Returns whether the thread was started. A caller that keeps a session has to
/// know: nothing else stops it, so a session whose watchdog could not be spawned
/// would trace every read and write on the machine for the life of the process.
fn spawn_trace_watchdog<T: Send + 'static>(
    thread_name: &'static str,
    slot: &'static std::sync::Mutex<Option<T>>,
    last_request: fn(&T) -> std::time::Instant,
    stopped_message: &'static str,
) -> bool {
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            loop {
                let wait = {
                    let mut guard = match slot.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let Some(trace) = guard.as_ref() else {
                        // Somebody else already stopped it; nothing to watch.
                        return;
                    };
                    let idle = last_request(trace).elapsed();
                    if idle >= NET_TRACE_IDLE {
                        // Dropping stops the session and joins its consumer.
                        *guard = None;
                        tracing::info!("{stopped_message}");
                        return;
                    }
                    NET_TRACE_IDLE - idle
                };
                std::thread::sleep(wait);
            }
        })
        .is_ok()
}

/// Stop the disk trace once the GUI stops polling, exactly like the network
/// one. Its own thread because the two traces idle out independently.
///
/// Returns whether the thread was started: this watchdog is the only thing
/// that ever stops the session, so the caller must not keep one without it.
fn spawn_disk_trace_watchdog() -> bool {
    spawn_trace_watchdog(
        "tm-disk-idle",
        disk_trace(),
        |trace| trace.last_request,
        "per-process disk trace stopped (idle)",
    )
}

/// Stop the trace once the GUI stops polling. Without this the session would
/// outlive the last interested window and trace forever, which is why the
/// caller drops the session when the thread could not be started.
fn spawn_net_trace_watchdog() -> bool {
    spawn_trace_watchdog(
        "tm-net-idle",
        net_trace(),
        |trace| trace.last_request,
        "per-process network trace stopped (idle)",
    )
}

/// The per-file `FileIo` trace, hosted either by the broker or by the GUI.
///
/// It runs for as long as its HOST does — started with the service, or with
/// an elevated GUI that has no service — and is not idled out with the page:
/// per-file counts that only accumulate while a dialog is open answer "what did
/// this process do while I watched", which is not the question. Its measured
/// cost is small (see `file_etw`'s lifetime docs). A first request still
/// starts it when it is not running yet. The GUI only hosts one when its own
/// token is elevated and the broker is not there, which is why the two hosts
/// keep separate sessions under separate names: an elevated GUI must never
/// join, stop, or double-count the service's.
struct FileTrace {
    usage: super::file_etw::FileEtw,
}

/// How often a running per-file trace is maintained: restarted when it was
/// stopped underneath us, and pruned of processes whose exit record ETW
/// dropped. Once a minute keeps a LocalSystem service from waking often for
/// housekeeping that is only a backstop — exits arrive as records, and a
/// request restarts a dead session at once.
const FILE_TRACE_MAINTENANCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Which process the trace runs in, and therefore which session it owns.
#[derive(Clone, Copy)]
enum FileTraceHost {
    /// The protected LocalSystem service, the normal host.
    Service,
    /// The GUI's own token; only possible when it is elevated.
    App,
}

impl FileTraceHost {
    /// The one slot this host's session lives in.
    ///
    /// Two statics rather than a map: each host's session is a single
    /// process-wide object with its own watchdog, and a lookup cannot fail.
    fn slot(self) -> &'static std::sync::Mutex<Option<FileTrace>> {
        static SERVICE: std::sync::OnceLock<std::sync::Mutex<Option<FileTrace>>> =
            std::sync::OnceLock::new();
        static APP: std::sync::OnceLock<std::sync::Mutex<Option<FileTrace>>> =
            std::sync::OnceLock::new();
        match self {
            Self::Service => SERVICE.get_or_init(|| std::sync::Mutex::new(None)),
            Self::App => APP.get_or_init(|| std::sync::Mutex::new(None)),
        }
    }

    /// The ETW session name follows from this: the two hosts must never share
    /// or reclaim each other's session (see [`super::etw::TraceRole`]).
    fn role(self) -> super::etw::TraceRole {
        match self {
            Self::Service => super::etw::TraceRole::Service,
            Self::App => super::etw::TraceRole::App,
        }
    }

    fn watchdog_thread(self) -> &'static str {
        match self {
            Self::Service => "tm-file-maint",
            Self::App => "tm-file-app-maint",
        }
    }
}

/// Maintain a host's trace for as long as the slot holds it; see
/// [`FILE_TRACE_MAINTENANCE`].
///
/// Returns whether the thread was started: a session is only kept with its
/// maintenance ([`keep_watched`]), so one stopped underneath us is restarted
/// even when no page asks.
fn spawn_file_trace_maintenance(host: FileTraceHost) -> bool {
    std::thread::Builder::new()
        .name(host.watchdog_thread().into())
        .spawn(move || {
            loop {
                std::thread::sleep(FILE_TRACE_MAINTENANCE);
                let mut slot = match host.slot().lock() {
                    Ok(slot) => slot,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let Some(trace) = slot.as_mut() else {
                    return;
                };
                if trace.usage.is_running() {
                    trace.usage.maintain(&live_pids());
                    continue;
                }
                match start_file_trace(host.role()) {
                    Ok(usage) => {
                        tracing::warn!(
                            host = host.watchdog_thread(),
                            "per-file I/O trace was stopped underneath us; restarted"
                        );
                        trace.usage = usage;
                    }
                    Err(refusal) => tracing::debug!(
                        host = host.watchdog_thread(),
                        ?refusal,
                        "per-file I/O trace restart refused; retrying at the next maintenance"
                    ),
                }
            }
        })
        .is_ok()
}

/// Start `host`'s per-file trace if it is not running yet, and keep it.
///
/// What the service does as soon as it is ready, and what an elevated GUI with
/// no service does at start-up, so per-file counts cover the host's whole
/// lifetime instead of the time a dialog was open. Returns whether a session is
/// now held. An app-side refusal is throttled like a request's.
fn ensure_file_trace(host: FileTraceHost, start: &FileTraceStarter<'_>) -> bool {
    if matches!(host, FileTraceHost::App)
        && !app_trace_start_due(app_trace_last_failure(), std::time::Instant::now())
    {
        return false;
    }
    let mut slot = match host.slot().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.is_some() {
        return true;
    }
    match start(host.role()) {
        Ok(usage) => {
            let kept = keep_watched(&mut slot, FileTrace { usage }, || {
                spawn_file_trace_maintenance(host)
            });
            if !kept {
                tracing::error!(
                    host = host.watchdog_thread(),
                    "could not start the per-file I/O maintenance thread; the session was not kept"
                );
            }
            kept
        }
        Err(refusal) => {
            if matches!(host, FileTraceHost::App) {
                remember_app_trace_failure(Some(std::time::Instant::now()));
            }
            tracing::debug!(
                host = host.watchdog_thread(),
                ?refusal,
                "per-file I/O trace could not be started"
            );
            false
        }
    }
}

/// Start the GUI's own per-file trace for the rest of its run, when it can
/// host one (elevated) and the service is not there to host it instead.
///
/// Called once, off the UI thread, after start-up: probing the broker is a
/// pipe round trip, and starting a session is a few kernel calls plus a
/// process-table snapshot.
pub(super) fn ensure_app_file_trace_unless_served() {
    if !super::process_ops::is_elevated() {
        return;
    }
    if BrokerClient.call(BrokerRequest::Ping).is_ok() {
        // The service runs its own always-on session and answers the
        // requests; a second one here would double the machine-wide cost.
        return;
    }
    let _ = ensure_file_trace(FileTraceHost::App, &start_file_trace);
}

/// Put `session` in `slot` and start the watchdog that will stop it, or leave
/// the slot empty again and report that it was not kept.
///
/// The rule exists because the spawn can fail and its result is the difference
/// between a bounded session and one that traces every read and write on the
/// machine until the process exits. It is also what makes the replacement path
/// sound: a slot only ever holds a session whose watchdog is running, so
/// swapping the session in place cannot leave it unobserved.
fn keep_watched<T>(
    slot: &mut Option<T>,
    session: T,
    spawn_watchdog: impl FnOnce() -> bool,
) -> bool {
    *slot = Some(session);
    if spawn_watchdog() {
        return true;
    }
    *slot = None;
    false
}

/// Start a session for `role`, the one call both hosts make.
///
/// A seam rather than a direct call: what a FAILED start does to the slot, the
/// retry window and the snapshot is the part that needs pinning, and a window
/// set by hand cannot show it. The tests drive the production entry with this
/// replaced by a stub, so everything but the kernel call is the real path.
fn start_file_trace(
    role: super::etw::TraceRole,
) -> std::result::Result<super::file_etw::FileEtw, super::file_etw::FileTraceRefusal> {
    super::file_etw::FileEtw::try_start(role)
}

/// How a request starts a session; see [`start_file_trace`].
type FileTraceStarter<'a> = dyn Fn(
        super::etw::TraceRole,
    ) -> std::result::Result<super::file_etw::FileEtw, super::file_etw::FileTraceRefusal>
    + 'a;

/// What one request got from a host's session.
struct FileTraceAnswer {
    /// The sample, when the host has a session at all. A session that stopped
    /// is frozen and still answers: its state is
    /// [`crate::actions::FileIoAccountingState::Unavailable`], so the dialog
    /// reports a missing source instead of a stale number.
    sample: Option<super::file_etw::FileIoSample>,
    /// Whether this request tried to start a session and was refused — for a
    /// dead session that needed replacing as much as for a missing one.
    start_refused: bool,
    /// WHY the last start was refused, when one was attempted and failed.
    ///
    /// Carried to the snapshot so the dialog can say "another application owns
    /// the NT Kernel Logger" instead of the generic "no trace is running": the
    /// two need different words, and only one of them is about rights.
    refusal: Option<super::file_etw::FileTraceRefusal>,
}

impl FileTraceAnswer {
    /// The start failure to remember after this request, if there was one.
    ///
    /// A refusal is recorded even when the answer still carries a frozen
    /// sample: that sample says the source is gone, so the start WAS refused,
    /// and forgetting it makes the throttle clear itself on every poll — a
    /// session that keeps dying is then re-created at the poll rate.
    fn failure_at(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        self.start_refused.then_some(now)
    }
}

/// Cumulative per-file bytes for `pid` from `host`'s session, starting it on
/// demand.
///
/// No sample means no session could be started, which the caller must keep
/// rendering as [`crate::actions::FileIoAccountingState::Unavailable`]: a
/// per-file session needs elevation, and without one there is no source at
/// all — that state has to keep meaning exactly that.
///
/// `start_epoch_s` is the creation time the caller verified, passed through so
/// a reused pid cannot inherit the previous process's paths.
fn file_trace_sample(
    host: FileTraceHost,
    pid: u32,
    start_epoch_s: i64,
    start: &FileTraceStarter<'_>,
) -> FileTraceAnswer {
    let mut slot = match host.slot().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut refused = false;
    let mut refusal = None;
    if slot.as_ref().is_some_and(|trace| !trace.usage.is_running()) {
        // Stopped underneath us: its counters are frozen. Replace the session
        // in place so the slot's one watchdog keeps covering it, then report
        // from the replacement below — its state says "nothing delivered yet",
        // and a replacement that cannot be started keeps the dead session's,
        // which says the source is gone. This is safe because a slot only ever
        // holds a session whose watchdog is running; see [`keep_watched`].
        match host {
            FileTraceHost::Service => {
                tracing::warn!("broker file trace ended unexpectedly; restarting it");
            }
            FileTraceHost::App => {
                tracing::warn!("app file trace ended unexpectedly; restarting it");
            }
        }
        match start(host.role()) {
            Ok(usage) => {
                if let Some(trace) = slot.as_mut() {
                    trace.usage = usage;
                }
            }
            Err(reason) => {
                refused = true;
                refusal = Some(reason);
            }
        }
    }
    if slot.is_none() {
        match start(host.role()) {
            Ok(usage) => {
                let kept = keep_watched(&mut slot, FileTrace { usage }, || {
                    spawn_file_trace_maintenance(host)
                });
                if !kept {
                    // Nothing would stop this session, and a per-file
                    // session left running traces the machine for the life of
                    // the process. Report no source instead of a leak.
                    tracing::error!(
                        host = host.watchdog_thread(),
                        "could not start the per-file I/O maintenance thread; the session was not kept"
                    );
                    return FileTraceAnswer {
                        sample: None,
                        start_refused: true,
                        // The session DID start; it just was not kept. That is
                        // not something the snapshot should attribute to an
                        // owner.
                        refusal: None,
                    };
                }
            }
            Err(reason) => {
                // LocalSystem should always be able to start a session; if it
                // cannot, report unavailable rather than a fabricated zero.
                // An unelevated GUI cannot, and that is its ordinary state.
                match host {
                    FileTraceHost::Service => {
                        tracing::warn!("broker could not start the per-file I/O trace");
                    }
                    FileTraceHost::App => {
                        tracing::debug!(
                            "app cannot start the per-file I/O trace; per-file bytes need elevation"
                        );
                    }
                }
                return FileTraceAnswer {
                    sample: None,
                    start_refused: true,
                    refusal: Some(reason),
                };
            }
        }
    }
    let Some(trace) = slot.as_mut() else {
        return FileTraceAnswer {
            sample: None,
            start_refused: refused,
            refusal,
        };
    };
    let live = live_pids();
    FileTraceAnswer {
        sample: Some(trace.usage.sample(pid, start_epoch_s, &live)),
        start_refused: refused,
        refusal,
    }
}

/// The broker's answer for one request: LocalSystem's own session.
fn file_io_accounting(pid: u32, start_epoch_s: i64) -> FileTraceAnswer {
    file_trace_sample(
        FileTraceHost::Service,
        pid,
        start_epoch_s,
        &start_file_trace,
    )
}

/// How long an app-side trace start is given up for after it failed.
///
/// Starting a session costs kernel calls and an UNELEVATED GUI fails every
/// attempt, while the I/O page polls about once a second; retrying at that rate
/// would spin on a refusal for the whole time the dialog is open. Only the
/// failure is throttled — a running trace is stopped by its idle watchdog, and
/// elevation cannot be acquired mid-process, so the retry exists for a token
/// that became able to trace (an already elevated host whose name was taken by
/// a stale session that has since exited).
const APP_FILE_TRACE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// When the last app-side start failed. `None` means "no failure on record".
fn app_file_trace_failed_at() -> &'static std::sync::Mutex<Option<std::time::Instant>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<std::time::Instant>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Read the last app-side start failure.
fn app_trace_last_failure() -> Option<std::time::Instant> {
    match app_file_trace_failed_at().lock() {
        Ok(failure) => *failure,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Record — or, with `None`, clear — the last app-side start failure.
///
/// A caller records a refusal even when a dead session still answers it; see
/// [`FileTraceAnswer::failure_at`].
fn remember_app_trace_failure(at: Option<std::time::Instant>) {
    let mut failure = match app_file_trace_failed_at().lock() {
        Ok(failure) => failure,
        Err(poisoned) => poisoned.into_inner(),
    };
    *failure = at;
}

/// Whether a start may be attempted at `now` after a failure at `failed_at`.
///
/// Pure so the throttle is pinnable without a trace: a failed start must not be
/// retried on every poll, and a fresh failure must be.
fn app_trace_start_due(failed_at: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    failed_at.is_none_or(|at| now.saturating_duration_since(at) >= APP_FILE_TRACE_RETRY)
}

/// Cumulative per-file bytes from the GUI's OWN session: the answer whenever a
/// request does not get a served one, which is a direct run without the service
/// as well as a broker rejection.
///
/// This is what closes the "columns look broken" gap in a direct run: the trace
/// is hosted here and its sample is attached exactly as the broker attaches
/// its own, so a row means the same thing whichever session measured it. An
/// unelevated GUI cannot start the session at all, which leaves the snapshot at
/// [`crate::actions::FileIoAccountingState::Unavailable`] and every row
/// unmeasured — never a zero.
///
/// The start is throttled by [`APP_FILE_TRACE_RETRY`]; a running session is
/// sampled on every request and idled out by its watchdog.
///
/// What sets the window is the refusal, not the absence of a sample: a session
/// that already stopped still answers, so keying on the sample would clear the
/// window on every poll and re-create a dying session at the poll rate.
fn app_file_io_accounting(
    pid: u32,
    start_epoch_s: i64,
    start: &FileTraceStarter<'_>,
) -> FileTraceAnswer {
    let now = std::time::Instant::now();
    if !app_trace_start_due(app_trace_last_failure(), now) {
        // Inside the retry window: nothing is attempted, so this request has
        // no refusal to report and the snapshot keeps its `Unavailable`.
        return FileTraceAnswer {
            sample: None,
            start_refused: false,
            refusal: None,
        };
    }
    let answer = file_trace_sample(FileTraceHost::App, pid, start_epoch_s, start);
    remember_app_trace_failure(answer.failure_at(now));
    answer
}

/// Attach the GUI's own per-file bytes to `snapshot`, when it can host them.
///
/// The snapshot is left exactly as the handle walk produced it when no session
/// exists: every per-file number stays `None` (rendered as unavailable), and
/// the dialog explains why — including the one refusal it can name, a kernel
/// logger another application owns. The service must never call this — it has
/// its own session under [`FileTraceHost::Service`].
pub(super) fn attach_app_file_io(
    snapshot: &mut crate::actions::ProcessFileIoSnapshot,
    pid: u32,
    start_epoch_s: i64,
) {
    attach_app_file_io_with(snapshot, pid, start_epoch_s, &start_file_trace);
}

/// [`attach_app_file_io`] with the start injectable; see [`start_file_trace`].
fn attach_app_file_io_with(
    snapshot: &mut crate::actions::ProcessFileIoSnapshot,
    pid: u32,
    start_epoch_s: i64,
    start: &FileTraceStarter<'_>,
) {
    let answer = app_file_io_accounting(pid, start_epoch_s, start);
    match answer.sample {
        Some(sample) => super::file_etw::attach_file_io(snapshot, &sample),
        None => super::file_etw::attach_refusal(snapshot, answer.refusal),
    }
}

/// Charge rows that were dropped from the response to the omitted remainder.
///
/// A row that does not fit is still part of what the trace measured for the
/// process, so its bytes move into `omitted_*` instead of disappearing.
fn omit_file_bytes(
    accounting: &mut crate::actions::FileIoAccounting,
    files: impl IntoIterator<Item = crate::actions::ProcessFileIo>,
) {
    for file in files {
        if let Some(read) = file.read_bytes {
            accounting.omitted_read_bytes = accounting.omitted_read_bytes.saturating_add(read);
        }
        if let Some(write) = file.write_bytes {
            accounting.omitted_write_bytes = accounting.omitted_write_bytes.saturating_add(write);
        }
    }
}

/// Encoded length of a file-I/O snapshot, as it will be framed.
fn encoded_file_io_len(snapshot: &crate::actions::ProcessFileIoSnapshot) -> Result<usize> {
    serde_json::to_vec(snapshot)
        .map(|encoded| encoded.len())
        .map_err(|error| TmError::platform("broker process file io", error.to_string()))
}

/// Drop rows from the end of `snapshot.files` until the encoded snapshot fits
/// `budget` bytes, charging them to the omitted remainder.
///
/// Row COUNT is not a size bound (see the caller). Every row adds at least its
/// own bytes, so the encoded size grows with the row count and the longest
/// fitting prefix can be found by bisection — ten encodings for a full
/// thousand-row list, and none in the common case where the list already fits.
/// A snapshot that cannot fit even with no rows is a real error and still
/// fails.
fn trim_file_io_to_budget(
    snapshot: &mut crate::actions::ProcessFileIoSnapshot,
    budget: usize,
) -> Result<()> {
    if encoded_file_io_len(snapshot)? <= budget {
        return Ok(());
    }
    let rows = std::mem::take(&mut snapshot.files);
    let mut fits = 0usize;
    let mut over = rows.len();
    while fits < over {
        // Upper middle, so `fits` advances and the loop terminates on the
        // largest prefix that fits.
        let probe = fits + (over - fits).div_ceil(2);
        snapshot.files = rows[..probe].to_vec();
        if encoded_file_io_len(snapshot)? <= budget {
            fits = probe;
        } else {
            over = probe - 1;
        }
    }
    let mut kept = rows;
    let dropped = kept.split_off(fits);
    snapshot.files = kept;
    if !dropped.is_empty() {
        snapshot.accounting.handles_complete = false;
        omit_file_bytes(&mut snapshot.accounting, dropped);
    }
    // The bisection measured the snapshot while `omitted_*` still held their
    // pre-drop values, and charging the dropped rows grew them: the remainder's
    // own digits can push the prefix the search settled on back over the
    // budget. Re-check the authoritative length here and give back one row at a
    // time (each one both shrinks the list and grows the remainder), so the
    // answer is the rows that really fit rather than no list at all.
    while encoded_file_io_len(snapshot)? > budget {
        let Some(row) = snapshot.files.pop() else {
            return Err(TmError::platform(
                "broker process file io",
                "file I/O snapshot exceeds the bounded broker response limit",
            ));
        };
        snapshot.accounting.handles_complete = false;
        omit_file_bytes(&mut snapshot.accounting, std::iter::once(row));
    }
    Ok(())
}

/// Test hook for [`live_pids`], so an integration test can check the same
/// enumeration the broker prunes with.
#[doc(hidden)]
pub fn live_pids_for_test() -> std::collections::HashSet<u32> {
    live_pids()
}

/// PIDs that currently exist, from a ToolHelp snapshot (~1 ms).
fn live_pids() -> std::collections::HashSet<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let mut pids = std::collections::HashSet::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                pids.insert(entry.th32ProcessID);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    pids
}

fn dispatch(
    actions: &super::WinActions,
    request: BrokerRequest,
    requesting_gui_pid: u32,
) -> Result<BrokerValue> {
    match request {
        BrokerRequest::Ping => Ok(BrokerValue::Pong {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }),
        // The one read-only telemetry request. It takes no target and can
        // change nothing, so it deliberately skips `checked_target`.
        BrokerRequest::ProcessNetworkCounters => {
            Ok(BrokerValue::ProcessNetwork(process_network_sample()))
        }
        // Likewise read-only, target-less and unable to change anything.
        BrokerRequest::ProcessDiskCounters => Ok(BrokerValue::ProcessDisk(process_disk_sample())),
        BrokerRequest::ProcessSecurityInfo {
            pid,
            expected_start_epoch_s,
        } => {
            let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
                return Err(TmError::platform(
                    "broker process security",
                    "a valid sampled process creation time is required",
                ));
            };
            Ok(BrokerValue::ProcessSecurity(
                super::process_ops::process_security_info(pid, Some(start_epoch_s))?,
            ))
        }
        BrokerRequest::ProcessIdentity {
            pid,
            expected_start_epoch_s,
        } => {
            let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
                return Err(TmError::platform(
                    "broker process identity",
                    "a valid sampled process creation time is required",
                ));
            };
            Ok(BrokerValue::ProcessIdentity(
                super::process_ops::process_identity_checked(pid, Some(start_epoch_s))?,
            ))
        }
        BrokerRequest::ProcessModules {
            pid,
            expected_start_epoch_s,
        } => {
            let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
                return Err(TmError::platform(
                    "broker process modules",
                    "a valid sampled process creation time is required",
                ));
            };
            let modules = super::list_process_modules_with_fallback(
                super::etw::TraceRole::Service,
                pid,
                Some(start_epoch_s),
            )?;
            if modules.len() > MAX_MODULES_PER_RESPONSE {
                return Err(TmError::platform(
                    "broker process modules",
                    "module inventory exceeds the bounded broker entry limit",
                ));
            }
            let encoded_len = serde_json::to_vec(&modules)
                .map_err(|error| TmError::platform("broker process modules", error.to_string()))?
                .len();
            if encoded_len > MAX_RESPONSE_BYTES.saturating_sub(1024) {
                return Err(TmError::platform(
                    "broker process modules",
                    "module inventory exceeds the bounded broker response limit",
                ));
            }
            Ok(BrokerValue::ProcessModules(modules))
        }
        BrokerRequest::ProcessModuleCodeSamples {
            pid,
            expected_start_epoch_s,
        } => {
            let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
                return Err(TmError::platform(
                    "broker module code samples",
                    "a valid sampled process creation time is required",
                ));
            };
            // This read validates the exact process identity and supplies the
            // current module ranges used at drain time. In particular a
            // recycled PID is rejected before stale trace data can be exposed.
            let modules = super::list_process_modules_with_fallback(
                super::etw::TraceRole::Service,
                pid,
                Some(start_epoch_s),
            )?;
            if modules.len() > MAX_MODULES_PER_RESPONSE {
                return Err(TmError::platform(
                    "broker module code samples",
                    "module inventory exceeds the bounded broker entry limit",
                ));
            }
            let window = super::profile_etw::sample_process_module_code(
                super::etw::TraceRole::Service,
                pid,
                start_epoch_s,
                &modules,
            )?;
            if window
                .as_ref()
                .is_some_and(|window| window.modules.len() > MAX_MODULES_PER_RESPONSE)
            {
                return Err(TmError::platform(
                    "broker module code samples",
                    "module sample entries exceed the bounded broker entry limit",
                ));
            }
            Ok(BrokerValue::ProcessModuleCodeSamples(window))
        }
        BrokerRequest::ProcessThreadDetails {
            pid,
            expected_start_epoch_s,
        } => {
            let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
                return Err(TmError::platform(
                    "broker thread details",
                    "a valid sampled process creation time is required",
                ));
            };
            let threads = super::process_threads::list_process_threads(pid, Some(start_epoch_s))?;
            let details: Vec<ProcessThreadDetail> = threads
                .threads
                .into_iter()
                .filter(|thread| thread.start_address.is_some() || thread.cycles.is_some())
                .map(|thread| ProcessThreadDetail {
                    tid: thread.tid,
                    created_epoch_s: thread.created_epoch_s,
                    start_address: thread.start_address,
                    cycles: thread.cycles,
                })
                .collect();
            if details.len() > MAX_THREAD_DETAILS_PER_RESPONSE {
                return Err(TmError::platform(
                    "broker thread details",
                    "thread evidence exceeds the bounded broker entry limit",
                ));
            }
            let encoded_len = serde_json::to_vec(&details)
                .map_err(|error| TmError::platform("broker thread details", error.to_string()))?
                .len();
            if encoded_len > MAX_RESPONSE_BYTES.saturating_sub(1024) {
                return Err(TmError::platform(
                    "broker thread details",
                    "thread evidence exceeds the bounded broker response limit",
                ));
            }
            Ok(BrokerValue::ProcessThreadDetails(details))
        }
        BrokerRequest::ProcessFileIo {
            pid,
            expected_start_epoch_s,
        } => {
            let Some(start_epoch_s) = expected_start_epoch_s.filter(|value| *value > 0) else {
                return Err(TmError::platform(
                    "broker process file io",
                    "a valid sampled process creation time is required",
                ));
            };
            let mut snapshot =
                super::process_file_io::list_process_file_io(pid, Some(start_epoch_s))?;
            // The handle walk names the files; only the kernel file trace knows
            // how many bytes went through them. Attached before the response is
            // trimmed, so rows dropped by the frame budget add to the omitted
            // remainder instead of vanishing from the totals. With no session
            // at all, the refusal (if the start got an actionable one) becomes
            // the snapshot's state — never a number.
            let answer = file_io_accounting(pid, start_epoch_s);
            match answer.sample {
                Some(sample) => super::file_etw::attach_file_io(&mut snapshot, &sample),
                None => super::file_etw::attach_refusal(&mut snapshot, answer.refusal),
            }
            if snapshot.files.len() > MAX_FILES_PER_RESPONSE {
                let omitted = snapshot.files.split_off(MAX_FILES_PER_RESPONSE);
                snapshot.accounting.handles_complete = false;
                omit_file_bytes(&mut snapshot.accounting, omitted);
            }
            // The entry cap bounds how many rows there are; it does not bound
            // how big they are. A row is a path plus five optional numbers, so
            // 1024 rows of long paths overflow the response while 1024 short
            // ones leave room to spare — and erroring here made the whole list
            // disappear for exactly the snapshots that carry the most.
            trim_file_io_to_budget(&mut snapshot, MAX_RESPONSE_BYTES.saturating_sub(1024))?;
            Ok(BrokerValue::ProcessFileIo(snapshot))
        }
        BrokerRequest::KillProcess {
            pid,
            expected_start_epoch_s,
            tree,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            super::process_ops::kill_process_excluding(
                pid,
                expected_start_epoch_s,
                tree,
                Some(requesting_gui_pid),
            )?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::SuspendProcess {
            pid,
            expected_start_epoch_s,
            suspend,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            actions.suspend_process_checked(pid, expected_start_epoch_s, suspend)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::SetPriority {
            pid,
            expected_start_epoch_s,
            priority,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            actions.set_priority_checked(pid, expected_start_epoch_s, priority)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::GetAffinity {
            pid,
            expected_start_epoch_s,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            Ok(BrokerValue::AffinityMask(
                actions.get_affinity_mask_checked(pid, expected_start_epoch_s)?,
            ))
        }
        BrokerRequest::SetAffinity {
            pid,
            expected_start_epoch_s,
            mask,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            if mask == 0 {
                return Err(TmError::platform("broker affinity", "mask cannot be zero"));
            }
            let system_mask = actions.system_affinity_mask()?;
            if mask & !system_mask != 0 {
                return Err(TmError::platform(
                    "broker affinity",
                    "mask contains processors outside the active system group",
                ));
            }
            actions.set_affinity_mask_checked(pid, expected_start_epoch_s, mask)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::SetEfficiencyMode {
            pid,
            expected_start_epoch_s,
            enabled,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            actions.set_efficiency_mode_checked(pid, expected_start_epoch_s, enabled)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::SetUacVirtualization {
            pid,
            expected_start_epoch_s,
            enabled,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            actions.set_uac_virtualization_checked(pid, expected_start_epoch_s, enabled)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::UnloadModule {
            pid,
            expected_start_epoch_s,
            base_address,
            expected_path,
        } => {
            checked_target(pid, expected_start_epoch_s, requesting_gui_pid)?;
            if base_address == 0 || expected_path.is_empty() || expected_path.len() > 32768 {
                return Err(TmError::platform(
                    "broker module",
                    "invalid module identity",
                ));
            }
            let outcome = actions.unload_process_module(
                pid,
                expected_start_epoch_s,
                base_address,
                &expected_path,
            )?;
            Ok(BrokerValue::ModuleUnload {
                still_mapped: outcome.still_mapped.unwrap_or(true),
                verified: outcome.still_mapped.is_some(),
                // v2 compatibility field: one confirmation is exactly one
                // successful remote FreeLibrary call.
                released: 1,
            })
        }
        BrokerRequest::ControlService { name, action } => {
            if !valid_service_name(&name) || name.eq_ignore_ascii_case(SERVICE_NAME) {
                return Err(TmError::platform("broker service", "invalid service name"));
            }
            actions.control_service(&name, action)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::ControlUserSession { session_id, action } => {
            if session_id == 0 {
                return Err(TmError::platform("broker session", "invalid session id"));
            }
            actions.control_user_session(session_id, action)?;
            Ok(BrokerValue::Unit)
        }
        BrokerRequest::TaskManagerReplacementState => {
            let gui = expected_installed_gui_path()?;
            Ok(BrokerValue::TaskManagerReplacementState(
                super::task_manager_replacement_state_for(&gui),
            ))
        }
        BrokerRequest::SetTaskManagerReplacement { enabled } => {
            let gui = expected_installed_gui_path()?;
            super::set_task_manager_replacement_direct_for(enabled, &gui)?;
            Ok(BrokerValue::Unit)
        }
    }
}

pub fn run_broker(
    stop_rx: std::sync::mpsc::Receiver<()>,
    on_ready: impl FnOnce() -> Result<()>,
) -> Result<()> {
    struct TraceCleanup;

    impl Drop for TraceCleanup {
        fn drop(&mut self) {
            super::etw::stop_live_sessions();
        }
    }

    let _trace_cleanup = TraceCleanup;
    let manifest = Arc::new(load_manifest()?);
    let sddl = pipe_sddl(&manifest.authorized_user_sid)?;
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped_for_wake = stopped.clone();
    let listener_ready = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let listener_ready_for_wake = listener_ready.clone();
    std::thread::Builder::new()
        .name("tm-broker-stop".into())
        .spawn(move || {
            let _ = stop_rx.recv();
            stopped_for_wake.store(true, std::sync::atomic::Ordering::Release);
            // Wait eventfully until an accept instance exists, then wake its
            // blocking ConnectNamedPipe. This closes the stop-vs-create race:
            // a stop arriving immediately before a new listener is created
            // cannot miss that listener and strand SCM shutdown.
            let (state, changed) = &*listener_ready_for_wake;
            let mut ready = tm_core::sync::lock(state);
            while !*ready {
                ready = changed
                    .wait(ready)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            drop(ready);
            // Authentication is checked after connect; the loop exits before
            // dispatching this synthetic client.
            let _ = open_client_pipe();
        })
        .map_err(TmError::Io)?;

    let (work_tx, work_rx) = std::sync::mpsc::sync_channel::<ClientWork>(WORK_QUEUE_CAP);
    let work_rx = Arc::new(std::sync::Mutex::new(work_rx));
    for index in 0..WORKER_COUNT {
        let receiver = work_rx.clone();
        std::thread::Builder::new()
            .name(format!("tm-broker-{index}"))
            .spawn(move || {
                let actions = super::WinActions;
                loop {
                    let next = tm_core::sync::lock(&receiver).recv();
                    match next {
                        Ok(work) => handle_client(work, &actions),
                        Err(_) => break,
                    }
                }
            })
            .map_err(TmError::Io)?;
    }

    // Report SERVICE_RUNNING only after the manifest, ACL, workers, and first
    // listening pipe have all been established.
    let mut listener = Some(create_pipe_instance(PIPE_NAME, &sddl, true)?);
    on_ready()?;
    // The per-file trace runs for the service's whole life, so a process's
    // per-file counts do not depend on when a dialog was first opened. Off the
    // accept loop: starting it snapshots the process table.
    if let Err(error) = std::thread::Builder::new()
        .name("tm-file-start".into())
        .spawn(|| {
            let _ = ensure_file_trace(FileTraceHost::Service, &start_file_trace);
        })
    {
        tracing::warn!(%error, "could not start the per-file I/O trace thread");
    }
    let mut consecutive_failures = 0u32;
    while !stopped.load(std::sync::atomic::Ordering::Acquire) {
        let pipe = match listener.take() {
            Some(pipe) => pipe,
            None => match create_pipe_instance(PIPE_NAME, &sddl, false) {
                Ok(pipe) => pipe,
                // Every instance is held — by queued and working clients, or
                // by rejected clients that have not closed their end yet.
                // That is back-pressure, not a fault: counting it toward the
                // failure limit let ~20 lingering client handles exhaust the
                // limit within microseconds and shut the broker down (SCM
                // then restarted it 5-60 s later). Wait for an instance to be
                // released, staying responsive to stop.
                Err(error) if is_pipe_busy(&error) => {
                    if broker_warning_allowed() {
                        tracing::warn!("every broker pipe instance is in use; waiting for one");
                    }
                    std::thread::sleep(PIPE_REARM_BACKOFF);
                    continue;
                }
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                        return Err(error);
                    }
                    if broker_warning_allowed() {
                        tracing::warn!(%error, "broker could not arm a listening pipe instance");
                    }
                    continue;
                }
            },
        };
        {
            let (state, changed) = &*listener_ready;
            *tm_core::sync::lock(state) = true;
            changed.notify_one();
        }
        let accepted = connect_pipe(&pipe);
        *tm_core::sync::lock(&listener_ready.0) = false;
        if stopped.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        // Re-arm the listener BEFORE resolving this client's identity. A pipe
        // with no instance in listening state answers every opener with
        // ERROR_PIPE_BUSY, and resolving a client image is slow enough that
        // the old accept-then-re-arm order put a busy window in front of most
        // calls - which the GUI then reported as an unavailable service.
        listener = create_pipe_instance(PIPE_NAME, &sddl, false).ok();
        match accepted {
            AcceptOutcome::Connected => consecutive_failures = 0,
            AcceptOutcome::ClientVanished => {
                consecutive_failures = 0;
                continue;
            }
            AcceptOutcome::Failed(error) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                    return Err(error);
                }
                if broker_warning_allowed() {
                    tracing::warn!(%error, "broker could not accept a client");
                }
                continue;
            }
        }
        let pid = match client_pid(&pipe) {
            Ok(pid) => pid,
            Err(error) => {
                if broker_warning_allowed() {
                    tracing::warn!(%error, "broker rejected client without process identity");
                }
                reject_client(pipe, "client process identity could not be established");
                continue;
            }
        };
        let client_path = match process_image_path(pid) {
            Ok(path) => path,
            Err(error) => {
                if broker_warning_allowed() {
                    tracing::warn!(%error, pid, "broker rejected unresolved client");
                }
                reject_client(pipe, "client executable could not be resolved");
                continue;
            }
        };
        if !paths_match(&client_path, &manifest.gui_path) {
            if broker_warning_allowed() {
                tracing::warn!(pid, path = %client_path.display(), "broker rejected unexpected client image");
            }
            reject_client(pipe, "client is not the protected TaskMan GUI");
            continue;
        }
        match work_tx.try_send(ClientWork {
            pipe,
            client_pid: pid,
        }) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(work)) => {
                if broker_warning_allowed() {
                    tracing::warn!("broker work queue full; rejecting client");
                }
                reject_client(work.pipe, "core service work queue is full");
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                return Err(TmError::ChannelClosed);
            }
        }
    }
    // The stop thread blocks until a listener exists. Leaving the loop between
    // accepting a client and arming the next instance would otherwise strand
    // it on the condvar.
    {
        let (state, changed) = &*listener_ready;
        *tm_core::sync::lock(state) = true;
        changed.notify_one();
    }
    drop(work_tx);
    Ok(())
}

pub fn selfcheck() -> Result<String> {
    let request = BrokerRequest::SetAffinity {
        pid: 42,
        expected_start_epoch_s: Some(1234),
        mask: 3,
    };
    let payload = serde_json::to_vec(&request)?;
    let mut frame = Vec::new();
    write_frame(&mut frame, FRAME_REQUEST, &payload, MAX_REQUEST_BYTES)?;
    let decoded = read_frame(&mut frame.as_slice(), FRAME_REQUEST, MAX_REQUEST_BYTES)?;
    let roundtrip: BrokerRequest = serde_json::from_slice(&decoded)?;
    if !matches!(
        roundtrip,
        BrokerRequest::SetAffinity {
            pid: 42,
            mask: 3,
            ..
        }
    ) {
        return Err(TmError::platform(
            "broker selfcheck",
            "protocol round trip changed",
        ));
    }
    let invalid = pipe_sddl("not-a-sid");
    if invalid.is_ok() {
        return Err(TmError::platform(
            "broker selfcheck",
            "invalid SID was accepted",
        ));
    }
    Ok(format!(
        "{{\"ok\":true,\"protocol_version\":{PROTOCOL_VERSION},\"request_limit\":{MAX_REQUEST_BYTES},\"response_limit\":{MAX_RESPONSE_BYTES}}}"
    ))
}

fn service_state(client: &BrokerClient) -> CoreServiceState {
    let ping = client.call(BrokerRequest::Ping);
    if let Ok(BrokerValue::Pong { version }) = ping {
        return CoreServiceState::Running { version };
    }

    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(manager) => manager,
        Err(error) => return CoreServiceState::Degraded(error.to_string()),
    };
    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return CoreServiceState::NotInstalled;
        }
        Err(error) => return CoreServiceState::Degraded(error.to_string()),
    };
    match service.query_status() {
        Ok(status) => match status.current_state {
            ServiceState::Running => {
                // A session running outside the protected install location can
                // never satisfy the broker's image-path check, so its ping
                // fails even while the service is healthy. Report that
                // distinctly: "repair" cannot help, switching to the
                // installed copy can.
                let current_exe = std::env::current_exe().ok();
                let installed_gui = expected_installed_gui_path().ok();
                if foreign_client_session(current_exe.as_deref(), installed_gui.as_deref()) {
                    CoreServiceState::ForeignClient
                } else if matches!(
                    ping,
                    Err(BrokerCallError::Unavailable(_) | BrokerCallError::NoAnswer(_))
                ) {
                    // The pipe never answered — either nothing was there to
                    // answer, or its answer was lost. That is a different fault
                    // from the broker answering and refusing us, and naming it
                    // "authentication failed" sent users to a repair that
                    // cannot help. Both stay degraded: a reachable service
                    // whose control pipe is silent is a real problem.
                    CoreServiceState::Degraded(
                        "service is running but its control pipe did not answer".into(),
                    )
                } else {
                    CoreServiceState::Degraded(
                        "service is running but broker authentication failed".into(),
                    )
                }
            }
            ServiceState::StartPending | ServiceState::ContinuePending => {
                CoreServiceState::Starting
            }
            _ => classify_stopped_state(&status.exit_code),
        },
        Err(error) => CoreServiceState::Degraded(error.to_string()),
    }
}

/// Maps a stopped service's last reported exit code onto the state surface.
/// The broker reports install-integrity failures under their own code, and
/// the GUI must name that condition instead of showing a bare "stopped": no
/// restart fixes it, Repair does.
fn classify_stopped_state(
    exit_code: &windows_service::service::ServiceExitCode,
) -> CoreServiceState {
    use windows_service::service::ServiceExitCode;
    match exit_code {
        ServiceExitCode::ServiceSpecific(SERVICE_EXIT_INSTALL_INTEGRITY) => {
            CoreServiceState::InstallIntegrityFailed
        }
        _ => CoreServiceState::Stopped,
    }
}

/// True when this process's image is not the protected installed GUI, which
/// the broker's client authorization rejects regardless of service health.
/// Unresolvable paths must keep the honest degraded classification instead of
/// claiming the session is foreign.
fn foreign_client_session(current_exe: Option<&Path>, installed_gui: Option<&Path>) -> bool {
    match (current_exe, installed_gui) {
        (Some(current), Some(installed)) => !paths_match(current, installed),
        _ => false,
    }
}

fn launch_service_helper(installed: bool) -> Result<()> {
    let authorized_user_sid = current_user_sid()?;
    if super::is_elevated() {
        return if installed {
            install(&authorized_user_sid)
        } else {
            uninstall()
        };
    }
    let exe = std::env::current_exe()?;
    let operation = if installed { "install" } else { "uninstall" };
    super::process_ops::run_new_task_wait(
        &format!(
            "\"{}\" --core-service={operation} --core-service-user={authorized_user_sid}",
            exe.to_string_lossy()
        ),
        true,
        std::time::Duration::from_secs(60),
    )
}

pub fn handle_helper(operation: &str, authorized_user_sid: Option<&str>) -> Result<()> {
    match operation {
        "install" => {
            let authorized_user_sid = authorized_user_sid.ok_or_else(|| {
                TmError::platform(
                    "install core service",
                    "the originating GUI user SID is required",
                )
            })?;
            install(authorized_user_sid)
        }
        "uninstall" => uninstall(),
        _ => Err(TmError::platform(
            "core service helper",
            "unknown operation",
        )),
    }
}

/// Redirect an unelevated package/portable launch to the protected GUI that
/// owns the service channel. The per-user marker is written only after that
/// user's elevated helper has successfully submitted the SCM start request.
/// No renderer or window has been initialized when this is called.
pub fn redirect_to_installed_gui(args: &[String]) -> Result<bool> {
    if super::is_elevated() {
        // A normal ShellExecute from an elevated parent would preserve its
        // token and defeat the split-process architecture.
        return Ok(false);
    }
    let Some(marked_path) = client_redirect_marker() else {
        return Ok(false);
    };
    let installed = expected_installed_gui_path()?;
    let current = std::env::current_exe()?;
    if paths_match(&current, &installed) {
        return Ok(false);
    }
    if !paths_match(&marked_path, &installed)
        || !installed.is_file()
        || has_reparse_attribute(&installed)?
    {
        return Err(TmError::platform(
            "core service GUI redirect",
            "the per-user marker does not name the protected TaskMan executable",
        ));
    }
    let mut current_file = pinned_source(&current)?;
    if sha256_reader(&mut current_file)? != sha256_file(&installed)? {
        // A different package build must remain open so it can explicitly
        // repair/upgrade the protected service generation after UAC approval.
        // It still cannot authenticate to the old broker from this path.
        tracing::info!("portable build differs from protected GUI; skipping redirect for upgrade");
        return Ok(false);
    }

    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| TmError::platform("open service manager", error.to_string()))?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(_) => {}
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            // External/manual service removal must not leave every future
            // portable launch paying a stale redirect check.
            set_client_redirect_marker(None)?;
            return Ok(false);
        }
        Err(error) => {
            return Err(TmError::platform(
                "open core service for redirect",
                error.to_string(),
            ));
        }
    }

    super::process_ops::run_new_task(&gui_command(&installed, args), false)?;
    Ok(true)
}

/// Mid-session counterpart of [`redirect_to_installed_gui`]: hand this
/// session over to the protected installed GUI. Used when the state surface
/// reports [`CoreServiceState::ForeignClient`], because no reinstall can make
/// a foreign image pass the broker's client authorization. Returns
/// `Ok(false)` when there is no installed generation to switch into.
pub fn relaunch_into_installed_gui(args: &[String]) -> Result<bool> {
    // Unlike the startup redirect, an elevated session may switch: this is an
    // explicit user action, so the installed GUI inheriting the session's
    // elevation is intended, not silently preserved.
    let installed = expected_installed_gui_path()?;
    if !installed.is_file() || has_reparse_attribute(&installed)? {
        return Ok(false);
    }
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| TmError::platform("open service manager", error.to_string()))?;
    if manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .is_err()
    {
        return Ok(false);
    }
    // The installed replacement must outlive this closing session: the
    // handoff flag makes it wait for this instance's single-instance mutex
    // instead of bouncing off the guard and exiting.
    let mut forwarded = args.to_vec();
    forwarded.push("--single-instance-handoff".to_string());
    super::process_ops::run_new_task(&gui_command(&installed, &forwarded), false)?;
    Ok(true)
}

fn gui_command(executable: &Path, args: &[String]) -> String {
    let mut command = format!("\"{}\"", executable.to_string_lossy());
    for argument in args {
        command.push(' ');
        command.push_str(&super::quote_win_arg(argument));
    }
    command
}

fn set_client_redirect_marker(path: Option<&Path>) -> Result<()> {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
    };

    let key_path = wide(CLIENT_REGISTRY_KEY);
    let value_name = wide(CLIENT_REGISTRY_VALUE);
    unsafe {
        let mut key = Default::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if status.is_err() {
            return Err(TmError::platform(
                "open core service client marker",
                format!("{status:?}"),
            ));
        }
        let result = if let Some(path) = path {
            let value = path_wide(path);
            let bytes = std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2);
            let status =
                RegSetValueExW(key, PCWSTR(value_name.as_ptr()), None, REG_SZ, Some(bytes));
            if status.is_err() {
                Err(TmError::platform(
                    "write core service client marker",
                    format!("{status:?}"),
                ))
            } else {
                Ok(())
            }
        } else {
            let status = RegDeleteValueW(key, PCWSTR(value_name.as_ptr()));
            if status.is_err() && status.0 != ERROR_FILE_NOT_FOUND.0 {
                Err(TmError::platform(
                    "delete core service client marker",
                    format!("{status:?}"),
                ))
            } else {
                Ok(())
            }
        };
        let _ = RegCloseKey(key);
        result
    }
}

fn client_redirect_marker() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    let key_path = wide(CLIENT_REGISTRY_KEY);
    let value_name = wide(CLIENT_REGISTRY_VALUE);
    unsafe {
        let mut key = Default::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return None;
        }
        let mut kind = REG_SZ;
        let mut bytes = 0u32;
        let size = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut bytes),
        );
        if size.is_err()
            || kind != REG_SZ
            || !(2..=64 * 1024).contains(&bytes)
            || !bytes.is_multiple_of(2)
        {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut data = vec![0u8; bytes as usize];
        let read = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut bytes),
        );
        let _ = RegCloseKey(key);
        if read.is_err()
            || kind != REG_SZ
            || bytes < 2
            || !bytes.is_multiple_of(2)
            || bytes as usize > data.len()
        {
            return None;
        }
        let wide: Vec<u16> = data[..bytes as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .take_while(|character| *character != 0)
            .collect();
        (!wide.is_empty()).then(|| PathBuf::from(String::from_utf16_lossy(&wide)))
    }
}

// Installation and lifecycle helpers are below the transport so protocol
// tests can exercise framing without touching SCM or machine-wide files.

fn expected_installed_gui_path() -> Result<PathBuf> {
    Ok(install_dir()?.join(GUI_EXE_NAME))
}

fn expected_installed_service_path() -> Result<PathBuf> {
    Ok(install_dir()?.join(SERVICE_EXE_NAME))
}

fn manifest_path() -> Result<PathBuf> {
    Ok(service_data_dir()?.join("broker.json"))
}

fn install_dir() -> Result<PathBuf> {
    Ok(known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFiles)?.join("TaskMan"))
}

pub fn service_data_dir() -> Result<PathBuf> {
    Ok(known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramData)?.join("TaskMan"))
}

/// Prepare the only directory the LocalSystem service is allowed to write.
///
/// Returns `Ok(None)` when file logging must stay off because the directory
/// cannot be proven safe: an unexpected entry, a reparse point, an external
/// hard link, or an ACL that cannot be applied. The broker MUST keep running
/// in that case. Log-file availability is not a security precondition, and
/// failing the service would let an unelevated user who pre-creates a file
/// under `%ProgramData%\TaskMan\logs` (writable to Users before the first
/// install) disable the privileged control plane until an administrator
/// removes it.
///
/// The security property is preserved: when anything about the directory is
/// unproven, no file appender is opened at all, so a planted path can never
/// become a privileged write primitive.
pub fn prepare_service_log_dir() -> Result<Option<PathBuf>> {
    let data = service_data_dir()?;
    if let Err(error) = ensure_secure_directory(&data, SERVICE_DATA_SDDL) {
        tracing::warn!(%error, "service data directory cannot be secured; file logging disabled");
        return Ok(None);
    }
    let logs = data.join("logs");
    if let Err(error) = ensure_secure_directory(&logs, SERVICE_DATA_SDDL) {
        tracing::warn!(%error, "service log directory cannot be secured; file logging disabled");
        return Ok(None);
    }
    if let Err(error) = verify_owned_log_entries(&logs) {
        tracing::warn!(%error, "service log directory contains unowned entries; file logging disabled");
        return Ok(None);
    }
    Ok(Some(logs))
}

/// Every existing entry must be an owned, single-link, regular log file whose
/// DACL can be repaired. Anything else (a planted file, a directory, a
/// reparse point, or a hard link) disables file logging: the rolling appender
/// must never be pointed at a directory whose contents an unprivileged user
/// can influence, because it would then follow that entry when creating a
/// log file of the same name.
fn verify_owned_log_entries(logs: &Path) -> Result<()> {
    for entry in std::fs::read_dir(logs)? {
        let entry = entry?;
        let path = entry.path();
        if !owned_service_log_name(&entry.file_name()) {
            return Err(TmError::platform(
                "core service log directory",
                format!("{} is not an owned service log", path.display()),
            ));
        }
        // Keep the entry pinned without write/delete sharing while its
        // topology is checked and its DACL is repaired. This also conflicts
        // with a pre-existing delete handle instead of racing a rename.
        let file = pinned_source(&path)?;
        ensure_single_link(&file, &path, "core service log directory")?;
        set_path_security(&path, SERVICE_DATA_SDDL)?;
    }
    Ok(())
}

fn owned_service_log_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(date) = name.strip_prefix(&format!("{SERVICE_LOG_FILE_PREFIX}.")) else {
        return false;
    };
    date.len() == 10
        && date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

fn known_folder(id: &windows::core::GUID) -> Result<PathBuf> {
    let value = unsafe {
        windows::Win32::UI::Shell::SHGetKnownFolderPath(id, Default::default(), None)
            .map_err(|error| TmError::platform("SHGetKnownFolderPath", error.to_string()))?
    };
    let path = unsafe { value.to_string() }
        .map_err(|error| TmError::platform("known folder path", error.to_string()))?;
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast()));
    }
    Ok(PathBuf::from(path))
}

fn ensure_elevated(operation: &'static str) -> Result<()> {
    if super::is_elevated() {
        Ok(())
    } else {
        Err(TmError::platform(
            operation,
            "administrator approval is required",
        ))
    }
}

fn has_reparse_attribute(path: &Path) -> std::io::Result<bool> {
    Ok(std::fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

fn ensure_secure_directory(path: &Path, sddl: &str) -> Result<()> {
    if path.exists() {
        if !path.is_dir() || has_reparse_attribute(path)? {
            return Err(TmError::platform(
                "core service install directory",
                format!("{} is not a regular directory", path.display()),
            ));
        }
    } else {
        std::fs::create_dir(path)?;
    }
    // Pin the final path against conflicting writers/deleters before changing
    // its security. This catches a user-precreated ProgramData directory with
    // a live mutation handle instead of trusting a path-only metadata check.
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .map_err(TmError::Io)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(TmError::platform(
            "core service protected directory",
            format!("{} changed while being secured", path.display()),
        ));
    }
    set_path_security(path, sddl)
}

fn set_path_security(path: &Path, sddl: &str) -> Result<()> {
    let descriptor = security_descriptor(sddl)?;
    let encoded = path_wide(path);
    let information = OWNER_SECURITY_INFORMATION
        | GROUP_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | PROTECTED_DACL_SECURITY_INFORMATION;
    let changed = unsafe { SetFileSecurityW(PCWSTR(encoded.as_ptr()), information, descriptor.0) };
    if !changed.as_bool() {
        return Err(TmError::platform(
            "SetFileSecurityW",
            format!("{}: {}", path.display(), std::io::Error::last_os_error()),
        ));
    }
    Ok(())
}

fn pinned_source(path: &Path) -> Result<File> {
    if !path.is_file() || has_reparse_attribute(path)? {
        return Err(TmError::platform(
            "core service source",
            format!(
                "{} is missing, not a regular file, or a reparse point",
                path.display()
            ),
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(TmError::Io)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(TmError::platform(
            "core service source",
            format!(
                "{} changed into a reparse point while opening",
                path.display()
            ),
        ));
    }
    Ok(file)
}

fn ensure_single_link(file: &File, path: &Path, operation: &'static str) -> Result<()> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information).map_err(
            |error| TmError::platform(operation, format!("{}: {error}", path.display())),
        )?;
    }
    if information.nNumberOfLinks != 1 {
        return Err(TmError::platform(
            operation,
            format!("{} is hard-linked", path.display()),
        ));
    }
    Ok(())
}

fn replace_file_from_pinned_source(source: &Path, destination: &Path, sddl: &str) -> Result<()> {
    // Pin before hashing. The restrictive share mode prevents a user-writable
    // source from being replaced or opened for writing between verification
    // and the administrative copy.
    let mut input = pinned_source(source)?;
    let source_hash = sha256_reader(&mut input)?;
    input.rewind()?;
    if destination.exists() {
        // Pin and secure an existing destination before trusting its hash.
        // A stale user-writable file with an open mutation handle or an
        // external hard link must fail here, not remain executable because it
        // happened to match this generation at one instant.
        let mut existing = pinned_source(destination)?;
        ensure_single_link(&existing, destination, "core service destination")?;
        let existing_hash = sha256_reader(&mut existing)?;
        set_path_security(destination, sddl)?;
        if existing_hash == source_hash {
            return Ok(());
        }
        drop(existing);
    }

    let temporary = destination.with_extension(format!("new-{}", std::process::id()));
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            TmError::platform(
                "core service staging file",
                format!("{}: {error}", temporary.display()),
            )
        })?;
    let copy_result = (|| -> std::io::Result<()> {
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()
    })();
    if let Err(error) = copy_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(TmError::Io(error));
    }
    if let Err(error) = set_path_security(&temporary, sddl) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    let from = path_wide(&temporary);
    let to = path_wide(destination);
    let moved = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = moved {
        let _ = std::fs::remove_file(&temporary);
        return Err(TmError::platform(
            "install protected binary",
            format!(
                "{} (stop/remove an older service before upgrading): {error}",
                destination.display()
            ),
        ));
    }
    set_path_security(destination, sddl)?;
    if sha256_file(destination)? != source_hash {
        return Err(TmError::platform(
            "install protected binary",
            "destination hash does not match pinned source",
        ));
    }
    Ok(())
}

pub(super) fn current_user_sid() -> Result<String> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|error| TmError::platform("OpenProcessToken", error.to_string()))?;
    }
    let result = (|| -> Result<String> {
        let mut required = 0u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut required) };
        if required < std::mem::size_of::<TOKEN_USER>() as u32 || required > 64 * 1024 {
            return Err(TmError::platform(
                "GetTokenInformation",
                "invalid token-user buffer size",
            ));
        }
        let word = std::mem::size_of::<usize>();
        let mut buffer = vec![0usize; (required as usize).div_ceil(word)];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                required,
                &mut required,
            )
            .map_err(|error| TmError::platform("GetTokenInformation", error.to_string()))?;
        }
        let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        let mut text = PWSTR::null();
        unsafe {
            ConvertSidToStringSidW(user.User.Sid, &mut text)
                .map_err(|error| TmError::platform("ConvertSidToStringSidW", error.to_string()))?;
        }
        let value = unsafe { text.to_string() }
            .map_err(|error| TmError::platform("user SID", error.to_string()))?;
        unsafe {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        }
        if !valid_sid_text(&value) {
            return Err(TmError::platform("user SID", "converted SID is malformed"));
        }
        Ok(value)
    })();
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

fn write_manifest(manifest: &BrokerManifest) -> Result<()> {
    let destination = manifest_path()?;
    if destination.exists() && has_reparse_attribute(&destination)? {
        return Err(TmError::platform(
            "broker manifest",
            format!("{} is a reparse point", destination.display()),
        ));
    }
    let temporary = destination.with_extension(format!("new-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    if bytes.len() > 64 * 1024 {
        return Err(TmError::platform(
            "broker manifest",
            "manifest is too large",
        ));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(TmError::Io(error));
    }
    if let Err(error) = set_path_security(&temporary, SERVICE_DATA_SDDL) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    let from = path_wide(&temporary);
    let to = path_wide(&destination);
    if let Err(error) = unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } {
        let _ = std::fs::remove_file(&temporary);
        return Err(TmError::platform(
            "write broker manifest",
            error.to_string(),
        ));
    }
    set_path_security(&destination, SERVICE_DATA_SDDL)
}

const REQUIRED_SERVICE_PRIVILEGES: [&str; 2] = ["SeDebugPrivilege", "SeSystemProfilePrivilege"];

fn required_service_privileges() -> Vec<u16> {
    let mut privileges = Vec::new();
    for privilege in REQUIRED_SERVICE_PRIVILEGES {
        privileges.extend(privilege.encode_utf16());
        privileges.push(0);
    }
    // SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO uses a REG_MULTI_SZ.
    privileges.push(0);
    privileges
}

fn set_required_service_privileges(service: &windows_service::service::Service) -> Result<()> {
    use windows::Win32::System::Services::{
        ChangeServiceConfig2W, SC_HANDLE, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
        SERVICE_REQUIRED_PRIVILEGES_INFOW,
    };
    let mut privileges = required_service_privileges();
    let info = SERVICE_REQUIRED_PRIVILEGES_INFOW {
        pmszRequiredPrivileges: PWSTR(privileges.as_mut_ptr()),
    };
    unsafe {
        ChangeServiceConfig2W(
            SC_HANDLE(service.raw_handle()),
            SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
            Some((&info as *const SERVICE_REQUIRED_PRIVILEGES_INFOW).cast()),
        )
        .map_err(|error| TmError::platform("service required privileges", error.to_string()))
    }
}

fn stop_service_for_upgrade(
    service: &windows_service::service::Service,
    status: &windows_service::service::ServiceStatus,
) -> Result<()> {
    use windows_service::service::ServiceState;

    if status.current_state == ServiceState::Stopped {
        return Ok(());
    }
    if !matches!(
        status.current_state,
        ServiceState::Running | ServiceState::StopPending
    ) {
        return Err(TmError::platform(
            "upgrade core service",
            format!(
                "service is busy in state {:?}; retry shortly",
                status.current_state
            ),
        ));
    }
    if status.current_state == ServiceState::Running {
        service.stop().map_err(|error| {
            TmError::platform("stop core service for upgrade", error.to_string())
        })?;
    }
    // The elevated installer can control the SCM service yet be denied
    // SYNCHRONIZE on its LocalSystem process. SCM status is the
    // authority for whether its executable is still in use.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let current = service
            .query_status()
            .map_err(|error| TmError::platform("query stopping core service", error.to_string()))?;
        if current.current_state == ServiceState::Stopped {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(TmError::platform(
                "upgrade core service",
                format!(
                    "timed out waiting for service to stop (state {:?})",
                    current.current_state
                ),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Elevated setup also uses this wait before removing files with an older
/// installed helper, whose uninstall may only request an asynchronous stop.
pub fn stop_existing_service_before_copy() -> Result<()> {
    ensure_elevated("stop installed core service")?;
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| TmError::platform("open service manager", error.to_string()))?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(());
        }
        Err(error) => {
            return Err(TmError::platform(
                "open existing core service",
                error.to_string(),
            ));
        }
    };
    let status = service
        .query_status()
        .map_err(|error| TmError::platform("query existing core service", error.to_string()))?;
    stop_service_for_upgrade(&service, &status)
}

/// Setup's service opt-out uses the same hardened directory and pinned-copy
/// rules as broker installation, rather than copying through existing links.
/// The caller must remove the old service before calling this entry point.
pub fn install_program_files_without_service(source_gui: &Path) -> Result<()> {
    ensure_elevated("install program files without service")?;
    ensure_secure_directory(&install_dir()?, INSTALL_SDDL)?;
    replace_file_from_pinned_source(source_gui, &expected_installed_gui_path()?, INSTALL_SDDL)?;
    replace_file_from_pinned_source(
        &source_gui.with_file_name(SERVICE_EXE_NAME),
        &expected_installed_service_path()?,
        INSTALL_SDDL,
    )
}

fn install(authorized_user_sid: &str) -> Result<()> {
    ensure_elevated("install core service")?;
    if !valid_sid_text(authorized_user_sid) {
        return Err(TmError::platform(
            "install core service",
            "the originating GUI user SID is malformed",
        ));
    }
    if !sid_is_user_account(authorized_user_sid) {
        return Err(TmError::platform(
            "install core service",
            "the authorized account must be a user account, not a group or alias SID",
        ));
    }
    let source_gui = std::env::current_exe()?;
    let migrate_task_manager_replacement = matches!(
        super::task_manager_replacement_state_for(&source_gui),
        TaskManagerReplacementState::Enabled | TaskManagerReplacementState::Stale(_)
    );
    let source_service = source_gui.with_file_name(SERVICE_EXE_NAME);
    if !source_service.is_file() {
        return Err(TmError::platform(
            "install core service",
            format!(
                "{} is missing; install from a release package containing both binaries",
                source_service.display()
            ),
        ));
    }

    let install_dir = install_dir()?;
    let data_dir = service_data_dir()?;
    ensure_secure_directory(&install_dir, INSTALL_SDDL)?;
    ensure_secure_directory(&data_dir, SERVICE_DATA_SDDL)?;
    // Stop an installed generation before replacing its image. This avoids
    // relying on executable-section sharing behavior during upgrades and
    // releases the active rolling-log handle before retained logs are pinned.
    stop_existing_service_before_copy()?;
    let _ = prepare_service_log_dir()?;

    let installed_gui = expected_installed_gui_path()?;
    let installed_service = expected_installed_service_path()?;
    replace_file_from_pinned_source(&source_gui, &installed_gui, INSTALL_SDDL)?;
    replace_file_from_pinned_source(&source_service, &installed_service, INSTALL_SDDL)?;
    if migrate_task_manager_replacement {
        super::set_task_manager_replacement_direct_for(true, &installed_gui)?;
    }

    let manifest = BrokerManifest {
        schema_version: MANIFEST_SCHEMA,
        protocol_version: PROTOCOL_VERSION,
        authorized_user_sid: authorized_user_sid.to_string(),
        gui_sha256: sha256_file(&installed_gui)?,
        service_sha256: sha256_file(&installed_service)?,
        gui_path: installed_gui.clone(),
        service_path: installed_service.clone(),
    };
    write_manifest(&manifest)?;

    use std::ffi::OsString;
    use std::time::Duration;
    use windows_service::service::{
        ServiceAccess, ServiceAction as FailureAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceSidType,
        ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| TmError::platform("open service manager", error.to_string()))?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: installed_service,
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP;
    let service = match manager.create_service(&info, access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1073) => {
            let service = manager
                .open_service(SERVICE_NAME, access)
                .map_err(|error| TmError::platform("open core service", error.to_string()))?;
            service
                .change_config(&info)
                .map_err(|error| TmError::platform("update core service", error.to_string()))?;
            service
        }
        Err(error) => {
            return Err(TmError::platform("create core service", error.to_string()));
        }
    };
    service
        .set_description(
            "Provides authenticated, allowlisted privileged process controls for TaskMan.",
        )
        .map_err(|error| TmError::platform("service description", error.to_string()))?;
    // Plain auto-start, not delayed: the broker must be ready as soon as the
    // desktop is, because privileged actions are exactly what a user needs
    // right after boot. The delayed flag lives outside `dwStartType`, so it
    // must be cleared explicitly here; `change_config` never touches it and
    // would leave an existing install delayed.
    service
        .set_delayed_auto_start(false)
        .map_err(|error| TmError::platform("service delayed start", error.to_string()))?;
    service
        .set_config_service_sid_info(ServiceSidType::Unrestricted)
        .map_err(|error| TmError::platform("service SID", error.to_string()))?;
    set_required_service_privileges(&service)?;
    let failure_actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            FailureAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            FailureAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(15),
            },
            FailureAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(60),
            },
        ]),
    };
    service
        .update_failure_actions(failure_actions)
        .map_err(|error| TmError::platform("service failure actions", error.to_string()))?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(|error| TmError::platform("service failure policy", error.to_string()))?;
    let status = service
        .query_status()
        .map_err(|error| TmError::platform("query core service", error.to_string()))?;
    stop_service_for_upgrade(&service, &status)?;
    service
        .start::<&str>(&[])
        .map_err(|error| TmError::platform("start core service", error.to_string()))?;
    Ok(())
}

fn uninstall() -> Result<()> {
    ensure_elevated("uninstall core service")?;
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| TmError::platform("open service manager", error.to_string()))?;
    let access = ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(());
        }
        Err(error) => {
            return Err(TmError::platform("open core service", error.to_string()));
        }
    };
    let status = service.query_status().map_err(|error| {
        TmError::platform("query core service before removal", error.to_string())
    })?;
    stop_service_for_upgrade(&service, &status)?;
    service
        .delete()
        .map_err(|error| TmError::platform("delete core service", error.to_string()))?;
    // Protected binaries and manifest intentionally remain. The installed GUI
    // may be executing, and deleting an in-use administrative binary tree is
    // neither atomic nor necessary for removing the privileged capability.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that flip the process-global app-trace retry window took this lock
    /// so they cannot interleave and read each other's failure timestamp.
    static TEST_OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn service_required_privileges_include_system_profile_as_multi_sz() {
        let actual = required_service_privileges();
        let expected: Vec<u16> = "SeDebugPrivilege\0SeSystemProfilePrivilege\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(actual, expected);
    }

    /// The telemetry response must survive a round trip through the wire
    /// format, and `active` must be part of it: without that flag the client
    /// cannot tell "measured zero" from "unknown", which is the whole
    /// never-fabricate-a-zero invariant.
    #[test]
    fn process_network_sample_round_trips_with_its_active_flag() {
        let sample = ProcessNetworkSample {
            active: true,
            entries: vec![ProcessNetworkEntry {
                pid: 4321,
                received: 9_000,
                sent: 120,
            }],
        };
        let value = BrokerValue::ProcessNetwork(sample);
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: BrokerValue = serde_json::from_slice(&encoded).unwrap();
        let BrokerValue::ProcessNetwork(decoded) = decoded else {
            panic!("wrong variant");
        };
        assert!(decoded.active);
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].pid, 4321);
        assert_eq!(decoded.entries[0].received, 9_000);
        assert_eq!(decoded.entries[0].sent, 120);

        // The default is the "unknown" answer, not an empty measurement.
        let unknown = ProcessNetworkSample::default();
        assert!(!unknown.active && unknown.entries.is_empty());
    }

    #[test]
    fn module_unload_response_accepts_every_v2_outcome_shape() {
        // The first v2 module-unload response predated the `released` field.
        // Keep it decodable so an installed service upgrade does not turn a
        // completed action into an "unexpected response" error.
        let old = r#"{"result":"module_unload","value":{"still_mapped":true}}"#;
        let BrokerValue::ModuleUnload {
            still_mapped,
            verified,
            released,
        } = serde_json::from_str::<BrokerValue>(old).unwrap()
        else {
            panic!("wrong variant");
        };
        assert!(still_mapped && verified);
        assert_eq!(released, 1);

        let current = BrokerValue::ModuleUnload {
            still_mapped: false,
            verified: true,
            released: 1,
        };
        let encoded = serde_json::to_vec(&current).unwrap();
        assert!(matches!(
            serde_json::from_slice::<BrokerValue>(&encoded).unwrap(),
            BrokerValue::ModuleUnload {
                still_mapped: false,
                verified: true,
                released: 1
            }
        ));
    }

    /// The request carries no target and must stay a bare tag on the wire —
    /// this is the one broker request that can change nothing.
    #[test]
    fn process_network_request_takes_no_parameters() {
        let encoded = serde_json::to_string(&BrokerRequest::ProcessNetworkCounters).unwrap();
        // Exactly the internally-tagged unit variant: no pid, no target, no
        // knob a caller could steer. (`deny_unknown_fields` does not apply to
        // unit variants of an internally tagged enum, which is harmless here
        // precisely because there is no field to confuse.)
        assert_eq!(encoded, r#"{"operation":"process_network_counters"}"#);
        assert!(matches!(
            serde_json::from_str::<BrokerRequest>(&encoded).unwrap(),
            BrokerRequest::ProcessNetworkCounters
        ));
    }

    /// A full-size answer has to fit the response frame. [`NET_MAX_ENTRIES`]
    /// is the hard cap; a desktop produces a few dozen.
    #[test]
    fn a_capped_network_response_fits_the_frame_limit() {
        let sample = ProcessNetworkSample {
            active: true,
            entries: (0..NET_MAX_ENTRIES as u32)
                .map(|i| ProcessNetworkEntry {
                    pid: 100_000 + i,
                    received: u64::MAX,
                    sent: u64::MAX,
                })
                .collect(),
        };
        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessNetwork(sample)),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() <= MAX_RESPONSE_BYTES,
            "worst-case response {} exceeds the {MAX_RESPONSE_BYTES}-byte cap",
            encoded.len()
        );
    }

    #[test]
    fn a_capped_disk_response_fits_the_frame_limit() {
        let time = |disk: u32| ProcessDiskTime {
            disk,
            service_time: u64::MAX,
        };
        // Spend the item budget on the entries, one disk each, then the rest
        // on the totals: the worst case for either cap.
        let per_entry = DISK_MAX_DISK_TIMES / DISK_MAX_ENTRIES;
        let sample = ProcessDiskSample {
            active: true,
            window_ms: u64::MAX,
            total_service_time: u64::MAX,
            events_seen: u64::MAX,
            events_decoded: u64::MAX,
            disk_totals: (0..(DISK_MAX_DISK_TIMES - per_entry * DISK_MAX_ENTRIES) as u32)
                .map(|i| time(u32::MAX - i))
                .collect(),
            entries: (0..DISK_MAX_ENTRIES as u32)
                .map(|i| ProcessDiskEntry {
                    pid: 100_000 + i,
                    service_time: u64::MAX,
                    disks: (0..per_entry as u32).map(|d| time(u32::MAX - d)).collect(),
                    read_bytes: u64::MAX,
                    write_bytes: u64::MAX,
                    ops: u64::MAX,
                })
                .collect(),
        };
        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessDisk(sample)),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() <= MAX_RESPONSE_BYTES,
            "{} > {MAX_RESPONSE_BYTES}",
            encoded.len()
        );
    }

    /// Regression: `take_window` drains the accumulators, so a drained window
    /// the caps refused was simply dropped — its service time, bytes and events
    /// never reached a total and the next window's shares were computed without
    /// them. A refused window is now held and measured with the next one.
    #[test]
    fn a_window_the_caps_refused_is_measured_with_the_next_one() {
        use super::super::disk_etw::{DiskTime, DiskWindow, PidDisk};

        let window =
            |since_ms: u64, ops: u64, read: u64, write: u64, service: u64, disks: Vec<DiskTime>| {
                DiskWindow {
                    since_ms,
                    events_seen: ops,
                    events_decoded: ops,
                    unattributed_service_time: ops,
                    disk_totals: disks.clone(),
                    procs: std::collections::HashMap::from([(
                        42u32,
                        PidDisk {
                            service_time: service,
                            per_disk: disks,
                            read_bytes: read,
                            write_bytes: write,
                            ops,
                        },
                    )]),
                }
            };
        let mut held = window(
            1_000,
            3,
            10,
            20,
            400,
            vec![DiskTime {
                disk: 1,
                service_time: 400,
            }],
        );
        let next = window(
            500,
            2,
            5,
            7,
            200,
            vec![
                DiskTime {
                    disk: 1,
                    service_time: 150,
                },
                DiskTime {
                    disk: 2,
                    service_time: 50,
                },
            ],
        );
        merge_disk_windows(&mut held, next);
        assert_eq!(held.since_ms, 1_500, "the merged window covers both spans");
        assert_eq!(held.events_seen, 5);
        assert_eq!(held.events_decoded, 5);
        assert_eq!(held.unattributed_service_time, 5);
        assert_eq!(
            held.disk_totals,
            vec![
                DiskTime {
                    disk: 1,
                    service_time: 550,
                },
                DiskTime {
                    disk: 2,
                    service_time: 50,
                },
            ],
            "the window's own totals add per disk, and a new disk is kept"
        );
        let row = &held.procs[&42];
        assert_eq!(row.ops, 5);
        assert_eq!(row.read_bytes, 15);
        assert_eq!(row.write_bytes, 27);
        assert_eq!(row.service_time, 600);
        assert_eq!(
            row.per_disk,
            vec![
                DiskTime {
                    disk: 1,
                    service_time: 550,
                },
                DiskTime {
                    disk: 2,
                    service_time: 50,
                },
            ]
        );
        assert_eq!(held.total_service_time(), 605);
    }

    /// A held window was drained by an earlier request, so its rows are never
    /// re-pruned by a drain. A process that exits while the hold lasts would
    /// otherwise keep its row for as long as the column is unknown — and the
    /// caps are counted again on every one of those ticks.
    #[test]
    fn a_held_disk_window_drops_rows_for_processes_that_exited() {
        use super::super::disk_etw::{DiskTime, DiskWindow, PidDisk};

        let row = |service: u64| PidDisk {
            service_time: service,
            per_disk: vec![DiskTime {
                disk: 1,
                service_time: service,
            }],
            read_bytes: 1,
            write_bytes: 2,
            ops: 1,
        };
        let mut window = DiskWindow {
            procs: std::collections::HashMap::from([(7u32, row(40)), (8, row(2))]),
            unattributed_service_time: 5,
            disk_totals: vec![DiskTime {
                disk: 1,
                service_time: 42,
            }],
            since_ms: 1_000,
            events_seen: 2,
            events_decoded: 2,
        };
        prune_held_disk_window(&mut window, &std::collections::HashSet::from([7u32]));
        assert!(
            window.procs.contains_key(&7),
            "a live process keeps its row"
        );
        assert!(
            !window.procs.contains_key(&8),
            "an exited process must not keep a row"
        );
        assert_eq!(
            window.unattributed_service_time, 7,
            "its service time stays in the denominator instead of vanishing"
        );
        assert_eq!(
            window.disk_totals[0].service_time, 42,
            "the window's own totals are unchanged: the work happened"
        );
        assert_eq!(window.total_service_time(), 47);

        // An empty live set means the caller could not enumerate processes,
        // which is never "nothing is alive".
        prune_held_disk_window(&mut window, &std::collections::HashSet::new());
        assert!(window.procs.contains_key(&7));
        assert_eq!(window.unattributed_service_time, 7);
    }

    /// The caps count exactly what the response carries: a row per process that
    /// completed a request, that row's per-disk split, and the window's own
    /// per-disk totals. The drained window is checked against them before
    /// anything is taken out of it, so both sides of each boundary are pinned.
    #[test]
    fn the_disk_caps_count_what_the_response_carries() {
        use super::super::disk_etw::{DiskTime, DiskWindow, PidDisk};

        let row = |ops: u64, disks: u32| PidDisk {
            ops,
            per_disk: (0..disks)
                .map(|disk| DiskTime {
                    disk,
                    service_time: 1,
                })
                .collect(),
            ..PidDisk::default()
        };
        let at_entry_cap = DiskWindow {
            procs: (0..DISK_MAX_ENTRIES as u32)
                .map(|pid| (pid, row(1, 0)))
                .collect(),
            ..DiskWindow::default()
        };
        assert!(disk_window_fits(&at_entry_cap));
        let mut over_entry_cap = at_entry_cap.clone();
        over_entry_cap
            .procs
            .insert(DISK_MAX_ENTRIES as u32, row(1, 0));
        assert!(
            !disk_window_fits(&over_entry_cap),
            "a row over the entry cap cannot be published"
        );

        // Two disks per row spends the per-disk budget exactly, so the window's
        // own totals are what pushes it over.
        let rows = (DISK_MAX_DISK_TIMES / 2) as u32;
        let mut at_disk_cap = DiskWindow {
            procs: (0..rows).map(|pid| (pid, row(1, 2))).collect(),
            ..DiskWindow::default()
        };
        assert!(disk_window_fits(&at_disk_cap));
        at_disk_cap.disk_totals.push(DiskTime {
            disk: 9,
            service_time: 1,
        });
        assert!(
            !disk_window_fits(&at_disk_cap),
            "the window's totals share the per-disk cap"
        );

        // A process that completed no request is not a row of the answer.
        let idle = DiskWindow {
            procs: std::collections::HashMap::from([(7u32, row(0, 4))]),
            ..DiskWindow::default()
        };
        assert!(disk_window_fits(&idle));
    }

    #[test]
    fn a_capped_module_code_response_fits_the_frame_limit() {
        let window = ProcessModuleCodeWindow {
            window_ms: u64::MAX,
            events_seen: u64::MAX,
            events_decoded: u64::MAX,
            events_lost: Some(u64::MAX),
            target_samples: u64::MAX,
            attributed_samples: u64::MAX,
            tracked_threads: u32::MAX,
            modules: (0..MAX_MODULES_PER_RESPONSE as u64)
                .map(|base| crate::actions::ProcessModuleCodeSamples {
                    base_address: u64::MAX - base,
                    samples: u64::MAX,
                })
                .collect(),
        };
        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessModuleCodeSamples(Some(window))),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() <= MAX_RESPONSE_BYTES,
            "{} > {MAX_RESPONSE_BYTES}",
            encoded.len()
        );
    }

    #[test]
    fn stalled_telemetry_worker_has_a_deadline_and_recovers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (release, held) = std::sync::mpsc::channel();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let worker = TelemetryBroker::start_with(move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = held.recv();
            }
            Ok(BrokerValue::ProcessNetwork(ProcessNetworkSample::default()))
        })
        .expect("worker starts");
        assert!(matches!(
            worker.call_with_timeout(
                BrokerRequest::ProcessNetworkCounters,
                std::time::Duration::from_millis(20)
            ),
            Err(BrokerCallError::Unavailable(_))
        ));
        // The 20 ms deadline can expire before the worker thread has even
        // picked the request up on a loaded machine; the property under test
        // is what happens once it has, so wait for that (bounded) first.
        let pickup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < pickup_deadline {
            std::thread::yield_now();
        }
        assert!(matches!(
            worker.call_with_timeout(
                BrokerRequest::ProcessNetworkCounters,
                std::time::Duration::from_millis(20)
            ),
            Err(BrokerCallError::Unavailable(_))
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one stalled request owns the lane"
        );
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while worker.busy.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!worker.busy.load(Ordering::Acquire));
        assert!(
            worker
                .call_with_timeout(
                    BrokerRequest::ProcessNetworkCounters,
                    std::time::Duration::from_secs(1)
                )
                .is_ok()
        );
    }

    #[test]
    fn live_service_identity_verifies_without_elevation() {
        // Live verification against the installed service: skip silently when
        // the service is not running so offline test runs stay green.
        let Ok(pipe) = open_client_pipe() else {
            eprintln!("core service pipe not reachable; skipping");
            return;
        };
        let Ok(server_pid) = server_pid(&pipe) else {
            eprintln!("no pipe server identity; skipping");
            return;
        };
        drop(pipe);
        let expected = expected_installed_service_path().unwrap();
        let verified = verify_pipe_server(server_pid, &expected).expect("identity check failed");
        assert!(
            verified,
            "pipe server {server_pid} is not the registered service"
        );
    }

    #[test]
    fn protocol_round_trip_is_bounded_and_versioned() {
        let request = BrokerRequest::KillProcess {
            pid: 101,
            expected_start_epoch_s: Some(202),
            tree: true,
        };
        let payload = serde_json::to_vec(&request).unwrap();
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FRAME_REQUEST, &payload, MAX_REQUEST_BYTES).unwrap();
        let decoded = read_frame(&mut bytes.as_slice(), FRAME_REQUEST, MAX_REQUEST_BYTES).unwrap();
        assert!(matches!(
            serde_json::from_slice::<BrokerRequest>(&decoded).unwrap(),
            BrokerRequest::KillProcess {
                pid: 101,
                tree: true,
                ..
            }
        ));
    }

    #[test]
    fn privileged_process_requests_require_an_exact_identity() {
        assert!(checked_target(101, None, 500).is_err());
        assert!(checked_target(500, Some(202), 500).is_err());
        let payload = br#"{
            "operation":"kill_process",
            "pid":101,
            "expected_start_epoch_s":202,
            "tree":false,
            "unexpected":true
        }"#;
        assert!(serde_json::from_slice::<BrokerRequest>(payload).is_err());
    }

    /// The identity read is a bound, read-only query: no positive sampled
    /// creation time means no answer, exactly like the security/module reads.
    #[test]
    fn the_identity_read_requires_an_exact_process_identity() {
        let probe = |expected_start_epoch_s| {
            dispatch(
                &crate::win::WinActions,
                BrokerRequest::ProcessIdentity {
                    pid: 101,
                    expected_start_epoch_s,
                },
                500,
            )
        };
        assert!(probe(None).is_err(), "pid-only reads are refused");
        assert!(
            probe(Some(0)).is_err(),
            "an implausible creation time is refused"
        );
    }

    /// Wire shape: a bounded identity payload that neither side can grow
    /// silently, and unknown fields stay rejected on the request.
    #[test]
    fn the_identity_read_round_trips_as_a_bounded_payload() {
        let request = BrokerRequest::ProcessIdentity {
            pid: 101,
            expected_start_epoch_s: Some(202),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"operation":"process_identity","pid":101,"expected_start_epoch_s":202}"#
        );
        assert!(matches!(
            serde_json::from_str::<BrokerRequest>(&encoded).unwrap(),
            BrokerRequest::ProcessIdentity {
                pid: 101,
                expected_start_epoch_s: Some(202),
            }
        ));
        let grown = encoded.replace('}', r#","extra":true}"#);
        assert!(serde_json::from_str::<BrokerRequest>(&grown).is_err());

        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessIdentity(
                crate::win::process_ops::ProcessTokenIdentity {
                    name: "SYSTEM".into(),
                    sid: Some("S-1-5-18".into()),
                    elevated: Some(true),
                    virtualization: Some(tm_core::model::UacVirtualization::NotAllowed),
                },
            )),
        };
        let decoded: BrokerResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        let Ok(BrokerValue::ProcessIdentity(identity)) = decoded.value else {
            panic!("identity response did not round trip");
        };
        assert_eq!(identity.name, "SYSTEM");
        assert_eq!(identity.sid.as_deref(), Some("S-1-5-18"));
        assert_eq!(identity.elevated, Some(true));
    }

    #[test]
    fn process_security_info_round_trips_with_parent_process_info() {
        let request = BrokerRequest::ProcessSecurityInfo {
            pid: 101,
            expected_start_epoch_s: Some(202),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"operation":"process_security_info","pid":101,"expected_start_epoch_s":202}"#
        );
        assert!(matches!(
            serde_json::from_str::<BrokerRequest>(&encoded).unwrap(),
            BrokerRequest::ProcessSecurityInfo {
                pid: 101,
                expected_start_epoch_s: Some(202),
            }
        ));
        let grown = encoded.replace('}', r#","extra":true}"#);
        assert!(serde_json::from_str::<BrokerRequest>(&grown).is_err());

        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessSecurity(
                crate::win::ProcessSecurityInfo {
                    protection: Some(crate::win::ProcessProtectionLevel::None),
                    machine: Some(crate::win::ProcessMachineType::X64),
                    critical: Some(false),
                    dep: Some(1),
                    dep_permanent: Some(true),
                    aslr: None,
                    dynamic_code: None,
                    strict_handle: None,
                    system_call: None,
                    extension_point: None,
                    control_flow_guard: None,
                    signature: None,
                    font: None,
                    image_load: None,
                    child_process: None,
                    user_shadow_stack: None,
                    sehop: None,
                    side_channel: None,
                    payload_restriction: None,
                    parent: Some(crate::win::ProcessParentInfo {
                        pid: 1496,
                        name: Some("services.exe".into()),
                        display: Some("Services and Controller app".into()),
                        path: Some(r"C:\Windows\System32\services.exe".into()),
                        alive: true,
                    }),
                },
            )),
        };
        let decoded: BrokerResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        let Ok(BrokerValue::ProcessSecurity(security)) = decoded.value else {
            panic!("security response did not round trip");
        };
        assert_eq!(security.critical, Some(false));
        let parent = security.parent.expect("parent info present");
        assert_eq!(parent.pid, 1496);
        assert_eq!(parent.name.as_deref(), Some("services.exe"));
        assert_eq!(
            parent.display.as_deref(),
            Some("Services and Controller app")
        );
        assert_eq!(
            parent.path.as_deref(),
            Some(r"C:\Windows\System32\services.exe")
        );
        assert!(parent.alive);
    }

    /// Sampled code is another read-only, exact-identity query. A PID alone or
    /// a fabricated creation time must be refused before the service can bind
    /// a profiler session to a recycled process id.
    #[test]
    fn the_module_code_sample_read_requires_an_exact_process_identity() {
        let probe = |expected_start_epoch_s| {
            dispatch(
                &crate::win::WinActions,
                BrokerRequest::ProcessModuleCodeSamples {
                    pid: 101,
                    expected_start_epoch_s,
                },
                500,
            )
        };
        assert!(probe(None).is_err(), "pid-only reads are refused");
        assert!(
            probe(Some(0)).is_err(),
            "an implausible creation time is refused"
        );
    }

    #[test]
    fn module_code_samples_round_trip_and_reject_grown_payloads() {
        let request = BrokerRequest::ProcessModuleCodeSamples {
            pid: 101,
            expected_start_epoch_s: Some(202),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"operation":"process_module_code_samples","pid":101,"expected_start_epoch_s":202}"#
        );
        let grown = encoded.replace('}', r#","extra":true}"#);
        assert!(serde_json::from_str::<BrokerRequest>(&grown).is_err());

        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessModuleCodeSamples(Some(
                ProcessModuleCodeWindow {
                    window_ms: 2_000,
                    events_seen: 9_000,
                    events_decoded: 9_000,
                    events_lost: Some(0),
                    target_samples: 42,
                    attributed_samples: 40,
                    tracked_threads: 12,
                    modules: vec![crate::actions::ProcessModuleCodeSamples {
                        base_address: 0x7FF0_0000_0000,
                        samples: 40,
                    }],
                },
            ))),
        };
        let decoded: BrokerResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        let Ok(BrokerValue::ProcessModuleCodeSamples(Some(window))) = decoded.value else {
            panic!("module code-sample response did not round trip");
        };
        assert_eq!(window.target_samples, 42);
        assert_eq!(window.attributed_samples, 40);
        assert_eq!(window.events_lost, Some(0));
        assert_eq!(window.modules[0].samples, 40);
    }

    /// Thread evidence is another read-only, exact-identity query. PID alone
    /// and an implausible creation time must be refused before any thread is
    /// opened.
    #[test]
    fn the_thread_evidence_read_requires_an_exact_process_identity() {
        let probe = |expected_start_epoch_s| {
            dispatch(
                &crate::win::WinActions,
                BrokerRequest::ProcessThreadDetails {
                    pid: 101,
                    expected_start_epoch_s,
                },
                500,
            )
        };
        assert!(probe(None).is_err(), "pid-only reads are refused");
        assert!(
            probe(Some(0)).is_err(),
            "an implausible creation time is refused"
        );
    }

    #[test]
    fn thread_evidence_round_trips_and_rejects_untargeted_or_grown_payloads() {
        let request = BrokerRequest::ProcessThreadDetails {
            pid: 101,
            expected_start_epoch_s: Some(202),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"operation":"process_thread_details","pid":101,"expected_start_epoch_s":202}"#
        );
        let grown = encoded.replace('}', r#","extra":true}"#);
        assert!(serde_json::from_str::<BrokerRequest>(&grown).is_err());

        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessThreadDetails(vec![
                ProcessThreadDetail {
                    tid: 7,
                    created_epoch_s: Some(100),
                    start_address: Some(0x7FF0_0000_1234),
                    cycles: Some(99),
                },
            ])),
        };
        let decoded: BrokerResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        let Ok(BrokerValue::ProcessThreadDetails(details)) = decoded.value else {
            panic!("thread-details response did not round trip");
        };
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].tid, 7);
        assert_eq!(details[0].start_address, Some(0x7FF0_0000_1234));
        assert_eq!(details[0].cycles, Some(99));
    }

    #[test]
    fn thread_evidence_merges_only_an_identity_matched_tid() {
        use crate::actions::{ProcessThread, ProcessThreads, ThreadState};

        let thread = |tid, created_epoch_s, start_address, cycles| ProcessThread {
            tid,
            start_address,
            priority: 8,
            base_priority: 8,
            nice: None,
            context_switches: Some(1),
            kernel_time_100ns: 2,
            user_time_100ns: 3,
            created_epoch_s,
            state: ThreadState::Waiting,
            wait_reason: Some(6),
            cycles,
        };
        let mut threads = ProcessThreads {
            process_base_priority: 8,
            threads: vec![
                thread(7, Some(100), None, None),
                thread(8, Some(200), Some(0x1000), Some(10)),
            ],
        };
        let details = [
            ProcessThreadDetail {
                tid: 7,
                created_epoch_s: Some(100),
                start_address: Some(0x7000),
                cycles: Some(20),
            },
            // Same TID, different incarnation: must not be merged into local
            // thread 8, which already has readable values.
            ProcessThreadDetail {
                tid: 8,
                created_epoch_s: Some(999),
                start_address: Some(0x8000),
                cycles: Some(30),
            },
        ];

        assert_eq!(merge_process_thread_details(&mut threads, &details), 1);
        assert_eq!(threads.threads[0].start_address, Some(0x7000));
        assert_eq!(threads.threads[0].cycles, Some(20));
        assert_eq!(threads.threads[1].start_address, Some(0x1000));
        assert_eq!(threads.threads[1].cycles, Some(10));
    }

    #[test]
    fn a_capped_thread_evidence_response_fits_the_frame_limit() {
        let details: Vec<ProcessThreadDetail> = (0..MAX_THREAD_DETAILS_PER_RESPONSE)
            .map(|index| ProcessThreadDetail {
                tid: u32::MAX - index as u32,
                created_epoch_s: Some(i64::MAX - index as i64),
                start_address: Some(u64::MAX - index as u64),
                cycles: Some(u64::MAX - index as u64),
            })
            .collect();
        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessThreadDetails(details)),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() <= MAX_RESPONSE_BYTES,
            "worst-case thread response {} exceeds the {MAX_RESPONSE_BYTES}-byte cap",
            encoded.len()
        );
    }

    #[test]
    fn the_file_io_read_requires_an_exact_process_identity() {
        let probe = |expected_start_epoch_s| {
            dispatch(
                &crate::win::WinActions,
                BrokerRequest::ProcessFileIo {
                    pid: 101,
                    expected_start_epoch_s,
                },
                500,
            )
        };
        assert!(probe(None).is_err(), "pid-only reads are refused");
        assert!(
            probe(Some(0)).is_err(),
            "an implausible creation time is refused"
        );
    }

    #[test]
    fn file_io_round_trips_and_rejects_untargeted_or_grown_payloads() {
        let request = BrokerRequest::ProcessFileIo {
            pid: 101,
            expected_start_epoch_s: Some(202),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            encoded,
            r#"{"operation":"process_file_io","pid":101,"expected_start_epoch_s":202}"#
        );
        assert!(matches!(
            serde_json::from_str::<BrokerRequest>(&encoded).unwrap(),
            BrokerRequest::ProcessFileIo {
                pid: 101,
                expected_start_epoch_s: Some(202),
            }
        ));
        let grown = encoded.replace('}', r#","extra":true}"#);
        assert!(serde_json::from_str::<BrokerRequest>(&grown).is_err());

        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessFileIo(
                crate::actions::ProcessFileIoSnapshot {
                    files: vec![crate::actions::ProcessFileIo {
                        path: r"C:\Windows\System32\kernel32.dll".into(),
                        read_bytes: Some(1024),
                        write_bytes: Some(0),
                        read_ops: None,
                        write_ops: None,
                        file_size: Some(2048),
                        offset: Some(1024),
                        access_known: true,
                        is_read: true,
                        is_write: false,
                        open_handles: 1,
                    }],
                    accounting: crate::actions::FileIoAccounting {
                        state: crate::actions::FileIoAccountingState::Unavailable,
                        since_ms: 0,
                        read_bytes: 0,
                        write_bytes: 0,
                        read_ops: 0,
                        write_ops: 0,
                        unnamed_read_bytes: 0,
                        unnamed_write_bytes: 0,
                        omitted_read_bytes: 0,
                        omitted_write_bytes: 0,
                        counter_read_bytes: Some(1024),
                        counter_write_bytes: Some(0),
                        events_lost: None,
                        cleared_file_objects: 0,
                        unnamed_creates: 0,
                        dropped_paths: 0,
                        handles_complete: true,
                        // v13: the trace's coverage crosses the pipe intact.
                        traced_since_process_start: true,
                        counter_window_read_bytes: Some(512),
                        counter_window_write_bytes: None,
                    },
                },
            )),
        };
        let decoded: BrokerResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        let Ok(BrokerValue::ProcessFileIo(snapshot)) = decoded.value else {
            panic!("file I/O response did not round trip");
        };
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, r"C:\Windows\System32\kernel32.dll");
        assert_eq!(snapshot.accounting.counter_read_bytes, Some(1024));
        assert!(snapshot.accounting.traced_since_process_start);
        assert_eq!(snapshot.accounting.counter_window_read_bytes, Some(512));
        assert_eq!(snapshot.accounting.counter_window_write_bytes, None);
    }

    #[test]
    fn a_capped_file_io_response_fits_the_frame_limit() {
        let files: Vec<crate::actions::ProcessFileIo> = (0..MAX_FILES_PER_RESPONSE)
            .map(|index| crate::actions::ProcessFileIo {
                path: format!(
                    r"C:\Program Files\Example\Subdirectory\VeryLongFileName_{index}.dat"
                ),
                read_bytes: Some(u64::MAX - index as u64),
                write_bytes: Some(index as u64),
                read_ops: Some(100),
                write_ops: Some(20),
                file_size: Some(1024 * 1024),
                offset: Some(512),
                access_known: true,
                is_read: true,
                is_write: false,
                open_handles: 1,
            })
            .collect();
        let response = BrokerResponse {
            protocol_version: PROTOCOL_VERSION,
            value: Ok(BrokerValue::ProcessFileIo(
                crate::actions::ProcessFileIoSnapshot {
                    files,
                    accounting: crate::actions::FileIoAccounting::default(),
                },
            )),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() <= MAX_RESPONSE_BYTES,
            "worst-case file I/O response {} exceeds the {MAX_RESPONSE_BYTES}-byte cap",
            encoded.len()
        );
    }

    #[test]
    fn framing_rejects_wrong_magic_version_kind_and_size() {
        let mut valid = Vec::new();
        write_frame(&mut valid, FRAME_REQUEST, b"{}", MAX_REQUEST_BYTES).unwrap();

        assert!(read_frame(&mut &valid[..FRAME_HEADER_LEN - 1], FRAME_REQUEST, 20).is_err());
        assert!(read_frame(&mut &valid[..valid.len() - 1], FRAME_REQUEST, 20).is_err());

        let mut wrong_magic = valid.clone();
        wrong_magic[0] = b'X';
        assert!(read_frame(&mut wrong_magic.as_slice(), FRAME_REQUEST, 20).is_err());

        let mut wrong_version = valid.clone();
        wrong_version[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        assert!(read_frame(&mut wrong_version.as_slice(), FRAME_REQUEST, 20).is_err());

        assert!(read_frame(&mut valid.as_slice(), FRAME_RESPONSE, 20).is_err());
        assert!(read_frame(&mut valid.as_slice(), FRAME_REQUEST, 1).is_err());
        assert!(write_frame(&mut Vec::new(), FRAME_RESPONSE, b"too large", 1).is_err());
        assert!(serde_json::from_slice::<BrokerRequest>(br#"{"operation":"shell"}"#).is_err());
    }

    #[test]
    fn pipe_acl_accepts_only_well_formed_sid_text() {
        let sddl = pipe_sddl("S-1-5-21-1-2-3-1001").unwrap();
        // The user ACE must cover the rights a pipe client end needs (data
        // read/write, attributes, synchronize); see USER_PIPE_ACCESS.
        assert!(sddl.contains(&format!("{USER_PIPE_ACCESS:#08x}")));
        assert!(sddl.contains("S-1-5-21-1-2-3-1001"));
        for bad in ["", "S-2-5", "S-1-", "S-1-5S", "S-1-5;GA", "not-a-sid"] {
            assert!(pipe_sddl(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn service_name_validation_is_narrow() {
        assert!(valid_service_name("Spooler"));
        assert!(valid_service_name("Some Service_1.0"));
        assert!(!valid_service_name(""));
        assert!(!valid_service_name("bad\\name"));
        assert!(!valid_service_name("bad\nname"));
    }

    #[test]
    fn protected_gui_command_quotes_forwarded_arguments() {
        let path = Path::new(r"C:\Program Files\TaskMan\taskman.exe");
        let args = vec!["--minimized-to-tray".into(), "two words".into()];
        assert_eq!(
            gui_command(path, &args),
            r#""C:\Program Files\TaskMan\taskman.exe" --minimized-to-tray "two words""#
        );
    }

    #[test]
    fn only_daily_owned_service_log_names_are_accepted() {
        assert!(owned_service_log_name(std::ffi::OsStr::new(
            "taskman-service.log.2026-08-31"
        )));
        for name in [
            "taskman-service.log",
            "taskman-service.log.evil",
            "taskman.log.2026-08-31",
            "desktop.ini",
        ] {
            assert!(!owned_service_log_name(std::ffi::OsStr::new(name)));
        }
    }

    /// The authorized pipe principal must be a real user account. A group or
    /// alias SID (`Users`, `Everyone`) would widen the ACE far beyond the one
    /// interactive user the boundary authorizes, and the SID arrives from a
    /// command-line argument of an elevated helper.
    #[test]
    fn only_user_account_sids_may_be_authorized() {
        // Well-known group/alias SIDs must be rejected.
        assert!(!sid_is_user_account("S-1-5-32-545"), "Users");
        assert!(!sid_is_user_account("S-1-5-32-544"), "Administrators");
        assert!(!sid_is_user_account("S-1-1-0"), "Everyone");
        assert!(!sid_is_user_account("S-1-5-11"), "Authenticated Users");
        assert!(!sid_is_user_account("not-a-sid"));
        // The installing user's own SID always resolves as a user account.
        let own = current_user_sid().expect("current user SID");
        assert!(sid_is_user_account(&own), "own SID {own} must be accepted");
    }

    /// A client that opens the pipe and drops the handle before the accept
    /// thread gets to `ConnectNamedPipe` leaves the instance in the
    /// `ERROR_NO_DATA` state. npfs connects an opener to a listening instance
    /// before the server accepts, so any ordinary client reaches this: an
    /// abandoned call, a GUI shutting down, the broker's own stop wake. It
    /// used to propagate out of the accept loop and terminate the LocalSystem
    /// broker, which SCM then restarted 5-60 s later; the GUI reported that
    /// gap as a service it no longer recognised.
    /// A full pipe must be recognisable as back-pressure: the accept loop
    /// waits on it instead of counting it as a broker failure.
    #[test]
    fn a_full_pipe_reports_busy_not_failure() {
        // The production ACL keeps FILE_CREATE_PIPE_INSTANCE for SYSTEM and
        // administrators; this test runs as the user, who therefore owns its
        // private test pipe outright.
        let sid = current_user_sid().expect("current user SID");
        let sddl = format!("D:P(A;;GA;;;{sid})");
        let name = format!(r"\\.\pipe\Taskman.Core.busytest.{}", std::process::id());
        let mut held = vec![create_pipe_instance(&name, &sddl, true).expect("first instance")];
        for _ in 1..PIPE_INSTANCE_CAP {
            held.push(create_pipe_instance(&name, &sddl, false).expect("instance under the cap"));
        }
        let error = create_pipe_instance(&name, &sddl, false).expect_err("cap reached");
        assert!(is_pipe_busy(&error), "{error}");
        drop(held.pop());
        assert!(create_pipe_instance(&name, &sddl, false).is_ok());
    }

    #[test]
    fn a_client_that_closes_before_accept_is_not_a_broker_failure() {
        let sid = current_user_sid().expect("current user SID");
        let sddl = pipe_sddl(&sid).expect("pipe sddl");
        let name = format!(r"\\.\pipe\Taskman.Core.test.{}", std::process::id());
        let pipe = create_pipe_instance(&name, &sddl, true).expect("listening instance");

        // Open and immediately close a client end, exactly as a cancelled
        // call does.
        let wide_name = wide(&name);
        let client = unsafe {
            CreateFileW(
                PCWSTR(wide_name.as_ptr()),
                USER_PIPE_ACCESS,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .expect("client end");
        unsafe { CloseHandle(client) }.expect("close client end");

        assert!(
            matches!(connect_pipe(&pipe), AcceptOutcome::ClientVanished),
            "a vanished client must be reported as spent, never as a failure"
        );
    }

    /// A planted entry under `%ProgramData%\TaskMan\logs` must disable file
    /// logging, never make the caller fail (the caller is the broker).
    #[test]
    fn foreign_log_entries_disable_file_logging_without_failing() {
        let dir = std::env::temp_dir().join(format!("tm-log-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Empty directory: usable.
        assert!(verify_owned_log_entries(&dir).is_ok());

        // Foreign file name: rejected before any ACL work.
        std::fs::write(dir.join("evil.txt"), b"x").unwrap();
        assert!(verify_owned_log_entries(&dir).is_err());
        std::fs::remove_file(dir.join("evil.txt")).unwrap();

        // A directory masquerading as a log file is not a regular file.
        std::fs::create_dir(dir.join("taskman-service.log.2026-01-01")).unwrap();
        assert!(verify_owned_log_entries(&dir).is_err());
        std::fs::remove_dir(dir.join("taskman-service.log.2026-01-01")).unwrap();

        // An external hard link to an owned-looking name is rejected too.
        let target = dir.join("target.bin");
        let link = dir.join("taskman-service.log.2026-01-02");
        std::fs::write(&target, b"x").unwrap();
        std::fs::hard_link(&target, &link).unwrap();
        assert!(verify_owned_log_entries(&dir).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&target).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The broker parser is a trust boundary: arbitrary bytes must never
    /// panic, whatever the header claims.
    #[test]
    fn arbitrary_frames_and_requests_never_panic() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        };
        for len in 0..96 {
            let bytes: Vec<u8> = (0..len).map(|_| next()).collect();
            let _ = read_frame(&mut bytes.as_slice(), FRAME_REQUEST, MAX_REQUEST_BYTES);
            let _ = read_frame(&mut bytes.as_slice(), FRAME_RESPONSE, MAX_RESPONSE_BYTES);
            let _ = serde_json::from_slice::<BrokerRequest>(&bytes);
            let _ = serde_json::from_slice::<BrokerResponse>(&bytes);
        }
    }

    #[test]
    fn foreign_client_classification_follows_the_installed_image() {
        // The test binary stands in for the installed GUI; it exists, so
        // paths_match can canonicalize it.
        let installed = std::env::current_exe().unwrap();
        assert!(!foreign_client_session(Some(&installed), Some(&installed)));
        // Case differences must not misclassify the authorized client.
        let upper = PathBuf::from(installed.to_string_lossy().to_uppercase());
        assert!(!foreign_client_session(Some(&upper), Some(&installed)));
        // A different existing image (dev tree / portable copy) is foreign.
        let foreign = std::env::temp_dir().join("taskman-foreign-client-probe.exe");
        std::fs::write(&foreign, b"probe").unwrap();
        assert!(foreign_client_session(Some(&foreign), Some(&installed)));
        let _ = std::fs::remove_file(&foreign);
        // Unresolvable paths keep the honest degraded classification.
        assert!(!foreign_client_session(None, Some(&installed)));
        assert!(!foreign_client_session(Some(&installed), None));
    }

    /// A hash mismatch must be reported under the install-integrity exit code
    /// (so `service_state` can name it) and its detail must name the drifted
    /// binary plus both fingerprints. The 2026-09-26 crash loop hid exactly
    /// this failure behind 853 opaque "service-specific error 1" events.
    #[test]
    fn a_pinned_hash_mismatch_is_an_install_integrity_failure() {
        let error = verify_pinned_hash("service", "0123456789abcdef0123", "fedcba9876543210fedc")
            .expect_err("mismatched hashes must fail");
        assert!(is_install_integrity_failure(&error));
        assert_eq!(service_exit_code(&error), SERVICE_EXIT_INSTALL_INTEGRITY);
        let detail = error.to_string();
        assert!(detail.contains("service"), "must name the binary: {detail}");
        assert!(
            detail.contains("0123456789ab"),
            "must carry the expected fingerprint: {detail}"
        );
        assert!(
            detail.contains("fedcba987654"),
            "must carry the actual fingerprint: {detail}"
        );
        // The pin comparison is case-insensitive hex coming from two
        // different code paths.
        verify_pinned_hash("GUI", "0123456789ABCDEF", "0123456789abcdef").unwrap();
    }

    /// Everything else keeps the historical generic code, and only the
    /// integrity code changes what the GUI shows for a stopped service.
    #[test]
    fn ordinary_broker_failures_keep_the_generic_exit_code() {
        use windows_service::service::ServiceExitCode;
        let error = TmError::platform("broker pipe", "accept failed");
        assert!(!is_install_integrity_failure(&error));
        assert_eq!(service_exit_code(&error), SERVICE_EXIT_FAILURE);
        assert_eq!(
            classify_stopped_state(&ServiceExitCode::ServiceSpecific(
                SERVICE_EXIT_INSTALL_INTEGRITY
            )),
            CoreServiceState::InstallIntegrityFailed
        );
        assert_eq!(
            classify_stopped_state(&ServiceExitCode::ServiceSpecific(SERVICE_EXIT_FAILURE)),
            CoreServiceState::Stopped
        );
        assert_eq!(
            classify_stopped_state(&ServiceExitCode::Win32(1)),
            CoreServiceState::Stopped
        );
    }

    fn file_row(path: &str) -> crate::actions::ProcessFileIo {
        crate::actions::ProcessFileIo {
            path: path.to_string(),
            read_bytes: None,
            write_bytes: None,
            read_ops: None,
            write_ops: None,
            file_size: Some(4_096),
            offset: Some(0),
            access_known: true,
            is_read: true,
            is_write: false,
            open_handles: 1,
        }
    }

    /// A row carries a path and five optional numbers, so a thousand long paths
    /// overflow the response body while a thousand short ones do not. That case
    /// used to error and the file list vanished; the tail is now trimmed into
    /// the omitted remainder instead.
    #[test]
    fn a_long_path_file_io_snapshot_is_trimmed_not_rejected() {
        let budget = MAX_RESPONSE_BYTES.saturating_sub(1024);
        // Well under the entry cap, well over the byte budget.
        let rows: Vec<crate::actions::ProcessFileIo> = (0..MAX_FILES_PER_RESPONSE)
            .map(|index| {
                let mut row = file_row(&format!(
                    r"C:\Program Files\Example\{index}\{}",
                    "x".repeat(400)
                ));
                row.read_bytes = Some(4_096);
                row.write_bytes = Some(1_024);
                row
            })
            .collect();
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: rows,
            accounting: crate::actions::FileIoAccounting::default(),
        };
        assert!(
            encoded_file_io_len(&snapshot).unwrap() > budget,
            "the fixture must exceed the budget, or this test proves nothing"
        );
        trim_file_io_to_budget(&mut snapshot, budget).expect("a long list must be trimmed");
        let encoded = encoded_file_io_len(&snapshot).unwrap();
        assert!(encoded <= budget, "{encoded} still over {budget}");
        assert!(!snapshot.files.is_empty(), "the list must not disappear");
        assert!(
            snapshot.files.len() < MAX_FILES_PER_RESPONSE,
            "the tail was dropped"
        );
        assert!(!snapshot.accounting.handles_complete);
        // Everything dropped is still part of the process total.
        let total_rows = MAX_FILES_PER_RESPONSE as u64;
        let kept_rows = snapshot.files.len() as u64;
        let kept: u64 = snapshot
            .files
            .iter()
            .filter_map(|file| file.read_bytes)
            .sum();
        assert_eq!(
            kept + snapshot.accounting.omitted_read_bytes,
            total_rows * 4_096
        );
        assert_eq!(
            snapshot.accounting.omitted_write_bytes,
            (total_rows - kept_rows) * 1_024
        );
        // A list that already fits is left exactly as it is.
        let mut small = crate::actions::ProcessFileIoSnapshot {
            files: vec![file_row(r"C:\data\a.bin")],
            accounting: crate::actions::FileIoAccounting {
                handles_complete: true,
                ..crate::actions::FileIoAccounting::default()
            },
        };
        trim_file_io_to_budget(&mut small, budget).expect("a short list fits");
        assert_eq!(small.files.len(), 1);
        assert!(
            small.accounting.handles_complete,
            "nothing was dropped, so nothing is incomplete"
        );
    }

    /// Regression: the bisection measured the snapshot while the omitted
    /// remainder still held its pre-drop values, but dropping rows adds their
    /// bytes to it — so the digits the search never saw could push the prefix it
    /// settled on over the budget, and the whole list came back as an error.
    /// A budget that holds exactly one row is the smallest case: charging the
    /// other seven grows the accounting past it, so the honest answer is no row
    /// at all, not a failed request.
    #[test]
    fn a_trimmed_file_list_fits_the_budget_its_grown_remainder_spends() {
        let rows = || {
            (0..8)
                .map(|index| {
                    let mut row = file_row(&format!(r"C:\data\file{index}.bin"));
                    row.read_bytes = Some(64 * 1024 * 1024);
                    row.write_bytes = Some(4 * 1024 * 1024);
                    row
                })
                .collect::<Vec<_>>()
        };
        let snapshot_with = |count: usize| crate::actions::ProcessFileIoSnapshot {
            files: rows().into_iter().take(count).collect(),
            accounting: crate::actions::FileIoAccounting::default(),
        };
        let budget = encoded_file_io_len(&snapshot_with(1)).unwrap();
        assert!(
            encoded_file_io_len(&snapshot_with(0)).unwrap() <= budget,
            "the empty form has to fit, or this test only proves the error path"
        );
        let mut snapshot = snapshot_with(8);
        assert!(encoded_file_io_len(&snapshot).unwrap() > budget);
        trim_file_io_to_budget(&mut snapshot, budget).expect("a trimmed list must fit its budget");
        assert!(
            snapshot.files.is_empty(),
            "one row no longer fits once its dropped siblings are charged to the remainder"
        );
        assert_eq!(
            snapshot.accounting.omitted_read_bytes,
            8 * 64 * 1024 * 1024,
            "every dropped row's bytes stay in the process total"
        );
        assert_eq!(snapshot.accounting.omitted_write_bytes, 8 * 4 * 1024 * 1024);
        assert!(!snapshot.accounting.handles_complete);
        let encoded = encoded_file_io_len(&snapshot).unwrap();
        assert!(encoded <= budget, "{encoded} still over {budget}");
    }

    /// The two hosts are separate sessions under separate names: an elevated
    /// GUI must never join, stop, or double-count the service's trace.
    ///
    /// Driven through the sampler with a recording start, because a comparison
    /// of the two accessors cannot catch a call site that stopped using them:
    /// each host must ASK for its own role, and the fixed names those roles map
    /// to must differ, or one host reclaims the other's session.
    #[test]
    fn each_host_starts_its_own_session() {
        let asked = std::sync::Mutex::new(Vec::new());
        let record = |role: super::super::etw::TraceRole| {
            asked.lock().unwrap().push(role);
            Err(super::super::file_etw::FileTraceRefusal::Unavailable)
        };
        assert!(
            file_trace_sample(FileTraceHost::App, std::process::id(), 1, &record)
                .sample
                .is_none(),
            "a refused start reports no sample"
        );
        assert!(
            file_trace_sample(FileTraceHost::Service, std::process::id(), 1, &record)
                .sample
                .is_none(),
            "a refused start reports no sample"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            vec![
                super::super::etw::TraceRole::App,
                super::super::etw::TraceRole::Service
            ],
            "each host must start the session named for itself"
        );
        assert_ne!(
            super::super::file_etw::session_name(super::super::etw::TraceRole::App),
            super::super::file_etw::session_name(super::super::etw::TraceRole::Service),
            "a shared name would let one host join or stop the other's session"
        );
        assert!(
            !std::ptr::eq(FileTraceHost::App.slot(), FileTraceHost::Service.slot()),
            "each host has its own slot, so neither can hand its session to the other"
        );
    }

    /// A session is kept only with the watchdog that will stop it.
    ///
    /// A spawn that fails used to be discarded, which left the session running
    /// with nothing to idle it out — and made the replacement path assume a
    /// watchdog that was never there. The rule is now part of the decision, and
    /// a dropped session is reported as no source rather than kept as a leak.
    #[test]
    fn a_session_is_kept_only_with_the_watchdog_that_will_stop_it() {
        let mut slot = None;
        assert!(
            keep_watched(&mut slot, 7u32, || true),
            "a spawned watchdog keeps the session"
        );
        assert_eq!(slot, Some(7), "and the session is in the slot");

        let mut slot = None;
        assert!(
            !keep_watched(&mut slot, 7u32, || false),
            "a session no watchdog can stop must not be kept"
        );
        assert_eq!(slot, None, "the slot is left as it was found");
    }

    /// A failed start must be driven through the production entry: the window
    /// has to be set BY the failure, not stood in for, or nothing tests what a
    /// refusal does to the slot, the window and the snapshot. Exactly one
    /// attempt is made, the answer is `None`, the accounting says unavailable
    /// and every row stays unmeasured — a fabricated zero here is the product
    /// invariant this whole path exists to keep.
    #[test]
    fn a_refused_app_trace_start_is_driven_through_the_entry() {
        let _serial = TEST_OVERRIDE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        remember_app_trace_failure(None);
        let attempts = std::cell::Cell::new(0u32);
        let refuse = |_: super::super::etw::TraceRole| {
            attempts.set(attempts.get() + 1);
            Err(super::super::file_etw::FileTraceRefusal::Unavailable)
        };
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![file_row(r"C:\data\a.bin"), file_row(r"C:\data\b.bin")],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_app_file_io_with(&mut snapshot, std::process::id(), 1, &refuse);
        assert_eq!(attempts.get(), 1, "one start per request");
        for file in &snapshot.files {
            assert_eq!(file.read_bytes, None);
            assert_eq!(file.write_bytes, None);
            assert_eq!(file.read_ops, None);
            assert_eq!(file.write_ops, None);
        }
        let accounting = &snapshot.accounting;
        assert_eq!(
            accounting.state,
            crate::actions::FileIoAccountingState::Unavailable,
            "the dialog keys its explanation off exactly this state"
        );
        assert_eq!(accounting.read_bytes, 0);
        assert_eq!(accounting.write_bytes, 0);
        assert_eq!(accounting.since_ms, 0);
        assert!(
            app_trace_last_failure().is_some(),
            "the refusal is on record, so the window starts here"
        );
        // The next poll is inside the window: no second attempt reaches the
        // kernel, and the answer is still unavailable.
        attach_app_file_io_with(&mut snapshot, std::process::id(), 1, &refuse);
        assert_eq!(attempts.get(), 1, "the window covers the next poll");
        assert_eq!(
            snapshot.accounting.state,
            crate::actions::FileIoAccountingState::Unavailable
        );
        remember_app_trace_failure(None);
    }

    /// The one refusal the snapshot can NAME is another application owning the
    /// NT Kernel Logger.
    ///
    /// Driven through the production app-side entry with a stub start, because
    /// the failure travels: start → answer → refusal → snapshot state → the
    /// dialog's line. Every per-file number stays unmeasured, since there is no
    /// session to measure with and a zero here would be a fabricated count.
    #[test]
    fn a_kernel_logger_owned_elsewhere_reaches_the_snapshot_as_its_own_state() {
        let _serial = TEST_OVERRIDE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        remember_app_trace_failure(None);
        let refuse = |_: super::super::etw::TraceRole| {
            Err(super::super::file_etw::FileTraceRefusal::KernelLoggerInUse)
        };
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![file_row(r"C:\data\a.bin"), file_row(r"C:\data\b.bin")],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_app_file_io_with(&mut snapshot, std::process::id(), 1, &refuse);
        for file in &snapshot.files {
            assert_eq!(file.read_bytes, None, "never a fabricated zero");
            assert_eq!(file.write_bytes, None);
            assert_eq!(file.read_ops, None);
            assert_eq!(file.write_ops, None);
        }
        let accounting = &snapshot.accounting;
        assert_eq!(
            accounting.state,
            crate::actions::FileIoAccountingState::KernelLoggerInUse,
            "the dialog names the owner instead of claiming a silent window"
        );
        assert_eq!(accounting.read_bytes, 0);
        assert_eq!(accounting.write_bytes, 0);
        assert_eq!(accounting.since_ms, 0);
        assert_eq!(accounting.events_lost, None, "no session, no loss count");
        remember_app_trace_failure(None);
    }

    /// The always-on start: a refusal leaves no session and, on the app side,
    /// sets the same retry window a request's refusal does, so an unelevated
    /// GUI asks the kernel once rather than on every start-up path; inside the
    /// window nothing is attempted at all.
    #[test]
    fn an_always_on_start_that_is_refused_keeps_nothing_and_is_throttled() {
        let _serial = TEST_OVERRIDE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        remember_app_trace_failure(None);
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let refuse = |_role: super::super::etw::TraceRole| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(super::super::file_etw::FileTraceRefusal::Unavailable)
        };
        assert!(!ensure_file_trace(FileTraceHost::App, &refuse));
        assert!(FileTraceHost::App.slot().lock().unwrap().is_none());
        assert!(
            app_trace_last_failure().is_some(),
            "the refusal opens the retry window"
        );
        assert!(!ensure_file_trace(FileTraceHost::App, &refuse));
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "inside the window the kernel is not asked again"
        );
        remember_app_trace_failure(None);
    }

    /// A refusal is a refusal even when a dead session still answers it.
    ///
    /// This is the persistent-restart case: the slot holds a session that
    /// stopped, every request tries to replace it, and the replacement is
    /// refused. Keying the window on "no sample came back" would read that as
    /// success and clear the failure on every poll, so the throttle could not
    /// cover a session that keeps dying.
    #[test]
    fn a_refused_replacement_is_recorded_even_though_a_dead_session_answers() {
        let now = std::time::Instant::now();
        let frozen = super::super::file_etw::FileIoSample {
            running: false,
            ..Default::default()
        };
        let refused = FileTraceAnswer {
            sample: Some(frozen.clone()),
            start_refused: true,
            refusal: Some(super::super::file_etw::FileTraceRefusal::Unavailable),
        };
        assert_eq!(
            refused.failure_at(now),
            Some(now),
            "the dead session must not make the refusal look like a success"
        );
        let answered = FileTraceAnswer {
            sample: Some(frozen),
            start_refused: false,
            refusal: None,
        };
        assert_eq!(
            answered.failure_at(now),
            None,
            "a live session clears the window"
        );
    }

    /// A failed start is throttled: an unelevated GUI fails it forever, and the
    /// I/O page polls about once a second, so retrying per poll would spin on a
    /// kernel refusal for as long as the dialog is open.
    #[test]
    fn a_failed_app_trace_start_is_not_retried_on_every_poll() {
        let now = std::time::Instant::now();
        assert!(
            app_trace_start_due(None, now),
            "with no failure on record a start is always allowed"
        );
        assert!(
            !app_trace_start_due(Some(now), now),
            "a start that just failed must not be repeated"
        );
        assert!(
            !app_trace_start_due(Some(now - (APP_FILE_TRACE_RETRY / 2)), now),
            "the window is not over"
        );
        assert!(
            app_trace_start_due(Some(now - APP_FILE_TRACE_RETRY), now),
            "once the window is over, a start is allowed again"
        );
    }

    /// An unelevated GUI cannot host the trace, and that must leave the
    /// snapshot exactly as the handle walk produced it: every per-file number
    /// unavailable (the dialog then explains why), never a fabricated zero.
    ///
    /// The retry window stands in for the failed start so this pins what the
    /// THROTTLED branch returns without waiting out the window; the failure
    /// that sets it is driven through the entry in
    /// `a_refused_app_trace_start_is_driven_through_the_entry`.
    #[test]
    fn an_unavailable_app_trace_leaves_every_row_unmeasured() {
        let _serial = TEST_OVERRIDE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        remember_app_trace_failure(Some(std::time::Instant::now()));
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![file_row(r"C:\data\a.bin"), file_row(r"C:\data\b.bin")],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_app_file_io(&mut snapshot, std::process::id(), 1);
        for file in &snapshot.files {
            assert_eq!(file.read_bytes, None);
            assert_eq!(file.write_bytes, None);
            assert_eq!(file.read_ops, None);
            assert_eq!(file.write_ops, None);
        }
        let accounting = &snapshot.accounting;
        assert_eq!(
            accounting.state,
            crate::actions::FileIoAccountingState::Unavailable,
            "the dialog keys its explanation off exactly this state"
        );
        assert_eq!(accounting.read_bytes, 0);
        assert_eq!(accounting.write_bytes, 0);
        assert_eq!(accounting.unnamed_read_bytes, 0);
        assert_eq!(accounting.omitted_read_bytes, 0);
        // Leave the failure window as it was for the other tests.
        remember_app_trace_failure(None);
    }
}
