//! Per-file read and written bytes from a real-time kernel `FileIo` trace.
//!
//! ## Why this exists
//!
//! The I/O page of Process Properties lists the files a process has open and
//! shows bytes read and written next to each. Nothing else on the system can
//! answer that per FILE: the kernel's per-process `IO_COUNTERS` (which the I/O
//! graphs plot) are one total per process, and the handle walk in
//! [`super::process_file_io`] can name a file but cannot say how much went
//! through it. Deriving it from a file's size or a handle's seek offset is what
//! commit ef95ba1 removed, and that reasoning still holds: a rewritten file
//! would be "written" again at full size, a seek is not a transfer, and a
//! memory-mapped view is invisible to both.
//!
//! The kernel's file I/O hooks are what know. They report every request a
//! process issues — Create, Read, Write, Cleanup, Close, ... — with the issuing
//! process in the record header and the `FILE_OBJECT` the request went to in
//! the payload, and they report each request's completion (`OpEnd`) with the
//! bytes it really transferred.
//!
//! ## The subscription — measured, not assumed
//!
//! Every shape below was checked with an elevated probe on Windows 11 build
//! 26300 that started the session, did its own file I/O, and histogrammed what
//! arrived (`llm-wiki/log/recent.md`, 2026-11-25):
//!
//! * `EVENT_TRACE_FLAG_FILE_IO` alone — and the SystemIo provider's
//!   `SYSTEM_IO_KW_FILE` keyword, which is the same stream — delivers ONLY
//!   `OpEnd` completions (opcode 76): ~22 000 in four seconds and not one
//!   request. Earlier builds subscribed exactly like that.
//! * `EVENT_TRACE_FLAG_FILE_IO_INIT` delivers the requests: Create 64,
//!   Cleanup 65, Close 66, Read 67, Write 68 (and the info, directory, flush
//!   and path operations around them). Earlier builds filtered on 12..16 —
//!   the manifest provider's EVENT IDS, which a classic record never carries —
//!   and so discarded every request even where one did arrive.
//! * Both flags work on a PRIVATE system logger
//!   ([`LoggerKind::SystemFile`]), which is the preferred session: it
//!   coexists with other tracing tools. The single global NT Kernel Logger
//!   ([`LoggerKind::KernelFile`]) is the fallback when no private session can
//!   be started, and it is EXCLUSIVE — when another application owns it, that
//!   is reported as [`FileTraceRefusal::KernelLoggerInUse`] rather than
//!   reclaimed.
//! * `EVENT_TRACE_FLAG_DISK_FILE_IO` (file-name rundown) was measured too and
//!   is NOT used: its rundown of already-open files arrives when the session
//!   STOPS, ~79 000 records at once, which is no help to a live trace.
//!
//! Records are identified the classic way: `FileIoGuid` in
//! `EventHeader.ProviderId`, the operation in the opcode, template version 3.
//! The payloads (x64, pointer `P` = 8 bytes; the fields are kernel addresses,
//! so `P` is the machine's width whatever the issuing process is):
//!
//! | opcode | event | layout |
//! | --- | --- | --- |
//! | 64 | Create | `Irp` P, `FileObject` P, `TTID` u32, `CreateOptions` u32, `FileAttributes` u32, `ShareAccess` u32, `OpenPath` UTF-16 |
//! | 65/66 | Cleanup/Close | `Irp` P, `FileObject` P, `FileKey` P, `TTID` u32 |
//! | 67/68 | Read/Write | `Offset` u64, `Irp` P, `FileObject` P, `FileKey` P, `TTID` u32, `IoSize` u32, `IoFlags` u32, `ExtraFlags` u32 |
//! | 76 | OpEnd | `Irp` P, `ExtraInfo` P, `NtStatus` u32 |
//!
//! Any other version is counted as undecodable and logged with its opcode,
//! version and length, never guessed at.
//!
//! ## What one count means
//!
//! **Completed bytes of logical requests.** A Read/Write is held as pending
//! under its `Irp`; its `OpEnd` charges `ExtraInfo` — the bytes the request
//! actually moved — to the request's process and file object. This matters,
//! and the probe showed why:
//!
//! * A cached write is often logged TWICE: a fast-I/O attempt that a file
//!   system filter refuses (`OpEnd` status `0xC01C0004`,
//!   `STATUS_FLT_DISALLOW_FAST_IO`, 0 bytes) and the IRP retry that does the
//!   work. Counting requests counted that write twice; a failed completion
//!   counts nothing.
//! * A read at end of file asks for more than it gets; the completion says
//!   what it got.
//!
//! **Paging I/O is excluded** (`IoFlags & IRP_PAGING_IO`). A flush of cached
//! data, the lazy writer and a page fault on a mapped view all show up as
//! paging requests — the probe saw a 128 KiB flush charged to the writer right
//! after its own two 64 KiB writes. Counting them would charge the same bytes
//! twice, and it is not what the per-process `IO_COUNTERS` count either. A
//! file read or written only through a memory-mapped view therefore shows no
//! bytes, which is the same thing the process counters show.
//!
//! An `Irp` value is reused once its request is done, so a pending entry that
//! another request on the same `Irp` overtakes is dropped (its completion was
//! lost); any other `FileIo` request carries its `Irp` first and invalidates
//! the same way, so a later completion of an unrelated operation can never be
//! charged as transferred bytes.
//!
//! ## Naming the file object
//!
//! Create carries `FileObject -> OpenPath`; transfers carry only the
//! `FileObject`. A handle opened BEFORE the session started has no Create in
//! it, which is the normal case for exactly the files that matter (a database,
//! a log, a VM image). Those objects are named from the system handle table
//! instead: [`super::process_file_io::name_file_objects`] reads the kernel
//! address of every file handle the process holds, names each through a
//! duplicate exactly the way the file list names its rows, and verifies the
//! duplicate still refers to that object. The probe confirmed the table's
//! object address IS the record's `FileObject`. This happens on the first
//! sample of a process and again (throttled by [`SEED_INTERVAL`]) whenever
//! bytes arrive on an object that is still unnamed — and bytes that arrived
//! before the name are moved onto it, so nothing measured is lost to timing.
//!
//! An object that is closed before it could be named keeps its bytes in the
//! unnamed remainder. Names are device paths (`\Device\HarddiskVolume3\...`)
//! and are translated to the DOS paths the rows use ([`dos_path`]).
//!
//! ## Bounds
//!
//! * [`MAX_OPEN_FILES`] caps the `FileObject -> name` map; on overflow it is
//!   cleared and the discarded entries counted.
//! * [`MAX_PENDING_REQUESTS`] caps in-flight requests; a lost `OpEnd` leaks at
//!   most until the next request on that `Irp`, and the map is cleared at the
//!   cap.
//! * [`MAX_UNRESOLVED_OBJECTS`] caps bytes waiting for a name; past it they go
//!   straight to the unnamed remainder.
//! * [`MAX_TRACKED_FILES_PER_PID`] and [`MAX_TRACKED_FILES`] cap paths; past
//!   them bytes go to the omitted remainder and the drop is counted.
//! * A `FileObject` used by a DIFFERENT process than the one that opened it (a
//!   duplicated or inherited handle) is not a name that process inherits: its
//!   bytes wait as unnamed until the handle table shows that process holding
//!   the object, and are charged to it from then on (see [`OpenFile`]).
//!
//! ## Lifetime
//!
//! The session is meant to run for as long as its host does — the service from
//! its start, an elevated GUI from its start — not just while a page asks:
//! per-file counts that start when a dialog opens and stop when it closes
//! answer "what did this process do while I watched", which is not the
//! question. Its measured cost is small (see the ignored
//! `the_file_trace_costs_little`, which counts CPU cycles of the threads
//! involved so other load on the machine cannot enter the numbers): ~900
//! consumer cycles per record, ~0.2% of one core at an idle desktop's ~8.6k
//! records/s; ~27k consumer cycles (~7 µs) per file in a create/write/read/
//! delete storm; and no difference in the storm's own thread outside the ±4%
//! its untraced runs vary by.
//!
//! Running for long changes three things:
//!
//! * **A reused pid inherits nothing, and a START record is where that is
//!   applied.** The session also carries `EVENT_TRACE_FLAG_PROCESS`, so it sees
//!   a `Process/Start` (opcode 1), the session-start rundown (opcode 3) and a
//!   `Process/End` (opcode 2), each carrying the process's `UniqueProcessKey` at
//!   payload offset 0 (assumed from the MOF; only the pid offset at `P` is pinned
//!   by a live probe) and its pid at `P`. A start whose key differs from the
//!   pid's recorded one is proof that the pid changed owner, and it drops the
//!   predecessor's buckets, names, pending requests and waiting bytes — so the
//!   successor cannot inherit them, and the epoch binding cannot later remove
//!   that stale bucket and destroy the successor's already-measured bytes with
//!   it. An exit drops that state only while the key it carries still matches:
//!   the class can arrive a flush interval late, and a mismatched exit belongs
//!   to an earlier holder of the pid, whose start record already dropped that
//!   state.
//!   The rundown only seeds keys, because it is a snapshot that a live start can
//!   overtake. A transfer charged before the start record that proves a reuse
//!   arrives is dropped with whatever bucket the pid owns at that moment — the
//!   predecessor's, or the fresh one a sample's epoch binding already created —
//!   unknown, never attributed to either process.
//! * **Coverage** is stated, not implied: a process created after the session
//!   started has had its WHOLE life traced; an older one only since the
//!   session started (Windows keeps no per-file history to recover the rest).
//!   The process's own `IO_COUNTERS` are snapshotted just before the session
//!   starts ([`super::process_file_io::snapshot_io_counters`]), so the page can
//!   compare the process's total reads and writes over exactly the traced
//!   window with what reached files — the rest went to pipes, devices or the
//!   network, which the `FileIo` stream does not carry.
//! * **Bounds evict instead of refusing**: when a process's (or the session's)
//!   path budget is full, the least recently used paths that are no longer
//!   open are folded into the omitted remainder, so a long-running process
//!   keeps current rows instead of freezing on its first 8 192 files.
//!
//! ## States
//!
//! A session that never started is [`FileIoAccountingState::Unavailable`] (or
//! `KernelLoggerInUse`, see above). A running session that has not decoded one
//! completed transfer anywhere on the machine yet is `Undelivered`; records
//! that arrived and could not be decoded are `Undecodable`. Once the session
//! has decoded a completed transfer it is proven to see every process, so a
//! process with no transfer is a MEASURED zero — that is a measurement, not a
//! gap. Rows still read unavailable unless the trace knows their file object.
//!
//! ## What this module publishes
//!
//! [`FileEtw::sample`] returns **cumulative** bytes per (process, path) since
//! the session started — not a differential window. The rows' fields are
//! documented as "since accounting began" and the dialog merges snapshots by
//! taking the larger of two counts, so a per-window delta would be read as if
//! it were a total. [`FileIoSample::since_ms`] says how long accounting has been
//! running.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::Win32::System::Diagnostics::Etw::{EVENT_RECORD, FileIoGuid, ProcessGuid};
use windows::core::PCWSTR;

use super::etw::{self, KERNEL_LOGGER, LoggerKind, Session, StartError, TraceContext, TraceRole};
use crate::actions::FileIoAccountingState;

/// Why no per-file session could be started.
///
/// The distinction is what keeps the dialog honest: neither of these is a
/// measurement, and they are not the same failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTraceRefusal {
    /// The private session was refused AND the kernel logger fallback is owned
    /// by another application (`ERROR_ALREADY_EXISTS` from `StartTraceW`), so
    /// there is no session at all.
    ///
    /// Reported as its own state because it is actionable and because it must
    /// never be mistaken for a silent window: nothing started, so nothing can
    /// arrive later without the owner going away first.
    KernelLoggerInUse,
    /// No session for any other reason: no elevation, no free logger slot.
    Unavailable,
}

/// One ordered attempt at a session.
///
/// A table rather than two inline calls so the ORDER — which is the design —
/// is data a test can read, instead of control flow it has to infer.
#[derive(Debug, Clone, Copy)]
pub(super) struct FileSource {
    /// ETW session name to start. The private session's is per role, so the
    /// two hosts never share one; the kernel logger's is fixed and global.
    pub name: &'static str,
    pub kind: LoggerKind,
}

/// The ordered attempts for one host role, best first; see the module docs.
///
/// Both kinds subscribe through their `EnableFlags` alone, so neither enables
/// a provider. The order is re-tried best-first on every (re)start.
pub(super) fn sources(role: TraceRole) -> [FileSource; 2] {
    [
        FileSource {
            name: session_name(role),
            kind: LoggerKind::SystemFile,
        },
        FileSource {
            name: KERNEL_LOGGER,
            kind: LoggerKind::KernelFile,
        },
    ]
}

/// Opcodes of the classic `FileIo` class this module reads; see the module
/// docs for their layouts.
const EVENT_CREATE: u8 = 64;
const EVENT_CLEANUP: u8 = 65;
const EVENT_CLOSE: u8 = 66;
const EVENT_READ: u8 = 67;
const EVENT_WRITE: u8 = 68;
const EVENT_OP_END: u8 = 76;

/// Whether `opcode` is one of the other `FileIo` REQUEST opcodes (set/query
/// information, directory enumeration and notification, flush, file-system
/// control, path operations). Their payloads lead with the `Irp`, which is all
/// this module reads from them: a new request on an `Irp` retires whatever was
/// pending on it.
///
/// The range is split around [`EVENT_OP_END`] (76) on purpose. It belongs to
/// the same numbered family but is a COMPLETION, not a request, and it is
/// decoded rather than retired: a single `69..=86` range silently classified a
/// completion as "another request on this `Irp`", so any reordering of
/// [`handle_payload`]'s arms (or a future early dispatch) would have retired a
/// transfer instead of charging it and dropped every byte it carried. Name
/// records (0, 32, 35, 36) lead with a file key instead and are not in this
/// range either.
fn is_other_request(opcode: u8) -> bool {
    matches!(opcode, 69..=75 | 77..=86)
}

/// The template version every layout here was measured on.
const FILE_IO_VERSION: u8 = 3;

/// `IRP_PAGING_IO` in a transfer's `IoFlags`: cache flushes, the lazy writer
/// and page faults. Excluded; see the module docs.
const IRP_PAGING_IO: u32 = 0x2;

/// A single request bigger than this did not come from this payload layout.
/// Windows splits I/O long before a gigabyte, so anything above it is a read
/// through a wrong offset rather than a real transfer.
const MAX_PLAUSIBLE_TRANSFER: u32 = 1 << 30;

/// Upper bound on the `FileObject -> name` map.
///
/// A lost Close leaks one entry, and ETW does drop events under load; on
/// overflow the map is cleared (see [`Shared::create`]) and the discarded
/// entries are counted, so it cannot grow without limit. At `MAX_PATH` names
/// the whole map is roughly 37 MB.
const MAX_OPEN_FILES: usize = 65_536;

/// Upper bound on requests waiting for their completion. A request whose
/// `OpEnd` was lost stays until another request reuses its `Irp`; the cap
/// keeps a burst of such losses from growing the map, and clearing at it is
/// counted.
const MAX_PENDING_REQUESTS: usize = 65_536;

/// Upper bound on unnamed file objects whose bytes wait for a name from the
/// handle table. Past it, bytes go straight to the unnamed remainder.
const MAX_UNRESOLVED_OBJECTS: usize = 65_536;

/// Upper bound on distinct files tracked per process. A build or a package
/// manager can touch hundreds of thousands of files; past this cap a file's
/// bytes land in the omitted remainder instead of a row.
const MAX_TRACKED_FILES_PER_PID: usize = 8_192;

/// Upper bound on distinct (process, path) pairs across all processes, so a
/// thousand live processes cannot hold eight million paths. Past it a file's
/// bytes land in the requesting process's omitted remainder and the drop is
/// counted in [`State::dropped_paths`].
const MAX_TRACKED_FILES: usize = 65_536;

/// Shortest gap between two handle-table namings of unnamed objects. A read of
/// the system handle table costs ~10–20 ms on a desktop, twice per naming (the
/// second read verifies), so a process churning through short-lived files must
/// not trigger one per poll. A process's FIRST sample is never throttled.
const SEED_INTERVAL: Duration = Duration::from_secs(2);

/// `Process` class opcode of a process start: it names the pid's
/// `UniqueProcessKey`.
const PROCESS_START: u8 = 1;
/// `Process` class opcode of a process exit: it ends the accounting of the
/// process that holds the pid; see [`State::process_ended`].
const PROCESS_END: u8 = 2;
/// The session-start rundown's start record: every process alive when the
/// session opened, with the same layout as [`PROCESS_START`].
const PROCESS_DC_START: u8 = 3;

/// How long a miss may reuse the drive list before it is rescanned. A miss is
/// normal (`\Device\Mup\...` is a network share and never maps to a drive), and
/// a scan costs one `QueryDosDeviceW` per drive letter.
const DEVICE_SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// Bytes read and written, as the trace counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileBytes {
    pub read_bytes: u64,
    pub write_bytes: u64,
}

impl FileBytes {
    fn add(&mut self, write: bool, bytes: u64) {
        if write {
            self.write_bytes = self.write_bytes.saturating_add(bytes);
        } else {
            self.read_bytes = self.read_bytes.saturating_add(bytes);
        }
    }

    fn merge(&mut self, other: FileBytes) {
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
        self.write_bytes = self.write_bytes.saturating_add(other.write_bytes);
    }
}

/// Bytes of one process that no row can carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Remainder {
    pub read_bytes: u64,
    pub write_bytes: u64,
}

impl Remainder {
    fn add(&mut self, write: bool, bytes: u64) {
        if write {
            self.write_bytes = self.write_bytes.saturating_add(bytes);
        } else {
            self.read_bytes = self.read_bytes.saturating_add(bytes);
        }
    }

    fn merge(&mut self, bytes: FileBytes) {
        self.read_bytes = self.read_bytes.saturating_add(bytes.read_bytes);
        self.write_bytes = self.write_bytes.saturating_add(bytes.write_bytes);
    }
}

