//! Per-process disk activity from an ETW real-time session.
//!
//! ## Why this exists
//!
//! The Performance page reports a disk's **Active time**, and the obvious
//! follow-up question — *which process is doing that?* — has no answer in the
//! per-process I/O byte counters the Processes and Details pages show. Those
//! come from the kernel's `IO_COUNTERS`, which count every I/O the process
//! ISSUED: reads the cache manager served without touching a disk, writes that
//! are still sitting in the cache, socket and named-pipe traffic. They also
//! miss paging I/O completely. A process reading one cached file in a loop
//! tops that column while the disk is idle; a process thrashing the page file
//! barely shows up at all.
//!
//! `Microsoft-Windows-Kernel-Disk` reports what actually reached a disk: one
//! event per completed request, carrying the transfer size, the service time
//! the request took, and the id of the thread that issued it. Summed per
//! process over a window, the service times give each process's SHARE of the
//! disk time the machine spent — which is exactly the number "Active time"
//! is made of.
//!
//! ## Things that are easy to get wrong here
//!
//! * **The events come from the KERNEL providers, not the manifest ones.**
//!   `Microsoft-Windows-Kernel-Disk` looks like the obvious source and is not:
//!   its `DiskRead`/`DiskWrite` template is `DiskNumber, IrpFlags,
//!   TransferSize, Reserved, ByteOffset, FileObject, IORequestPacket,
//!   HighResResponseTime` — no issuing thread, so nothing in it can be
//!   attributed to a process. (Verified against the `WEVT_TEMPLATE` resource
//!   of `Microsoft-Windows-System-Events.dll`; this cost one release.) The
//!   classic kernel `DiskIo` record does carry `IssuingThreadId`, and the way
//!   to subscribe to it without seizing the single global "NT Kernel Logger"
//!   is a system-logger session on `SystemIoProviderGuid`.
//! * **Attribution is by issuing THREAD, not by event header.** Disk events
//!   complete in whatever context the DPC ran in, so `EventHeader.ProcessId`
//!   is usually `System`. `IssuingThreadId` from the payload is the truth, and
//!   it has to be mapped to a process — which is why this session also enables
//!   the kernel thread events and seeds itself from a one-shot thread
//!   snapshot at start.
//! * **A window in which nothing could be decoded is UNKNOWN, not zero.** The
//!   decoder rejecting every record looks exactly like an idle disk if you
//!   only count attributed time, and reporting "0 %" for every process while
//!   the disk sits at 100 % is precisely the fabricated measurement this
//!   program must never produce. [`DiskWindow::decoded`] separates the two.
//! * **A request whose thread cannot be mapped is NOT charged to anyone.** It
//!   goes into an unattributed remainder. Guessing would put another process's
//!   disk time on an innocent row, which is worse than an honest gap.
//! * **Only ratios of the service time are ever published.** The provider's
//!   time unit is not documented anywhere authoritative, so this module never
//!   exposes it as a duration; every consumer divides it by the window total.
//!   Transfer sizes and request counts, which ARE unambiguous, are exposed
//!   directly.
//! * The session lifetime rules (fixed names, orphan reclamation, the
//!   administrator requirement) live in [`super::etw`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::System::Diagnostics::Etw::{
    DiskIoGuid, EVENT_RECORD, SYSTEM_IO_KW_DISK, SYSTEM_PROCESS_KW_THREAD, SystemIoProviderGuid,
    SystemProcessProviderGuid, ThreadGuid,
};

use super::etw::{
    self, LoggerKind, Provider, Session, TraceContext, TraceRole, plausible_client_id,
};

/// Classic `DiskIo` opcodes (`DiskIo_TypeGroup1`).
const OPCODE_DISK_READ: u8 = 10;
const OPCODE_DISK_WRITE: u8 = 11;
/// Classic `Thread` opcodes. The DC pair is the rundown the session emits for
/// threads that already existed when it started.
const OPCODE_THREAD_START: u8 = 1;
const OPCODE_THREAD_STOP: u8 = 2;
const OPCODE_THREAD_DC_START: u8 = 3;
const OPCODE_THREAD_DC_STOP: u8 = 4;

