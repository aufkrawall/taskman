//! Shared plumbing for the real-time ETW sessions this program hosts.
//!
//! Two features need one: per-process network bytes ([`super::net_etw`]) and
//! per-process disk service time ([`super::disk_etw`]). The lifetime rules are
//! identical for both and every one of them has already been learned the hard
//! way, so they live here once instead of being copied:
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

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, EVENT_TRACE_SYSTEM_LOGGER_MODE, EnableTraceEx2, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE,
    ProcessTrace, StartTraceW, WNODE_FLAG_TRACED_GUID,
};
use windows::core::{GUID, PCWSTR, PWSTR};

/// Informational level; the data events of every provider used here are
/// emitted at this level.
pub(crate) const TRACE_LEVEL_INFORMATION: u8 = 4;

/// Invalid real-time processing handle, as returned by `OpenTraceW` on
/// failure. `PROCESSTRACE_HANDLE` is a plain u64 in the Win32 headers.
const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;

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
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let (session, mut properties) = start_session(&name, kind)?;

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
                tracing::debug!(error = enable.0, "ETW provider not enabled");
                stop_session(session, &name, &mut properties);
                return None;
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
            tracing::debug!("OpenTraceW failed for a real-time session");
            stop_session(session, &name, &mut properties);
            // Reclaim the reference the callback would have owned.
            drop(unsafe { Arc::from_raw(context) });
            return None;
        }

        let worker = std::thread::Builder::new()
            .name(thread_name.to_owned())
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

        Some(Self {
            shared,
            kind,
            context,
            session,
            trace,
            name,
            worker,
        })
    }

    pub(crate) fn shared(&self) -> &Arc<T> {
        &self.shared
    }
}

impl<T: TraceContext> Drop for Session<T> {
    fn drop(&mut self) {
        // Order matters: stop the session so `ProcessTrace` returns, then
        // close the consumer and join before the context is reclaimed.
        self.shared.stop();
        let mut properties = properties_buffer(&self.name, self.kind);
        stop_session(self.session, &self.name, &mut properties);
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
        (*properties).LogFileMode = EVENT_TRACE_REAL_TIME_MODE
            | match kind {
                LoggerKind::Manifest => 0,
                // The only way to subscribe to the kernel's own providers
                // without taking over the single global "NT Kernel Logger".
                LoggerKind::System => EVENT_TRACE_SYSTEM_LOGGER_MODE,
            };
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
fn start_session(name: &[u16], kind: LoggerKind) -> Option<(CONTROLTRACE_HANDLE, Vec<u64>)> {
    let mut properties = properties_buffer(name, kind);
    let mut handle = CONTROLTRACE_HANDLE::default();
    let mut status = unsafe {
        StartTraceW(
            &mut handle,
            PCWSTR(name.as_ptr()),
            properties.as_mut_ptr().cast(),
        )
    };
    if status == ERROR_ALREADY_EXISTS {
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
        // Access denied without administrator rights is the normal case, not
        // an error worth shouting about.
        tracing::debug!(error = status.0, "real-time ETW session unavailable");
        return None;
    }
    Some((handle, properties))
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