/// One process's cumulative file bytes, as of one [`FileEtw::sample`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileIoSample {
    /// Whether the session is still receiving events. A session that stopped
    /// has frozen counters and must not be presented as current.
    pub running: bool,
    /// How long this process's counts have been accumulating, in milliseconds.
    pub since_ms: u64,
    /// Cumulative bytes per file path, since the session started. A file the
    /// trace knows the process has open but saw no completed transfer on is
    /// listed with zero bytes: that is a measurement.
    pub files: Vec<(String, FileBytes)>,
    /// Bytes of transfers whose file object could not be named: opened before
    /// the session and closed before the handle table could name it, a name
    /// that did not resolve to a DOS path, or a map dropped at its cap.
    pub unnamed: Remainder,
    /// Bytes of files dropped by [`MAX_TRACKED_FILES_PER_PID`] or by
    /// [`MAX_TRACKED_FILES`].
    pub omitted: Remainder,
    /// File records of the opcodes this module reads that the callback was
    /// handed.
    pub events_seen: u64,
    /// ...of which the payload decoder accepted.
    pub events_decoded: u64,
    /// Completed logical transfers decoded across the WHOLE session. Non-zero
    /// proves the subscription delivers transfers, which is what makes a
    /// process with none a measured zero rather than an unknown.
    pub session_transfers: u64,
    /// Completed Read and Write transfers decoded for THIS process.
    pub pid_read_transfers: u64,
    pub pid_write_transfers: u64,
    /// Entries discarded from the `FileObject -> name` map when it reached
    /// [`MAX_OPEN_FILES`], over the whole session.
    pub cleared_file_objects: u64,
    /// Creates whose name did not resolve to a DOS path (a network share, a
    /// `\SystemRoot\` name, a raw device), over the whole session.
    pub unnamed_creates: u64,
    /// Paths never tracked because a capacity limit (this process's or the
    /// session's) was full, over the whole session.
    pub dropped_paths: u64,
    /// Cumulative events ETW reports as lost; counts are lower bounds when
    /// this is non-zero or unknown.
    pub events_lost: Option<u64>,
    /// Whether the process was created after the session started, so its
    /// whole life is in these counts.
    pub traced_since_process_start: bool,
    /// The process's own kernel read/write transfer counters at the start of
    /// the traced window: zero for a process traced since its start, the
    /// pre-session snapshot for an older one, `None` when unknown.
    pub counter_baseline: Option<FileBytes>,
}

impl FileIoSample {
    /// Read and Write transfers decoded for this process, both directions.
    pub fn pid_transfers(&self) -> u64 {
        self.pid_read_transfers
            .saturating_add(self.pid_write_transfers)
    }

    /// Whether, and how far, this sample may be trusted.
    ///
    /// [`FileIoAccountingState::Measured`] requires that the payload layout
    /// matched AND that the session has completed at least one transfer
    /// anywhere: from then on the trace demonstrably sees every process's
    /// requests, so this process's count — zero included — is measured. Before
    /// that, silence cannot be told apart from a broken subscription.
    pub fn state(&self) -> FileIoAccountingState {
        if !self.running {
            return FileIoAccountingState::Unavailable;
        }
        if self.events_seen == 0 {
            return FileIoAccountingState::Undelivered;
        }
        if self.events_decoded == 0 {
            return FileIoAccountingState::Undecodable;
        }
        if self.session_transfers == 0 {
            return FileIoAccountingState::Undelivered;
        }
        FileIoAccountingState::Measured
    }

    /// Bytes attributed to this process, named or not: `files` plus both
    /// remainders. Rows plus [`Self::unnamed`] plus [`Self::omitted`] add up to
    /// it exactly.
    pub fn attributed(&self) -> FileBytes {
        let mut total = FileBytes::default();
        for (_, bytes) in &self.files {
            total.merge(*bytes);
        }
        total.read_bytes = total
            .read_bytes
            .saturating_add(self.unnamed.read_bytes)
            .saturating_add(self.omitted.read_bytes);
        total.write_bytes = total
            .write_bytes
            .saturating_add(self.unnamed.write_bytes)
            .saturating_add(self.omitted.write_bytes);
        total
    }
}

/// Attach a per-file trace sample to the handle rows it names.
///
/// This is the one place a [`FileIoSample`] becomes visible data, and both
/// hosts of the trace call it: the LocalSystem service for the rows it answers
/// with, and the GUI for the session it hosts itself when it is elevated and
/// no service is installed. A row therefore means the same thing whichever
/// session measured it.
///
/// Paths are compared case-insensitively: the trace's names were resolved from
/// the provider's device paths while the rows come from
/// `GetFinalPathNameByHandleW`, and neither side guarantees the other's casing.
///
/// A row with no record keeps `None` (rendered as unavailable). A record with
/// no row — a file the process opened, read or wrote, and CLOSED before the
/// handle walk ran — becomes a row of its own with no open handle, which the
/// page shows as "(closed)". This is the common shape of real per-file I/O:
/// a service that rereads its config every few seconds, a log appended through
/// open-write-close, a temporary file. Folding those into the unnamed remainder
/// made a process with plainly visible disk traffic read "0 B" on every row.
/// Rows plus the unnamed and omitted remainders stay the process total.
///
/// Several records can name ONE row: the trace's path is the provider's own
/// (resolved from a device path), the row's is the handle walk's, and the
/// caller's casing survives both, so `C:\Windows\a.dll` and `c:\windows\a.dll`
/// are two buckets for one file. Their bytes are ADDED into the row; assigning
/// would let the last bucket overwrite the first, and the row would then hold
/// less than the process total while nothing absorbed the difference.
pub(super) fn attach_file_io(
    snapshot: &mut crate::actions::ProcessFileIoSnapshot,
    sample: &FileIoSample,
) {
    let unnamed = sample.unnamed;
    let accounting = &mut snapshot.accounting;
    accounting.state = sample.state();
    accounting.since_ms = sample.since_ms;
    accounting.events_lost = sample.events_lost;
    // Why rows can read unavailable while the state is measured: these say it
    // happened, instead of leaving a user to conclude the feature is broken.
    accounting.cleared_file_objects = sample.cleared_file_objects;
    accounting.unnamed_creates = sample.unnamed_creates;
    accounting.dropped_paths = sample.dropped_paths;
    if accounting.state != FileIoAccountingState::Measured {
        // Nothing was decoded for this process, so there is nothing to attach
        // and every row stays unavailable.
        return;
    }
    // Everything the trace attributed to the process, read before the rows are
    // touched: rows plus both remainders are exactly this total.
    let attributed = sample.attributed();
    let mut index_by_path: std::collections::HashMap<String, usize> = snapshot
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.to_lowercase(), index))
        .collect();
    for (path, bytes) in &sample.files {
        let key = path.to_lowercase();
        let index = match index_by_path.get(&key) {
            Some(&index) => Some(index),
            // A file the trace knows open with nothing transferred, that the
            // walk did not list (opened after it): no row to add for a zero.
            None if *bytes == FileBytes::default() => None,
            None => {
                // Closed since the handle walk, or opened and closed between
                // two walks: its own row, marked closed by having no handle.
                index_by_path.insert(key, snapshot.files.len());
                snapshot.files.push(closed_file_row(path));
                Some(snapshot.files.len() - 1)
            }
        };
        if let Some(index) = index {
            let file = &mut snapshot.files[index];
            file.read_bytes = Some(
                file.read_bytes
                    .unwrap_or(0)
                    .saturating_add(bytes.read_bytes),
            );
            file.write_bytes = Some(
                file.write_bytes
                    .unwrap_or(0)
                    .saturating_add(bytes.write_bytes),
            );
        }
    }
    let accounting = &mut snapshot.accounting;
    accounting.traced_since_process_start = sample.traced_since_process_start;
    if let Some(baseline) = sample.counter_baseline {
        // The process's own counters over the same window the per-file counts
        // cover; a counter that went backwards is a different process and
        // stays unknown.
        accounting.counter_window_read_bytes = accounting
            .counter_read_bytes
            .and_then(|now| now.checked_sub(baseline.read_bytes));
        accounting.counter_window_write_bytes = accounting
            .counter_write_bytes
            .and_then(|now| now.checked_sub(baseline.write_bytes));
    }
    accounting.read_bytes = attributed.read_bytes;
    accounting.write_bytes = attributed.write_bytes;
    accounting.read_ops = sample.pid_read_transfers;
    accounting.write_ops = sample.pid_write_transfers;
    accounting.unnamed_read_bytes = unnamed.read_bytes;
    accounting.unnamed_write_bytes = unnamed.write_bytes;
    accounting.omitted_read_bytes = sample.omitted.read_bytes;
    accounting.omitted_write_bytes = sample.omitted.write_bytes;
}

/// A row for a file the trace measured that the process no longer holds open.
///
/// Only the path is known: size, position and access belong to an open handle
/// and stay unavailable rather than being guessed.
fn closed_file_row(path: &str) -> crate::actions::ProcessFileIo {
    crate::actions::ProcessFileIo {
        path: path.to_string(),
        read_bytes: None,
        write_bytes: None,
        read_ops: None,
        write_ops: None,
        file_size: None,
        offset: None,
        access_known: false,
        is_read: false,
        is_write: false,
        open_handles: 0,
    }
}

/// Say why no session could start, without inventing anything.
///
/// This is the counterpart of [`attach_file_io`] for the case where there is no
/// sample at all. The snapshot keeps every number the handle walk produced —
/// none of them per-file byte counts — and gains only a state, which is what
/// the dialog keys its explanation off.
///
/// Only [`FileTraceRefusal::KernelLoggerInUse`] needs saying: the snapshot
/// already starts at [`FileIoAccountingState::Unavailable`], and that is
/// exactly what [`FileTraceRefusal::Unavailable`] means. A caller that never
/// got to try (a throttled app-side start) passes `None` and gets the same
/// treatment, because "we did not ask" is not "another application has it".
///
/// Nothing here touches a byte count, so a refusal can never become a zero.
pub(super) fn attach_refusal(
    snapshot: &mut crate::actions::ProcessFileIoSnapshot,
    refusal: Option<FileTraceRefusal>,
) {
    if refusal != Some(FileTraceRefusal::KernelLoggerInUse) {
        return;
    }
    let accounting = &mut snapshot.accounting;
    accounting.state = FileIoAccountingState::KernelLoggerInUse;
    // No session ran, so no count covers any span of time.
    accounting.since_ms = 0;
}

/// One decoded record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileEvent<'a> {
    /// `name` is the raw little-endian UTF-16 tail; decoding it allocates, so
    /// it happens off the callback's borrow of the record.
    Create {
        irp: u64,
        object: u64,
        name: &'a [u8],
    },
    Close {
        irp: u64,
        object: u64,
    },
    /// A Read or Write REQUEST; its bytes are charged when it completes.
    Request {
        irp: u64,
        object: u64,
        size: u32,
        write: bool,
        paging: bool,
    },
    /// An `OpEnd`: the request on `irp` finished with `status`, moving `bytes`.
    Completion {
        irp: u64,
        bytes: u64,
        status: u32,
    },
}

/// Pointer width of a `FileIo` payload: always this machine's.
///
/// [`super::etw::pointer_bytes`] answers a different question — how wide the
/// pointers are in a payload a USER-MODE process wrote. The records here are
/// written by the kernel: `Irp`, `FileObject` and `FileKey` are kernel
/// addresses in every record, whoever issued the request, so a WOW64 caller's
/// header flag must not shift the decode.
const KERNEL_FILE_POINTER_BYTES: usize = std::mem::size_of::<usize>();

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(
        payload.get(offset..end)?.try_into().ok()?,
    ))
}

fn read_pointer(payload: &[u8], offset: usize, pointer_bytes: usize) -> Option<u64> {
    if pointer_bytes == 4 {
        return read_u32(payload, offset).map(u64::from);
    }
    let end = offset.checked_add(8)?;
    Some(u64::from_le_bytes(
        payload.get(offset..end)?.try_into().ok()?,
    ))
}

/// A kernel `FILE_OBJECT` address. The kernel allocates these from the pool, so
/// they are never null and always pointer-aligned; a misread field rarely is.
fn plausible_file_object(value: u64, pointer_bytes: usize) -> bool {
    value != 0 && value.is_multiple_of(pointer_bytes as u64)
}

/// Decode the `OpenPath` tail of a Create payload.
///
/// ETW writes the string as little-endian UTF-16 running to the end of the
/// payload, NUL-terminated, with no length prefix. An empty or odd-length tail
/// is not a name, and the caller then keeps the handle anonymous rather than
/// inventing one.
fn decode_utf16(tail: &[u8]) -> Option<String> {
    if tail.len() < 2 || !tail.len().is_multiple_of(2) {
        return None;
    }
    // Even length: the chunk remainder is empty by construction.
    let (pairs, _) = tail.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    if units.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

/// Decode one record of the six opcodes this module reads, or `None` when it
/// does not look like the version-3 template.
///
/// Every rejection here is a field that a wrong offset, a truncated payload or
/// a template change would produce, and the honest answer to all of them is
/// "unknown" — never a byte count.
fn parse_event(
    opcode: u8,
    version: u8,
    payload: &[u8],
    pointer_bytes: usize,
) -> Option<FileEvent<'_>> {
    if version != FILE_IO_VERSION {
        return None;
    }
    let p = pointer_bytes;
    match opcode {
        EVENT_CREATE => {
            let irp = read_pointer(payload, 0, p)?;
            let object = read_pointer(payload, p, p)?;
            if !plausible_file_object(object, p) {
                return None;
            }
            let name = payload.get(2 * p + 16..).unwrap_or_default();
            Some(FileEvent::Create { irp, object, name })
        }
        EVENT_READ | EVENT_WRITE => {
            if payload.len() < 8 + 3 * p + 16 {
                return None;
            }
            let irp = read_pointer(payload, 8, p)?;
            let object = read_pointer(payload, 8 + p, p)?;
            if !plausible_file_object(object, p) {
                return None;
            }
            let size = read_u32(payload, 8 + 3 * p + 4)?;
            if size > MAX_PLAUSIBLE_TRANSFER {
                return None;
            }
            let flags = read_u32(payload, 8 + 3 * p + 8)?;
            Some(FileEvent::Request {
                irp,
                object,
                size,
                write: opcode == EVENT_WRITE,
                paging: flags & IRP_PAGING_IO != 0,
            })
        }
        EVENT_CLEANUP | EVENT_CLOSE => {
            if payload.len() < 3 * p + 4 {
                return None;
            }
            let irp = read_pointer(payload, 0, p)?;
            let object = read_pointer(payload, p, p)?;
            if !plausible_file_object(object, p) {
                return None;
            }
            Some(FileEvent::Close { irp, object })
        }
        EVENT_OP_END => {
            if payload.len() < 2 * p + 4 {
                return None;
            }
            Some(FileEvent::Completion {
                irp: read_pointer(payload, 0, p)?,
                bytes: read_pointer(payload, p, p)?,
                status: read_u32(payload, 2 * p)?,
            })
        }
        _ => None,
    }
}

/// The `FileObject -> name` correlation map.
struct OpenFile {
    /// The process whose Create (or handle) produced this entry. A transfer
    /// under another pid is only charged to this name when the handle table
    /// showed that process holding the object too (`others`); see
    /// [`State::complete`].
    pid: u32,
    path: Arc<str>,
    /// Further processes the handle table showed holding this same object —
    /// an inherited or duplicated handle (a redirected stdout, a log handed to
    /// a child). Without them a child's writes to its parent's file would stay
    /// unnamed for good.
    others: Vec<u32>,
}

impl OpenFile {
    fn new(pid: u32, path: Arc<str>) -> Self {
        Self {
            pid,
            path,
            others: Vec::new(),
        }
    }

    fn held_by(&self, pid: u32) -> bool {
        self.pid == pid || self.others.contains(&pid)
    }
}

/// A Read or Write waiting for its completion.
#[derive(Debug, Clone, Copy)]
struct Pending {
    pid: u32,
    object: u64,
    write: bool,
    requested: u32,
}

/// Completed bytes on a file object that has no name yet.
struct Unresolved {
    pid: u32,
    bytes: FileBytes,
    /// Whether the handle table was already asked about this object, so a
    /// name it could not give is not asked for again on every poll.
    asked: bool,
}

/// One tracked path: its bytes, and when it last moved any (for eviction).
#[derive(Default, Clone, Copy)]
struct Tracked {
    bytes: FileBytes,
    /// [`State::clock`] at the last transfer.
    touched: u64,
}

/// Per-process accumulators.
#[derive(Default)]
struct PidFiles {
    /// The process creation time this bucket has been bound to, once a sample
    /// has named it. A pid is reused, and the counters of the process that held
    /// it before must not be handed to its successor; see [`State::bind_epoch`].
    epoch_s: Option<i64>,
    /// Whether every file handle of this process has been named from the
    /// handle table once in this session.
    handles_named: bool,
    /// Cumulative bytes per path, since the session started.
    files: HashMap<Arc<str>, Tracked>,
    /// Completed Read and Write transfers decoded for this process.
    read_transfers: u64,
    write_transfers: u64,
    unnamed: Remainder,
    omitted: Remainder,
}

#[derive(Default)]
struct State {
    events_seen: u64,
    events_decoded: u64,
    /// Opcode, template version and payload length of the most recent record
    /// the decoder rejected, so a layout change and a truncated payload can be
    /// told apart from a log line instead of a debugger.
    rejected_opcode: u8,
    rejected_version: u8,
    rejected_payload_len: u32,
    open: HashMap<u64, OpenFile>,
    pending: HashMap<u64, Pending>,
    /// Bytes waiting for a name, per object and then per process (one object
    /// can be used by several processes through inherited handles).
    unresolved: HashMap<u64, Vec<Unresolved>>,
    /// Entries across all of `unresolved`, for [`MAX_UNRESOLVED_OBJECTS`].
    unresolved_count: usize,
    pids: HashMap<u32, PidFiles>,
    /// `UniqueProcessKey` of the process currently holding a pid, from its
    /// `Process` start record (or the session-start rundown).
    ///
    /// The callback cannot tell which incarnation of a pid a record belongs to,
    /// which is why every other decision here is made from a record's own
    /// evidence. An exit record was the exception: it dropped by pid alone.
    /// Windows reuses a pid within about a second while the `Process` class
    /// itself arrives up to a flush interval (the session asks for a 1 s timer)
    /// late, so the PREVIOUS owner's exit could arrive after the pid's new
    /// owner had already written — and dropping by pid alone threw those bytes
    /// away with the bucket, so the new process read as a measured zero until
    /// its next transfer. The key the record carries settles it; a pid with no
    /// recorded key (started before the session, or its start record was lost)
    /// keeps the old behaviour: an exit drops it.
    process_keys: HashMap<u32, u64>,
    /// Distinct paths held across every bucket in `pids`. Rebuilt by
    /// [`State::prune_dead`] and [`State::bind_epoch`] after they drop buckets.
    tracked_paths: usize,
    /// Completed logical transfers across the whole session.
    session_transfers: u64,
    /// Entries discarded when the open-file map was cleared at its cap.
    cleared_file_objects: u64,
    /// Creates whose name did not resolve to a DOS path.
    unnamed_creates: u64,
    /// Paths never tracked because a capacity limit was full, or evicted to
    /// make room once closed and least recently used.
    dropped_paths: u64,
    /// Monotonic counter stamped on a path at each transfer; orders eviction.
    clock: u64,
    // Diagnostics only: logged, never shown as numbers.
    /// Paging requests skipped (flushes, lazy writes, page faults).
    paging_skipped: u64,
    /// Completions with an error status (refused fast I/O, failed requests).
    failed_completions: u64,
    /// Requests whose completion never arrived before their `Irp` was reused.
    orphaned_requests: u64,
}