/// A single request bigger than this did not come from this payload layout.
/// Windows splits I/O long before a gigabyte, so anything above it is a read
/// through a wrong offset rather than a real transfer.
const MAX_PLAUSIBLE_TRANSFER: u32 = 1 << 30;

/// Upper bound on the thread map. ETW can drop events under load, and a lost
/// ThreadStop leaks one entry; past this the map is thrown away and reseeded
/// from a fresh snapshot rather than growing without limit.
const MAX_TRACKED_THREADS: usize = 250_000;

/// What one process did to the disks during a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidDisk {
    /// Summed service time of this process's completed requests, in the
    /// provider's own units. Meaningful only as a ratio — see the module docs.
    pub service_time: u64,
    /// Bytes that actually reached a disk.
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Completed requests, i.e. the queue pressure this process created.
    pub ops: u64,
}

impl PidDisk {
    fn add(&mut self, write: bool, size: u32, service_time: u64) {
        if write {
            self.write_bytes = self.write_bytes.saturating_add(u64::from(size));
        } else {
            self.read_bytes = self.read_bytes.saturating_add(u64::from(size));
        }
        self.service_time = self.service_time.saturating_add(service_time);
        self.ops = self.ops.saturating_add(1);
    }
}

/// One drained measurement window.
#[derive(Debug, Clone, Default)]
pub struct DiskWindow {
    pub procs: HashMap<u32, PidDisk>,
    /// Service time of requests whose issuing thread could not be mapped to a
    /// live process. Kept so the shares stay honest: it is part of the
    /// denominator, it is simply not charged to a row.
    pub unattributed_service_time: u64,
    /// How long this window covered, in milliseconds. Measured by the producer
    /// because only it knows when the accumulators were last reset.
    pub since_ms: u64,
    /// Disk read/write records the callback was handed.
    pub events_seen: u64,
    /// ...of which the payload decoder accepted. The pair is the difference
    /// between "the disk was idle" and "we could not read a single record",
    /// which are indistinguishable from the totals alone.
    pub events_decoded: u64,
}

impl DiskWindow {
    /// Total service time observed in the window, attributed or not.
    pub fn total_service_time(&self) -> u64 {
        self.procs
            .values()
            .fold(self.unattributed_service_time, |acc, p| {
                acc.saturating_add(p.service_time)
            })
    }

    /// Whether this window is a measurement at all.
    ///
    /// No events is a real answer — a live session over an idle disk. Events
    /// that none of them could be decoded is not: it means the payload does
    /// not look the way this module expects, and every share derived from it
    /// would be a fabricated zero.
    pub fn decoded(&self) -> bool {
        self.events_seen == 0 || self.events_decoded > 0
    }
}

/// Byte offsets into the classic `DiskIo_TypeGroup1` payload.
///
/// DiskNumber@0, IrpFlags@4, TransferSize@8, Reserved@12, ByteOffset@16,
/// FileObject, Irp, HighResResponseTime, IssuingThreadId — the last four
/// depending on the pointer size of the process the event came from, which
/// ETW reports per record.
struct DiskLayout {
    response_time: usize,
    issuing_thread: usize,
    min_len: usize,
}

impl DiskLayout {
    const fn for_pointer_size(pointer_bytes: usize) -> Self {
        let response_time = 24 + 2 * pointer_bytes;
        Self {
            response_time,
            issuing_thread: response_time + 8,
            min_len: response_time + 12,
        }
    }
}

/// One disk request, as far as this module cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskRequest {
    thread_id: u32,
    transfer_size: u32,
    service_time: u64,
}

/// Decode a `DiskRead`/`DiskWrite` payload.
///
/// Returns `None` for anything that does not look like the documented record:
/// a short payload, an implausible transfer size, or an issuing thread id that
/// did not come out of the handle table. Those are the symptoms a wrong offset
/// would produce, and the honest answer to them is "unknown".
fn parse_disk_event(payload: &[u8], pointer_bytes: usize) -> Option<DiskRequest> {
    let layout = DiskLayout::for_pointer_size(pointer_bytes);
    if payload.len() < layout.min_len {
        return None;
    }
    let transfer_size = u32::from_le_bytes(payload[8..12].try_into().ok()?);
    if transfer_size > MAX_PLAUSIBLE_TRANSFER {
        return None;
    }
    let service_time = u64::from_le_bytes(
        payload[layout.response_time..layout.response_time + 8]
            .try_into()
            .ok()?,
    );
    let thread_id = u32::from_le_bytes(
        payload[layout.issuing_thread..layout.issuing_thread + 4]
            .try_into()
            .ok()?,
    );
    if !plausible_client_id(thread_id) {
        return None;
    }
    Some(DiskRequest {
        thread_id,
        transfer_size,
        service_time,
    })
}

