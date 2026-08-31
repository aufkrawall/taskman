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
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
    LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
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
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_ACCESS_RIGHTS,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::actions::{
    Capabilities, CoreServiceState, PlatformActions, ProcessModule, ServiceAction,
    TaskManagerReplacementState, UserSessionAction,
};

pub const SERVICE_NAME: &str = "TaskmanCore";
pub const SERVICE_DISPLAY_NAME: &str = "TaskMan Core Service";
pub const SERVICE_EXE_NAME: &str = "taskman-service.exe";
pub const GUI_EXE_NAME: &str = "taskman.exe";
pub const SERVICE_LOG_FILE_PREFIX: &str = "taskman-service.log";
pub const PROTOCOL_VERSION: u16 = 1;

const PIPE_NAME: &str = r"\\.\pipe\Taskman.Core.v1";
const FRAME_MAGIC: [u8; 4] = *b"TMB1";
const FRAME_REQUEST: u16 = 1;
const FRAME_RESPONSE: u16 = 2;
const FRAME_HEADER_LEN: usize = 12;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const WORKER_COUNT: usize = 2;
const WORK_QUEUE_CAP: usize = 16;
// Two active workers + the full queue + one listening/rejection instance.
const PIPE_INSTANCE_CAP: u32 = (WORKER_COUNT + WORK_QUEUE_CAP + 1) as u32;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum BrokerValue {
    Pong { version: String },
    Unit,
    AffinityMask(u64),
    TaskManagerReplacementState(TaskManagerReplacementState),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerResponse {
    protocol_version: u16,
    value: std::result::Result<BrokerValue, String>,
}

#[derive(Debug)]
enum BrokerCallError {
    Unavailable(String),
    Rejected(String),
}

impl std::fmt::Display for BrokerCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(formatter, "{detail}"),
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

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
}