impl State {
    /// Drop the accounting of processes that no longer exist.
    ///
    /// Every map is pinned to live processes for the same reason the disk
    /// trace is: a process that exited can never be asked about again, and its
    /// entries would otherwise accumulate for the lifetime of the service.
    fn prune_dead(&mut self, live_pids: &HashSet<u32>) {
        self.pids.retain(|pid, _| live_pids.contains(pid));
        self.process_keys.retain(|pid, _| live_pids.contains(pid));
        self.keep_pids(|pid| live_pids.contains(&pid));
        self.tracked_paths = self.pids.values().map(|files| files.files.len()).sum();
    }

    /// Drop every name, pending request and waiting byte of the processes
    /// `keep` rejects. An object another live process also holds keeps its
    /// name for that process.
    fn keep_pids(&mut self, keep: impl Fn(u32) -> bool) {
        self.open.retain(|_, file| {
            file.others.retain(|pid| keep(*pid));
            if keep(file.pid) {
                return true;
            }
            match file.others.pop() {
                Some(next) => {
                    file.pid = next;
                    true
                }
                None => false,
            }
        });
        self.pending.retain(|_, pending| keep(pending.pid));
        self.unresolved.retain(|_, waiting| {
            waiting.retain(|entry| keep(entry.pid));
            !waiting.is_empty()
        });
        self.unresolved_count = self.unresolved.values().map(Vec::len).sum();
    }

    /// Bind `pid`'s bucket to the process creation time the caller verified.
    ///
    /// The callback cannot know which incarnation of a pid a record belongs to;
    /// the broker does, because every request carries the creation time it
    /// sampled. Binding the bucket to it means a pid reused between two samples
    /// starts empty instead of inheriting the previous process's paths, and the
    /// `FileObject` names and pending bytes that process had are dropped for the
    /// same reason (they belong to a dead object space).
    fn bind_epoch(&mut self, pid: u32, start_epoch_s: i64) {
        let reused = self.pids.get(&pid).is_some_and(|files| {
            files
                .epoch_s
                .is_some_and(|epoch_s| epoch_s != start_epoch_s)
        });
        if reused {
            self.forget_pid(pid);
            // `process_keys[pid]` is deliberately NOT cleared with the bucket:
            // a delayed exit record for the PREVIOUS incarnation is still
            // compared against it, and forgetting it would make that record
            // look unverifiable and drop this bucket instead.
            tracing::debug!(
                pid,
                start_epoch_s,
                "file trace saw a reused pid; its bucket starts fresh"
            );
        }
        self.pids.entry(pid).or_default().epoch_s = Some(start_epoch_s);
    }

    /// Retire whatever request was pending on `irp`: a new request on the same
    /// `Irp` means the old one completed and its `OpEnd` was lost. Keeping it
    /// would charge the NEXT operation's completion as transferred bytes.
    fn retire(&mut self, irp: u64) {
        if self.pending.remove(&irp).is_some() {
            self.orphaned_requests = self.orphaned_requests.saturating_add(1);
        }
    }

    /// An object address was opened anew or closed: whatever was waiting for
    /// a name under it belongs to an object that no longer exists, so its
    /// bytes become the process's unnamed remainder.
    fn forget_unresolved(&mut self, object: u64) {
        if let Some(waiting) = self.unresolved.remove(&object) {
            self.unresolved_count = self.unresolved_count.saturating_sub(waiting.len());
            for entry in waiting {
                self.pids
                    .entry(entry.pid)
                    .or_default()
                    .unnamed
                    .merge(entry.bytes);
            }
        }
    }

    /// Charge `bytes` on `path` to `pid`, inside the path budgets.
    ///
    /// A new path at a full budget first evicts the least recently used closed
    /// paths (this process's, or the largest process's for the session-wide
    /// budget); only when nothing can be evicted are the bytes refused into
    /// the omitted remainder. Either way the drop is counted.
    fn charge_path(&mut self, pid: u32, path: Arc<str>, bytes: FileBytes) {
        let new_path = !self
            .pids
            .get(&pid)
            .is_some_and(|files| files.files.contains_key(path.as_ref()));
        if new_path {
            if self
                .pids
                .get(&pid)
                .is_some_and(|files| files.files.len() >= MAX_TRACKED_FILES_PER_PID)
            {
                self.evict(pid);
            }
            if self.tracked_paths >= MAX_TRACKED_FILES
                && let Some(largest) = self
                    .pids
                    .iter()
                    .max_by_key(|(_, files)| files.files.len())
                    .map(|(pid, _)| *pid)
            {
                self.evict(largest);
            }
            let pid_room = self
                .pids
                .get(&pid)
                .is_none_or(|files| files.files.len() < MAX_TRACKED_FILES_PER_PID);
            if !pid_room || self.tracked_paths >= MAX_TRACKED_FILES {
                // Charging them to a neighbouring file would be a made-up
                // per-file number.
                self.pids.entry(pid).or_default().omitted.merge(bytes);
                self.dropped_paths = self.dropped_paths.saturating_add(1);
                return;
            }
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        let tracked = self
            .pids
            .entry(pid)
            .or_default()
            .files
            .entry(path)
            .or_default();
        tracked.bytes.merge(bytes);
        tracked.touched = clock;
        if new_path {
            self.tracked_paths = self.tracked_paths.saturating_add(1);
        }
    }

    /// Fold the least recently used quarter of `pid`'s CLOSED paths into its
    /// omitted remainder.
    ///
    /// A quarter at a time so a process streaming through new files pays the
    /// sort once per few thousand files, not per file. Open paths are never
    /// evicted: their row is on screen and must keep counting. That includes
    /// the objects this process only holds through an inherited or duplicated
    /// handle — [`State::complete`] charges those to it, so folding them into
    /// the omitted remainder would drop a live row's bytes and recreate the
    /// row at zero on its next transfer.
    fn evict(&mut self, pid: u32) {
        let open: HashSet<Arc<str>> = self
            .open
            .values()
            .filter(|file| file.held_by(pid))
            .map(|file| Arc::clone(&file.path))
            .collect();
        let Some(files) = self.pids.get_mut(&pid) else {
            return;
        };
        let mut closed: Vec<(u64, Arc<str>)> = files
            .files
            .iter()
            .filter(|(path, _)| !open.contains(path.as_ref()))
            .map(|(path, tracked)| (tracked.touched, Arc::clone(path)))
            .collect();
        if closed.is_empty() {
            return;
        }
        closed.sort_unstable_by_key(|(touched, _)| *touched);
        let count = closed.len().div_ceil(4);
        for (_, path) in closed.into_iter().take(count) {
            if let Some(tracked) = files.files.remove(path.as_ref()) {
                files.omitted.merge(tracked.bytes);
            }
        }
        self.tracked_paths = self.tracked_paths.saturating_sub(count);
        self.dropped_paths = self.dropped_paths.saturating_add(count as u64);
    }

    /// Drop everything one pid owns: its bucket, the file names and pending
    /// requests it made, and the bytes still waiting for a name.
    ///
    /// Shared by the three things that end a pid's accounting — its exit, a
    /// start record that proves the pid changed owner, and an epoch binding that
    /// proves it between two samples — so the bookkeeping cannot drift between
    /// them.
    fn forget_pid(&mut self, pid: u32) {
        if let Some(files) = self.pids.remove(&pid) {
            self.tracked_paths = self.tracked_paths.saturating_sub(files.files.len());
        }
        self.keep_pids(|other| other != pid);
    }

    /// A `Process/Start` record: bind `pid` to the `UniqueProcessKey` that just
    /// started, and apply the reuse when the pid's recorded key is a DIFFERENT
    /// one.
    ///
    /// This is where a reused pid is cut loose, and it is not optional. The
    /// bucket holds the predecessor's paths, its bytes and its transfer counts;
    /// leaving it in place publishes all three as the live process's, and when
    /// the bucket was also bound to the predecessor's creation time,
    /// [`State::bind_epoch`] would eventually remove it and take the successor's
    /// already-measured bytes with it. Both are worse than dropping the stale
    /// bucket here, where the session can PROVE the pid changed owner.
    ///
    /// The cost is deliberate: a transfer charged between the moment the process
    /// really started and the moment this record arrives (the `Process` class can
    /// be a flush interval late) goes with the predecessor's bucket and becomes
    /// unknown. Attributing those bytes to either process would not be honest.
    ///
    /// A pid with NO recorded key keeps its bucket: the session never knew a
    /// predecessor for it, and clearing here would throw away a brand-new
    /// process's own first bytes. The mirror-image cost is that a predecessor the
    /// session never named (no start record and no rundown entry survived) can
    /// leave its paths in the bucket the successor inherits: an epoch binding
    /// ADOPTS that bucket rather than removing it — there is no bound creation
    /// time to disagree with — so only a later start record (which would prove
    /// the reuse) or [`State::prune_dead`] closes it. Until then there is no
    /// evidence to separate the two owners.
    fn process_started(&mut self, pid: u32, key: u64) {
        if self
            .process_keys
            .get(&pid)
            .is_some_and(|known| *known != key)
        {
            self.forget_pid(pid);
        }
        self.process_keys.insert(pid, key);
    }

    /// The session-start rundown's record for a process that already existed:
    /// seed the pid's key, never act as a reuse point.
    ///
    /// The rundown is a snapshot taken while the session was being created, so a
    /// process that started and took over a pid in that window delivers its own
    /// [`Self::process_started`] first; letting the stale rundown entry overwrite
    /// that key — and clear its bucket — would discard live bytes. Seeding is all
    /// it is needed for: without a key, an exit for a process that already
    /// existed could not be recognised as its own.
    fn process_seeded(&mut self, pid: u32, key: u64) {
        self.process_keys.entry(pid).or_insert(key);
    }

    /// A `Process/End` record: everything the process owned in the session goes,
    /// so a pid reused later starts from nothing.
    ///
    /// The exit record carries the `UniqueProcessKey` of the process that
    /// actually exited ([`Self::process_keys`]), and a mismatched one is IGNORED
    /// rather than applied: it names an earlier holder of the pid — Windows
    /// reuses pids quickly and this class can be a flush interval late — so the
    /// bucket in place belongs to the process that holds the pid now, and that
    /// process's start record already dropped the predecessor's (see
    /// [`Self::process_started`]). The live owner's own exit record is the one
    /// that drops it.
    ///
    /// A pid with no recorded key drops on any exit, as before. `on_event` always
    /// has one — the key at offset 0 is readable whenever the pid at `P` is — so
    /// `None` only comes from a caller that had no record to read.
    fn process_ended(&mut self, pid: u32, key: Option<u64>) {
        if let (Some(known), Some(ending)) = (self.process_keys.get(&pid).copied(), key)
            && known != ending
        {
            tracing::trace!(
                pid,
                "file trace ignored a process exit that names an earlier holder of the pid"
            );
            return;
        }
        self.process_keys.remove(&pid);
        self.forget_pid(pid);
    }

    /// A completed logical transfer of `bytes` by `pid` on `object`.
    fn complete(&mut self, pid: u32, object: u64, write: bool, bytes: u64) {
        self.session_transfers = self.session_transfers.saturating_add(1);
        {
            let files = self.pids.entry(pid).or_default();
            if write {
                files.write_transfers = files.write_transfers.saturating_add(1);
            } else {
                files.read_transfers = files.read_transfers.saturating_add(1);
            }
        }
        let mut amount = FileBytes::default();
        amount.add(write, bytes);
        if let Some(open) = self.open.get(&object)
            && open.held_by(pid)
        {
            let path = Arc::clone(&open.path);
            self.charge_path(pid, path, amount);
            return;
        }
        // Nameless for THIS process — never seen, or named for another process
        // (an inherited or duplicated handle, or a reused address after a
        // missed Close). Hold the bytes under (object, process) until the
        // handle table can say whether this process holds that object (see
        // [`FileEtw::sample`]), within budget.
        let waiting = self.unresolved.entry(object).or_default();
        if let Some(entry) = waiting.iter_mut().find(|entry| entry.pid == pid) {
            entry.bytes.add(write, bytes);
            return;
        }
        if self.unresolved_count >= MAX_UNRESOLVED_OBJECTS {
            if waiting.is_empty() {
                self.unresolved.remove(&object);
            }
            self.pids.entry(pid).or_default().unnamed.add(write, bytes);
            return;
        }
        waiting.push(Unresolved {
            pid,
            bytes: amount,
            asked: false,
        });
        self.unresolved_count = self.unresolved_count.saturating_add(1);
    }

    /// Apply names the handle table gave for `pid`'s file objects: each one
    /// becomes known for later transfers, and bytes that waited for it move
    /// onto its path.
    fn apply_names(&mut self, pid: u32, names: Vec<(u64, String)>) {
        for (object, path) in names {
            let path: Arc<str> = Arc::from(path);
            // A Create the trace saw is the better witness for the NAME; the
            // table only adds this process as a holder of that object. Never
            // clear the map for a name that is only a seed.
            let name = match self.open.get_mut(&object) {
                Some(open) => {
                    if !open.held_by(pid) {
                        open.others.push(pid);
                    }
                    Arc::clone(&open.path)
                }
                None => {
                    if self.open.len() < MAX_OPEN_FILES {
                        self.open
                            .insert(object, OpenFile::new(pid, Arc::clone(&path)));
                    }
                    path
                }
            };
            let waited = self.unresolved.get_mut(&object).and_then(|waiting| {
                let index = waiting.iter().position(|entry| entry.pid == pid)?;
                Some(waiting.swap_remove(index))
            });
            if self
                .unresolved
                .get(&object)
                .is_some_and(|waiting| waiting.is_empty())
            {
                self.unresolved.remove(&object);
            }
            if let Some(entry) = waited {
                self.unresolved_count = self.unresolved_count.saturating_sub(1);
                self.charge_path(pid, name, entry.bytes);
            }
        }
    }
}

/// Device path prefix -> drive letter, resolved on demand and cached.
#[derive(Default)]
struct DeviceCache {
    /// `None` until the drive list has been resolved once. Holds the moment of
    /// the last scan with the list it produced.
    loaded: Option<(Instant, Vec<(String, String)>)>,
}

struct Shared {
    state: Mutex<State>,
    devices: Mutex<DeviceCache>,
    live: AtomicBool,
}

impl TraceContext for Shared {
    fn stop(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// A file record the decoder could not read. Counted, never guessed at.
    fn undecodable(&self, opcode: u8, version: u8, payload_len: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let mut state = self.lock();
        state.events_seen = state.events_seen.saturating_add(1);
        state.rejected_opcode = opcode;
        state.rejected_version = version;
        state.rejected_payload_len = payload_len;
    }

    /// Account one decoded record. A Create's name is resolved before the
    /// state lock is taken because resolving can rescan the drive list.
    fn record(&self, pid: u32, event: FileEvent<'_>) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let resolved = match event {
            FileEvent::Create { name, .. } => Some(self.resolve_name(name)),
            _ => None,
        };
        let mut state = self.lock();
        state.events_seen = state.events_seen.saturating_add(1);
        state.events_decoded = state.events_decoded.saturating_add(1);
        match event {
            FileEvent::Create { irp, object, .. } => {
                state.retire(irp);
                // A Create on an address means whatever object held it before
                // is gone: its name and its waiting bytes must not pass on.
                state.open.remove(&object);
                state.forget_unresolved(object);
                let Some(path) = resolved.flatten() else {
                    // Not a resolvable device or DOS path (a network share, a
                    // `\SystemRoot\` name, a raw device): its bytes stay
                    // unnamed. Nothing is inserted, so it must not cost the map
                    // anything at the cap either.
                    state.unnamed_creates = state.unnamed_creates.saturating_add(1);
                    return;
                };
                if state.open.len() >= MAX_OPEN_FILES {
                    // A dropped Close leaves an entry behind, so the map can
                    // reach the cap with nothing that is safe to evict.
                    // Clearing recovers the memory; the files still open lose
                    // their names (counted) until they are opened again.
                    state.cleared_file_objects = state
                        .cleared_file_objects
                        .saturating_add(state.open.len() as u64);
                    state.open.clear();
                    tracing::debug!(
                        cap = MAX_OPEN_FILES,
                        "file trace dropped its open-file map at the cap"
                    );
                }
                state.open.insert(object, OpenFile::new(pid, path));
            }
            FileEvent::Close { irp, object } => {
                state.retire(irp);
                state.open.remove(&object);
                state.forget_unresolved(object);
            }
            FileEvent::Request {
                irp,
                object,
                size,
                write,
                paging,
            } => {
                state.retire(irp);
                if paging {
                    state.paging_skipped = state.paging_skipped.saturating_add(1);
                    return;
                }
                if state.pending.len() >= MAX_PENDING_REQUESTS {
                    let dropped = state.pending.len() as u64;
                    state.orphaned_requests = state.orphaned_requests.saturating_add(dropped);
                    state.pending.clear();
                }
                state.pending.insert(
                    irp,
                    Pending {
                        pid,
                        object,
                        write,
                        requested: size,
                    },
                );
            }
            FileEvent::Completion { irp, bytes, status } => {
                // Most completions belong to operations this module does not
                // count (creates, queries, closes); only a pending transfer
                // matches.
                let Some(request) = state.pending.remove(&irp) else {
                    return;
                };
                if (status as i32) < 0 {
                    // Refused fast I/O (retried as an IRP, which completes on
                    // its own) or a failed request: nothing was transferred.
                    state.failed_completions = state.failed_completions.saturating_add(1);
                    return;
                }
                // A completion cannot move more than was asked for; a larger
                // number is a misread, and the request size bounds it.
                let moved = bytes.min(u64::from(request.requested));
                state.complete(request.pid, request.object, request.write, moved);
            }
        }
    }

    /// A `Process` start record: the pid's process key, and the reuse a changed
    /// key proves; see [`State::process_started`].
    fn process_started(&self, pid: u32, key: u64) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        self.lock().process_started(pid, key);
    }