/// Decode the `(ProcessID, ThreadID)` prefix of a Kernel-Process thread event.
fn parse_thread_event(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let tid = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    (plausible_client_id(pid) && plausible_client_id(tid)).then_some((pid, tid))
}

#[derive(Default)]
struct State {
    events_seen: u64,
    events_decoded: u64,
    /// Payload length of the most recent record the decoder rejected, so a
    /// layout change is diagnosable from a log line instead of a debugger.
    rejected_payload_len: u32,
    /// Thread id -> owning process id, seeded from a snapshot and kept current
    /// by the Kernel-Process thread events.
    threads: HashMap<u32, u32>,
    totals: HashMap<u32, PidDisk>,
    unattributed_service_time: u64,
    /// Set when the thread map had to be dropped; the next collection reseeds
    /// it from a fresh snapshot.
    needs_reseed: bool,
}

struct Shared {
    state: Mutex<State>,
    live: AtomicBool,
}

impl TraceContext for Shared {
    fn stop(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

impl Shared {
    /// A disk record the decoder could not read. Counted, never guessed at.
    fn record_undecodable(&self, payload_len: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.events_seen = state.events_seen.saturating_add(1);
            state.rejected_payload_len = payload_len;
        }
    }

    fn record_request(&self, request: DiskRequest, write: bool) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.events_seen = state.events_seen.saturating_add(1);
        state.events_decoded = state.events_decoded.saturating_add(1);
        match state.threads.get(&request.thread_id).copied() {
            Some(pid) => state.totals.entry(pid).or_default().add(
                write,
                request.transfer_size,
                request.service_time,
            ),
            None => {
                state.unattributed_service_time = state
                    .unattributed_service_time
                    .saturating_add(request.service_time)
            }
        }
    }

    fn track_thread(&self, pid: u32, tid: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.threads.len() >= MAX_TRACKED_THREADS {
            // Dropped ThreadStop events would otherwise grow this forever.
            state.threads.clear();
            state.needs_reseed = true;
        }
        state.threads.insert(tid, pid);
    }

    fn forget_thread(&self, tid: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.threads.remove(&tid);
        }
    }
}

/// A running per-process disk trace.
pub struct DiskUsage {
    session: Session<Shared>,
    /// Start of the window currently being accumulated.
    window_start: Mutex<Instant>,
}