fn security_descriptor(sddl: &str) -> Result<SecurityDescriptor> {
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

fn validate_manifest(manifest: &BrokerManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA
        || manifest.protocol_version != PROTOCOL_VERSION
        || !valid_sid_text(&manifest.authorized_user_sid)
    {
        return Err(TmError::platform(
            "broker manifest",
            "unsupported schema/protocol or invalid SID",
        ));
    }
    let own_exe = std::env::current_exe()?;
    if !paths_match(&own_exe, &manifest.service_path) {
        return Err(TmError::platform(
            "broker manifest",
            "service is not running from its protected installed path",
        ));
    }
    if !paths_match(&expected_installed_gui_path()?, &manifest.gui_path)
        || !paths_match(&expected_installed_service_path()?, &manifest.service_path)
    {
        return Err(TmError::platform(
            "broker manifest",
            "binary paths do not match the protected install location",
        ));
    }
    let service_hash = sha256_file(&manifest.service_path)?;
    let gui_hash = sha256_file(&manifest.gui_path)?;
    if !service_hash.eq_ignore_ascii_case(&manifest.service_sha256)
        || !gui_hash.eq_ignore_ascii_case(&manifest.gui_sha256)
    {
        return Err(TmError::platform(
            "broker manifest",
            "installed binary hash mismatch",
        ));
    }
    Ok(())
}

fn load_manifest() -> Result<BrokerManifest> {
    let path = manifest_path()?;
    let mut file = pinned_source(&path)?;
    let length = file.metadata()?.len();
    if length > 64 * 1024 {
        return Err(TmError::platform(
            "broker manifest",
            "manifest is too large",
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)?;
    let manifest: BrokerManifest = serde_json::from_slice(&bytes)?;
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
            .map_err(|error| BrokerCallError::Rejected(error.to_string()))?;
        write_frame(&mut pipe, FRAME_REQUEST, &payload, MAX_REQUEST_BYTES).map_err(|error| {
            BrokerCallError::Rejected(format!(
                "request delivery failed; action state is unknown: {error}"
            ))
        })?;
        let response =
            read_frame(&mut pipe, FRAME_RESPONSE, MAX_RESPONSE_BYTES).map_err(|error| {
                BrokerCallError::Rejected(format!(
                    "service response was lost; action state is unknown: {error}"
                ))
            })?;
        let response: BrokerResponse = serde_json::from_slice(&response)
            .map_err(|error| BrokerCallError::Rejected(error.to_string()))?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(BrokerCallError::Rejected(
                "service protocol version changed during request".into(),
            ));
        }
        response.value.map_err(BrokerCallError::Rejected)
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
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }

    fn value_or_local<T>(
        &self,
        request: BrokerRequest,
        decode: impl FnOnce(BrokerValue) -> Option<T>,
        local: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        match self.client.call(request) {
            Ok(value) => decode(value)
                .ok_or_else(|| TmError::platform("core service", "unexpected response type")),
            Err(BrokerCallError::Unavailable(_)) => local(),
            Err(BrokerCallError::Rejected(detail)) => {
                Err(TmError::platform("core service", detail))
            }
        }
    }
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
        )
    }

    fn list_process_modules(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
    ) -> Result<Vec<ProcessModule>> {
        // Module inventory is telemetry, not a privileged action. Keeping it
        // in the user process prevents the LocalSystem broker from becoming a
        // cross-session information oracle. Exact unload requests are still
        // re-enumerated and validated inside the broker before execution.
        self.local.list_process_modules(pid, expected_start_epoch_s)
    }

    fn unload_process_module(
        &self,
        pid: u32,
        expected_start_epoch_s: Option<i64>,
        base_address: u64,
        expected_path: &str,
    ) -> Result<()> {
        let Some(start_epoch_s) = expected_start_epoch_s else {
            return self
                .local
                .unload_process_module(pid, None, base_address, expected_path);
        };
        self.unit_or_local(
            BrokerRequest::UnloadModule {
                pid,
                expected_start_epoch_s: Some(start_epoch_s),
                base_address,
                expected_path: expected_path.to_string(),
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

    fn run_new_task(&self, command_line: &str, elevate: bool) -> Result<()> {
        self.local.run_new_task(command_line, elevate)
    }

    fn run_new_task_probe(&self, command_line: &str, elevate: bool) -> Result<()> {
        self.local.run_new_task_probe(command_line, elevate)
    }

    fn relaunch_elevated(&self) -> Result<()> {
        self.local.relaunch_elevated()
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

    fn set_start_with_windows(&self, enabled: bool, start_minimized: bool) -> Result<()> {
        self.local.set_start_with_windows(enabled, start_minimized)
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
    ) -> Result<()> {
        // A path must never become a SYSTEM write primitive. The GUI's user
        // token owns this operation until handle transfer is implemented.
        self.local
            .create_dump_file(pid, expected_start_epoch_s, path)
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
    let handle = match open() {
        Ok(handle) => handle,
        Err(error) if error.code() == ERROR_PIPE_BUSY.to_hresult() => {
            let waited = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 250) };
            if !waited.as_bool() {
                return Err(BrokerCallError::Unavailable(
                    "core service is busy or stopped".into(),
                ));
            }
            open().map_err(|error| BrokerCallError::Unavailable(error.to_string()))?
        }
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() || error.code().0 == 5 => {
            return Err(BrokerCallError::Unavailable(error.to_string()));
        }
        Err(error) => return Err(BrokerCallError::Unavailable(error.to_string())),
    };
    Ok(unsafe { File::from_raw_handle(handle.0) })
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

fn create_pipe_instance(sddl: &str, first: bool) -> Result<File> {
    let descriptor = security_descriptor(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    let name = wide(PIPE_NAME);
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
        return Err(TmError::platform(
            "CreateNamedPipeW",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn connect_pipe(pipe: &File) -> Result<()> {
    match unsafe { ConnectNamedPipe(HANDLE(pipe.as_raw_handle()), None) } {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
        Err(error) => Err(TmError::platform("ConnectNamedPipe", error.to_string())),
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
    if expected_start_epoch_s.is_none_or(|created| created <= 0) {
        return Err(TmError::platform(
            "broker target",
            "a valid process creation time is required",
        ));
    }
    if pid <= 4 || pid == std::process::id() || pid == requesting_gui_pid {
        return Err(TmError::platform(
            "broker target",
            "system, broker, and requesting GUI processes are protected",
        ));
    }
    super::process_ops::refuse_critical_process(pid)
}

fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b' '))
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
            actions.unload_process_module(
                pid,
                expected_start_epoch_s,
                base_address,
                &expected_path,
            )?;
            Ok(BrokerValue::Unit)
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

    let mut first = true;
    let mut on_ready = Some(on_ready);
    while !stopped.load(std::sync::atomic::Ordering::Acquire) {
        let pipe = create_pipe_instance(&sddl, first)?;
        if let Some(on_ready) = on_ready.take() {
            // Report SERVICE_RUNNING only after the manifest, ACL, workers,
            // and first listening pipe have all been established.
            on_ready()?;
        }
        first = false;
        {
            let (state, changed) = &*listener_ready;
            *tm_core::sync::lock(state) = true;
            changed.notify_one();
        }
        let connected = connect_pipe(&pipe);
        *tm_core::sync::lock(&listener_ready.0) = false;
        connected?;
        if stopped.load(std::sync::atomic::Ordering::Acquire) {
            break;
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
    if let Ok(BrokerValue::Pong { version }) = client.call(BrokerRequest::Ping) {
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
                } else {
                    CoreServiceState::Degraded(
                        "service is running but broker authentication failed".into(),
                    )
                }
            }
            ServiceState::StartPending | ServiceState::ContinuePending => {
                CoreServiceState::Starting
            }
            _ => CoreServiceState::Stopped,
        },
        Err(error) => CoreServiceState::Degraded(error.to_string()),
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
/// Existing entries are accepted only as ordinary files and have their ACLs
/// re-applied; a reparse point or nested directory disables file logging
/// instead of turning the logger into an arbitrary privileged writer.
pub fn prepare_service_log_dir() -> Result<PathBuf> {
    let data = service_data_dir()?;
    ensure_secure_directory(&data, SERVICE_DATA_SDDL)?;
    let logs = data.join("logs");
    ensure_secure_directory(&logs, SERVICE_DATA_SDDL)?;
    for entry in std::fs::read_dir(&logs)? {
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
    Ok(logs)
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

fn current_user_sid() -> Result<String> {
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

fn set_required_service_privileges(service: &windows_service::service::Service) -> Result<()> {
    use windows::Win32::System::Services::{
        ChangeServiceConfig2W, SC_HANDLE, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
        SERVICE_REQUIRED_PRIVILEGES_INFOW,
    };
    let mut privileges: Vec<u16> = "SeDebugPrivilege".encode_utf16().chain([0, 0]).collect();
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
    let pid = status.process_id.ok_or_else(|| {
        TmError::platform("upgrade core service", "running service has no process id")
    })?;
    let process = unsafe { OpenProcess(PROCESS_ACCESS_RIGHTS(0x0010_0000), false, pid) }
        .map_err(|error| TmError::platform("open core service process", error.to_string()))?;

    let stop_result = if status.current_state == ServiceState::Running {
        service
            .stop()
            .map(|_| ())
            .map_err(|error| TmError::platform("stop core service for upgrade", error.to_string()))
    } else {
        Ok(())
    };
    if let Err(error) = stop_result {
        unsafe {
            let _ = CloseHandle(process);
        }
        return Err(error);
    }

    let wait = unsafe { WaitForSingleObject(process, 20_000) };
    unsafe {
        let _ = CloseHandle(process);
    }
    if wait == WAIT_OBJECT_0 {
        Ok(())
    } else if wait == WAIT_TIMEOUT {
        Err(TmError::platform(
            "upgrade core service",
            "timed out waiting for the old service process to exit",
        ))
    } else {
        Err(TmError::platform(
            "upgrade core service",
            std::io::Error::last_os_error().to_string(),
        ))
    }
}

fn stop_existing_service_before_copy() -> Result<()> {
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

fn install(authorized_user_sid: &str) -> Result<()> {
    ensure_elevated("install core service")?;
    if !valid_sid_text(authorized_user_sid) {
        return Err(TmError::platform(
            "install core service",
            "the originating GUI user SID is malformed",
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
    service
        .set_delayed_auto_start(true)
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
    use windows_service::service::{ServiceAccess, ServiceState};
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
    if let Ok(status) = service.query_status()
        && !matches!(
            status.current_state,
            ServiceState::Stopped | ServiceState::StopPending
        )
    {
        service
            .stop()
            .map_err(|error| TmError::platform("stop core service", error.to_string()))?;
    }
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
}
