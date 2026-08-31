//! Per-process network bytes from an ETW real-time session.
//!
//! Windows exposes no per-process byte counter through a plain query API; the
//! numbers Task Manager shows come from kernel network events. This module
//! runs a private real-time session on `Microsoft-Windows-Kernel-Network` and
//! accumulates the `size` field of the TCP/UDP data events per payload PID.
//!
//! ## Things that are easy to get wrong here
//!
//! * **The PID must come from the PAYLOAD, not the event header.** Kernel
//!   network events are emitted from arbitrary (often System) context, so
//!   `EventHeader.ProcessId` is not the owner of the traffic. Every one of the
//!   data events starts with `PID: u32` followed by `size: u32`, for TCP and
//!   UDP and for both address families — which is exactly, and only, what we
//!   read. The rest of each payload differs per event and is ignored.
//! * **Starting a session needs administrator rights** (or membership in
//!   "Performance Log Users"). When that fails the monitor stays inactive and
//!   the snapshot keeps reporting `None`, which the UI renders as "—". It must
//!   NEVER fall back to reporting zero bytes: that would be a fabricated
//!   measurement (core product invariant).
//! * **A session outlives its process if it is not stopped.** The name is
//!   per-PID and a stale session with the same name is stopped and restarted,
//!   so a previous crash cannot permanently wedge the feature.
//! * **`ProcessTrace` blocks** until the session is stopped, so it owns a
//!   dedicated thread and never runs on the sampler.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW,
    WNODE_FLAG_TRACED_GUID,
};
use windows::core::{GUID, PCWSTR, PWSTR};

/// `Microsoft-Windows-Kernel-Network`.
const KERNEL_NETWORK_GUID: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);

/// Informational level; the data events are emitted at this level.
const TRACE_LEVEL_INFORMATION: u8 = 4;

/// Invalid real-time processing handle, as returned by `OpenTraceW` on
/// failure. `PROCESSTRACE_HANDLE` is a plain u64 in the Win32 headers.
const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;

/// Direction of one kernel network data event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Sent,
    Received,
}

/// Map an event id of `Microsoft-Windows-Kernel-Network` to a traffic
/// direction. Only the *data* events carry byte counts; connect/disconnect/
/// retransmit/ACK bookkeeping events are deliberately ignored so bytes are
/// never counted twice.
fn direction_of(event_id: u16) -> Option<Direction> {
    match event_id {
        // TCP v4 / TCP v6 / UDP v4 / UDP v6 "Datasent"
        10 | 26 | 42 | 58 => Some(Direction::Sent),
        // ...and the matching "Datareceived"
        11 | 27 | 43 | 59 => Some(Direction::Received),
        _ => None,
    }
}

/// The `(pid, size)` prefix shared by every kernel network data event.
fn parse_pid_and_size(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let size = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    Some((pid, size))
}

/// Cumulative bytes observed for one process since the session started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidBytes {
    pub received: u64,
    pub sent: u64,
}

/// Shared state between the ETW callback thread and the sampler.
struct Shared {
    totals: Mutex<HashMap<u32, PidBytes>>,
    /// Cleared when the session is torn down so a late callback cannot touch
    /// a map the sampler has already stopped reading.
    live: AtomicBool,
}

impl Shared {
    fn record(&self, pid: u32, size: u32, dir: Direction) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut totals) = self.totals.lock() else {
            return;
        };
        let entry = totals.entry(pid).or_default();
        match dir {
            Direction::Sent => entry.sent = entry.sent.saturating_add(u64::from(size)),
            Direction::Received => entry.received = entry.received.saturating_add(u64::from(size)),
        }
    }
}

/// A running per-process network trace.
pub struct NetworkUsage {
    shared: Arc<Shared>,
    /// Raw pointer handed to the ETW callback; reclaimed on teardown.
    context: *const Shared,
    session: CONTROLTRACE_HANDLE,
    trace: PROCESSTRACE_HANDLE,
    session_name: Vec<u16>,
    worker: Option<std::thread::JoinHandle<()>>,
}

// The raw context pointer is only dereferenced by the ETW callback, which is
// alive exactly between `start` and `Drop`; nothing else touches it.
unsafe impl Send for NetworkUsage {}