    /// The session-start rundown's record for a process that already existed:
    /// seed the pid's key only; see [`State::process_seeded`].
    fn process_seeded(&self, pid: u32, key: u64) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        self.lock().process_seeded(pid, key);
    }

    /// A `Process/End` record: drop everything `pid` owned, unless that pid
    /// belongs to a later process than the one that exited; see
    /// [`State::process_ended`].
    fn process_ended(&self, pid: u32, key: Option<u64>) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        self.lock().process_ended(pid, key);
    }

    /// Another `FileIo` request on `irp`: retire what was pending on it.
    fn other_request(&self, irp: u64) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let mut state = self.lock();
        if state.pending.contains_key(&irp) {
            state.retire(irp);
        }
    }

    /// The DOS path for a Create's file name, or `None` when it cannot be
    /// resolved.
    fn resolve_name(&self, name: &[u8]) -> Option<Arc<str>> {
        let name = decode_utf16(name)?;
        let mut cache = match self.devices.lock() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((_, devices)) = cache.loaded.as_ref()
            && let Some(path) = dos_path(&name, devices)
        {
            return Some(Arc::from(path));
        }
        // A miss is normal, so rescanning per event would cost a
        // `QueryDosDeviceW` per drive letter on every network-share path.
        if cache
            .loaded
            .as_ref()
            .is_some_and(|(scanned, _)| scanned.elapsed() < DEVICE_SCAN_INTERVAL)
        {
            return None;
        }
        let devices = volume_devices();
        let resolved = dos_path(&name, &devices).map(Arc::from);
        cache.loaded = Some((Instant::now(), devices));
        resolved
    }
}

/// Translate a kernel file name into the DOS path the handle walk reports.
///
/// `\Device\HarddiskVolume3\Users\me\a.txt` becomes `C:\Users\me\a.txt` for the
/// drive whose `QueryDosDeviceW` target is that device, `\??\C:\...` loses its
/// NT prefix, and a UNC name is already what `GetFinalPathNameByHandleW` would
/// return. Anything else stays unresolved rather than being approximated: the
/// device list is matched longest-first so a volume whose name is a prefix of
/// another cannot win.
fn dos_path(name: &str, devices: &[(String, String)]) -> Option<String> {
    if let Some(rest) = name.strip_prefix(r"\??\") {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if name.starts_with(r"\\") {
        return Some(name.to_string());
    }
    for (device, drive) in devices {
        if let Some(rest) = name.strip_prefix(device.as_str())
            && (rest.starts_with('\\') || rest.is_empty())
        {
            return Some(format!("{drive}{rest}"));
        }
    }
    None
}

/// `\Device\HarddiskVolume3 -> C:` for every drive that currently has a device
/// path, longest device name first.
fn volume_devices() -> Vec<(String, String)> {
    let mut devices = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", char::from(letter));
        let wide: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buffer = vec![0u16; 512];
        let len = unsafe { QueryDosDeviceW(PCWSTR(wide.as_ptr()), Some(&mut buffer)) };
        if len == 0 {
            continue;
        }
        // The API returns the length it wrote, which a hostile or changed
        // answer could report past the buffer; slicing would then panic.
        let Some(units) = buffer.get(..len as usize) else {
            continue;
        };
        let target = String::from_utf16_lossy(units);
        let target = target.trim_end_matches('\0');
        if target.is_empty() {
            continue;
        }
        devices.push((target.to_string(), drive));
    }
    // Longest first: `\Device\HarddiskVirtualDisk1` must not be matched by a
    // shorter prefix that happens to share its first characters.
    devices.sort_by_key(|(device, _)| std::cmp::Reverse(device.len()));
    devices
}

/// How a sample names file objects the trace has no Create for; see
/// [`super::process_file_io::name_file_objects`]. A seam so the accounting
/// around it is testable without a privileged handle-table read.
type ObjectNamer<'a> = dyn Fn(u32, i64, Option<&HashSet<u64>>) -> Vec<(u64, String)> + 'a;

/// A running per-file I/O trace.
pub struct FileEtw {
    session: Session<Shared>,
    started: Instant,
    /// Which of [`sources`] actually started. Kept because "which session is
    /// carrying this" is the first question any report about the feature has to
    /// answer, and it cannot be re-derived from a live session afterwards.
    source: FileSource,
    /// When unnamed objects were last named from the handle table.
    last_naming: Mutex<Option<Instant>>,
    /// Wall clock at session start, in the 100-ns Unix ticks process
    /// creation identities use: a process created later has had its whole
    /// life traced.
    started_unix_ticks: i64,
    /// pid -> (creation ticks, read bytes, write bytes) of every process that
    /// existed just before the session started; see the module docs.
    baselines: HashMap<u32, (i64, u64, u64)>,
}

/// Now, in 100-ns ticks since the Unix epoch.
fn unix_ticks_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            (since.as_nanos() / 100).min(i64::MAX as u128) as i64
        })
}

impl FileEtw {
    /// Which session is carrying this trace; see [`sources`].
    pub(super) fn source(&self) -> FileSource {
        self.source
    }

    /// Whether events are still arriving; see [`Session::is_running`].
    pub fn is_running(&self) -> bool {
        self.session.is_running()
    }

    /// Start a session, trying [`sources`] in order.
    ///
    /// The first shape that starts wins and is logged. A refusal is only the
    /// answer once EVERY shape was refused, and
    /// [`FileTraceRefusal::KernelLoggerInUse`] is preferred over
    /// [`FileTraceRefusal::Unavailable`] in that case: "another application
    /// owns the kernel logger" is the fact that explains the whole failure and
    /// the only one a user can act on.
    pub fn try_start(role: TraceRole) -> Result<Self, FileTraceRefusal> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            devices: Mutex::new(DeviceCache::default()),
            live: AtomicBool::new(true),
        });
        let mut refusal = FileTraceRefusal::Unavailable;
        // Taken BEFORE the session starts, so a process's counter delta over
        // the traced window can only include more than the trace saw, never
        // less.
        let baselines = super::process_file_io::snapshot_io_counters();
        for source in sources(role) {
            match Session::try_start(
                source.name,
                source.kind,
                &[],
                Arc::clone(&shared),
                on_event,
                "tm-file-etw",
            ) {
                Ok(session) => {
                    tracing::info!(
                        session = source.name,
                        kind = ?source.kind,
                        "per-file I/O trace started"
                    );
                    return Ok(Self {
                        session,
                        started: Instant::now(),
                        source,
                        last_naming: Mutex::new(None),
                        // After the start: "created later" must mean the
                        // session saw the process's first request.
                        started_unix_ticks: unix_ticks_now(),
                        baselines,
                    });
                }
                Err(error) => {
                    // `ERROR_ALREADY_EXISTS` on the PER-ROLE name would mean a
                    // session that keeps being taken back by another process,
                    // not that the kernel logger is somebody else's; only the
                    // kernel attempt can report the state that names an owner.
                    if source.kind == LoggerKind::KernelFile && error == StartError::AlreadyExists {
                        tracing::warn!(
                            session = source.name,
                            "the NT Kernel Logger is owned by another application"
                        );
                        refusal = FileTraceRefusal::KernelLoggerInUse;
                    } else {
                        tracing::debug!(
                            session = source.name,
                            kind = ?source.kind,
                            %error,
                            "per-file I/O session attempt refused"
                        );
                    }
                }
            }
        }
        Err(refusal)
    }

    /// Everything the trace has attributed to `pid` since it started.
    ///
    /// Cumulative, not a window: the rows it fills are documented as totals
    /// since accounting began and the dialog merges two snapshots by keeping
    /// the larger count, so a per-window delta would be read as a total.
    ///
    /// Names file objects the trace saw no Create for from the system handle
    /// table first (see the module docs): on the process's first sample every
    /// file handle it holds, later only objects that carry unnamed bytes, at
    /// most once per [`SEED_INTERVAL`].
    ///
    /// An EMPTY `live_pids` means "the caller could not enumerate processes",
    /// never "nothing is alive": pruning against it would throw away every
    /// measurement. `start_epoch_s` is the creation time the caller verified
    /// for `pid`, and it is what keeps a reused pid from inheriting the
    /// previous process's counters (see [`State::bind_epoch`]).
    pub fn sample(&self, pid: u32, start_epoch_s: i64, live_pids: &HashSet<u32>) -> FileIoSample {
        self.sample_with(
            pid,
            start_epoch_s,
            live_pids,
            &super::process_file_io::name_file_objects,
        )
    }

    /// [`Self::sample`] with the handle-table naming injectable.
    fn sample_with(
        &self,
        pid: u32,
        start_epoch_s: i64,
        live_pids: &HashSet<u32>,
        namer: &ObjectNamer<'_>,
    ) -> FileIoSample {
        let shared = self.session.shared();
        let request = {
            let mut state = shared.lock();
            if !live_pids.is_empty() {
                state.prune_dead(live_pids);
            }
            state.bind_epoch(pid, start_epoch_s);
            let mut last = match self.last_naming.lock() {
                Ok(last) => last,
                Err(poisoned) => poisoned.into_inner(),
            };
            naming_request(&mut state, pid, &mut last, Instant::now())
        };
        if let Some(wanted) = request {
            // Outside the lock: duplicating handles and reading the table
            // must not stall the ETW callback.
            let names = namer(pid, start_epoch_s, wanted.as_ref());
            let named = names.len();
            shared.lock().apply_names(pid, names);
            tracing::debug!(
                pid,
                all = wanted.is_none(),
                asked = wanted.as_ref().map(HashSet::len),
                named,
                "file trace named objects from the handle table"
            );
        }
        let (mut sample, diagnostics) = {
            let state = shared.lock();
            let since_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let sample = sample_from(&state, pid, self.session.is_running(), since_ms);
            let diagnostics = (
                state.rejected_opcode,
                state.rejected_version,
                state.rejected_payload_len,
                state.paging_skipped,
                state.failed_completions,
                state.orphaned_requests,
            );
            (sample, diagnostics)
        };
        if sample.events_seen > 0 && sample.events_decoded == 0 {
            // Loud on purpose, once per request while it lasts: this is the
            // failure that silently turns every byte count into a gap.
            tracing::warn!(
                events_seen = sample.events_seen,
                opcode = diagnostics.0,
                version = diagnostics.1,
                payload_len = diagnostics.2,
                "file trace decoded no records - unknown FileIo payload layout or rejected payloads"
            );
        }
        tracing::trace!(
            pid,
            session_transfers = sample.session_transfers,
            paging_skipped = diagnostics.3,
            failed_completions = diagnostics.4,
            orphaned_requests = diagnostics.5,
            "file trace sample"
        );
        let coverage = self.coverage(pid, start_epoch_s);
        sample.traced_since_process_start = coverage.0;
        sample.counter_baseline = coverage.1;
        // Queried outside the lock: the session properties live in the kernel.
        sample.events_lost = self.session.events_lost();
        sample
    }

    /// Whether `pid` (created at `start_epoch_s`) has been traced since its
    /// start, and its counters at the start of the traced window.
    fn coverage(&self, pid: u32, start_epoch_s: i64) -> (bool, Option<FileBytes>) {
        coverage_of(pid, start_epoch_s, self.started_unix_ticks, &self.baselines)
    }

    /// Housekeeping for a long-running session: drop what belongs to
    /// processes that no longer exist. Process exit records do this as they
    /// arrive; this is the backstop for one ETW dropped. An EMPTY `live_pids`
    /// means the caller could not enumerate processes and prunes nothing.
    pub fn maintain(&self, live_pids: &HashSet<u32>) {
        if live_pids.is_empty() {
            return;
        }
        self.session.shared().lock().prune_dead(live_pids);
    }
}

/// See [`FileEtw::coverage`]; free so it is testable without a session.
fn coverage_of(
    pid: u32,
    start_epoch_s: i64,
    started_unix_ticks: i64,
    baselines: &HashMap<u32, (i64, u64, u64)>,
) -> (bool, Option<FileBytes>) {
    if start_epoch_s > started_unix_ticks {
        return (true, Some(FileBytes::default()));
    }
    let baseline = baselines
        .get(&pid)
        .filter(|(created, _, _)| {
            tm_core::model::creation_timestamps_match(start_epoch_s, *created)
        })
        .map(|(_, read_bytes, write_bytes)| FileBytes {
            read_bytes: *read_bytes,
            write_bytes: *write_bytes,
        });
    (false, baseline)
}

/// One process's view of the accumulators: its rows, its remainders and the
/// session-wide counters that qualify them.
fn sample_from(state: &State, pid: u32, running: bool, since_ms: u64) -> FileIoSample {
    let files = state.pids.get(&pid);
    let mut paths: Vec<(String, FileBytes)> = files
        .map(|files| {
            files
                .files
                .iter()
                .map(|(path, tracked)| (path.to_string(), tracked.bytes))
                .collect()
        })
        .unwrap_or_default();
    // Files the trace knows this process has open, with no completed
    // transfer: a measured zero, which is different from a row the
    // trace knows nothing about.
    let mut known: HashSet<&str> = paths.iter().map(|(path, _)| path.as_str()).collect();
    let mut idle = Vec::new();
    for open in state.open.values() {
        if open.pid == pid && known.insert(open.path.as_ref()) {
            idle.push((open.path.to_string(), FileBytes::default()));
        }
    }
    drop(known);
    paths.extend(idle);
    let mut unnamed = files.map_or_else(Remainder::default, |files| files.unnamed);
    for waiting in state.unresolved.values().flatten() {
        if waiting.pid == pid {
            unnamed.merge(waiting.bytes);
        }
    }
    FileIoSample {
        running,
        since_ms,
        files: paths,
        unnamed,
        omitted: files.map_or_else(Remainder::default, |files| files.omitted),
        events_seen: state.events_seen,
        events_decoded: state.events_decoded,
        session_transfers: state.session_transfers,
        pid_read_transfers: files.map_or(0, |files| files.read_transfers),
        pid_write_transfers: files.map_or(0, |files| files.write_transfers),
        cleared_file_objects: state.cleared_file_objects,
        unnamed_creates: state.unnamed_creates,
        dropped_paths: state.dropped_paths,
        events_lost: None,
        traced_since_process_start: false,
        counter_baseline: None,
    }
}

/// Whether this sample should name file objects from the handle table, and
/// which: `Some(None)` for every file handle of `pid` (its first sample in this
/// session), `Some(Some(objects))` for objects with unnamed bytes that were not
/// asked about yet, `None` for neither.
///
/// The first naming of a process is never throttled — it is what makes the
/// first view of a long-running process complete. Later namings wait for
/// [`SEED_INTERVAL`]; objects are marked as asked only when a naming actually
/// happens, so a throttled poll loses nothing.
fn naming_request(
    state: &mut State,
    pid: u32,
    last: &mut Option<Instant>,
    now: Instant,
) -> Option<Option<HashSet<u64>>> {
    let files = state.pids.entry(pid).or_default();
    if !files.handles_named {
        files.handles_named = true;
        for waiting in state.unresolved.values_mut().flatten() {
            if waiting.pid == pid {
                waiting.asked = true;
            }
        }
        *last = Some(now);
        return Some(None);
    }
    if last.is_some_and(|at| now.saturating_duration_since(at) < SEED_INTERVAL) {
        return None;
    }
    let mut wanted = HashSet::new();
    for (object, waiting) in &mut state.unresolved {
        for entry in waiting.iter_mut() {
            if entry.pid == pid && !entry.asked {
                entry.asked = true;
                wanted.insert(*object);
            }
        }
    }
    if wanted.is_empty() {
        return None;
    }
    *last = Some(now);
    Some(Some(wanted))
}

/// ETW record callback. Kept minimal; only a Create allocates.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(shared) = (unsafe { etw::context_of::<Shared>(record) }) else {
        return;
    };
    if record.EventHeader.ProviderId == ProcessGuid {
        // A start names the pid's `UniqueProcessKey` and is where a proven reuse
        // drops the predecessor's accounting; the rundown only seeds a key; an
        // exit ends the accounting of the process that holds the pid, which the
        // key is what decides (see [`State::process_started`] and
        // [`State::process_ended`]). None of them says anything about file I/O,
        // so a bucket is still created by that process's first transfer, and
        // coverage comes from its creation time.
        let opcode = record.EventHeader.EventDescriptor.Opcode;
        if !matches!(opcode, PROCESS_START | PROCESS_END | PROCESS_DC_START) {
            return;
        }
        let Some(payload) = (unsafe { etw::payload_of(record) }) else {
            return;
        };
        // `UniqueProcessKey` at 0, the pid at `P`. A payload too short for
        // either is no identity evidence, and both cases keep the old
        // behaviour: an exit with no readable key drops the bucket.
        let Some(pid) = read_u32(payload, KERNEL_FILE_POINTER_BYTES) else {
            return;
        };
        let key = read_pointer(payload, 0, KERNEL_FILE_POINTER_BYTES);
        match opcode {
            PROCESS_END => shared.process_ended(pid, key),
            // A start is the reuse point: a key that differs from the pid's
            // recorded one proves the pid changed owner; see
            // [`State::process_started`].
            PROCESS_START => {
                if let Some(key) = key {
                    shared.process_started(pid, key);
                }
            }
            // Only the rundown is left after the filter above, and it may never
            // act as a reuse point; see [`State::process_seeded`].
            _ => {
                if let Some(key) = key {
                    shared.process_seeded(pid, key);
                }
            }
        }
        return;
    }
    // Classic (MOF) identity: the class GUID in `ProviderId`, the operation in
    // the opcode. No manifest event id is filled in for these records.
    if record.EventHeader.ProviderId != FileIoGuid {
        return;
    }
    let opcode = record.EventHeader.EventDescriptor.Opcode;
    let version = record.EventHeader.EventDescriptor.Version;
    // A file event without a payload is exactly as undecodable as a short one,
    // so it is counted rather than dropped.
    let payload = unsafe { etw::payload_of(record) }.unwrap_or_default();
    handle_payload(
        shared,
        record.EventHeader.ProcessId,
        opcode,
        version,
        payload,
    );
}

/// Decode one record's payload and account it.
///
/// Split from the callback so a test can drive the decode and the accounting
/// without a live session. The payload width is pinned to
/// [`KERNEL_FILE_POINTER_BYTES`] here rather than taken from the record header;
/// a WOW64 caller's flag says nothing about a kernel-written payload.
fn handle_payload(shared: &Shared, pid: u32, opcode: u8, version: u8, payload: &[u8]) {
    match opcode {
        EVENT_CREATE | EVENT_CLEANUP | EVENT_CLOSE | EVENT_READ | EVENT_WRITE | EVENT_OP_END => {
            match parse_event(opcode, version, payload, KERNEL_FILE_POINTER_BYTES) {
                Some(event) => shared.record(pid, event),
                None => shared.undecodable(opcode, version, payload.len() as u32),
            }
        }
        _ if is_other_request(opcode) && version == FILE_IO_VERSION => {
            if let Some(irp) = read_pointer(payload, 0, KERNEL_FILE_POINTER_BYTES) {
                shared.other_request(irp);
            }
        }
        _ => {}
    }
}

