//! Privileged loader rundown for processes whose module list cannot be opened.
//!
//! Protected processes such as `csrss.exe` reject ToolHelp and PEB access even
//! for many administrative callers. The kernel's image events still describe
//! every load and unload, and the `DCStart` rundown enumerates images that were
//! already mapped when tracing began. TaskManCore can therefore recover the
//! module inventory without reading the target's address space.
//!
//! The trace is target-scoped: only image events carrying the requested PID are
//! retained. Before a cached list is returned, the PID's creation time is
//! re-read from the kernel process table, so a recycled PID can never inherit
//! a dead process's modules. The first request starts the demand-only session;
//! subsequent Modules-page refreshes receive the completed rundown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tm_core::error::{Result, TmError};
use windows::Win32::System::Diagnostics::Etw::{
    EVENT_RECORD, ImageLoadGuid, SYSTEM_PROCESS_KW_LOADER, SystemProcessProviderGuid,
};

use super::etw::{
    self, LoggerKind, Provider, Session, TraceContext, TraceRole, plausible_client_id,
};
use crate::actions::ProcessModule;

const OPCODE_IMAGE_UNLOAD: u8 = 2;
const OPCODE_IMAGE_DC_START: u8 = 3;
const OPCODE_IMAGE_DC_STOP: u8 = 4;
const OPCODE_IMAGE_LOAD: u8 = 10;

/// A protected process cannot have more images than this. Overflow makes the
/// snapshot unavailable instead of silently returning a partial inventory.
const MAX_TRACKED_MODULES: usize = 8_192;
const MAX_PATH_CHARS: usize = 32_768;

/// The session flushes at one second. A rundown is only exposed after that
/// first interval so a fast secondary request cannot mistake a partly drained
/// `DCStart` sequence for the complete module list.
const RUNDOWN_SETTLE: Duration = Duration::from_secs(1);
const MODULE_TRACE_IDLE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageEvent {
    pid: u32,
    base_address: u64,
    size_bytes: u64,
    path: String,
}