impl DiskUsage {
    /// Start a real-time session for `role`. Returns `None` when ETW is
    /// unavailable to this token (the common unelevated case), which keeps
    /// per-process disk activity reported as unknown rather than zero.
    pub fn start(role: TraceRole) -> Option<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                // Seed before the session exists: every thread already running
                // predates the ThreadStart events, and the busiest I/O threads
                // on a machine are exactly the long-lived ones.
                threads: super::threads_map::thread_owners(),
                ..State::default()
            }),
            live: AtomicBool::new(true),
        });
        let session = Session::start(
            session_name(role),
            LoggerKind::System,
            &[
                Provider {
                    guid: SystemIoProviderGuid,
                    match_any_keyword: SYSTEM_IO_KW_DISK,
                },
                // Thread start/stop plus the rundown of threads that already
                // existed, which is what makes `IssuingThreadId` resolvable.
                Provider {
                    guid: SystemProcessProviderGuid,
                    match_any_keyword: SYSTEM_PROCESS_KW_THREAD,
                },
            ],
            shared,
            on_event,
            "tm-disk-etw",
        )?;
        tracing::info!("per-process disk trace started");
        Some(Self {
            session,
            window_start: Mutex::new(Instant::now()),
        })
    }

    /// Drain everything observed since the last call.
    ///
    /// Draining rather than accumulating: the published value is a share of
    /// ONE window, so keeping running totals would only invite a consumer to
    /// difference them and get the window boundaries wrong.
    ///
    /// An EMPTY `live_pids` means "the caller could not enumerate processes",
    /// never "nothing is alive": pruning against it would discard every
    /// measurement and report a machine whose disks nobody is using.
    pub fn take_window(&self, live_pids: &HashSet<u32>) -> DiskWindow {
        let now = Instant::now();
        let since_ms = {
            let mut start = match self.window_start.lock() {
                Ok(start) => start,
                Err(poisoned) => poisoned.into_inner(),
            };
            let elapsed = now.duration_since(*start);
            *start = now;
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64
        };
        let mut state = match self.session.shared().state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let unattributed = std::mem::take(&mut state.unattributed_service_time);
        let mut procs = std::mem::take(&mut state.totals);
        let events_seen = std::mem::take(&mut state.events_seen);
        let events_decoded = std::mem::take(&mut state.events_decoded);
        let rejected_payload_len = state.rejected_payload_len;
        if state.needs_reseed {
            state.threads = super::threads_map::thread_owners();
            state.needs_reseed = false;
        }
        drop(state);
        if events_seen > 0 && events_decoded == 0 {
            // Loud on purpose, and only once per window: this is the failure
            // that silently turns the whole column into zeros.
            tracing::warn!(
                events_seen,
                rejected_payload_len,
                "disk trace decoded no records - the DiskIo payload layout changed"
            );
        }
        let mut unattributed_service_time = unattributed;
        if !live_pids.is_empty() {
            procs.retain(|pid, disk| {
                if live_pids.contains(pid) {
                    return true;
                }
                // A process that exited mid-window really did use the disk;
                // its time stays in the denominator so the surviving shares
                // are not inflated.
                unattributed_service_time =
                    unattributed_service_time.saturating_add(disk.service_time);
                false
            });
        }
        DiskWindow {
            procs,
            unattributed_service_time,
            since_ms,
            events_seen,
            events_decoded,
        }
    }
}

/// ETW record callback. Kept minimal and allocation-free on the hot path.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(shared) = (unsafe { etw::context_of::<Shared>(record) }) else {
        return;
    };
    let Some(payload) = (unsafe { etw::payload_of(record) }) else {
        return;
    };
    // Classic (MOF) records carry the event class in `ProviderId` and the
    // event kind in the opcode; there is no event id to match on.
    let provider = record.EventHeader.ProviderId;
    let opcode = record.EventHeader.EventDescriptor.Opcode;
    if provider == DiskIoGuid {
        let write = match opcode {
            OPCODE_DISK_READ => false,
            OPCODE_DISK_WRITE => true,
            // Flush and the *Init variants carry no completed transfer.
            _ => return,
        };
        match parse_disk_event(payload, pointer_bytes(record)) {
            Some(request) => shared.record_request(request, write),
            None => shared.record_undecodable(record.UserDataLength.into()),
        }
    } else if provider == ThreadGuid {
        match opcode {
            OPCODE_THREAD_START | OPCODE_THREAD_DC_START => {
                if let Some((pid, tid)) = parse_thread_event(payload) {
                    shared.track_thread(pid, tid);
                }
            }
            OPCODE_THREAD_STOP | OPCODE_THREAD_DC_STOP => {
                if let Some((_, tid)) = parse_thread_event(payload) {
                    shared.forget_thread(tid);
                }
            }
            _ => {}
        }
    }
}

/// Pointer width of the process the record came from. ETW flags 32-bit
/// records explicitly, and the disk payload embeds two pointers before the
/// fields we read.
fn pointer_bytes(record: &EVENT_RECORD) -> usize {
    const EVENT_HEADER_FLAG_32_BIT_HEADER: u16 = 0x0020;
    if record.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
        4
    } else {
        8
    }
}