/// Fixed session name per role. See [`super::etw`]: this MUST NOT include the
/// pid, or an orphaned session can never be reclaimed.
pub(super) fn session_name(role: TraceRole) -> &'static str {
    match role {
        TraceRole::Service => "TaskMan-File-Service",
        TraceRole::App => "TaskMan-File-App",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: usize = KERNEL_FILE_POINTER_BYTES;
    /// The `Irp` every synthetic request in these tests is issued on, unless a
    /// test needs two in flight.
    const IRP: u64 = 0x1111_1111_1111_1110;
    /// `STATUS_FLT_DISALLOW_FAST_IO`, the refusal the live probe saw before a
    /// cached write was retried as an IRP.
    const STATUS_FLT_DISALLOW_FAST_IO: u32 = 0xC01C_0004;
    /// `IoFlags` of a paging write as the live probe saw it
    /// (`IRP_NOCACHE | IRP_PAGING_IO | IRP_SYNCHRONOUS_PAGING_IO` plus the
    /// upper ETW bits).
    const PAGING_FLAGS: u32 = 0x0006_0043;

    /// `Create` payload, version 3.
    fn create_payload_on(irp: u64, object: u64, name: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&irp.to_le_bytes()[..P]); // Irp
        payload.extend_from_slice(&object.to_le_bytes()[..P]); // FileObject
        payload.extend_from_slice(&1_000u32.to_le_bytes()); // TTID
        payload.extend_from_slice(&0u32.to_le_bytes()); // CreateOptions
        payload.extend_from_slice(&0u32.to_le_bytes()); // FileAttributes
        payload.extend_from_slice(&3u32.to_le_bytes()); // ShareAccess
        for unit in name.encode_utf16() {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
        payload.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
        payload
    }

    fn create_payload(object: u64, name: &str) -> Vec<u8> {
        create_payload_on(0x2222_2222_2222_2220, object, name)
    }

    /// `Read`/`Write` payload, version 3.
    fn request_payload_on(irp: u64, object: u64, size: u32, flags: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4_096u64.to_le_bytes()); // Offset
        payload.extend_from_slice(&irp.to_le_bytes()[..P]); // Irp
        payload.extend_from_slice(&object.to_le_bytes()[..P]); // FileObject
        payload.extend(std::iter::repeat_n(0x22u8, P)); // FileKey
        payload.extend_from_slice(&1_000u32.to_le_bytes()); // TTID
        payload.extend_from_slice(&size.to_le_bytes()); // IoSize
        payload.extend_from_slice(&flags.to_le_bytes()); // IoFlags
        payload.extend_from_slice(&0u32.to_le_bytes()); // ExtraFlags
        payload
    }

    fn request_payload(object: u64, size: u32) -> Vec<u8> {
        request_payload_on(IRP, object, size, 0)
    }

    /// `Cleanup`/`Close` payload, version 3.
    fn close_payload(object: u64) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x3333_3333_3333_3330u64.to_le_bytes()[..P]); // Irp
        payload.extend_from_slice(&object.to_le_bytes()[..P]); // FileObject
        payload.extend(std::iter::repeat_n(0x22u8, P)); // FileKey
        payload.extend_from_slice(&1_000u32.to_le_bytes()); // TTID
        payload
    }

    /// `OpEnd` payload, version 3.
    fn op_end_payload(irp: u64, bytes: u64, status: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&irp.to_le_bytes()[..P]); // Irp
        payload.extend_from_slice(&bytes.to_le_bytes()[..P]); // ExtraInfo
        payload.extend_from_slice(&status.to_le_bytes()); // NtStatus
        payload
    }

    /// `Process` class start/exit payload, version 4: `UniqueProcessKey`, then
    /// the pid.
    fn process_payload(key: u64, pid: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&key.to_le_bytes()[..P]); // UniqueProcessKey
        payload.extend_from_slice(&pid.to_le_bytes()); // ProcessId
        payload.extend_from_slice(&[0u8; 60]); // the rest of the template
        payload
    }

    /// Drive a `Process` class record of `opcode` through the real callback.
    fn deliver_process(shared: &Shared, opcode: u8, key: u64, pid: u32) {
        deliver_record_as(
            shared,
            windows::Win32::System::Diagnostics::Etw::ProcessGuid,
            opcode,
            4,
            pid,
            &process_payload(key, pid),
        );
    }

    fn shared() -> Shared {
        Shared {
            state: Mutex::new(State::default()),
            devices: Mutex::new(DeviceCache::default()),
            live: AtomicBool::new(true),
        }
    }

    /// The callback's work, minus the ETW plumbing: decode, then account. A
    /// Read or Write request is followed by its successful completion for the
    /// full requested size, which is what an ordinary cached transfer looks
    /// like; tests about completions drive [`handle_payload`] themselves.
    fn deliver(shared: &Shared, pid: u32, opcode: u8, payload: &[u8]) {
        handle_payload(shared, pid, opcode, FILE_IO_VERSION, payload);
        if matches!(opcode, EVENT_READ | EVENT_WRITE) {
            let irp = read_pointer(payload, 8, P).expect("a request carries its Irp");
            let size = read_u32(payload, 8 + 3 * P + 4).expect("and its size");
            complete(shared, irp, u64::from(size), 0);
        }
    }

    fn complete(shared: &Shared, irp: u64, bytes: u64, status: u32) {
        handle_payload(
            shared,
            // `OpEnd` is logged in whatever context completes the request; the
            // pid in its header is not what the bytes are charged to.
            4,
            EVENT_OP_END,
            FILE_IO_VERSION,
            &op_end_payload(irp, bytes, status),
        );
    }

    /// Drive the REAL callback with a synthetic record of `provider`'s classic
    /// class: the class GUID in `ProviderId`, the operation in the opcode, and
    /// no manifest event id.
    fn deliver_record_as(
        shared: &Shared,
        provider: windows::core::GUID,
        opcode: u8,
        version: u8,
        pid: u32,
        payload: &[u8],
    ) {
        // SAFETY: `EVENT_RECORD` is plain data, and a zeroed one is a record
        // with no payload, no context and an all-zero descriptor — every field
        // this test cares about is set below.
        let mut record: EVENT_RECORD = unsafe { std::mem::zeroed() };
        record.EventHeader.ProviderId = provider;
        record.EventHeader.EventDescriptor.Opcode = opcode;
        record.EventHeader.EventDescriptor.Version = version;
        // Deliberately zero: the classic identity carries no event id.
        record.EventHeader.EventDescriptor.Id = 0;
        record.EventHeader.ProcessId = pid;
        record.UserData = payload.as_ptr() as *mut core::ffi::c_void;
        record.UserDataLength = payload.len() as u16;
        record.UserContext = shared as *const Shared as *mut core::ffi::c_void;
        // SAFETY: both pointers stay alive across the call and the callback
        // copies nothing out of them.
        unsafe { on_event(&mut record) };
    }

    fn deliver_record(shared: &Shared, opcode: u8, pid: u32, payload: &[u8]) {
        deliver_record_as(shared, FileIoGuid, opcode, FILE_IO_VERSION, pid, payload);
    }

    fn sample_of(shared: &Shared, pid: u32) -> FileIoSample {
        sample_from(&shared.lock(), pid, true, 0)
    }

    fn bytes(sample: &FileIoSample, path: &str) -> Option<FileBytes> {
        sample
            .files
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, bytes)| *bytes)
    }

    fn read(read_bytes: u64) -> Option<FileBytes> {
        Some(FileBytes {
            read_bytes,
            write_bytes: 0,
        })
    }

    fn written(write_bytes: u64) -> Option<FileBytes> {
        Some(FileBytes {
            read_bytes: 0,
            write_bytes,
        })
    }

    /// The identity the live kernel delivers, end to end through the REAL
    /// callback: `FileIoGuid`, opcodes 64/67/68/76/66, version 3.
    ///
    /// Regression: the decoder accepted opcodes 12..16 — the manifest
    /// provider's EVENT IDS — and threw away every classic record that
    /// arrived, so the page read "a trace is running but has not delivered a
    /// file event yet" forever.
    #[test]
    fn the_live_classic_identity_decodes_into_a_measured_sample() {
        let shared = shared();
        deliver_record(
            &shared,
            EVENT_CREATE,
            20,
            &create_payload(0x1000, r"\??\C:\data\a.bin"),
        );
        deliver_record(&shared, EVENT_READ, 20, &request_payload(0x1000, 4_096));
        deliver_record(&shared, EVENT_OP_END, 4, &op_end_payload(IRP, 4_096, 0));
        deliver_record(&shared, EVENT_WRITE, 20, &request_payload(0x1000, 512));
        deliver_record(&shared, EVENT_OP_END, 4, &op_end_payload(IRP, 512, 0));
        let sample = sample_of(&shared, 20);
        assert_eq!(sample.events_seen, 5);
        assert_eq!(sample.events_decoded, 5);
        assert_eq!(sample.state(), FileIoAccountingState::Measured);
        assert_eq!(
            bytes(&sample, r"C:\data\a.bin"),
            Some(FileBytes {
                read_bytes: 4_096,
                write_bytes: 512,
            })
        );
        // Close forgets the object, so a later transfer through the address is
        // not charged to the file that had it.
        deliver_record(&shared, EVENT_CLOSE, 20, &close_payload(0x1000));
        deliver_record(&shared, EVENT_READ, 20, &request_payload(0x1000, 128));
        deliver_record(&shared, EVENT_OP_END, 4, &op_end_payload(IRP, 128, 0));
        let sample = sample_of(&shared, 20);
        assert_eq!(
            bytes(&sample, r"C:\data\a.bin"),
            Some(FileBytes {
                read_bytes: 4_096,
                write_bytes: 512,
            })
        );
        assert_eq!(
            sample.unnamed,
            Remainder {
                read_bytes: 128,
                write_bytes: 0,
            }
        );
    }

    /// The manifest event ids the old decoder matched on are NOT file records
    /// in the classic class: a record under them is ignored, not counted as a
    /// file event, and certainly not decoded as a transfer.
    #[test]
    fn the_manifest_event_ids_are_not_classic_file_opcodes() {
        let shared = shared();
        for old in 12u8..=16 {
            deliver_record(&shared, old, 20, &request_payload(0x1000, 4_096));
        }
        let state = shared.lock();
        assert_eq!(state.events_seen, 0);
        assert!(state.pending.is_empty());
        drop(state);
        // And a record of another class with a file opcode is not ours either.
        deliver_record_as(
            &shared,
            windows::Win32::System::Diagnostics::Etw::DiskIoGuid,
            EVENT_READ,
            FILE_IO_VERSION,
            20,
            &request_payload(0x1000, 4_096),
        );
        assert_eq!(shared.lock().events_seen, 0);
    }

    /// The session prefers its own private logger and keeps the exclusive
    /// kernel logger as the fallback; neither enables a provider (the flags are
    /// the subscription), and the two hosts never share the private name.
    #[test]
    fn the_private_session_is_tried_before_the_kernel_logger() {
        for role in [TraceRole::Service, TraceRole::App] {
            let [primary, fallback] = sources(role);
            assert_eq!(primary.kind, LoggerKind::SystemFile);
            assert_eq!(primary.name, session_name(role));
            assert_ne!(primary.name, KERNEL_LOGGER);
            assert_eq!(fallback.kind, LoggerKind::KernelFile);
            assert_eq!(fallback.name, KERNEL_LOGGER);
        }
        assert_ne!(
            sources(TraceRole::Service)[0].name,
            sources(TraceRole::App)[0].name
        );
    }

    /// A refusal that names the kernel logger's owner becomes that state, and
    /// no byte count is invented: the snapshot keeps the handle walk's rows
    /// exactly as they were and gains only a state.
    #[test]
    fn a_kernel_logger_refusal_becomes_the_state_that_names_its_owner() {
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![file_row(r"C:\data\a.bin"), file_row(r"C:\data\b.bin")],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_refusal(&mut snapshot, Some(FileTraceRefusal::KernelLoggerInUse));
        for file in &snapshot.files {
            assert_eq!(file.read_bytes, None, "never a fabricated zero");
            assert_eq!(file.write_bytes, None);
            assert_eq!(file.read_ops, None);
            assert_eq!(file.write_ops, None);
        }
        let accounting = &snapshot.accounting;
        assert_eq!(accounting.state, FileIoAccountingState::KernelLoggerInUse);
        assert_eq!(accounting.since_ms, 0, "no session ran for any span");
        assert_eq!(accounting.read_bytes, 0);
        assert_eq!(accounting.write_bytes, 0);

        // Every other answer leaves the snapshot at `Unavailable`: "we did not
        // get to ask" and "no elevation" are not somebody else's ownership.
        for refusal in [Some(FileTraceRefusal::Unavailable), None] {
            let mut snapshot = crate::actions::ProcessFileIoSnapshot {
                files: vec![file_row(r"C:\data\a.bin")],
                accounting: crate::actions::FileIoAccounting::default(),
            };
            attach_refusal(&mut snapshot, refusal);
            assert_eq!(
                snapshot.accounting.state,
                FileIoAccountingState::Unavailable,
                "{refusal:?}"
            );
            assert_eq!(snapshot.files[0].read_bytes, None);
        }
    }

    /// A cached write the filter manager refuses on the fast path is logged
    /// twice: the refused attempt and the IRP retry, on the same `Irp`. Only
    /// the completion that moved bytes counts.
    ///
    /// Regression: counting REQUESTS charged the probe's 2 x 64 KiB of writes
    /// as 192 KiB.
    #[test]
    fn a_refused_fast_io_attempt_and_its_retry_count_once() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\w.bin"),
        );
        // The sequence the live probe recorded for one 64 KiB write.
        handle_payload(
            &shared,
            20,
            EVENT_WRITE,
            FILE_IO_VERSION,
            &request_payload_on(IRP, 0x1000, 65_536, 0),
        );
        complete(&shared, IRP, 0, STATUS_FLT_DISALLOW_FAST_IO);
        handle_payload(
            &shared,
            20,
            EVENT_WRITE,
            FILE_IO_VERSION,
            &request_payload_on(IRP, 0x1000, 65_536, 0x0006_0a00),
        );
        complete(&shared, IRP, 65_536, 0);
        let sample = sample_of(&shared, 20);
        assert_eq!(bytes(&sample, r"C:\data\w.bin"), written(65_536));
        assert_eq!(sample.pid_write_transfers, 1, "one write, not two");
        assert_eq!(shared.lock().failed_completions, 1);
    }

    /// The bytes charged are the bytes the completion reports: a read at end of
    /// file asks for more than it gets.
    #[test]
    fn a_short_read_counts_the_bytes_it_completed_with() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\short.bin"),
        );
        handle_payload(
            &shared,
            20,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 100_000),
        );
        complete(&shared, IRP, 131, 0);
        assert_eq!(
            bytes(&sample_of(&shared, 20), r"C:\data\short.bin"),
            read(131)
        );
        // A completion can never exceed the request: a larger number is a
        // misread and is bounded by the requested size.
        handle_payload(
            &shared,
            20,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 10),
        );
        complete(&shared, IRP, 1 << 40, 0);
        assert_eq!(
            bytes(&sample_of(&shared, 20), r"C:\data\short.bin"),
            read(141)
        );
    }

    /// The completion is charged to the process that ISSUED the request, not
    /// to whatever context the `OpEnd` was logged in.
    #[test]
    fn a_completion_is_charged_to_the_requesting_process() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\async.bin"),
        );
        handle_payload(
            &shared,
            20,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 8_192),
        );
        // `complete` logs the OpEnd under pid 4.
        complete(&shared, IRP, 8_192, 0);
        assert_eq!(
            bytes(&sample_of(&shared, 20), r"C:\data\async.bin"),
            read(8_192)
        );
        assert_eq!(sample_of(&shared, 4).pid_transfers(), 0);
    }

    /// Paging I/O — a flush of cached data, the lazy writer, a page fault — is
    /// not a logical transfer and would charge cached bytes a second time.
    ///
    /// Regression: the probe's `FlushFileBuffers` after 2 x 64 KiB of writes
    /// added a 128 KiB paging write to the same process.
    #[test]
    fn paging_io_is_not_counted() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\flushed.bin"),
        );
        deliver(&shared, 20, EVENT_WRITE, &request_payload(0x1000, 65_536));
        handle_payload(
            &shared,
            20,
            EVENT_WRITE,
            FILE_IO_VERSION,
            &request_payload_on(0x4444_4444_4444_4440, 0x1000, 131_072, PAGING_FLAGS),
        );
        complete(&shared, 0x4444_4444_4444_4440, 131_072, 0);
        let sample = sample_of(&shared, 20);
        assert_eq!(bytes(&sample, r"C:\data\flushed.bin"), written(65_536));
        assert_eq!(sample.pid_write_transfers, 1);
        assert_eq!(shared.lock().paging_skipped, 1);
    }

    /// An `Irp` is reused once its request is done. When a request's
    /// completion was lost, the next operation on that `Irp` — of ANY kind —
    /// retires it, so the next completion is never charged as the lost
    /// transfer's bytes.
    #[test]
    fn a_request_whose_completion_was_lost_is_never_charged_later() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\lost.bin"),
        );
        handle_payload(
            &shared,
            20,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 4_096),
        );
        // A query-information request (opcode 74) reuses the Irp; its
        // completion reports 56 bytes of information, not of file data.
        let mut query = Vec::new();
        query.extend_from_slice(&IRP.to_le_bytes()[..P]);
        query.extend_from_slice(&0x1000u64.to_le_bytes()[..P]);
        query.extend(std::iter::repeat_n(0u8, 24));
        handle_payload(&shared, 20, 74, FILE_IO_VERSION, &query);
        complete(&shared, IRP, 56, 0);
        let sample = sample_of(&shared, 20);
        assert_eq!(sample.pid_transfers(), 0);
        assert_eq!(
            bytes(&sample, r"C:\data\lost.bin"),
            Some(FileBytes::default())
        );
        assert_eq!(shared.lock().orphaned_requests, 1);

        // The same holds for a Create on the Irp.
        handle_payload(
            &shared,
            20,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 4_096),
        );
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload_on(IRP, 0x2000, r"\??\C:\data\other.bin"),
        );
        complete(&shared, IRP, 128, 0);
        assert_eq!(sample_of(&shared, 20).pid_transfers(), 0);
    }

    /// A process with no completed transfer is a MEASURED zero once the
    /// session has proven it delivers transfers; before that it is unknown.
    #[test]
    fn a_process_without_io_is_measured_once_the_session_delivers() {
        let shared = shared();
        // Only creates so far: the subscription has not proven it carries
        // transfers.
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\idle.bin"),
        );
        let idle = sample_of(&shared, 20);
        assert_eq!(idle.state(), FileIoAccountingState::Undelivered);
        // Another process transfers: now silence from pid 20 is a measurement.
        deliver(
            &shared,
            30,
            EVENT_CREATE,
            &create_payload(0x2000, r"\??\C:\data\busy.bin"),
        );
        deliver(&shared, 30, EVENT_READ, &request_payload(0x2000, 10));
        let idle = sample_of(&shared, 20);
        assert_eq!(idle.state(), FileIoAccountingState::Measured);
        assert_eq!(idle.attributed(), FileBytes::default());
        // The file it has open is known to the trace: a zero, not a gap.
        assert_eq!(
            bytes(&idle, r"C:\data\idle.bin"),
            Some(FileBytes::default())
        );
    }

    #[test]
    fn a_read_only_handle_reports_only_read_bytes() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\a.bin"),
        );
        deliver(&shared, 20, EVENT_READ, &request_payload(0x1000, 4_096));
        deliver(&shared, 20, EVENT_READ, &request_payload(0x1000, 512));
        let sample = sample_of(&shared, 20);
        assert_eq!(bytes(&sample, r"C:\data\a.bin"), read(4_608));
        assert_eq!(sample.unnamed, Remainder::default());
        assert_eq!(sample.omitted, Remainder::default());
    }

    #[test]
    fn a_write_only_handle_reports_only_write_bytes() {
        let shared = shared();
        deliver(
            &shared,
            24,
            EVENT_CREATE,
            &create_payload(0x2000, r"\??\C:\data\b.bin"),
        );
        deliver(&shared, 24, EVENT_WRITE, &request_payload(0x2000, 8_192));
        assert_eq!(
            bytes(&sample_of(&shared, 24), r"C:\data\b.bin"),
            written(8_192)
        );
    }

    /// A handle opened before the session has no Create in it. Its bytes wait
    /// under the object, the handle table names the object, and the bytes that
    /// arrived BEFORE the name move onto it — later ones land there directly.
    #[test]
    fn a_handle_opened_before_the_trace_is_named_from_the_handle_table() {
        let shared = shared();
        deliver(&shared, 30, EVENT_WRITE, &request_payload(0x4000, 8_192));
        deliver(&shared, 30, EVENT_READ, &request_payload(0x4000, 4_096));
        let sample = sample_of(&shared, 30);
        assert!(sample.files.is_empty(), "nothing is charged to a guess");
        assert_eq!(
            sample.unnamed,
            Remainder {
                read_bytes: 4_096,
                write_bytes: 8_192,
            },
            "waiting bytes are part of the process total meanwhile"
        );
        let mut state = shared.lock();
        state.apply_names(30, vec![(0x4000, r"C:\db\store.db".to_string())]);
        drop(state);
        deliver(&shared, 30, EVENT_WRITE, &request_payload(0x4000, 100));
        let sample = sample_of(&shared, 30);
        assert_eq!(
            bytes(&sample, r"C:\db\store.db"),
            Some(FileBytes {
                read_bytes: 4_096,
                write_bytes: 8_292,
            })
        );
        assert_eq!(sample.unnamed, Remainder::default());
    }

    /// A name from the handle table never overrides a Create the trace saw,
    /// and never belongs to another process's bytes.
    #[test]
    fn a_handle_table_name_is_only_a_fallback() {
        let shared = shared();
        deliver(
            &shared,
            30,
            EVENT_CREATE,
            &create_payload(0x4000, r"\??\C:\seen.bin"),
        );
        // Bytes on an unnamed object of ANOTHER process.
        deliver(&shared, 31, EVENT_READ, &request_payload(0x5000, 64));
        let mut state = shared.lock();
        state.apply_names(
            30,
            vec![
                (0x4000, r"C:\guess.bin".to_string()),
                (0x5000, r"C:\theirs.bin".to_string()),
            ],
        );
        drop(state);
        deliver(&shared, 30, EVENT_READ, &request_payload(0x4000, 10));
        assert_eq!(bytes(&sample_of(&shared, 30), r"C:\seen.bin"), read(10));
        let other = sample_of(&shared, 31);
        assert!(other.files.is_empty());
        assert_eq!(other.unnamed.read_bytes, 64);
    }

    /// An unnamed object that is closed before it could be named keeps its
    /// bytes, as unnamed; a new object at the same address starts clean.
    #[test]
    fn a_closed_unnamed_object_keeps_its_bytes_as_unnamed() {
        let shared = shared();
        deliver(&shared, 30, EVENT_READ, &request_payload(0x4000, 512));
        deliver(&shared, 30, EVENT_CLOSE, &close_payload(0x4000));
        assert!(shared.lock().unresolved.is_empty());
        deliver(
            &shared,
            30,
            EVENT_CREATE,
            &create_payload(0x4000, r"\??\C:\new.bin"),
        );
        deliver(&shared, 30, EVENT_READ, &request_payload(0x4000, 8));
        let sample = sample_of(&shared, 30);
        assert_eq!(bytes(&sample, r"C:\new.bin"), read(8));
        assert_eq!(sample.unnamed.read_bytes, 512);
    }

    /// The first sample of a process names every file handle it holds; later
    /// samples name only objects with waiting bytes, at most once per
    /// interval, and never the same object twice.
    #[test]
    fn handle_table_naming_is_bounded() {
        let mut state = State::default();
        let mut last = None;
        let start = Instant::now();
        assert_eq!(
            naming_request(&mut state, 30, &mut last, start),
            Some(None),
            "the first sample names every handle"
        );
        assert_eq!(naming_request(&mut state, 30, &mut last, start), None);
        state.complete(30, 0x4000, false, 10);
        assert_eq!(
            naming_request(&mut state, 30, &mut last, start + SEED_INTERVAL / 2),
            None,
            "throttled"
        );
        let wanted = naming_request(&mut state, 30, &mut last, start + SEED_INTERVAL)
            .expect("due")
            .expect("only the waiting objects");
        assert_eq!(wanted, HashSet::from([0x4000]));
        assert_eq!(
            naming_request(&mut state, 30, &mut last, start + 3 * SEED_INTERVAL),
            None,
            "an object is asked about once"
        );
        // Another process's first sample is never throttled.
        assert_eq!(
            naming_request(&mut state, 31, &mut last, start + 3 * SEED_INTERVAL),
            Some(None)
        );
    }

    /// Bytes are attributed to the process in the request header: the same
    /// file object used by another process is not its name to inherit.
    #[test]
    fn bytes_are_attributed_to_the_header_process() {
        let shared = shared();
        let path = r"\??\C:\data\shared.bin";
        deliver(&shared, 40, EVENT_CREATE, &create_payload(0x5000, path));
        deliver(&shared, 40, EVENT_READ, &request_payload(0x5000, 4_096));
        // A handle duplicated into another process resolves the same object
        // but belongs to that process's accounting, not to the creator's.
        deliver(&shared, 44, EVENT_READ, &request_payload(0x5000, 512));
        assert_eq!(
            bytes(&sample_of(&shared, 40), r"C:\data\shared.bin"),
            read(4_096)
        );
        let other = sample_of(&shared, 44);
        assert!(
            other.files.is_empty(),
            "a foreign pid must not inherit the name"
        );
        assert_eq!(other.unnamed.read_bytes, 512);
        assert_eq!(other.pid_transfers(), 1);
    }

    /// The open-file map is capped, and what it discards is counted rather than
    /// silently lost. A Create that will not be inserted must not pay that
    /// price.
    #[test]
    fn the_open_file_map_is_capped_and_counts_what_it_drops() {
        let shared = shared();
        for index in 0..MAX_OPEN_FILES {
            let path = format!(r"\??\C:\f{index}");
            deliver(
                &shared,
                8,
                EVENT_CREATE,
                &create_payload(0x10_0000 + index as u64 * 8, &path),
            );
        }
        assert_eq!(shared.lock().open.len(), MAX_OPEN_FILES);
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x30_0000, r"\SystemRoot\unresolvable"),
        );
        {
            let state = shared.lock();
            assert_eq!(state.open.len(), MAX_OPEN_FILES, "nothing was inserted");
            assert_eq!(state.cleared_file_objects, 0, "nothing was cleared");
            assert_eq!(state.unnamed_creates, 1);
        }
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x20_0000, r"\??\C:\last"),
        );
        let state = shared.lock();
        assert_eq!(state.open.len(), 1, "the map restarts bounded");
        assert_eq!(state.cleared_file_objects, MAX_OPEN_FILES as u64);
        assert!(state.open.contains_key(&0x20_0000));
    }

    /// In-flight requests are bounded too, and the drop is counted.
    #[test]
    fn pending_requests_are_capped() {
        let shared = shared();
        for index in 0..=MAX_PENDING_REQUESTS as u64 {
            handle_payload(
                &shared,
                8,
                EVENT_READ,
                FILE_IO_VERSION,
                &request_payload_on(0x100_0000 + index * 8, 0x1000, 1, 0),
            );
        }
        let state = shared.lock();
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.orphaned_requests, MAX_PENDING_REQUESTS as u64);
    }

    /// The session-wide path budget is a real bound and a real count: a file
    /// that cannot be tracked is charged to the omitted remainder, never to a
    /// neighbouring row and never dropped on the floor.
    #[test]
    fn the_session_path_budget_drops_into_the_omitted_remainder() {
        let shared = shared();
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\budget.bin"),
        );
        shared.lock().tracked_paths = MAX_TRACKED_FILES;
        deliver(&shared, 8, EVENT_WRITE, &request_payload(0x1000, 4_096));
        let sample = sample_of(&shared, 8);
        assert_eq!(
            bytes(&sample, r"C:\data\budget.bin"),
            Some(FileBytes::default()),
            "the open file is known, but its bytes had no room"
        );
        assert_eq!(sample.omitted.write_bytes, 4_096);
        assert_eq!(sample.dropped_paths, 1);
        assert_eq!(sample.attributed().write_bytes, 4_096);

        shared.lock().tracked_paths = MAX_TRACKED_FILES - 1;
        deliver(&shared, 8, EVENT_WRITE, &request_payload(0x1000, 256));
        let sample = sample_of(&shared, 8);
        assert_eq!(
            bytes(&sample, r"C:\data\budget.bin"),
            written(256),
            "the rejected transfer is not replayed, only this one counts"
        );
        assert_eq!(shared.lock().tracked_paths, MAX_TRACKED_FILES);
    }

    /// A pid reused between two samples belongs to a different process, so the
    /// previous process's paths, open file objects, pending requests and
    /// waiting bytes must not carry over.
    #[test]
    fn a_reused_pid_starts_a_fresh_bucket() {
        let shared = shared();
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\old.bin"),
        );
        deliver(&shared, 8, EVENT_READ, &request_payload(0x1000, 4_096));
        deliver(&shared, 8, EVENT_READ, &request_payload(0x7000, 64));
        handle_payload(
            &shared,
            8,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 1),
        );
        {
            let mut state = shared.lock();
            state.bind_epoch(8, 1_000);
            assert_eq!(state.pids.get(&8).unwrap().epoch_s, Some(1_000));
            assert_eq!(state.tracked_paths, 1);
            state.bind_epoch(8, 2_000);
            assert_eq!(state.pids.get(&8).unwrap().epoch_s, Some(2_000));
            assert!(state.pids.get(&8).unwrap().files.is_empty());
            assert!(!state.open.contains_key(&0x1000));
            assert!(state.pending.is_empty());
            assert!(state.unresolved.is_empty());
            assert_eq!(state.tracked_paths, 0);
        }
        deliver(&shared, 8, EVENT_READ, &request_payload(0x1000, 512));
        let sample = sample_of(&shared, 8);
        assert!(sample.files.is_empty(), "the old name is not inherited");
        assert_eq!(sample.unnamed.read_bytes, 512);
        // Re-binding the same creation time keeps the bucket.
        let mut state = shared.lock();
        state.bind_epoch(8, 2_000);
        assert_eq!(state.pids.get(&8).unwrap().read_transfers, 1);
    }

    /// Per-process tracking is capped too, and a file beyond the cap never
    /// appears as a truncated row.
    #[test]
    fn per_process_files_are_capped_into_the_omitted_remainder() {
        let shared = shared();
        for index in 0..MAX_TRACKED_FILES_PER_PID {
            let path = format!(r"\??\C:\g{index}");
            let object = 0x10_0000 + index as u64 * 8;
            deliver(&shared, 8, EVENT_CREATE, &create_payload(object, &path));
            deliver(&shared, 8, EVENT_WRITE, &request_payload(object, 256));
        }
        let object = 0x90_0000;
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(object, r"\??\C:\over"),
        );
        deliver(&shared, 8, EVENT_READ, &request_payload(object, 4_096));
        let sample = sample_of(&shared, 8);
        assert_eq!(bytes(&sample, r"C:\over"), Some(FileBytes::default()));
        assert_eq!(sample.omitted.read_bytes, 4_096);
        assert_eq!(sample.attributed().read_bytes, 4_096);
        assert_eq!(sample.dropped_paths, 1);
    }

    /// The payload is written by the kernel, so its pointers are native width
    /// even when the record is flagged as coming from a 32-bit caller.
    #[test]
    fn a_32_bit_header_flag_does_not_narrow_a_kernel_payload() {
        let record = EVENT_RECORD {
            EventHeader: windows::Win32::System::Diagnostics::Etw::EVENT_HEADER {
                // EVENT_HEADER_FLAG_32_BIT_HEADER, as a WOW64 caller's record
                // carries it.
                Flags: 0x0020,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(etw::pointer_bytes(&record), 4);
        assert_eq!(KERNEL_FILE_POINTER_BYTES, std::mem::size_of::<usize>());
        let payload = request_payload(0x8000, 4_096);
        // Four-byte pointers would read a different object out of the record.
        assert_ne!(
            parse_event(EVENT_READ, FILE_IO_VERSION, &payload, 4),
            parse_event(EVENT_READ, FILE_IO_VERSION, &payload, P)
        );
    }

    /// The state ladder: no session is unavailable, a session that delivered
    /// nothing or no transfer yet is undelivered, a session whose records all
    /// failed to decode is undecodable, and a session with a completed transfer
    /// anywhere is measured.
    #[test]
    fn the_state_follows_what_the_session_has_proven() {
        let empty = FileIoSample {
            running: true,
            ..FileIoSample::default()
        };
        assert_eq!(empty.state(), FileIoAccountingState::Undelivered);
        let broken = FileIoSample {
            running: true,
            events_seen: 4_000,
            ..FileIoSample::default()
        };
        assert_eq!(broken.state(), FileIoAccountingState::Undecodable);
        let creates_only = FileIoSample {
            running: true,
            events_seen: 3,
            events_decoded: 3,
            ..FileIoSample::default()
        };
        assert_eq!(creates_only.state(), FileIoAccountingState::Undelivered);
        let measured = FileIoSample {
            running: true,
            events_seen: 3,
            events_decoded: 3,
            session_transfers: 1,
            ..FileIoSample::default()
        };
        assert_eq!(measured.state(), FileIoAccountingState::Measured);
        let stopped = FileIoSample {
            running: false,
            ..measured
        };
        assert_eq!(stopped.state(), FileIoAccountingState::Unavailable);
    }

    /// Undecodable records are counted, never charged, and the opcode, version
    /// and payload length of the last rejection are kept for the log line.
    #[test]
    fn undecodable_records_are_counted_but_never_charged() {
        let shared = shared();
        // Zeroing the file object fails the plausibility check.
        deliver(&shared, 8, EVENT_CREATE, &create_payload(0, r"\??\C:\z"));
        // An implausible size.
        handle_payload(
            &shared,
            8,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, u32::MAX),
        );
        // A known record in an unknown template version.
        handle_payload(&shared, 8, EVENT_READ, 2, &request_payload(0x1000, 64));
        // A truncated completion; its identity is the most recent rejection.
        handle_payload(&shared, 8, EVENT_OP_END, FILE_IO_VERSION, &[0u8; 12]);
        let state = shared.lock();
        assert_eq!(state.events_seen, 4);
        assert_eq!(state.events_decoded, 0);
        assert!(state.pending.is_empty());
        assert_eq!(state.rejected_opcode, EVENT_OP_END);
        assert_eq!(state.rejected_version, FILE_IO_VERSION);
        assert_eq!(state.rejected_payload_len, 12);
    }

    /// The opcode that RETIRES a pending request must never be the one that
    /// completes it.
    ///
    /// Regression risk this pins: the "other request" range was `69..=86`,
    /// which contains `OpEnd` (76). It only escaped retiring a transfer
    /// because [`handle_payload`] matches the decoded opcodes first, so any
    /// reordering of its arms would have dropped every completed transfer's
    /// bytes with no counter changing.
    #[test]
    fn the_completion_opcode_is_not_an_other_request() {
        assert!(
            !is_other_request(EVENT_OP_END),
            "a completion completes a request; it does not retire one"
        );
        for decoded in [
            EVENT_CREATE,
            EVENT_CLEANUP,
            EVENT_CLOSE,
            EVENT_READ,
            EVENT_WRITE,
        ] {
            assert!(!is_other_request(decoded), "decoded opcode {decoded}");
        }
        // The request family around the completion still retires.
        for request in [69u8, 70, 74, 75, 77, 78, 86] {
            assert!(is_other_request(request), "opcode {request}");
        }
        // Name records and the ends of the range are not requests.
        for other in [0u8, 32, 35, 36, 68, 87, 255] {
            assert!(!is_other_request(other), "opcode {other}");
        }
        // And driving the real dispatch: a completion for a pending request
        // still charges its bytes.
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\kept.bin"),
        );
        handle_payload(
            &shared,
            20,
            EVENT_READ,
            FILE_IO_VERSION,
            &request_payload(0x1000, 4_096),
        );
        handle_payload(
            &shared,
            20,
            EVENT_OP_END,
            FILE_IO_VERSION,
            &op_end_payload(IRP, 4_096, 0),
        );
        assert_eq!(
            bytes(&sample_of(&shared, 20), r"C:\data\kept.bin"),
            read(4_096)
        );
    }

    /// The version-3 layouts, pinned against the bytes the live probe dumped
    /// (x64): a 100 000-byte read and a 64 KiB write on one file object, its
    /// Cleanup, and an `OpEnd`.
    #[test]
    fn the_layouts_match_the_records_the_kernel_wrote() {
        if P != 8 {
            return;
        }
        let hex = |text: &str| -> Vec<u8> {
            let text: String = text.split_whitespace().collect();
            (0..text.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
                .collect()
        };
        let read = hex(
            "0000000000000000 f8f0abbd07d0ffff 602c08ea07d0ffff 70ab6cf48c80ffff \
             50580000a0860100 0000000000000000",
        );
        assert_eq!(
            parse_event(EVENT_READ, 3, &read, 8),
            Some(FileEvent::Request {
                irp: 0xffff_d007_bdab_f0f8,
                object: 0xffff_d007_ea08_2c60,
                size: 100_000,
                write: false,
                paging: false,
            })
        );
        let write = hex(
            "0000000000000000 f8f0abbd07d0ffff 602c08ea07d0ffff 70ab6cf48c80ffff \
             5058000000000100 000a060000000000",
        );
        assert_eq!(
            parse_event(EVENT_WRITE, 3, &write, 8),
            Some(FileEvent::Request {
                irp: 0xffff_d007_bdab_f0f8,
                object: 0xffff_d007_ea08_2c60,
                size: 65_536,
                write: true,
                paging: false,
            })
        );
        let cleanup = hex("f8f0abbd07d0ffff 602c08ea07d0ffff 70ab6cf48c80ffff 50580000");
        assert_eq!(
            parse_event(EVENT_CLEANUP, 3, &cleanup, 8),
            Some(FileEvent::Close {
                irp: 0xffff_d007_bdab_f0f8,
                object: 0xffff_d007_ea08_2c60,
            })
        );
        // The refused fast-I/O attempt's completion.
        let op_end = hex("385119d507d0ffff 0000000000000000 04001cc0");
        assert_eq!(
            parse_event(EVENT_OP_END, 3, &op_end, 8),
            Some(FileEvent::Completion {
                irp: 0xffff_d007_d519_5138,
                bytes: 0,
                status: STATUS_FLT_DISALLOW_FAST_IO,
            })
        );
        // Every other version is a gap, not a guess.
        for version in [0u8, 1, 2, 4] {
            assert_eq!(parse_event(EVENT_READ, version, &read, 8), None);
        }
    }

    /// A `Process/End` record ends everything the process owned in the
    /// session, so a pid reused before the next sample starts from nothing.
    ///
    /// Regression risk this pins: an always-on session lives far longer than
    /// one dialog, and the epoch binding alone only resets a bucket once a
    /// sample names the new process.
    #[test]
    fn a_process_exit_drops_its_whole_accounting() {
        let shared = shared();
        deliver(
            &shared,
            30,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\gone.bin"),
        );
        deliver(&shared, 30, EVENT_READ, &request_payload(0x1000, 512));
        deliver(&shared, 30, EVENT_READ, &request_payload(0x7000, 64));
        handle_payload(
            &shared,
            30,
            EVENT_WRITE,
            FILE_IO_VERSION,
            &request_payload(0x1000, 8),
        );
        // Another process is untouched.
        deliver(
            &shared,
            31,
            EVENT_CREATE,
            &create_payload(0x2000, r"\??\C:\data\kept.bin"),
        );
        deliver(&shared, 31, EVENT_READ, &request_payload(0x2000, 16));
        // `Process/End` v4: UniqueProcessKey, then the exiting pid.
        let mut end = Vec::new();
        end.extend_from_slice(&0xffff_d007_0000_0040u64.to_le_bytes()[..P]);
        end.extend_from_slice(&30u32.to_le_bytes());
        end.extend_from_slice(&[0u8; 60]);
        deliver_record_as(
            &shared,
            windows::Win32::System::Diagnostics::Etw::ProcessGuid,
            PROCESS_END,
            4,
            30,
            &end,
        );
        {
            let state = shared.lock();
            assert!(!state.pids.contains_key(&30));
            assert!(state.open.values().all(|file| file.pid != 30));
            assert!(state.pending.values().all(|pending| pending.pid != 30));
            assert!(
                state
                    .unresolved
                    .values()
                    .flatten()
                    .all(|waiting| waiting.pid != 30)
            );
            assert_eq!(state.tracked_paths, 1, "only pid 31's path is left");
        }
        assert_eq!(
            bytes(&sample_of(&shared, 31), r"C:\data\kept.bin"),
            read(16)
        );
        // The pid comes back as a new process: nothing is inherited.
        deliver(&shared, 30, EVENT_READ, &request_payload(0x1000, 4));
        let reborn = sample_of(&shared, 30);
        assert!(reborn.files.is_empty());
        assert_eq!(reborn.pid_transfers(), 1);
    }

    /// A reused pid must publish NOTHING of its predecessor, and a delayed exit
    /// of that predecessor must not erase what the live owner measured.
    ///
    /// Windows reuses a pid within about a second and the `Process` class
    /// arrives up to a flush interval late, so the states are: the predecessor's
    /// exit can arrive after the successor's start, and the successor can write
    /// before its own start record lands. The successor's `Process/Start` is the
    /// proof that the pid changed owner, so it is where the predecessor's bucket
    /// goes — the alternative (keeping it and only ignoring the exit) published
    /// the dead process's paths, bytes and transfer count as the live process's,
    /// and when the bucket was epoch-bound, the next binding removed it anyway
    /// and took the successor's bytes with it.
    ///
    /// The accepted cost is visible here: the predecessor's 64-byte read is
    /// GONE (not inherited), and the successor's own write, made after its start
    /// record, is kept. `pid_transfers()` is 1, not 2 — the old expectation
    /// asserted the fabricated attribution.
    #[test]
    fn a_reused_pid_never_inherits_its_predecessor() {
        const OLD_KEY: u64 = 0xffff_d007_0000_0040;
        const NEW_KEY: u64 = 0xffff_d007_0000_0100;
        let shared = shared();
        // The rundown seeded the predecessor's key, and it measured a transfer.
        deliver_process(&shared, PROCESS_DC_START, OLD_KEY, 50);
        deliver(
            &shared,
            50,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\old\gone.bin"),
        );
        deliver(&shared, 50, EVENT_READ, &request_payload(0x1000, 64));
        assert_eq!(
            bytes(&sample_of(&shared, 50), r"C:\old\gone.bin"),
            read(64),
            "the predecessor was measured while it lived"
        );

        // The pid is reused. Its start record is the reuse point: everything the
        // predecessor measured goes with it.
        deliver_process(&shared, PROCESS_START, NEW_KEY, 50);
        let emptied = sample_of(&shared, 50);
        assert!(emptied.files.is_empty(), "no inherited row");
        assert_eq!(emptied.pid_transfers(), 0, "no inherited transfer count");

        // The new owner opens and writes its own file, and only THEN does the
        // old owner's exit record arrive.
        deliver(
            &shared,
            50,
            EVENT_CREATE,
            &create_payload(0x2000, r"\??\C:\new\kept.bin"),
        );
        deliver(&shared, 50, EVENT_WRITE, &request_payload(0x2000, 40));
        deliver_process(&shared, PROCESS_END, OLD_KEY, 50);
        let sample = sample_of(&shared, 50);
        assert_eq!(
            bytes(&sample, r"C:\new\kept.bin"),
            written(40),
            "the live owner keeps the bytes this session measured"
        );
        assert_eq!(
            bytes(&sample, r"C:\old\gone.bin"),
            None,
            "the predecessor's path is not the live owner's row"
        );
        assert_eq!(sample.pid_transfers(), 1, "only the live owner's write");
        assert_eq!(
            shared.lock().process_keys.get(&50).copied(),
            Some(NEW_KEY),
            "the live owner's key is the one on record"
        );

        // The live owner's own exit drops everything.
        deliver_process(&shared, PROCESS_END, NEW_KEY, 50);
        let sample = sample_of(&shared, 50);
        assert!(sample.files.is_empty());
        assert_eq!(sample.pid_transfers(), 0);
        assert!(!shared.lock().process_keys.contains_key(&50));

        // A pid with NO recorded key cannot be told apart, so an exit still
        // drops it: the old behaviour survives where there is no evidence.
        deliver(&shared, 50, EVENT_WRITE, &request_payload(0x2000, 8));
        assert!(shared.lock().pids.contains_key(&50));
        deliver_process(&shared, PROCESS_END, NEW_KEY, 50);
        assert!(!shared.lock().pids.contains_key(&50));
    }

    /// The epoch-bound variant: a bucket already bound to the predecessor's
    /// creation time must not survive the reuse, and the successor's fresh
    /// bucket must survive the successor's own epoch binding.
    ///
    /// Regression: keeping the stale bucket until `bind_epoch` noticed meant the
    /// new owner's already-measured bytes were removed WITH it — the symptom the
    /// delayed-exit handling was supposed to cure.
    #[test]
    fn a_reused_pid_drops_a_bound_bucket_before_the_next_sample() {
        const OLD_KEY: u64 = 0xffff_d007_0000_0040;
        const NEW_KEY: u64 = 0xffff_d007_0000_0100;
        let shared = shared();
        deliver_process(&shared, PROCESS_START, OLD_KEY, 60);
        deliver(
            &shared,
            60,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\old\gone.bin"),
        );
        deliver(&shared, 60, EVENT_READ, &request_payload(0x1000, 64));
        // A sample bound the bucket to the predecessor's creation time.
        shared.lock().bind_epoch(60, 1_000);
        assert_eq!(
            shared.lock().pids.get(&60).and_then(|f| f.epoch_s),
            Some(1_000)
        );

        // The pid is reused, and the successor writes after its start record.
        deliver_process(&shared, PROCESS_START, NEW_KEY, 60);
        assert!(
            !shared.lock().pids.contains_key(&60),
            "the predecessor's bound bucket goes with its start record"
        );
        deliver(
            &shared,
            60,
            EVENT_CREATE,
            &create_payload(0x2000, r"\??\C:\new\kept.bin"),
        );
        deliver(&shared, 60, EVENT_WRITE, &request_payload(0x2000, 32));

        // The successor's own sample: a fresh bucket has no epoch to disagree
        // with, so binding it must not remove it.
        shared.lock().bind_epoch(60, 2_000);
        let sample = sample_of(&shared, 60);
        assert_eq!(bytes(&sample, r"C:\new\kept.bin"), written(32));
        assert_eq!(bytes(&sample, r"C:\old\gone.bin"), None);
        assert_eq!(sample.pid_transfers(), 1);
        assert_eq!(
            shared.lock().pids.get(&60).and_then(|f| f.epoch_s),
            Some(2_000)
        );
    }

    /// The session-start rundown only SEEDS a key: it is a snapshot taken while
    /// the session was created, so it may not overwrite the key of a process
    /// that started and took the pid in that window (which would drop the live
    /// process's bucket).
    #[test]
    fn the_rundown_only_seeds_a_key() {
        const LIVE_KEY: u64 = 0xffff_d007_0000_0200;
        const STALE_KEY: u64 = 0xffff_d007_0000_0040;
        let shared = shared();
        deliver_process(&shared, PROCESS_START, LIVE_KEY, 70);
        deliver(&shared, 70, EVENT_WRITE, &request_payload(0x3000, 16));
        // The rundown's entry for the pid it snapshotted before that start.
        deliver_process(&shared, PROCESS_DC_START, STALE_KEY, 70);
        assert_eq!(shared.lock().process_keys.get(&70).copied(), Some(LIVE_KEY));
        assert_eq!(
            sample_of(&shared, 70).pid_transfers(),
            1,
            "the live bucket survives the rundown"
        );
        // A start with no recorded key still fills it in.
        shared.lock().process_keys.remove(&70);
        deliver_process(&shared, PROCESS_DC_START, STALE_KEY, 70);
        assert_eq!(
            shared.lock().process_keys.get(&70).copied(),
            Some(STALE_KEY)
        );
    }

    /// An inherited handle — a parent's redirected output written by its child
    /// — names one object for two processes. Once the handle table shows the
    /// child holding it, the child's bytes (including those that arrived
    /// first) land on that file for the child, later ones directly, and the
    /// name outlives the parent's exit for as long as the child holds it.
    ///
    /// Regression: the live test's child wrote 186 bytes to its inherited
    /// stdout file and they stayed unnamed, because the object was already
    /// named for the parent.
    #[test]
    fn an_inherited_handle_is_named_for_every_process_that_holds_it() {
        let shared = shared();
        deliver(
            &shared,
            40,
            EVENT_CREATE,
            &create_payload(0x5000, r"\??\C:\logs\out.txt"),
        );
        deliver(&shared, 40, EVENT_WRITE, &request_payload(0x5000, 10));
        deliver(&shared, 44, EVENT_WRITE, &request_payload(0x5000, 186));
        assert_eq!(
            sample_of(&shared, 44).unnamed.write_bytes,
            186,
            "unnamed until the table says the child holds it"
        );
        shared
            .lock()
            .apply_names(44, vec![(0x5000, r"C:\logs\out.txt".to_string())]);
        deliver(&shared, 44, EVENT_WRITE, &request_payload(0x5000, 14));
        let child = sample_of(&shared, 44);
        assert_eq!(bytes(&child, r"C:\logs\out.txt"), written(200));
        assert_eq!(child.unnamed, Remainder::default());
        assert_eq!(
            bytes(&sample_of(&shared, 40), r"C:\logs\out.txt"),
            written(10),
            "the parent keeps only its own bytes"
        );
        // The parent exits; the child still holds the object and its name.
        shared.lock().process_ended(40, None);
        deliver(&shared, 44, EVENT_WRITE, &request_payload(0x5000, 1));
        assert_eq!(
            bytes(&sample_of(&shared, 44), r"C:\logs\out.txt"),
            written(201)
        );
        // A third process the table never named for it stays unnamed.
        deliver(&shared, 48, EVENT_WRITE, &request_payload(0x5000, 5));
        assert!(sample_of(&shared, 48).files.is_empty());
    }

    /// A long-running process at its path budget keeps counting new files by
    /// evicting the least recently used CLOSED ones into the omitted
    /// remainder; a file that is still open is never evicted, and nothing is
    /// lost from the process total.
    #[test]
    fn a_full_path_budget_evicts_old_closed_paths() {
        let shared = shared();
        // An open file used first, so it is the oldest of all.
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x50_0000, r"\??\C:\db\open.db"),
        );
        deliver(&shared, 8, EVENT_WRITE, &request_payload(0x50_0000, 1));
        for index in 0..MAX_TRACKED_FILES_PER_PID - 1 {
            let object = 0x10_0000 + index as u64 * 8;
            deliver(
                &shared,
                8,
                EVENT_CREATE,
                &create_payload(object, &format!(r"\??\C:\g{index}")),
            );
            deliver(&shared, 8, EVENT_WRITE, &request_payload(object, 2));
            deliver(&shared, 8, EVENT_CLOSE, &close_payload(object));
        }
        assert_eq!(sample_of(&shared, 8).dropped_paths, 0, "exactly full");
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x90_0000, r"\??\C:\new.bin"),
        );
        deliver(&shared, 8, EVENT_READ, &request_payload(0x90_0000, 4_096));
        let sample = sample_of(&shared, 8);
        assert_eq!(
            bytes(&sample, r"C:\new.bin"),
            read(4_096),
            "the new file has a row"
        );
        assert_eq!(
            bytes(&sample, r"C:\db\open.db"),
            written(1),
            "the open file survives although it is the oldest"
        );
        assert_eq!(
            bytes(&sample, r"C:\g0"),
            None,
            "the oldest closed file was evicted"
        );
        let evicted = (MAX_TRACKED_FILES_PER_PID - 1).div_ceil(4) as u64;
        assert_eq!(sample.dropped_paths, evicted);
        assert_eq!(sample.omitted.write_bytes, evicted * 2);
        assert_eq!(
            sample.attributed().write_bytes,
            1 + (MAX_TRACKED_FILES_PER_PID as u64 - 1) * 2,
            "evicted bytes stay in the process total"
        );
        assert_eq!(
            shared.lock().tracked_paths,
            MAX_TRACKED_FILES_PER_PID - evicted as usize + 1
        );
    }

    /// A path this process holds through an INHERITED handle is open for it
    /// too: once the handle table named the object for this process,
    /// [`State::complete`] charges its transfers to it, so eviction must not
    /// fold those bytes into the omitted remainder while the handle lives.
    ///
    /// Regression: the protected set was built from `OpenFile::pid` alone, so a
    /// child's redirected stdout — named for the parent that opened it — was
    /// evicted as if it were closed: its bytes moved to the omitted remainder
    /// and the next write recreated the row at zero, with the file still open.
    #[test]
    fn eviction_keeps_a_path_held_through_an_inherited_handle() {
        let mut state = State::default();
        // The parent's Create named the object; the handle table then showed
        // pid 44 holding it as well (an inherited or duplicated handle).
        state
            .open
            .insert(0x5000, OpenFile::new(40, Arc::from(r"C:\logs\out.txt")));
        state.apply_names(44, vec![(0x5000, r"C:\logs\out.txt".to_string())]);
        assert!(
            state.open.get(&0x5000).is_some_and(|file| file.held_by(44)),
            "the handle table named the object for pid 44"
        );
        state.charge_path(
            44,
            Arc::from(r"C:\logs\out.txt"),
            FileBytes {
                read_bytes: 0,
                write_bytes: 10,
            },
        );
        // Two files the process opened and closed, the oldest first.
        state.charge_path(
            44,
            Arc::from(r"C:\old.tmp"),
            FileBytes {
                read_bytes: 0,
                write_bytes: 2,
            },
        );
        state.charge_path(
            44,
            Arc::from(r"C:\older.tmp"),
            FileBytes {
                read_bytes: 0,
                write_bytes: 3,
            },
        );
        state.evict(44);
        let files = &state.pids.get(&44).expect("a bucket").files;
        assert!(
            files.contains_key(r"C:\logs\out.txt"),
            "a path held through an inherited handle is open, not closed"
        );
        assert!(
            !files.contains_key(r"C:\old.tmp"),
            "the least recently used CLOSED path went instead"
        );
        assert_eq!(files.len(), 2);
        assert_eq!(
            state.pids.get(&44).expect("a bucket").omitted.write_bytes,
            2
        );
        assert_eq!(state.tracked_paths, 2);
    }

    /// Coverage: a process created after the session started has been traced
    /// for its whole life and its counter window starts at zero; an older one
    /// starts at its pre-session counters, but only when the snapshot is of
    /// the SAME process (creation time), never of a pid's previous owner.
    #[test]
    fn coverage_follows_the_creation_time() {
        let started = 1_000_000_000;
        let baselines = HashMap::from([(30u32, (500_000_000i64, 4_000u64, 9_000u64))]);
        assert_eq!(
            coverage_of(31, started + 10, started, &baselines),
            (true, Some(FileBytes::default()))
        );
        assert_eq!(
            coverage_of(30, 500_000_000, started, &baselines),
            (
                false,
                Some(FileBytes {
                    read_bytes: 4_000,
                    write_bytes: 9_000,
                })
            )
        );
        assert_eq!(
            coverage_of(30, 600_000_000, started, &baselines),
            (false, None),
            "a reused pid must not take its predecessor's counters"
        );
        assert_eq!(
            coverage_of(32, 500_000_000, started, &baselines),
            (false, None)
        );
    }

    /// The attach step turns the baseline into the process's counter window,
    /// and a counter that went backwards (a different process) stays unknown.
    #[test]
    fn the_attach_step_states_the_counter_window() {
        let mut sample = measured_sample();
        sample.traced_since_process_start = false;
        sample.counter_baseline = Some(FileBytes {
            read_bytes: 1_000,
            write_bytes: 50_000,
        });
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![file_row(r"C:\Windows\System32\a.dll")],
            accounting: crate::actions::FileIoAccounting {
                counter_read_bytes: Some(5_000),
                counter_write_bytes: Some(40_000),
                ..Default::default()
            },
        };
        attach_file_io(&mut snapshot, &sample);
        let accounting = &snapshot.accounting;
        assert_eq!(accounting.counter_window_read_bytes, Some(4_000));
        assert_eq!(accounting.counter_window_write_bytes, None);
        assert!(!accounting.traced_since_process_start);

        let mut lifetime = measured_sample();
        lifetime.traced_since_process_start = true;
        lifetime.counter_baseline = Some(FileBytes::default());
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: Vec::new(),
            accounting: crate::actions::FileIoAccounting {
                counter_read_bytes: Some(5_000),
                counter_write_bytes: Some(40_000),
                ..Default::default()
            },
        };
        attach_file_io(&mut snapshot, &lifetime);
        assert!(snapshot.accounting.traced_since_process_start);
        assert_eq!(snapshot.accounting.counter_window_read_bytes, Some(5_000));
        assert_eq!(snapshot.accounting.counter_window_write_bytes, Some(40_000));
    }

    /// The provider names files as device paths while the rows are DOS paths;
    /// without this translation no byte would ever reach a row.
    #[test]
    fn device_paths_translate_to_the_dos_paths_the_rows_use() {
        let devices = vec![
            (r"\Device\HarddiskVolume3".to_string(), "C:".to_string()),
            (r"\Device\HarddiskVolume30".to_string(), "D:".to_string()),
        ];
        assert_eq!(
            dos_path(r"\Device\HarddiskVolume3\Users\me\a.txt", &devices).as_deref(),
            Some(r"C:\Users\me\a.txt")
        );
        // Longest prefix wins, so volume 3 cannot swallow volume 30.
        assert_eq!(
            dos_path(r"\Device\HarddiskVolume30\x", &devices).as_deref(),
            Some(r"D:\x")
        );
        // A verbatim NT name is already DOS.
        assert_eq!(
            dos_path(r"\??\C:\a\b.txt", &devices).as_deref(),
            Some(r"C:\a\b.txt")
        );
        // A UNC name is already what the handle walk reports.
        assert_eq!(
            dos_path(r"\\server\share\a.txt", &devices).as_deref(),
            Some(r"\\server\share\a.txt")
        );
        // Anything else stays unresolved instead of being approximated.
        assert_eq!(dos_path(r"\Device\Mup\server\share\a", &devices), None);
        assert_eq!(dos_path(r"\Device\HarddiskVolume", &devices), None);
        assert_eq!(dos_path(r"\SystemRoot\a", &devices), None);
        assert_eq!(dos_path(r"\??\", &devices), None);
    }

    #[test]
    fn names_decode_from_utf16_and_stop_at_the_terminator() {
        let payload = create_payload(0x1000, "C:\\ä.txt");
        let FileEvent::Create { name, .. } =
            parse_event(EVENT_CREATE, FILE_IO_VERSION, &payload, P).unwrap()
        else {
            panic!("expected a Create");
        };
        assert_eq!(decode_utf16(name).as_deref(), Some("C:\\ä.txt"));
        // An odd-length or empty tail is not a name.
        assert_eq!(decode_utf16(&[]), None);
        assert_eq!(decode_utf16(&[0x41]), None);
        assert_eq!(decode_utf16(&[0x00, 0x00]), None);
    }

    /// The crash teardown stops sessions BY NAME from a fixed list; a session
    /// name that does not reach it leaves a kernel trace running after a crash.
    #[test]
    fn every_session_name_is_known_to_the_crash_teardown() {
        for role in [TraceRole::Service, TraceRole::App] {
            let name = session_name(role);
            assert!(
                crate::win::etw::SESSION_NAMES.contains(&name),
                "{name} is missing from etw::SESSION_NAMES"
            );
        }
    }

    #[test]
    fn each_role_owns_a_distinct_fixed_session_name() {
        assert_ne!(
            session_name(TraceRole::Service),
            session_name(TraceRole::App)
        );
        for role in [TraceRole::Service, TraceRole::App] {
            assert!(session_name(role).contains("File"));
        }
        // ...and it must not collide with the other traces' names.
        assert_ne!(session_name(TraceRole::App), "TaskMan-Disk-App");
        assert_ne!(session_name(TraceRole::App), "TaskMan-Net-App");
    }

    /// A late callback after teardown must not resurrect the accumulators.
    #[test]
    fn records_are_dropped_once_the_session_is_gone() {
        let shared = shared();
        shared.live.store(false, Ordering::Relaxed);
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\a"),
        );
        deliver(&shared, 8, EVENT_READ, &request_payload(0x1000, 4_096));
        let state = shared.lock();
        assert!(state.open.is_empty());
        assert!(state.pending.is_empty());
        assert!(state.pids.is_empty());
        assert_eq!(state.events_seen, 0);
    }

    /// The keys of the sampled map are strings the broker can compare.
    #[test]
    fn a_sample_carries_paths_and_counts() {
        let shared = shared();
        shared.devices.lock().unwrap().loaded = Some((Instant::now(), Vec::new()));
        deliver(
            &shared,
            12,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\x\y.log"),
        );
        deliver(&shared, 12, EVENT_WRITE, &request_payload(0x1000, 700));
        let sample = sample_of(&shared, 12);
        assert_eq!(
            sample.files,
            vec![(
                r"C:\x\y.log".to_string(),
                FileBytes {
                    read_bytes: 0,
                    write_bytes: 700,
                }
            )]
        );
        assert_eq!(sample.pid_transfers(), 1);
        assert_eq!(sample.session_transfers, 1);
        // Create, Write and its OpEnd.
        assert_eq!(sample.events_seen, 3);
        assert_eq!(sample.events_decoded, 3);
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

    fn measured_sample() -> FileIoSample {
        FileIoSample {
            running: true,
            since_ms: 12_000,
            files: vec![
                // A different casing of a real row: the trace's names come from
                // device-path resolution, the rows from the handle walk.
                (
                    r"c:\windows\system32\A.dll".to_string(),
                    FileBytes {
                        read_bytes: 1_000,
                        write_bytes: 20,
                    },
                ),
                // No row: the handle was closed since the walk.
                (
                    r"C:\temp\gone.tmp".to_string(),
                    FileBytes {
                        read_bytes: 500,
                        write_bytes: 5,
                    },
                ),
            ],
            unnamed: Remainder {
                read_bytes: 70,
                write_bytes: 7,
            },
            omitted: Remainder {
                read_bytes: 300,
                write_bytes: 3,
            },
            events_seen: 40,
            events_decoded: 39,
            session_transfers: 11,
            pid_read_transfers: 9,
            pid_write_transfers: 2,
            // The session had to drop names and had nowhere to put paths; both
            // explain a row that reads unavailable while the state is measured.
            cleared_file_objects: 12,
            unnamed_creates: 3,
            dropped_paths: 4,
            events_lost: Some(1),
            traced_since_process_start: false,
            counter_baseline: None,
        }
    }

    /// The attach step, driven by synthetic records through the decoder rather
    /// than by a hand-built sample, because the GUI's own trace depends on this
    /// identity too: every byte the session attributed to the process is on a
    /// row or in one of the two remainders, and a row only carries what named
    /// it.
    #[test]
    fn the_attach_step_accounts_for_every_byte_the_trace_measured() {
        let shared = shared();
        let pid = 20;
        // A file the handle walk also reports, under the caller's casing.
        deliver(
            &shared,
            pid,
            EVENT_CREATE,
            &create_payload(0x1000, r"\??\C:\data\a.bin"),
        );
        deliver(&shared, pid, EVENT_READ, &request_payload(0x1000, 4_096));
        deliver(&shared, pid, EVENT_WRITE, &request_payload(0x1000, 1_024));
        // A transfer on a file object whose Create was never seen: unnamed.
        deliver(&shared, pid, EVENT_READ, &request_payload(0x9000, 512));
        // A file past the session's path budget, while nothing the process
        // has is closed and evictable: omitted, and no row.
        deliver(
            &shared,
            pid,
            EVENT_CREATE,
            &create_payload(0x2000, r"\??\C:\data\b.bin"),
        );
        let tracked = shared.lock().tracked_paths;
        shared.lock().tracked_paths = MAX_TRACKED_FILES;
        deliver(&shared, pid, EVENT_WRITE, &request_payload(0x2000, 2_048));
        shared.lock().tracked_paths = tracked;
        // A file read and closed before the handle walk: a closed row.
        deliver(
            &shared,
            pid,
            EVENT_CREATE,
            &create_payload(0x3000, r"\??\C:\data\config.ini"),
        );
        deliver(&shared, pid, EVENT_READ, &request_payload(0x3000, 96_167));
        deliver(&shared, pid, EVENT_CLOSE, &close_payload(0x3000));

        let sample = sample_of(&shared, pid);
        assert_eq!(sample.state(), FileIoAccountingState::Measured);

        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![
                // Deliberately not the trace's casing: the walk and the trace
                // resolve the same file differently.
                file_row(r"c:\DATA\A.BIN"),
                file_row(r"C:\data\untouched.bin"),
            ],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_file_io(&mut snapshot, &sample);

        assert_eq!(snapshot.files[0].read_bytes, Some(4_096));
        assert_eq!(snapshot.files[0].write_bytes, Some(1_024));
        assert_eq!(
            snapshot.files[1].read_bytes, None,
            "a row with no record stays unmeasured"
        );
        assert_eq!(snapshot.files[1].write_bytes, None);
        // Regression: a service rereading its config every few seconds showed
        // "0 B" on every row because closed files fell into the remainder.
        let closed = snapshot
            .files
            .iter()
            .find(|file| file.path == r"C:\data\config.ini")
            .expect("the closed file has a row");
        assert_eq!(closed.open_handles, 0, "rendered as closed");
        assert_eq!(closed.read_bytes, Some(96_167));
        assert_eq!(closed.write_bytes, Some(0));
        assert_eq!(closed.file_size, None, "no handle, no size");
        assert!(!closed.access_known);
        assert_eq!(
            snapshot.files.len(),
            3,
            "the budget-dropped file has no row"
        );
        let accounting = &snapshot.accounting;
        assert_eq!(accounting.state, FileIoAccountingState::Measured);
        assert_eq!(accounting.read_ops, sample.pid_read_transfers);
        assert_eq!(accounting.write_ops, sample.pid_write_transfers);
        assert_eq!(accounting.unnamed_read_bytes, 512);
        assert_eq!(accounting.unnamed_write_bytes, 0);
        assert_eq!(accounting.omitted_read_bytes, 0);
        assert_eq!(accounting.omitted_write_bytes, 2_048);
        // The identity: rows plus both remainders are the attribution for the
        // process, so the dialog's totals never understate what it moved.
        let rows_read: u64 = snapshot
            .files
            .iter()
            .filter_map(|file| file.read_bytes)
            .sum();
        let rows_written: u64 = snapshot
            .files
            .iter()
            .filter_map(|file| file.write_bytes)
            .sum();
        assert_eq!(
            rows_read + accounting.unnamed_read_bytes + accounting.omitted_read_bytes,
            accounting.read_bytes
        );
        assert_eq!(
            rows_written + accounting.unnamed_write_bytes + accounting.omitted_write_bytes,
            accounting.write_bytes
        );
        assert_eq!(accounting.read_bytes, sample.attributed().read_bytes);
        assert_eq!(accounting.write_bytes, sample.attributed().write_bytes);
        assert_eq!(accounting.read_bytes, 4_096 + 96_167 + 512);
        assert_eq!(accounting.write_bytes, 1_024 + 2_048);
    }

    /// The whole point of the wiring: bytes reach exactly the rows they name,
    /// everything else is accounted for as a remainder, and nothing is
    /// invented. Rows plus both remainders must add up to the process totals.
    #[test]
    fn per_file_bytes_reach_only_the_rows_they_name() {
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![
                file_row(r"C:\Windows\System32\a.dll"),
                file_row(r"C:\data\untouched.bin"),
            ],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_file_io(&mut snapshot, &measured_sample());
        assert_eq!(
            snapshot.files[0].read_bytes,
            Some(1_000),
            "case-insensitive match"
        );
        assert_eq!(snapshot.files[0].write_bytes, Some(20));
        assert_eq!(
            snapshot.files[1].read_bytes, None,
            "a row with no record stays unavailable"
        );
        assert_eq!(snapshot.files[1].write_bytes, None);
        let accounting = &snapshot.accounting;
        assert_eq!(
            accounting.state,
            crate::actions::FileIoAccountingState::Measured
        );
        assert_eq!(accounting.since_ms, 12_000);
        assert_eq!(accounting.events_lost, Some(1));
        // A user needs to see WHY a row reads unavailable while the state is
        // measured: a lost name, a refused open, or a full path budget.
        assert_eq!(accounting.cleared_file_objects, 12);
        assert_eq!(accounting.unnamed_creates, 3);
        assert_eq!(accounting.dropped_paths, 4);
        // gone.tmp is a closed row of its own; only the 70/7 is unnamed.
        let gone = snapshot
            .files
            .iter()
            .find(|file| file.path == r"C:\temp\gone.tmp")
            .expect("a closed row");
        assert_eq!((gone.read_bytes, gone.write_bytes), (Some(500), Some(5)));
        assert_eq!(gone.open_handles, 0);
        assert_eq!(accounting.unnamed_read_bytes, 70);
        assert_eq!(accounting.unnamed_write_bytes, 7);
        assert_eq!(accounting.omitted_read_bytes, 300);
        assert_eq!(accounting.omitted_write_bytes, 3);
        assert_eq!(accounting.read_ops, 9);
        assert_eq!(accounting.write_ops, 2);
        // The row + the two remainders are the attribution for the process.
        assert_eq!(accounting.read_bytes, 1_000 + 500 + 70 + 300);
        assert_eq!(accounting.write_bytes, 20 + 5 + 7 + 3);
    }

    /// One file can arrive as two trace buckets — `C:\...` and `c:\...` — and
    /// both match the same row. The row must hold their SUM: overwriting with
    /// the second bucket left the row short of the process total with nothing
    /// absorbing the difference, so "rows + remainders == attributed" was false.
    #[test]
    fn two_case_variants_of_one_path_accumulate_on_that_row() {
        let mut sample = measured_sample();
        sample.files = vec![
            (
                r"C:\Windows\System32\A.dll".to_string(),
                FileBytes {
                    read_bytes: 1_000,
                    write_bytes: 20,
                },
            ),
            (
                r"c:\windows\system32\a.dll".to_string(),
                FileBytes {
                    read_bytes: 400,
                    write_bytes: 5,
                },
            ),
        ];
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![
                file_row(r"C:\Windows\System32\a.dll"),
                file_row(r"C:\data\untouched.bin"),
            ],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_file_io(&mut snapshot, &sample);
        assert_eq!(
            snapshot.files[0].read_bytes,
            Some(1_400),
            "both buckets land on the row"
        );
        assert_eq!(snapshot.files[0].write_bytes, Some(25));
        assert_eq!(snapshot.files[1].read_bytes, None);
        let accounting = &snapshot.accounting;
        let rows_read: u64 = snapshot
            .files
            .iter()
            .filter_map(|file| file.read_bytes)
            .sum();
        assert_eq!(
            rows_read + accounting.unnamed_read_bytes + accounting.omitted_read_bytes,
            accounting.read_bytes,
            "rows plus remainders are still the whole attribution"
        );
        assert_eq!(accounting.read_bytes, 1_400 + 70 + 300);
    }

    /// A measured zero is a measurement and renders as 0; an absent record is
    /// not, and stays unavailable. Collapsing the two is the invariant this
    /// program exists to keep.
    #[test]
    fn a_measured_zero_stays_distinct_from_an_absent_record() {
        let mut sample = measured_sample();
        sample.files = vec![(
            r"C:\Windows\System32\a.dll".to_string(),
            FileBytes::default(),
        )];
        sample.unnamed = Remainder::default();
        sample.omitted = Remainder::default();
        let mut snapshot = crate::actions::ProcessFileIoSnapshot {
            files: vec![
                file_row(r"C:\Windows\System32\a.dll"),
                file_row(r"C:\data\untouched.bin"),
            ],
            accounting: crate::actions::FileIoAccounting::default(),
        };
        attach_file_io(&mut snapshot, &sample);
        assert_eq!(snapshot.files[0].read_bytes, Some(0));
        assert_eq!(snapshot.files[0].write_bytes, Some(0));
        assert_eq!(snapshot.files[1].read_bytes, None);
        assert_eq!(snapshot.files[1].write_bytes, None);
        assert_eq!(snapshot.accounting.read_bytes, 0);
    }

    /// A trace that decoded nothing for this process leaves every row alone.
    /// Attaching zeros here would turn "the subscription delivered nothing"
    /// into "this process did no I/O".
    #[test]
    fn an_unmeasured_sample_leaves_every_row_unavailable() {
        for sample in [
            FileIoSample::default(),
            FileIoSample {
                running: true,
                events_seen: 500,
                events_decoded: 0,
                ..FileIoSample::default()
            },
        ] {
            let mut snapshot = crate::actions::ProcessFileIoSnapshot {
                files: vec![file_row(r"C:\Windows\System32\a.dll")],
                accounting: crate::actions::FileIoAccounting::default(),
            };
            attach_file_io(&mut snapshot, &sample);
            assert_eq!(snapshot.files[0].read_bytes, None);
            assert_eq!(snapshot.files[0].write_bytes, None);
            assert_ne!(
                snapshot.accounting.state,
                crate::actions::FileIoAccountingState::Measured
            );
            assert_eq!(snapshot.accounting.read_bytes, 0);
            assert_eq!(snapshot.accounting.unnamed_read_bytes, 0);
        }
    }
}
