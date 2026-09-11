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
//! * **Attribution is by issuing THREAD, not by event header.** Disk events
//!   complete in whatever context the DPC ran in, so `EventHeader.ProcessId`
//!   is usually `System`. `IssuingThreadId` from the payload is the truth, and
//!   it has to be mapped to a process — which is why this session also enables
//!   the thread events of `Microsoft-Windows-Kernel-Process` and seeds itself
//!   from a one-shot thread snapshot at start.
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

use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;
use windows::core::GUID;

use super::etw::{self, Provider, Session, TraceContext, TraceRole, plausible_client_id};

/// `Microsoft-Windows-Kernel-Disk`.
const KERNEL_DISK_GUID: GUID = GUID::from_u128(0xc7bde69a_e1e0_4177_b6ef_283ad1525271);
/// `Microsoft-Windows-Kernel-Process`, enabled only for its thread events.
const KERNEL_PROCESS_GUID: GUID = GUID::from_u128(0x22fb2cd6_0e7b_422b_a0c7_2fad1fd0e716);
/// `WINEVENT_KEYWORD_THREAD` of `Microsoft-Windows-Kernel-Process`: the
/// ThreadStart/ThreadStop pair and nothing else. Process, image, priority and
/// job events would be pure overhead here.
const KEYWORD_THREAD: u64 = 0x20;

/// `Microsoft-Windows-Kernel-Disk` event ids.
const EVENT_DISK_READ: u16 = 10;
const EVENT_DISK_WRITE: u16 = 11;
/// `Microsoft-Windows-Kernel-Process` event ids.
const EVENT_THREAD_START: u16 = 3;
const EVENT_THREAD_STOP: u16 = 4;

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
}

/// Byte offsets into the `DiskRead`/`DiskWrite` payload of
/// `Microsoft-Windows-Kernel-Disk`.
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
    fn record_request(&self, request: DiskRequest, write: bool) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
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
            &[
                Provider {
                    guid: KERNEL_DISK_GUID,
                    match_any_keyword: 0,
                },
                Provider {
                    guid: KERNEL_PROCESS_GUID,
                    match_any_keyword: KEYWORD_THREAD,
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
        if state.needs_reseed {
            state.threads = super::threads_map::thread_owners();
            state.needs_reseed = false;
        }
        drop(state);
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
    let provider = record.EventHeader.ProviderId;
    let id = record.EventHeader.EventDescriptor.Id;
    if provider == KERNEL_DISK_GUID {
        let write = match id {
            EVENT_DISK_READ => false,
            EVENT_DISK_WRITE => true,
            // Flush events carry no transfer and no issuing thread.
            _ => return,
        };
        if let Some(request) = parse_disk_event(payload, pointer_bytes(record)) {
            shared.record_request(request, write);
        }
    } else if provider == KERNEL_PROCESS_GUID {
        match id {
            EVENT_THREAD_START => {
                if let Some((pid, tid)) = parse_thread_event(payload) {
                    shared.track_thread(pid, tid);
                }
            }
            EVENT_THREAD_STOP => {
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