/// Fixed session name per role. See [`super::etw`]: this MUST NOT include the
/// pid, or an orphaned session can never be reclaimed.
fn session_name(role: TraceRole) -> &'static str {
    match role {
        TraceRole::Service => "TaskMan-Disk-Service",
        TraceRole::App => "TaskMan-Disk-App",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk_payload(
        pointer_bytes: usize,
        transfer_size: u32,
        service_time: u64,
        thread_id: u32,
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // DiskNumber
        payload.extend_from_slice(&0u32.to_le_bytes()); // IrpFlags
        payload.extend_from_slice(&transfer_size.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes()); // Reserved
        payload.extend_from_slice(&4096u64.to_le_bytes()); // ByteOffset
        payload.extend(std::iter::repeat_n(0xAAu8, pointer_bytes)); // FileObject
        payload.extend(std::iter::repeat_n(0xBBu8, pointer_bytes)); // Irp
        payload.extend_from_slice(&service_time.to_le_bytes());
        payload.extend_from_slice(&thread_id.to_le_bytes());
        payload
    }

    #[test]
    fn disk_payload_is_decoded_for_both_pointer_sizes() {
        for pointer_bytes in [4usize, 8] {
            let payload = disk_payload(pointer_bytes, 65_536, 1_234, 4_660);
            assert_eq!(
                parse_disk_event(&payload, pointer_bytes),
                Some(DiskRequest {
                    thread_id: 4_660,
                    transfer_size: 65_536,
                    service_time: 1_234,
                }),
                "{pointer_bytes}-byte pointers"
            );
        }
        // Reading a 64-bit record with the 32-bit layout lands on the wrong
        // fields; the thread-id check is what catches it.
        let wide = disk_payload(8, 65_536, 1_234, 4_660);
        assert_eq!(parse_disk_event(&wide, 4), None);
    }

    #[test]
    fn implausible_records_are_rejected_instead_of_charged() {
        // Truncated.
        assert_eq!(parse_disk_event(&[0u8; 20], 8), None);
        // A transfer size no storage stack produces.
        assert_eq!(
            parse_disk_event(&disk_payload(8, u32::MAX, 1, 4_660), 8),
            None
        );
        // Thread ids are handle-table ids: non-zero multiples of four.
        assert_eq!(parse_disk_event(&disk_payload(8, 4_096, 1, 0), 8), None);
        assert_eq!(parse_disk_event(&disk_payload(8, 4_096, 1, 4_661), 8), None);
        // A zero service time is a real measurement (a cache-backed request
        // that completed inside the timer's resolution), not a rejection.
        assert!(parse_disk_event(&disk_payload(8, 4_096, 0, 4_660), 8).is_some());
    }

    #[test]
    fn thread_events_decode_their_prefix() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1_236_u32.to_le_bytes()); // ProcessID
        payload.extend_from_slice(&5_680_u32.to_le_bytes()); // ThreadID
        payload.extend_from_slice(&[0u8; 64]); // stacks, start addresses, ...
        assert_eq!(parse_thread_event(&payload), Some((1_236, 5_680)));
        assert_eq!(parse_thread_event(&[0u8; 4]), None);
        // A pid of zero is the idle process and never starts tracked threads.
        assert_eq!(parse_thread_event(&[0u8; 8]), None);
        // Handle-table ids are multiples of four; anything else is a misread.
        let mut odd = Vec::new();
        odd.extend_from_slice(&1_235_u32.to_le_bytes());
        odd.extend_from_slice(&5_680_u32.to_le_bytes());
        assert_eq!(parse_thread_event(&odd), None);
    }

    fn shared_with(threads: HashMap<u32, u32>) -> Shared {
        Shared {
            state: Mutex::new(State {
                threads,
                ..State::default()
            }),
            live: AtomicBool::new(true),
        }
    }

    #[test]
    fn requests_are_charged_to_the_issuing_thread_s_process() {
        let shared = shared_with(HashMap::from([(100, 8), (200, 12)]));
        shared.record_request(
            DiskRequest {
                thread_id: 100,
                transfer_size: 4_096,
                service_time: 50,
            },
            false,
        );
        shared.record_request(
            DiskRequest {
                thread_id: 100,
                transfer_size: 8_192,
                service_time: 70,
            },
            true,
        );
        shared.record_request(
            DiskRequest {
                thread_id: 200,
                transfer_size: 512,
                service_time: 30,
            },
            false,
        );
        let state = shared.state.lock().unwrap();
        assert_eq!(
            state.totals[&8],
            PidDisk {
                service_time: 120,
                read_bytes: 4_096,
                write_bytes: 8_192,
                ops: 2,
            }
        );
        assert_eq!(state.totals[&12].read_bytes, 512);
        assert_eq!(state.unattributed_service_time, 0);
    }

    /// An unmappable thread must never have its disk time guessed onto a row.
    #[test]
    fn an_unknown_thread_becomes_an_unattributed_remainder() {
        let shared = shared_with(HashMap::new());
        shared.record_request(
            DiskRequest {
                thread_id: 100,
                transfer_size: 4_096,
                service_time: 90,
            },
            false,
        );
        let state = shared.state.lock().unwrap();
        assert!(state.totals.is_empty(), "nothing may be charged");
        assert_eq!(state.unattributed_service_time, 90);
    }

    /// A late callback after teardown must not resurrect the accumulators.
    #[test]
    fn records_are_dropped_once_the_session_is_gone() {
        let shared = shared_with(HashMap::from([(100, 8)]));
        shared.live.store(false, Ordering::Relaxed);
        shared.record_request(
            DiskRequest {
                thread_id: 100,
                transfer_size: 4_096,
                service_time: 90,
            },
            false,
        );
        shared.track_thread(16, 104);
        let state = shared.state.lock().unwrap();
        assert!(state.totals.is_empty());
        assert_eq!(state.unattributed_service_time, 0);
        assert!(!state.threads.contains_key(&104));
    }

    #[test]
    fn thread_map_follows_start_and_stop() {
        let shared = shared_with(HashMap::new());
        shared.track_thread(8, 100);
        assert_eq!(shared.state.lock().unwrap().threads.get(&100), Some(&8));
        shared.forget_thread(100);
        assert!(shared.state.lock().unwrap().threads.is_empty());
    }

    /// The share denominator has to include the time nobody could be charged
    /// with, or a machine where most I/O is unattributable would report a
    /// handful of processes as responsible for all of it.
    /// A window where nothing could be decoded is not a measurement. Calling
    /// it one is how the column ended up reporting 0 % for every process while
    /// the disk was pinned at 100 %.
    #[test]
    fn an_undecodable_window_is_not_a_measurement() {
        let idle = DiskWindow {
            events_seen: 0,
            events_decoded: 0,
            ..DiskWindow::default()
        };
        assert!(
            idle.decoded(),
            "a live session over an idle disk is a real 0"
        );

        let broken = DiskWindow {
            events_seen: 4_000,
            events_decoded: 0,
            ..DiskWindow::default()
        };
        assert!(!broken.decoded(), "records arrived and none could be read");

        let working = DiskWindow {
            events_seen: 4_000,
            events_decoded: 3_998,
            ..DiskWindow::default()
        };
        assert!(working.decoded());
    }

    /// Undecodable records must be counted, not silently dropped: the count is
    /// the only signal that separates a broken decoder from an idle disk.
    #[test]
    fn undecodable_records_are_counted_but_never_charged() {
        let shared = shared_with(HashMap::from([(100, 8)]));
        shared.record_undecodable(48);
        shared.record_undecodable(48);
        let state = shared.state.lock().unwrap();
        assert_eq!(state.events_seen, 2);
        assert_eq!(state.events_decoded, 0);
        assert_eq!(state.rejected_payload_len, 48);
        assert!(state.totals.is_empty());
        assert_eq!(state.unattributed_service_time, 0);
    }

    #[test]
    fn the_window_total_counts_unattributed_time() {
        let window = DiskWindow {
            procs: HashMap::from([
                (
                    8,
                    PidDisk {
                        service_time: 30,
                        ..PidDisk::default()
                    },
                ),
                (
                    12,
                    PidDisk {
                        service_time: 20,
                        ..PidDisk::default()
                    },
                ),
            ]),
            unattributed_service_time: 50,
            since_ms: 1_000,
            events_seen: 3,
            events_decoded: 3,
        };
        assert_eq!(window.total_service_time(), 100);
    }

    #[test]
    fn each_role_owns_a_distinct_fixed_session_name() {
        assert_ne!(
            session_name(TraceRole::Service),
            session_name(TraceRole::App)
        );
        // ...and neither collides with the network trace's names.
        for role in [TraceRole::Service, TraceRole::App] {
            assert!(session_name(role).contains("Disk"));
        }
    }
}