fn read_pointer(payload: &[u8], offset: usize, pointer_bytes: usize) -> Option<u64> {
    match pointer_bytes {
        4 => Some(u64::from(u32::from_le_bytes(
            payload.get(offset..offset + 4)?.try_into().ok()?,
        ))),
        8 => Some(u64::from_le_bytes(
            payload.get(offset..offset + 8)?.try_into().ok()?,
        )),
        _ => None,
    }
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        payload.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_utf16_string(payload: &[u8], offset: usize) -> Option<String> {
    let bytes = payload.get(offset..)?;
    let words = bytes.len() / 2;
    if words == 0 || words > MAX_PATH_CHARS {
        return None;
    }
    let mut wide = Vec::with_capacity(words);
    for index in 0..words {
        let offset = index * 2;
        let unit = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        if unit == 0 {
            return (!wide.is_empty()).then(|| String::from_utf16_lossy(&wide));
        }
        wide.push(unit);
    }
    None
}

/// Decode one classic `Image_Load` record.
///
/// Version 0 is the original load-only layout and has no payload process id;
/// its load event header names the loading process. Version 1 is the compact
/// load-only layout. Versions 2 and 3 carry the same full prefix; v3 replaces
/// v2's reserved `uint32` with signature fields, so their field offsets are
/// identical. Unknown future versions fail closed rather than being guessed.
///
/// `ImageSize` is pointer-sized despite the MOF declaring it `uint32`;
/// Microsoft documents that explicitly, and the offset calculation therefore
/// follows the event architecture.
fn parse_image_event(
    payload: &[u8],
    pointer_bytes: usize,
    version: u8,
    header_pid: u32,
    load: bool,
) -> Option<ImageEvent> {
    if !matches!(pointer_bytes, 4 | 8) {
        return None;
    }
    let base_address = read_pointer(payload, 0, pointer_bytes)?;
    let pid_offset = pointer_bytes.checked_mul(2)?;
    let (size_bytes, pid, path_offset) = match version {
        // Image_V0 is load-only and its payload omits ProcessId. PerfView's
        // standard kernel parser uses the event header PID for this layout.
        0 if load => (
            u64::from(read_u32(payload, pointer_bytes)?),
            header_pid,
            pointer_bytes.checked_add(4)?,
        ),
        1 => (
            read_pointer(payload, pointer_bytes, pointer_bytes)?,
            read_u32(payload, pid_offset)?,
            pid_offset.checked_add(4)?,
        ),
        2 | 3 => (
            read_pointer(payload, pointer_bytes, pointer_bytes)?,
            read_u32(payload, pid_offset)?,
            // Checksum, timestamp, signature/reserved, pointer-sized default
            // base, and four reserved DWORDs follow the process id.
            pid_offset.checked_add(pointer_bytes)?.checked_add(32)?,
        ),
        _ => return None,
    };
    if base_address == 0 || size_bytes == 0 {
        return None;
    }
    if !plausible_client_id(pid) {
        return None;
    }
    let path = read_utf16_string(payload, path_offset)?;
    Some(ImageEvent {
        pid,
        base_address,
        size_bytes,
        path,
    })
}

fn image_event_pid(
    payload: &[u8],
    pointer_bytes: usize,
    version: u8,
    header_pid: u32,
    load: bool,
) -> Option<u32> {
    if !matches!(pointer_bytes, 4 | 8) {
        return None;
    }
    if version == 0 {
        return load.then_some(header_pid);
    }
    // Every observed non-v0 layout keeps ProcessId immediately after the two
    // pointer-sized base/size fields. This lets malformed records from another
    // process be ignored without letting a malformed target record pass.
    read_u32(payload, pointer_bytes.checked_mul(2)?)
}

fn image_path(path: &str) -> String {
    super::image_path::etw_dos_path(path)
}

fn image_name(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[derive(Default)]
struct State {
    modules: HashMap<u64, ProcessModule>,
    events_seen: u64,
    events_decoded: u64,
    rejected_payload_len: u16,
    rejected_version: Option<u8>,
    target_decode_failures: u64,
    overflowed: bool,
    logged_incomplete: bool,
}

impl State {
    fn record(
        &mut self,
        target_pid: u32,
        event: Option<ImageEvent>,
        pid: Option<u32>,
        payload_len: u16,
        version: u8,
        load: bool,
    ) {
        self.events_seen = self.events_seen.saturating_add(1);
        let Some(pid) = pid else {
            self.target_decode_failures = self.target_decode_failures.saturating_add(1);
            self.rejected_payload_len = payload_len;
            self.rejected_version = Some(version);
            return;
        };
        if pid != target_pid {
            return;
        }
        let Some(event) = event else {
            self.target_decode_failures = self.target_decode_failures.saturating_add(1);
            self.rejected_payload_len = payload_len;
            self.rejected_version = Some(version);
            return;
        };
        self.events_decoded = self.events_decoded.saturating_add(1);
        debug_assert_eq!(event.pid, target_pid);
        if load {
            if !self.modules.contains_key(&event.base_address)
                && self.modules.len() >= MAX_TRACKED_MODULES
            {
                self.overflowed = true;
                return;
            }
            let path = image_path(&event.path);
            self.modules.insert(
                event.base_address,
                ProcessModule {
                    name: image_name(&path),
                    path,
                    base_address: event.base_address,
                    size_bytes: event.size_bytes,
                },
            );
        } else {
            self.modules.remove(&event.base_address);
        }
    }

    fn snapshot(&mut self, events_lost: Option<u64>) -> Option<Vec<ProcessModule>> {
        let incomplete = self.overflowed
            || self.modules.is_empty()
            || self.events_decoded == 0
            || self.target_decode_failures != 0
            || events_lost != Some(0);
        if incomplete {
            if !self.logged_incomplete {
                tracing::warn!(
                    events_seen = self.events_seen,
                    events_decoded = self.events_decoded,
                    rejected_payload_len = self.rejected_payload_len,
                    rejected_version = self.rejected_version,
                    target_decode_failures = self.target_decode_failures,
                    events_lost,
                    modules = self.modules.len(),
                    overflowed = self.overflowed,
                    "protected module rundown is incomplete"
                );
                self.logged_incomplete = true;
            }
            return None;
        }
        let mut modules: Vec<ProcessModule> = self.modules.values().cloned().collect();
        modules.sort_by_key(|module| module.base_address);
        Some(modules)
    }
}

struct Shared {
    target_pid: u32,
    state: Mutex<State>,
    live: AtomicBool,
}

impl TraceContext for Shared {
    fn stop(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

impl Shared {
    fn record(
        &self,
        event: Option<ImageEvent>,
        pid: Option<u32>,
        payload_len: u16,
        version: u8,
        load: bool,
    ) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.record(self.target_pid, event, pid, payload_len, version, load);
        }
    }
}

struct ModuleTrace {
    session: Session<Shared>,
    started_at: Instant,
}

impl ModuleTrace {
    fn start(role: TraceRole, pid: u32) -> Option<Self> {
        let shared = Arc::new(Shared {
            target_pid: pid,
            state: Mutex::new(State::default()),
            live: AtomicBool::new(true),
        });
        let session = Session::start(
            session_name(role),
            LoggerKind::SystemLoader,
            &[Provider {
                guid: SystemProcessProviderGuid,
                match_any_keyword: SYSTEM_PROCESS_KW_LOADER,
            }],
            shared,
            on_event,
            "tm-module-etw",
        )?;
        tracing::info!(pid, "protected-process module rundown started");
        Some(Self {
            session,
            started_at: Instant::now(),
        })
    }

    fn is_running(&self) -> bool {
        self.session.is_running()
    }

    fn rundown_settled(&self) -> bool {
        self.started_at.elapsed() >= RUNDOWN_SETTLE
    }

    fn snapshot(&self) -> Option<Vec<ProcessModule>> {
        // Hold the callback-state lock while querying loss. Samples cannot be
        // inserted between those two reads, and a non-zero or unqueryable loss
        // count cannot be mistaken for a complete loader rundown.
        let mut state = match self.session.shared().state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let events_lost = self.session.events_lost();
        state.snapshot(events_lost)
    }
}

fn pointer_bytes(record: &EVENT_RECORD) -> usize {
    const EVENT_HEADER_FLAG_32_BIT_HEADER: u16 = 0x0020;
    if record.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
        4
    } else {
        8
    }
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(shared) = (unsafe { etw::context_of::<Shared>(record) }) else {
        return;
    };
    if record.EventHeader.ProviderId != ImageLoadGuid {
        return;
    }
    let load = match record.EventHeader.EventDescriptor.Opcode {
        OPCODE_IMAGE_LOAD | OPCODE_IMAGE_DC_START => true,
        OPCODE_IMAGE_UNLOAD | OPCODE_IMAGE_DC_STOP => false,
        _ => return,
    };
    let version = record.EventHeader.EventDescriptor.Version;
    let (pid, event) = unsafe { etw::payload_of(record) }.map_or((None, None), |payload| {
        let pointer_bytes = pointer_bytes(record);
        let pid = image_event_pid(
            payload,
            pointer_bytes,
            version,
            record.EventHeader.ProcessId,
            load,
        );
        // The rundown covers every image on the machine. Decode paths only for
        // the one target process; decoding all ~20k paths made the consumer
        // itself a bottleneck and delayed the target's initial DCStart batch.
        let event = if pid == Some(shared.target_pid) {
            parse_image_event(
                payload,
                pointer_bytes,
                version,
                record.EventHeader.ProcessId,
                load,
            )
        } else {
            None
        };
        (pid, event)
    });
    shared.record(event, pid, record.UserDataLength, version, load);
}

struct ActiveTrace {
    trace: ModuleTrace,
    role: TraceRole,
    pid: u32,
    expected_start_epoch_s: i64,
    generation: u64,
    last_request: Instant,
}

fn active_trace() -> &'static Mutex<Option<ActiveTrace>> {
    static SLOT: std::sync::OnceLock<Mutex<Option<ActiveTrace>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(crate) fn trace_is_running() -> bool {
    match active_trace().lock() {
        Ok(slot) => slot
            .as_ref()
            .is_some_and(|active| active.trace.is_running()),
        Err(poisoned) => poisoned
            .into_inner()
            .as_ref()
            .is_some_and(|active| active.trace.is_running()),
    }
}

static TRACE_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) enum ModuleRundown {
    Ready(Vec<ProcessModule>),
    Pending,
    Unavailable,
}