impl NetworkUsage {
    /// Start a real-time session. Returns `None` when ETW is unavailable to
    /// this token (the common unelevated case), which keeps per-process
    /// network reported as unknown rather than zero.
    pub fn start() -> Option<Self> {
        let name = session_name();
        let shared = Arc::new(Shared {
            totals: Mutex::new(HashMap::new()),
            live: AtomicBool::new(true),
        });

        let (session, mut properties) = start_session(&name)?;

        let enable = unsafe {
            EnableTraceEx2(
                session,
                &KERNEL_NETWORK_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                TRACE_LEVEL_INFORMATION,
                // MatchAnyKeyword 0 means "every event of this provider";
                // `direction_of` does the real filtering.
                0,
                0,
                0,
                None,
            )
        };
        if enable != ERROR_SUCCESS {
            tracing::debug!(error = enable.0, "kernel-network provider not enabled");
            stop_session(session, &name, &mut properties);
            return None;
        }

        // The callback owns a strong reference for as long as the trace runs.
        let context = Arc::into_raw(Arc::clone(&shared));
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_ptr() as *mut u16),
            Anonymous1: windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_LOGFILEW_0 {
                ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: windows::Win32::System::Diagnostics::Etw::EVENT_TRACE_LOGFILEW_1 {
                EventRecordCallback: Some(on_event),
            },
            Context: context as *mut core::ffi::c_void,
            ..Default::default()
        };
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == INVALID_PROCESSTRACE_HANDLE {
            tracing::debug!("OpenTraceW failed for the per-process network session");
            stop_session(session, &name, &mut properties);
            // Reclaim the reference the callback would have owned.
            drop(unsafe { Arc::from_raw(context) });
            return None;
        }

        let worker = std::thread::Builder::new()
            .name("tm-net-etw".into())
            .spawn(move || {
                // Blocks until the session is stopped; the return code is not
                // actionable (a stopped session reports "cancelled").
                let _ = unsafe { ProcessTrace(&[trace], None, None) };
            })
            .ok();
        if worker.is_none() {
            stop_session(session, &name, &mut properties);
            let _ = unsafe { CloseTrace(trace) };
            drop(unsafe { Arc::from_raw(context) });
            return None;
        }

        tracing::info!("per-process network trace started");
        Some(Self {
            shared,
            context,
            session,
            trace,
            session_name: name,
            worker,
        })
    }

    /// Current cumulative byte counters, pruned to the processes that still
    /// exist. Pruning here bounds the map and stops a recycled PID from
    /// inheriting a dead process's totals across a gap.
    pub fn totals_pruned(
        &self,
        live_pids: &std::collections::HashSet<u32>,
    ) -> HashMap<u32, PidBytes> {
        let Ok(mut totals) = self.shared.totals.lock() else {
            return HashMap::new();
        };
        totals.retain(|pid, _| live_pids.contains(pid));
        totals.clone()
    }
}

impl Drop for NetworkUsage {
    fn drop(&mut self) {
        // Order matters: stop the session so `ProcessTrace` returns, then
        // close the consumer and join before the context is reclaimed.
        self.shared.live.store(false, Ordering::Relaxed);
        let mut properties = properties_buffer(&self.session_name);
        stop_session(self.session, &self.session_name, &mut properties);
        let _ = unsafe { CloseTrace(self.trace) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        drop(unsafe { Arc::from_raw(self.context) });
    }
}

/// ETW record callback. Kept minimal and allocation-free on the hot path.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(dir) = direction_of(record.EventHeader.EventDescriptor.Id) else {
        return;
    };
    let shared = record.UserContext as *const Shared;
    if shared.is_null() || record.UserData.is_null() || record.UserDataLength < 8 {
        return;
    }
    let payload = unsafe {
        std::slice::from_raw_parts(record.UserData as *const u8, record.UserDataLength as usize)
    };
    let Some((pid, size)) = parse_pid_and_size(payload) else {
        return;
    };
    unsafe { &*shared }.record(pid, size, dir);
}

/// Session name, unique per process so two running copies never collide.
fn session_name() -> Vec<u16> {
    format!("TaskMan-Net-{}\0", std::process::id())
        .encode_utf16()
        .collect()
}

/// `EVENT_TRACE_PROPERTIES` plus room for the trailing session name, which
/// the API copies in at `LoggerNameOffset`.
fn properties_buffer(name: &[u16]) -> Vec<u8> {
    let header = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = header + name.len() * 2;
    let mut buffer = vec![0u8; total];
    // SAFETY: the buffer is at least one EVENT_TRACE_PROPERTIES long and
    // correctly aligned (Vec<u8> from the global allocator is 8-aligned, and
    // the struct's alignment is 8).
    let properties = buffer.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
    unsafe {
        (*properties).Wnode.BufferSize = total as u32;
        (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        // QPC timestamps: cheapest clock, and we only need ordering.
        (*properties).Wnode.ClientContext = 1;
        (*properties).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*properties).LoggerNameOffset = header as u32;
        // A small buffer set with a 1 s flush keeps latency at roughly one
        // sampling tick without reserving much non-paged memory.
        (*properties).BufferSize = 64;
        (*properties).MinimumBuffers = 4;
        (*properties).MaximumBuffers = 16;
        (*properties).FlushTimer = 1;
    }
    buffer
}