/// Return a complete loader rundown for an exact process, or `None` while the
/// first rundown is still being collected. The caller never receives an empty
/// module list in either non-ready state.
pub(crate) fn protected_process_modules(
    role: TraceRole,
    pid: u32,
    expected_start_epoch_s: i64,
) -> Result<ModuleRundown> {
    if super::cpu_load::current_creation_epoch(pid) != Some(expected_start_epoch_s) {
        return Err(TmError::ProcessNotFound { pid });
    }
    let mut slot = match active_trace().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    let same_target = slot.as_ref().is_some_and(|active| {
        active.role == role
            && active.pid == pid
            && active.expected_start_epoch_s == expected_start_epoch_s
            && active.trace.is_running()
    });
    if !same_target {
        *slot = None;
        let Some(trace) = ModuleTrace::start(role, pid) else {
            return Ok(ModuleRundown::Unavailable);
        };
        let generation = TRACE_GENERATION.fetch_add(1, Ordering::Relaxed);
        *slot = Some(ActiveTrace {
            trace,
            role,
            pid,
            expected_start_epoch_s,
            generation,
            last_request: Instant::now(),
        });
        spawn_trace_watchdog(generation);
        return Ok(ModuleRundown::Pending);
    }
    let active = slot.as_mut().expect("same_target checked above");
    active.last_request = Instant::now();
    if !active.trace.rundown_settled() {
        return Ok(ModuleRundown::Pending);
    }
    let snapshot = active.trace.snapshot();
    // Revalidate after reading the cache and immediately before it crosses the
    // privileged broker boundary. A PID recycled during the rundown cannot
    // receive the previous process's module list.
    if super::cpu_load::current_creation_epoch(pid) != Some(expected_start_epoch_s) {
        *slot = None;
        return Err(TmError::ProcessNotFound { pid });
    }
    Ok(match snapshot {
        Some(modules) => ModuleRundown::Ready(modules),
        None => ModuleRundown::Unavailable,
    })
}

fn spawn_trace_watchdog(generation: u64) {
    let _ = std::thread::Builder::new()
        .name("tm-module-idle".into())
        .spawn(move || {
            loop {
                let wait = {
                    let mut slot = match active_trace().lock() {
                        Ok(slot) => slot,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let Some(active) = slot.as_ref() else {
                        return;
                    };
                    if active.generation != generation {
                        return;
                    }
                    let idle = active.last_request.elapsed();
                    if idle >= MODULE_TRACE_IDLE {
                        *slot = None;
                        tracing::info!("protected-process module rundown stopped (idle)");
                        return;
                    }
                    MODULE_TRACE_IDLE - idle
                };
                std::thread::sleep(wait);
            }
        });
}

fn session_name(role: TraceRole) -> &'static str {
    match role {
        TraceRole::Service => "TaskMan-Modules-Service",
        TraceRole::App => "TaskMan-Modules-App",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_payload_with_layout(
        pointer_bytes: usize,
        version: u8,
        base: u64,
        size: u64,
        pid: u32,
        path: &str,
    ) -> Vec<u8> {
        assert!(matches!(version, 0..=3));
        let pid_offset = pointer_bytes * 2;
        let path_offset = match version {
            0 => pointer_bytes + 4,
            1 => pid_offset + 4,
            2 | 3 => pid_offset + pointer_bytes + 32,
            _ => unreachable!(),
        };
        let mut payload = vec![0u8; path_offset];
        if version == 0 {
            if pointer_bytes == 8 {
                payload[0..8].copy_from_slice(&base.to_le_bytes());
            } else {
                payload[0..4].copy_from_slice(&(base as u32).to_le_bytes());
            }
            payload[pointer_bytes..pointer_bytes + 4].copy_from_slice(&(size as u32).to_le_bytes());
        } else if pointer_bytes == 8 {
            payload[0..8].copy_from_slice(&base.to_le_bytes());
            payload[8..16].copy_from_slice(&size.to_le_bytes());
        } else {
            payload[0..4].copy_from_slice(&(base as u32).to_le_bytes());
            payload[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        }
        if version != 0 {
            payload[pid_offset..pid_offset + 4].copy_from_slice(&pid.to_le_bytes());
        }
        for unit in path.encode_utf16().chain([0]) {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
        payload
    }

    #[test]
    fn image_layout_decodes_both_architectures() {
        let path = r"\Device\HarddiskVolume3\Windows\System32\ntdll.dll";
        for pointer_bytes in [4, 8] {
            for version in 0..=3 {
                let payload = image_payload_with_layout(
                    pointer_bytes,
                    version,
                    0x7FF0_0000_1000,
                    0x20_0000,
                    1_208,
                    path,
                );
                assert_eq!(
                    parse_image_event(&payload, pointer_bytes, version, 1_208, true),
                    Some(ImageEvent {
                        pid: 1_208,
                        base_address: if pointer_bytes == 8 {
                            0x7FF0_0000_1000
                        } else {
                            0x1000
                        },
                        size_bytes: 0x20_0000,
                        path: path.into(),
                    }),
                    "{pointer_bytes}-byte pointers, version {version}"
                );
            }
        }
    }

    #[test]
    fn truncated_and_implausible_image_records_are_rejected() {
        assert_eq!(parse_image_event(&[], 8, 3, 100, true), None);
        let mut payload = image_payload_with_layout(8, 3, 0, 100, 100, r"\Device\X\a.dll");
        assert_eq!(parse_image_event(&payload, 8, 3, 100, true), None);
        payload[0..8].copy_from_slice(&0x1000u64.to_le_bytes());
        payload[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(parse_image_event(&payload, 8, 3, 100, true), None);
        let mut odd_pid = image_payload_with_layout(8, 3, 0x1000, 100, 100, r"\Device\X\a.dll");
        odd_pid[16..20].copy_from_slice(&101u32.to_le_bytes());
        assert_eq!(parse_image_event(&odd_pid, 8, 3, 100, true), None);
        let v0 = image_payload_with_layout(8, 0, 0x1000, 100, 100, r"\Device\X\a.dll");
        assert_eq!(
            parse_image_event(&v0, 8, 0, 100, true)
                .expect("v0 load uses the event header PID")
                .pid,
            100
        );
        assert_eq!(
            parse_image_event(&v0, 8, 0, 100, false),
            None,
            "v0 has no PID for an unload/rundown record"
        );
        let valid = image_payload_with_layout(8, 3, 0x1000, 100, 100, r"\Device\X\a.dll");
        assert_eq!(
            parse_image_event(&valid, 8, 4, 100, true),
            None,
            "an unknown payload version must fail closed"
        );
    }

    #[test]
    fn target_loads_and_unloads_build_the_module_map() {
        let mut state = State::default();
        let image = |base_address, path: &str| ImageEvent {
            pid: 42,
            base_address,
            size_bytes: 0x1000,
            path: path.into(),
        };
        state.record(
            42,
            Some(image(0x1000, r"C:\Windows\a.dll")),
            Some(42),
            0,
            3,
            true,
        );
        state.record(
            42,
            Some(image(0x2000, r"C:\Windows\b.dll")),
            Some(42),
            0,
            3,
            true,
        );
        state.record(
            42,
            Some(image(0x3000, r"C:\Windows\other.dll")),
            Some(84),
            0,
            3,
            true,
        );
        let modules = state.snapshot(Some(0)).expect("modules");
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].name, "a.dll");
        assert_eq!(modules[1].name, "b.dll");
        state.record(
            42,
            Some(image(0x1000, r"C:\Windows\a.dll")),
            Some(42),
            0,
            3,
            false,
        );
        let modules = state.snapshot(Some(0)).expect("remaining module");
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].base_address, 0x2000);
    }

    #[test]
    fn unrelated_undecodable_records_do_not_poison_a_target_rundown() {
        let mut state = State::default();
        state.record(42, None, Some(84), 9, 4, true);
        assert_eq!(state.snapshot(Some(0)), None);
        state.record(
            42,
            Some(ImageEvent {
                pid: 42,
                base_address: 0x1000,
                size_bytes: 0x1000,
                path: r"C:\Windows\a.dll".into(),
            }),
            Some(42),
            0,
            3,
            true,
        );
        assert_eq!(
            state.snapshot(Some(0)).map(|modules| modules.len()),
            Some(1)
        );
        assert_eq!(state.target_decode_failures, 0);
    }

    #[test]
    fn a_target_record_that_cannot_be_decoded_keeps_the_rundown_unavailable() {
        let mut state = State::default();
        state.record(
            42,
            Some(ImageEvent {
                pid: 42,
                base_address: 0x1000,
                size_bytes: 0x1000,
                path: r"C:\Windows\a.dll".into(),
            }),
            Some(42),
            0,
            3,
            true,
        );
        state.record(42, None, Some(42), 9, 4, true);
        assert_eq!(state.snapshot(Some(0)), None);
        assert_eq!(state.rejected_version, Some(4));
        assert_eq!(state.target_decode_failures, 1);

        let mut unknown_pid = State::default();
        unknown_pid.record(42, None, None, 8, 3, true);
        assert_eq!(unknown_pid.snapshot(Some(0)), None);
        assert_eq!(unknown_pid.target_decode_failures, 1);
    }

    #[test]
    fn event_loss_or_an_unqueryable_loss_count_cannot_produce_a_partial_list() {
        let mut state = State::default();
        state.record(
            42,
            Some(ImageEvent {
                pid: 42,
                base_address: 0x1000,
                size_bytes: 0x1000,
                path: r"C:\Windows\a.dll".into(),
            }),
            Some(42),
            0,
            3,
            true,
        );
        assert_eq!(state.snapshot(None), None);
        assert_eq!(state.snapshot(Some(1)), None);
        assert_eq!(
            state.snapshot(Some(0)).map(|modules| modules.len()),
            Some(1)
        );
    }

    #[test]
    fn every_module_session_name_is_known_to_crash_teardown() {
        for role in [TraceRole::Service, TraceRole::App] {
            let name = session_name(role);
            assert!(crate::win::etw::SESSION_NAMES.contains(&name));
            assert!(name.contains("Modules"));
        }
        assert_ne!(
            session_name(TraceRole::Service),
            session_name(TraceRole::App)
        );
    }
}