/// Start the session, retrying once after clearing a stale session of the
/// same name (left behind by a crash).
fn start_session(name: &[u16]) -> Option<(CONTROLTRACE_HANDLE, Vec<u8>)> {
    let mut properties = properties_buffer(name);
    let mut handle = CONTROLTRACE_HANDLE::default();
    let mut status = unsafe {
        StartTraceW(
            &mut handle,
            PCWSTR(name.as_ptr()),
            properties.as_mut_ptr().cast(),
        )
    };
    if status == ERROR_ALREADY_EXISTS {
        let mut stale = properties_buffer(name);
        let _ = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(name.as_ptr()),
                stale.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        properties = properties_buffer(name);
        status = unsafe {
            StartTraceW(
                &mut handle,
                PCWSTR(name.as_ptr()),
                properties.as_mut_ptr().cast(),
            )
        };
    }
    if status != ERROR_SUCCESS {
        // Access denied without administrator rights is the normal case, not
        // an error worth shouting about.
        tracing::debug!(error = status.0, "per-process network trace unavailable");
        return None;
    }
    Some((handle, properties))
}

fn stop_session(handle: CONTROLTRACE_HANDLE, name: &[u16], properties: &mut [u8]) {
    let status: WIN32_ERROR = unsafe {
        ControlTraceW(
            handle,
            PCWSTR(name.as_ptr()),
            properties.as_mut_ptr().cast(),
            EVENT_TRACE_CONTROL_STOP,
        )
    };
    if status != ERROR_SUCCESS {
        tracing::debug!(error = status.0, "stopping the network trace failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_data_events_carry_bytes() {
        // TCP/UDP, IPv4/IPv6 send and receive.
        for id in [10, 26, 42, 58] {
            assert_eq!(direction_of(id), Some(Direction::Sent), "id {id}");
        }
        for id in [11, 27, 43, 59] {
            assert_eq!(direction_of(id), Some(Direction::Received), "id {id}");
        }
        // Connect/disconnect/retransmit/ACK bookkeeping must not be counted,
        // or every retransmitted byte would be billed twice.
        for id in [0, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 44, 60] {
            assert_eq!(direction_of(id), None, "id {id} must be ignored");
        }
    }

    #[test]
    fn payload_prefix_is_pid_then_size() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4321u32.to_le_bytes());
        payload.extend_from_slice(&1460u32.to_le_bytes());
        payload.extend_from_slice(&[0xAA; 24]); // addresses/ports we ignore
        assert_eq!(parse_pid_and_size(&payload), Some((4321, 1460)));
    }

    #[test]
    fn truncated_payloads_are_rejected_instead_of_read() {
        assert_eq!(parse_pid_and_size(&[]), None);
        assert_eq!(parse_pid_and_size(&[1, 2, 3, 4, 5, 6, 7]), None);
    }

    #[test]
    fn totals_accumulate_per_direction_and_pid() {
        let shared = Shared {
            totals: Mutex::new(HashMap::new()),
            live: AtomicBool::new(true),
        };
        shared.record(7, 100, Direction::Sent);
        shared.record(7, 40, Direction::Sent);
        shared.record(7, 900, Direction::Received);
        shared.record(9, 5, Direction::Received);
        let totals = shared.totals.lock().unwrap();
        assert_eq!(
            totals[&7],
            PidBytes {
                received: 900,
                sent: 140
            }
        );
        assert_eq!(
            totals[&9],
            PidBytes {
                received: 5,
                sent: 0
            }
        );
    }

    /// A late callback after teardown must not resurrect the map.
    #[test]
    fn records_are_dropped_once_the_session_is_gone() {
        let shared = Shared {
            totals: Mutex::new(HashMap::new()),
            live: AtomicBool::new(false),
        };
        shared.record(7, 100, Direction::Sent);
        assert!(shared.totals.lock().unwrap().is_empty());
    }

    #[test]
    fn properties_buffer_reserves_room_for_the_session_name() {
        let name = session_name();
        let buffer = properties_buffer(&name);
        assert_eq!(
            buffer.len(),
            std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2
        );
        let properties = buffer.as_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            assert_eq!((*properties).Wnode.BufferSize as usize, buffer.len());
            assert_eq!(
                (*properties).LoggerNameOffset as usize,
                std::mem::size_of::<EVENT_TRACE_PROPERTIES>()
            );
            assert_eq!((*properties).LogFileMode, EVENT_TRACE_REAL_TIME_MODE);
        }
    }

    #[test]
    fn session_name_is_process_unique_and_nul_terminated() {
        let name = session_name();
        assert_eq!(name.last(), Some(&0));
        let text = String::from_utf16_lossy(&name[..name.len() - 1]);
        assert_eq!(text, format!("TaskMan-Net-{}", std::process::id()));
    }
}
