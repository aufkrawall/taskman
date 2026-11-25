//! Per-file read and written bytes from a real-time Kernel-File ETW session.
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
//! `Microsoft-Windows-Kernel-File` is the provider that knows. It reports one
//! event per read and write with the request size, plus the Create that names
//! the file object it was issued on. Summed per (process, file) that is the
//! number the dialog shows.
//!
//! ## What is subscribed, and how it is decoded
//!
//! The events come from the KERNEL's file I/O hooks and arrive as classic
//! (MOF) `FileIo` records. Which session delivers them is the whole problem,
//! and it is answered by three ordered attempts ([`sources`]):
//!
//! 1. **The single global kernel session — [`etw::KERNEL_LOGGER`]** with
//!    `EnableFlags = EVENT_TRACE_FLAG_FILE_IO`
//!    ([`LoggerKind::KernelFile`]). This is the session the classic `FileIo`
//!    class has always been delivered by, and the flag IS the subscription:
//!    it is the kernel logger's own enable mechanism. Enabled with no provider
//!    calls ([`KERNEL_PROVIDERS`] is empty) and decoded by class GUID + opcode
//!    like every other classic kernel record.
//! 2. **A private system logger** ([`LoggerKind::SystemFile`]) subscribing to
//!    the SystemIo GROUP with `SYSTEM_IO_KW_FILE | SYSTEM_IO_KW_FILENAME`
//!    ([`PROVIDERS`]) — the documented SystemIo keyword set, and the same
//!    shape [`super::disk_etw`] uses live for `SYSTEM_IO_KW_DISK`. This exists
//!    because the kernel logger is EXCLUSIVE: when another tracing tool owns
//!    it (Process Hacker-style monitors, `wpr`, an installer's logger), a
//!    private session is the only session left that may still carry the class.
//! 3. **Nothing left**, and then the answer is a state, never a number:
//!    [`FileTraceRefusal::KernelLoggerInUse`] when attempt 1 reported
//!    `ERROR_ALREADY_EXISTS` and attempt 2 was refused as well, which
//!    [`attach_refusal`] turns into
//!    [`crate::actions::FileIoAccountingState::KernelLoggerInUse`], or
//!    [`FileTraceRefusal::Unavailable`] for every other refusal (no elevation,
//!    no free logger slot, a provider enable Windows said no to).
//!
//! Three shapes that look right deliver nothing, and a live elevated selfcheck
//! reported `undelivered` with `events_lost` at 0 on each of them: enabling the
//! MANIFEST GUID of `Microsoft-Windows-Kernel-File` on an ordinary manifest
//! session (what this module originally did), the same provider on a system
//! logger changed to a provider enable of the manifest GUID, and asking a
//! PRIVATE system logger for `EVENT_TRACE_FLAG_FILE_IO` through `EnableFlags`.
//! `EnableTraceEx2` returns success on all three, the session runs, and not one
//! record ever arrives. That is why attempt 1 uses the flag on the kernel
//! logger — where it is the mechanism — and attempt 2 uses a provider enable
//! with empty `EnableFlags`. The two are NOT interchangeable.
//!
//! Records are identified the way every classic kernel record is — the enable
//! names a GROUP, each record names its class: the MOF class GUID in
//! `EventHeader.ProviderId` ([`FileIoGuid`]) and the operation in
//! `EventHeader.EventDescriptor.Opcode`. Those opcodes are the numbers the
//! provider's manifest lists as event ids (`Create` 12, `Cleanup` 13,
//! `Close` 14, `Read` 15, `Write` 16 — the same kernel hooks, so the
//! id-to-operation mapping is unchanged). Every other operation is ignored, but
//! neither subscription can select a subset of the class: directory
//! enumeration, flush, information classes and deletes are delivered too and
//! discarded in the callback, which is why the private session gets the
//! largest buffer set in [`super::etw`].
//!
//! The correlation, decoding and accounting below are shared by both attempts
//! on purpose: the records are the same class whichever session carried them,
//! so a byte measured through the kernel logger and a byte measured through
//! the private session mean the same thing — and the callback accepts either.
//!
//! Beyond the kernel logger, the fallback rides its own system logger rather
//! than the disk session's: Windows multiplexes the SystemTraceProvider over up
//! to eight logger sessions and reserves only the first two for the NT Kernel
//! Logger and the Circular Kernel Context Logger (SystemTraceProvider session
//! documentation), so the disk, loader, profile and file traces each host one
//! of the remaining slots. One session per feature is also what lets this
//! trace live exactly as long as the I/O page needs it: adding the FILE
//! keyword to the disk session would trace every file on the machine for as
//! long as the Performance page samples disks.
//!
//! Payloads are decoded by hand, at offsets taken from the provider's manifest
//! templates — `wevtutil gp Microsoft-Windows-Kernel-File /ge:true` lists the
//! ids and tasks, and PowerShell's
//! `[System.Diagnostics.Eventing.Reader.ProviderMetadata]` prints each
//! version's fields; there is no TDH decoding path in this crate, and one event
//! family does not justify adding one. The classic payload is the same one the
//! manifest templates describe: `Microsoft-Windows-Kernel-Disk` has no tasks
//! and lists its events as values 10 and 11, matching the classic `DiskIo`
//! opcodes `super::disk_etw` decodes.
//!
//! | id | event | v1 layout (after `ByteOffset` for the transfers) |
//! | --- | --- | --- |
//! | 12 | Create | `Irp` ptr, `FileObject` ptr, `IssuingThreadId` u32, `CreateOptions`, `CreateAttributes`, `ShareAccess`, `FileName` |
//! | 13 | Cleanup | `Irp` ptr, `FileObject` ptr, `FileKey` ptr, `IssuingThreadId` u32 |
//! | 14 | Close | same as Cleanup |
//! | 15 | Read | u64, `Irp` ptr, `FileObject` ptr, `FileKey` ptr, `IssuingThreadId` u32, `IOSize` u32, `IOFlags`, `ExtraFlags` |
//! | 16 | Write | same as Read |
//!
//! **Both template versions are decoded.** Version 0 spells the issuing thread
//! as a pointer (`ThreadId`) and carries no `ExtraFlags`; version 1 uses a
//! 32-bit `IssuingThreadId`. They differ by four bytes from the second field
//! on, so reading a v0 record with the v1 offsets lands on a neighbouring
//! field — the size would be a number read out of the wrong four bytes, which
//! is a fabricated measurement, not a gap. The version is therefore part of the
//! decode and an unknown version is counted as undecodable.
//!
//! ## Correlation, and its bounds
//!
//! Create carries `FileObject -> FileName`; Read and Write carry the
//! `FileObject` and no name. The map from one to the other is the whole
//! feature, and both ends of it are bounded:
//!
//! * [`MAX_OPEN_FILES`] caps the `FileObject -> name` map. A lost Close (ETW
//!   drops events under load) leaks one entry, so on overflow the map is
//!   dropped wholesale and the discarded entries are counted; files that stay
//!   open then read and write into the unnamed remainder until they are opened
//!   again. Dropping is visible, growing is not.
//! * [`MAX_TRACKED_FILES_PER_PID`] caps distinct paths per process. Beyond it a
//!   file's bytes go into the omitted remainder instead of a row: a row whose
//!   count started mid-stream would look complete and be wrong.
//! * [`MAX_TRACKED_FILES`] caps distinct (process, path) pairs across the whole
//!   session, because the per-process cap alone would let a thousand live
//!   processes hold eight million paths. A file past it also lands in the
//!   omitted remainder, and the drop is counted.
//! * Bytes whose `FileObject` was never seen are NOT attributed to anything.
//!   That is the common case at the start of a session for long-lived handles
//!   (a database, a log file, a VM image opened hours ago), and the honest
//!   answer is a remainder that the dialog shows as unavailable.
//! * A `FileObject` seen under a DIFFERENT process than the Create means either
//!   a duplicated handle or a reused pointer after a missed Close. Either way
//!   the old name cannot be trusted for this record, so it stays unnamed.
//! * **Within one process, pointer reuse is invisible.** The payload carries
//!   nothing but the address, so if both the Close and the next Create are
//!   lost, a transfer on the reused address is charged to the name of whatever
//!   held that address before. The bytes stay in the process's attribution —
//!   only the name is stale — and no field in the record can detect it.
//!
//! ## Things that are easy to get wrong here
//!
//! * **The name is a DEVICE path.** Kernel-File reports
//!   `\Device\HarddiskVolume3\Users\...`, while the rows come from
//!   `GetFinalPathNameByHandleW(VOLUME_NAME_DOS)` as `C:\Users\...`. Without
//!   translating the device prefix the two never compare equal and not one byte
//!   would ever reach a row. [`dos_path`] does that translation against
//!   `QueryDosDeviceW`, and an unresolvable name stays a remainder.
//! * **A session in which nothing could be decoded is UNKNOWN, not zero.** See
//!   [`FileIoSample::state`]: an idle machine is [`FileIoAccountingState::Undelivered`]
//!   when no file event arrived at all or none of them attributed a read or a
//!   write to the process being asked about, and
//!   [`FileIoAccountingState::Undecodable`] when records arrived and none of
//!   them parsed. A session that never STARTED is neither: it is
//!   [`FileIoAccountingState::KernelLoggerInUse`] when another application owns
//!   the kernel logger ([`attach_refusal`]), and
//!   [`FileIoAccountingState::Unavailable`] for every other refusal. Collapsing
//!   a refusal into `Undelivered` would report a session that does not exist.
//! * **These are REQUESTED sizes.** The record is written when the file system
//!   receives the request, so a request that fails or completes short counts at
//!   its requested size. It is what every consumer of this provider reports,
//!   and it is not a file size or an offset.
//! * The session lifetime rules (fixed names, orphan reclamation, the
//!   administrator requirement) live in [`super::etw`]. The kernel logger is the
//!   one name that is NOT ours to reclaim; see [`LoggerKind::KernelFile`].
//!
//! ## What is verified here, and what is not
//!
//! The ids, versions and field offsets above were read from the installed
//! provider manifest. The fallback enable is the one [`super::disk_etw`] already
//! uses live for the `DiskIo` class (`SystemIoProviderGuid` with a keyword,
//! records decoded by their class GUID, on a system logger); the preferred
//! session is the kernel logger with `EVENT_TRACE_FLAG_FILE_IO`, the shape
//! Process Hacker-style tools have always used and the SDK's kernel-logger
//! example documents (`SystemTraceControlGuid` in `Wnode.Guid`, real-time,
//! no `EVENT_TRACE_SYSTEM_LOGGER_MODE`). The decode/accounting path is covered
//! by tests with synthetic records, including records driven through the real
//! callback under the classic identity — and both attempts share that path, so
//! the same synthetic `FileIo` record proves either session's records decode.
//!
//! What a synthetic record cannot prove is that a given Windows build ACCEPTS
//! one of these session shapes, that names really arrive as device paths, and
//! that ETW delivers every record at machine rates. A run that delivers nothing
//! reports [`FileIoAccountingState::Undelivered`] and a run that delivers
//! records this decoder rejects reports
//! [`FileIoAccountingState::Undecodable`] with the rejected opcode, version and
//! payload length logged, so a broken subscription shows up as a state rather
//! than as zeros. `--selfcheck` reports that state headlessly (see
//! `llm-wiki/debug-tools.md`), including which attempt ran, since the log line
//! names the session and its kind.
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
use windows::Win32::System::Diagnostics::Etw::{
    EVENT_RECORD, FileIoGuid, SYSTEM_IO_KW_FILE, SYSTEM_IO_KW_FILENAME, SystemIoProviderGuid,
};
use windows::core::PCWSTR;

use super::etw::{
    self, KERNEL_LOGGER, LoggerKind, Provider, Session, StartError, TraceContext, TraceRole,
};
use crate::actions::FileIoAccountingState;

/// The FALLBACK session shape see the module docs: the private system logger.
///
/// Named so a test can pin the wiring: the kind is the system logger half of
/// the subscription, and the manifest kind this module used before delivers
/// nothing at all.
pub(super) const LOGGER_KIND: LoggerKind = LoggerKind::SystemFile;

/// What the FALLBACK session enables: the SystemIo group with the file
/// keywords.
///
/// This is a PROVIDER call, not a legacy `EnableFlags` flag — `EnableTraceEx2`
/// with `SystemIoProviderGuid` and the keywords against a system logger (System
/// Providers documentation). Other shapes are accepted and deliver no record:
/// the manifest GUID of `Microsoft-Windows-Kernel-File`, and
/// `EVENT_TRACE_FLAG_FILE_IO` on a PRIVATE system logger (a live elevated
/// selfcheck reported `undelivered` with `events_lost` 0 on them). The enable is
/// therefore the shape [`super::disk_etw`] already uses live, a keyword set at a
/// time so the FILE class is the only thing this stream carries.
///
/// `SYSTEM_IO_KW_FILE` is the class; `SYSTEM_IO_KW_FILENAME` is its name
/// companion, kept because a Create is only useful when its name arrives.
///
/// The keywords name the GROUP; the records still arrive under their classic
/// class GUID ([`FileIoGuid`]) — see [`on_event`].
pub(super) const PROVIDERS: [Provider; 1] = [Provider {
    guid: SystemIoProviderGuid,
    match_any_keyword: SYSTEM_IO_KW_FILE | SYSTEM_IO_KW_FILENAME,
}];

/// What the PREFERRED session enables: nothing through a provider.
///
/// The kernel logger's subscription is its `EnableFlags`
/// (`EVENT_TRACE_FLAG_FILE_IO`), set in [`super::etw::LoggerKind::KernelFile`].
/// Enabling a provider on it as well would subscribe twice and deliver the
/// class twice.
pub(super) const KERNEL_PROVIDERS: [Provider; 0] = [];

/// Why no per-file session could be started.
///
/// The distinction is what keeps the dialog honest: neither of these is a
/// measurement, and they are not the same failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTraceRefusal {
    /// The kernel logger is owned by another application
    /// (`ERROR_ALREADY_EXISTS` from `StartTraceW`) AND the private fallback was
    /// refused as well, so there is no session at all.
    ///
    /// Reported as its own state because it is actionable and because it must
    /// never be mistaken for a silent window: nothing started, so nothing can
    /// arrive later without the owner going away first.
    KernelLoggerInUse,
    /// No session for any other reason: no elevation, no free
    /// SystemTraceProvider slot, a refused provider enable.
    Unavailable,
}

/// One ordered attempt at a session: what to start, and what that shape
/// subscribes with.
///
/// A table rather than two inline calls so the ORDER — which is the design —
/// is data a test can read, instead of control flow it has to infer.
#[derive(Debug, Clone, Copy)]
pub(super) struct FileSource {
    /// ETW session name to start. The kernel logger's is fixed and global; the
    /// fallback's is per role, so the two hosts never share one.
    pub name: &'static str,
    pub kind: LoggerKind,
    /// Provider enables. Empty for the kernel logger, whose `EnableFlags` are
    /// the subscription.
    pub providers: &'static [Provider],
}

/// The ordered attempts for one host role, best first; see the module docs.
///
/// Attempt 1 comes first every time a session is (re)started, not just at
/// startup: the kernel logger can be released by its owner at any moment, and
/// the next restart should prefer it again rather than stay on the fallback
/// for the life of the process.
pub(super) fn sources(role: TraceRole) -> [FileSource; 2] {
    [
        FileSource {
            name: KERNEL_LOGGER,
            kind: LoggerKind::KernelFile,
            providers: &KERNEL_PROVIDERS,
        },
        FileSource {
            name: session_name(role),
            kind: LOGGER_KIND,
            providers: &PROVIDERS,
        },
    ]
}

/// Opcodes of the five operations this module reads, in the classic `FileIo`
/// class. They are the provider's task values too, which is why the manifest
/// and classic generations agree on them; every other opcode (name create,
/// directory enumeration, flush, information classes, deletes, ...) is ignored.
const EVENT_CREATE: u16 = 12;
const EVENT_CLEANUP: u16 = 13;
const EVENT_CLOSE: u16 = 14;
const EVENT_READ: u16 = 15;
const EVENT_WRITE: u16 = 16;

/// A single request bigger than this did not come from this payload layout.
/// Windows splits I/O long before a gigabyte, so anything above it is a read
/// through a wrong offset rather than a real transfer.
const MAX_PLAUSIBLE_TRANSFER: u32 = 1 << 30;

/// Upper bound on the `FileObject -> name` map.
///
/// A lost Close leaks one entry, and ETW does drop events under load; on
/// overflow the map is cleared (see [`Shared::create`]) and the discarded
/// entries are counted, so it cannot grow without limit. The cost is not the
/// entry (a `u64` key and a `{pid, Arc<str>}` value) but the name it points at:
/// at `MAX_PATH` a name is 260 UTF-16 units, so ~520 bytes plus an allocation
/// header, and the whole map is roughly 37 MB — not the few megabytes an
/// ordinary process's short names would suggest.
const MAX_OPEN_FILES: usize = 65_536;

/// Upper bound on distinct files tracked per process. A build or a package
/// manager can touch hundreds of thousands of files; past this cap a file's
/// bytes land in the omitted remainder instead of a row.
const MAX_TRACKED_FILES_PER_PID: usize = 8_192;

/// Upper bound on distinct (process, path) pairs across all processes.
///
/// The per-process cap alone is not a memory bound: a machine with a thousand
/// live processes could hold eight million paths. At `MAX_PATH` names an entry
/// is ~570 bytes, so this budget is roughly 37 MB for the whole session. Past
/// it a file's bytes land in the requesting process's omitted remainder and the
/// drop is counted in [`State::dropped_paths`].
const MAX_TRACKED_FILES: usize = 65_536;

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
    fn add(&mut self, write: bool, size: u32) {
        let bytes = u64::from(size);
        if write {
            self.write_bytes = self.write_bytes.saturating_add(bytes);
        } else {
            self.read_bytes = self.read_bytes.saturating_add(bytes);
        }
    }
}

/// Bytes of one process that no row can carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Remainder {
    pub read_bytes: u64,
    pub write_bytes: u64,
}

impl Remainder {
    fn add(&mut self, write: bool, size: u32) {
        let bytes = u64::from(size);
        if write {
            self.write_bytes = self.write_bytes.saturating_add(bytes);
        } else {
            self.read_bytes = self.read_bytes.saturating_add(bytes);
        }
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
    /// Cumulative bytes per file path, since the session started.
    pub files: Vec<(String, FileBytes)>,
    /// Bytes of requests whose file object could not be named: a Create that
    /// was never seen, one whose name did not resolve to a DOS path, or one
    /// dropped at a cap.
    pub unnamed: Remainder,
    /// Bytes of files dropped by [`MAX_TRACKED_FILES_PER_PID`] or by
    /// [`MAX_TRACKED_FILES`].
    pub omitted: Remainder,
    /// File events the callback was handed.
    pub events_seen: u64,
    /// ...of which the payload decoder accepted.
    pub events_decoded: u64,
    /// Read and Write records decoded for THIS process. Create events do not
    /// count: they carry no bytes, so they cannot make a byte count measured.
    pub pid_read_transfers: u64,
    pub pid_write_transfers: u64,
    /// Entries discarded from the `FileObject -> name` map when it reached
    /// [`MAX_OPEN_FILES`], over the whole session. From that point on their
    /// transfers are inside [`Self::unnamed`], which is why a row can read
    /// dashes while the session is still measured.
    pub cleared_file_objects: u64,
    /// Creates whose name did not resolve to a DOS path (a network share, a
    /// `\SystemRoot\` name, a raw device), over the whole session. Their
    /// transfers are inside [`Self::unnamed`].
    pub unnamed_creates: u64,
    /// Paths never tracked because a capacity limit (this process's or the
    /// session's) was full, over the whole session. Their transfers are inside
    /// [`Self::omitted`].
    pub dropped_paths: u64,
    /// Cumulative events ETW reports as lost; counts are lower bounds when
    /// this is non-zero or unknown.
    pub events_lost: Option<u64>,
}

impl FileIoSample {
    /// Read and Write records decoded for this process, both directions.
    pub fn pid_transfers(&self) -> u64 {
        self.pid_read_transfers
            .saturating_add(self.pid_write_transfers)
    }

    /// Whether, and how far, this sample may be trusted for one process.
    ///
    /// [`FileIoAccountingState::Measured`] requires that the payload layout
    /// matched AND that at least one read or write was decoded for this
    /// process, so a process the trace has seen nothing of keeps every row
    /// unavailable rather than reporting a measured zero.
    pub fn state(&self) -> FileIoAccountingState {
        if !self.running {
            return FileIoAccountingState::Unavailable;
        }
        if self.events_seen == 0 {
            // The session is up and delivered nothing at all: silence cannot be
            // told apart from a broken subscription.
            return FileIoAccountingState::Undelivered;
        }
        if self.events_decoded == 0 {
            return FileIoAccountingState::Undecodable;
        }
        if self.pid_transfers() == 0 {
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
            total.read_bytes = total.read_bytes.saturating_add(bytes.read_bytes);
            total.write_bytes = total.write_bytes.saturating_add(bytes.write_bytes);
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

/// Attach a Kernel-File sample to the handle rows it names.
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
/// A row with no record keeps `None` (rendered as unavailable), and a record
/// with no row is NOT dropped: it goes into the unnamed remainder, which is
/// what makes `read_bytes`/`write_bytes` the whole attribution for the process.
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
    let mut unnamed = sample.unnamed;
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
    let index_by_path: std::collections::HashMap<String, usize> = snapshot
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.to_lowercase(), index))
        .collect();
    for (path, bytes) in &sample.files {
        match index_by_path.get(&path.to_lowercase()) {
            Some(&index) => {
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
            None => {
                // Closed since the handle walk, or a file the walk could not
                // report at all.
                unnamed.read_bytes = unnamed.read_bytes.saturating_add(bytes.read_bytes);
                unnamed.write_bytes = unnamed.write_bytes.saturating_add(bytes.write_bytes);
            }
        }
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
        object: u64,
        name: &'a [u8],
    },
    Close {
        object: u64,
    },
    Transfer {
        object: u64,
        size: u32,
        write: bool,
    },
}

/// Pointer width of a Kernel-File payload: always this machine's.
///
/// [`super::etw::pointer_bytes`] answers a different question — how wide the
/// pointers are in a payload a USER-MODE process wrote, which is why the
/// record header flags a WOW64 caller (`EVENT_HEADER_FLAG_32_BIT_HEADER`) and
/// why the loader and profile providers need it. The records here are written
/// by the kernel: `FileObject`, `Irp` and `FileKey` are kernel addresses in
/// every record, whoever issued the request. Taking the header flag at face
/// value would read a 32-bit process's records four bytes off — either
/// rejected by the alignment check or charged to the wrong file — leaving that
/// process with dashes on every row while the session reports Measured.
const KERNEL_FILE_POINTER_BYTES: usize = std::mem::size_of::<usize>();

/// Offset of `FileObject` and of the trailing `FileName` in a Create payload,
/// or `None` for a template version this build does not know.
fn create_layout(version: u8, pointer_bytes: usize) -> Option<(usize, usize)> {
    match version {
        // v0: Irp, ThreadId, FileObject, CreateOptions, CreateAttributes,
        // ShareAccess, FileName.
        0 => Some((2 * pointer_bytes, 3 * pointer_bytes + 12)),
        // v1: Irp, FileObject, IssuingThreadId, CreateOptions,
        // CreateAttributes, ShareAccess, FileName.
        1 => Some((pointer_bytes, 2 * pointer_bytes + 16)),
        _ => None,
    }
}

/// Offset of `FileObject`, offset of `IOSize`, and the shortest payload that
/// holds them, for a Read/Write payload.
fn transfer_layout(version: u8, pointer_bytes: usize) -> Option<(usize, usize, usize)> {
    match version {
        // v0: ByteOffset u64, Irp, ThreadId, FileObject, FileKey, IOSize,
        // IOFlags.
        0 => Some((
            8 + 2 * pointer_bytes,
            8 + 4 * pointer_bytes,
            8 + 4 * pointer_bytes + 8,
        )),
        // v1: ByteOffset u64, Irp, FileObject, FileKey, IssuingThreadId,
        // IOSize, IOFlags, ExtraFlags.
        1 => Some((
            8 + pointer_bytes,
            8 + 3 * pointer_bytes + 4,
            8 + 3 * pointer_bytes + 16,
        )),
        _ => None,
    }
}

/// Offset of `FileObject` and the shortest payload that holds it, for a
/// Cleanup/Close payload.
fn close_layout(version: u8, pointer_bytes: usize) -> Option<(usize, usize)> {
    match version {
        // v0: Irp, ThreadId, FileObject, FileKey.
        0 => Some((2 * pointer_bytes, 4 * pointer_bytes)),
        // v1: Irp, FileObject, FileKey, IssuingThreadId.
        1 => Some((pointer_bytes, 3 * pointer_bytes + 4)),
        _ => None,
    }
}

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

/// Decode the `FileName` tail of a Create payload.
///
/// ETW writes a `win:UnicodeString` as little-endian UTF-16 running to the end
/// of the payload, NUL-terminated, with no length prefix. An empty or
/// odd-length tail is not a name, and the caller then keeps the handle
/// anonymous rather than inventing one.
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

/// Decode one record, or `None` when it does not look like the template this
/// build expects.
///
/// Every rejection here is a field that a wrong offset, a truncated payload or
/// a template change would produce, and the honest answer to all of them is
/// "unknown" — never a byte count.
fn parse_event(
    event_id: u16,
    version: u8,
    payload: &[u8],
    pointer_bytes: usize,
) -> Option<FileEvent<'_>> {
    match event_id {
        EVENT_CREATE => {
            let (object_offset, name_offset) = create_layout(version, pointer_bytes)?;
            let object = read_pointer(payload, object_offset, pointer_bytes)?;
            if !plausible_file_object(object, pointer_bytes) {
                return None;
            }
            let name = payload.get(name_offset..).unwrap_or_default();
            Some(FileEvent::Create { object, name })
        }
        EVENT_READ | EVENT_WRITE => {
            let (object_offset, size_offset, min_len) = transfer_layout(version, pointer_bytes)?;
            if payload.len() < min_len {
                return None;
            }
            let object = read_pointer(payload, object_offset, pointer_bytes)?;
            if !plausible_file_object(object, pointer_bytes) {
                return None;
            }
            let size = read_u32(payload, size_offset)?;
            if size > MAX_PLAUSIBLE_TRANSFER {
                return None;
            }
            Some(FileEvent::Transfer {
                object,
                size,
                write: event_id == EVENT_WRITE,
            })
        }
        EVENT_CLEANUP | EVENT_CLOSE => {
            let (object_offset, min_len) = close_layout(version, pointer_bytes)?;
            if payload.len() < min_len {
                return None;
            }
            let object = read_pointer(payload, object_offset, pointer_bytes)?;
            if !plausible_file_object(object, pointer_bytes) {
                return None;
            }
            Some(FileEvent::Close { object })
        }
        _ => None,
    }
}

/// The `FileObject -> name` correlation map.
struct OpenFile {
    /// The process whose Create produced this entry. A transfer under another
    /// pid is not charged to this name; see [`Shared::transfer`].
    pid: u32,
    path: Arc<str>,
}

/// Per-process accumulators.
#[derive(Default)]
struct PidFiles {
    /// The process creation time this bucket has been bound to, once a sample
    /// has named it. A pid is reused, and the counters of the process that held
    /// it before must not be handed to its successor; see [`State::bind_epoch`].
    epoch_s: Option<i64>,
    /// Cumulative bytes per path, since the session started.
    files: HashMap<Arc<str>, FileBytes>,
    /// Read and Write records decoded for this process, by direction.
    read_transfers: u64,
    write_transfers: u64,
    unnamed: Remainder,
    omitted: Remainder,
}

#[derive(Default)]
struct State {
    events_seen: u64,
    events_decoded: u64,
    /// Id, template version and payload length of the most recent record the
    /// decoder rejected, so a layout change and a truncated or malformed
    /// payload can be told apart from a log line instead of a debugger.
    rejected_event_id: u16,
    rejected_version: u8,
    rejected_payload_len: u32,
    open: HashMap<u64, OpenFile>,
    pids: HashMap<u32, PidFiles>,
    /// Distinct paths held across every bucket in `pids`. Kept alongside the
    /// map rather than recomputed per transfer; [`State::prune_dead`] and
    /// [`State::bind_epoch`] rebuild it after they drop buckets.
    tracked_paths: usize,
    /// Entries discarded when the open-file map was cleared at its cap.
    cleared_file_objects: u64,
    /// Creates whose name did not resolve to a DOS path.
    unnamed_creates: u64,
    /// Paths never tracked because a capacity limit was full.
    dropped_paths: u64,
}

impl State {
    /// Drop the accounting of processes that no longer exist.
    ///
    /// Both maps are pinned to live processes for the same reason the disk
    /// trace is: a process that exited can never be asked about again, and its
    /// entries would otherwise accumulate for the lifetime of the service.
    /// `tracked_paths` is rebuilt from what survived instead of being
    /// decremented per bucket, so it cannot drift.
    fn prune_dead(&mut self, live_pids: &HashSet<u32>) {
        self.pids.retain(|pid, _| live_pids.contains(pid));
        self.open.retain(|_, file| live_pids.contains(&file.pid));
        self.tracked_paths = self.pids.values().map(|files| files.files.len()).sum();
    }

    /// Bind `pid`'s bucket to the process creation time the caller verified.
    ///
    /// The callback cannot know which incarnation of a pid a record belongs to;
    /// the broker does, because every request carries the creation time it
    /// sampled. Binding the bucket to it means a pid reused between two samples
    /// starts empty instead of inheriting the previous process's paths, and the
    /// `FileObject` names that process had open are dropped for the same reason
    /// (they belong to a dead object space).
    fn bind_epoch(&mut self, pid: u32, start_epoch_s: i64) {
        let reused = self.pids.get(&pid).is_some_and(|files| {
            files
                .epoch_s
                .is_some_and(|epoch_s| epoch_s != start_epoch_s)
        });
        if reused {
            if let Some(files) = self.pids.remove(&pid) {
                self.tracked_paths = self.tracked_paths.saturating_sub(files.files.len());
            }
            self.open.retain(|_, file| file.pid != pid);
            tracing::debug!(
                pid,
                start_epoch_s,
                "file trace saw a reused pid; its bucket starts fresh"
            );
        }
        if let Some(files) = self.pids.get_mut(&pid) {
            files.epoch_s = Some(start_epoch_s);
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
    /// A file record the decoder could not read. Counted, never guessed at.
    ///
    /// The identity of the rejected record is kept so a log line can say which
    /// layout is unknown (id and version) and whether the payload was truncated
    /// (its length) instead of just that something failed.
    fn undecodable(&self, event_id: u16, version: u8, payload_len: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.events_seen = state.events_seen.saturating_add(1);
            state.rejected_event_id = event_id;
            state.rejected_version = version;
            state.rejected_payload_len = payload_len;
        }
    }

    /// Name a Create's file object. The name is resolved before the state lock
    /// is taken because resolving can rescan the drive list.
    fn create(&self, pid: u32, object: u64, name: &[u8]) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let resolved = self.resolve_name(name);
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.events_seen = state.events_seen.saturating_add(1);
        state.events_decoded = state.events_decoded.saturating_add(1);
        let Some(path) = resolved else {
            // A name that is not a resolvable device or DOS path (a network
            // share, a `\SystemRoot\` name, a raw device) keeps its bytes
            // unnamed. Nothing is inserted, so it must not cost the map
            // anything at the cap either: clearing 65 536 live entries for a
            // Create that is then thrown away would turn every file already
            // open into a remainder.
            state.unnamed_creates = state.unnamed_creates.saturating_add(1);
            return;
        };
        if state.open.len() >= MAX_OPEN_FILES {
            // A dropped Close leaves an entry behind, so the map can reach the
            // cap with nothing that is safe to evict. Clearing recovers the
            // memory; the names of the files still open are lost, and their
            // reads and writes fall into the unnamed remainder (counted) until
            // the next Create, instead of being charged to a name this module
            // can no longer trust.
            state.cleared_file_objects = state
                .cleared_file_objects
                .saturating_add(state.open.len() as u64);
            state.open.clear();
            tracing::debug!(
                cap = MAX_OPEN_FILES,
                "file trace dropped its open-file map at the cap"
            );
        }
        state.open.insert(object, OpenFile { pid, path });
    }

    fn close(&self, object: u64) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.events_seen = state.events_seen.saturating_add(1);
            state.events_decoded = state.events_decoded.saturating_add(1);
            state.open.remove(&object);
        }
    }

    fn transfer(&self, pid: u32, object: u64, size: u32, write: bool) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.events_seen = state.events_seen.saturating_add(1);
        state.events_decoded = state.events_decoded.saturating_add(1);
        let path = match state.open.get(&object) {
            Some(open) if open.pid == pid => Some(Arc::clone(&open.path)),
            _ => None,
        };
        // Read before the bucket is borrowed: the bucket reference holds the
        // whole state, so the session-wide counters cannot be touched while it
        // is alive.
        let global_room = state.tracked_paths < MAX_TRACKED_FILES;
        let mut newly_tracked = false;
        let mut dropped_path = false;
        {
            let files = state.pids.entry(pid).or_default();
            if write {
                files.write_transfers = files.write_transfers.saturating_add(1);
            } else {
                files.read_transfers = files.read_transfers.saturating_add(1);
            }
            let Some(path) = path else {
                files.unnamed.add(write, size);
                return;
            };
            let new_path = !files.files.contains_key(path.as_ref());
            if new_path && (!global_room || files.files.len() >= MAX_TRACKED_FILES_PER_PID) {
                // Out of room for this process or for the session. The bytes
                // stay in the accounting remainder; charging them to a
                // neighbouring file would be a made-up per-file number.
                files.omitted.add(write, size);
                dropped_path = true;
            } else {
                files.files.entry(path).or_default().add(write, size);
                newly_tracked = new_path;
            }
        }
        if newly_tracked {
            state.tracked_paths = state.tracked_paths.saturating_add(1);
        }
        if dropped_path {
            state.dropped_paths = state.dropped_paths.saturating_add(1);
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

/// Translate a Kernel-File name into the DOS path the handle walk reports.
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

/// A running per-file I/O trace.
pub struct FileEtw {
    session: Session<Shared>,
    started: Instant,
    /// Which of [`sources`] actually started. Kept because "which session is
    /// carrying this" is the first question any report about the feature has to
    /// answer, and it cannot be re-derived from a live session afterwards.
    source: FileSource,
}

impl FileEtw {
    /// Which session is carrying this trace; see [`sources`].
    ///
    /// The kernel logger is the one that delivers the classic `FileIo` class
    /// machine-wide; the private fallback only runs because that one was taken.
    pub(super) fn source(&self) -> FileSource {
        self.source
    }

    /// Whether events are still arriving; see [`Session::is_running`].
    pub fn is_running(&self) -> bool {
        self.session.is_running()
    }

    /// Start a session, trying [`sources`] in order.
    ///
    /// The first shape that starts wins and is logged, because which one is
    /// running is the fact an operator needs: the kernel logger carries the
    /// class machine-wide, the private fallback only for as long as Windows
    /// keeps multiplexing it. A refusal is only the answer once EVERY shape was
    /// refused, and [`FileTraceRefusal::KernelLoggerInUse`] is preferred over
    /// [`FileTraceRefusal::Unavailable`] in that case: "another application
    /// owns the kernel logger" is the fact that explains the whole failure and
    /// the only one a user can act on.
    ///
    /// One [`Shared`] serves every attempt. A refused attempt never published a
    /// counter (it delivered no record), so a fallback starts from a clean
    /// slate under the same accounting, and the sample a caller already holds
    /// stays valid either way.
    pub fn try_start(role: TraceRole) -> Result<Self, FileTraceRefusal> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            devices: Mutex::new(DeviceCache::default()),
            live: AtomicBool::new(true),
        });
        let mut refusal = FileTraceRefusal::Unavailable;
        for source in sources(role) {
            match Session::try_start(
                source.name,
                source.kind,
                source.providers,
                Arc::clone(&shared),
                on_event,
                "tm-file-etw",
            ) {
                Ok(session) => {
                    tracing::info!(
                        session = source.name,
                        kind = ?source.kind,
                        providers = source.providers.len(),
                        "per-file I/O trace started"
                    );
                    return Ok(Self {
                        session,
                        started: Instant::now(),
                        source,
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
    /// the larger count, so a per-window delta would be read as a total. A
    /// session that the idle watchdog stopped and restarted starts a new
    /// epoch — the dialog's larger-of-two merge then keeps the previous
    /// epoch's count for a row that is still open, which is an older
    /// measurement of the same quantity rather than a new one.
    ///
    /// An EMPTY `live_pids` means "the caller could not enumerate processes",
    /// never "nothing is alive": pruning against it would throw away every
    /// measurement. `start_epoch_s` is the creation time the caller verified
    /// for `pid`, and it is what keeps a reused pid from inheriting the
    /// previous process's counters (see [`State::bind_epoch`]).
    pub fn sample(&self, pid: u32, start_epoch_s: i64, live_pids: &HashSet<u32>) -> FileIoSample {
        let (mut sample, rejected) = {
            let mut state = match self.session.shared().state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !live_pids.is_empty() {
                state.prune_dead(live_pids);
            }
            state.bind_epoch(pid, start_epoch_s);
            let events_seen = state.events_seen;
            let events_decoded = state.events_decoded;
            let files = state.pids.get(&pid);
            let sample = FileIoSample {
                running: self.session.is_running(),
                since_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                files: files
                    .map(|files| {
                        files
                            .files
                            .iter()
                            .map(|(path, bytes)| (path.to_string(), *bytes))
                            .collect()
                    })
                    .unwrap_or_default(),
                unnamed: files.map_or_else(Remainder::default, |files| files.unnamed),
                omitted: files.map_or_else(Remainder::default, |files| files.omitted),
                events_seen,
                events_decoded,
                pid_read_transfers: files.map_or(0, |files| files.read_transfers),
                pid_write_transfers: files.map_or(0, |files| files.write_transfers),
                cleared_file_objects: state.cleared_file_objects,
                unnamed_creates: state.unnamed_creates,
                dropped_paths: state.dropped_paths,
                events_lost: None,
            };
            let rejected = (
                state.rejected_event_id,
                state.rejected_version,
                state.rejected_payload_len,
            );
            (sample, rejected)
        };
        if sample.events_seen > 0 && sample.events_decoded == 0 {
            // Loud on purpose, once per request while it lasts: this is the
            // failure that silently turns every byte count into a gap. The id,
            // version and length separate the two causes: an id and version
            // this build has no offsets for is a layout change, while a
            // rejected id/version we DO know is a truncated or malformed
            // payload.
            tracing::warn!(
                events_seen = sample.events_seen,
                event_id = rejected.0,
                version = rejected.1,
                payload_len = rejected.2,
                "file trace decoded no records - unknown Kernel-File payload layout or rejected payloads"
            );
        }
        // Queried outside the lock: the session properties live in the kernel.
        sample.events_lost = self.session.events_lost();
        sample
    }
}

/// ETW record callback. Kept minimal; only a Create allocates.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(shared) = (unsafe { etw::context_of::<Shared>(record) }) else {
        return;
    };
    // Classic (MOF) identity: the class GUID in `ProviderId`, the operation in
    // the opcode. The `SYSTEM_IO_KW_FILE` enable names the SystemIo GROUP, and
    // each record it delivers still carries its own class here — the same
    // split [`super::disk_etw`] relies on for the `DiskIo` class. The kernel
    // fills in no manifest event id for these records, so matching on one
    // discarded every file event that did arrive.
    if record.EventHeader.ProviderId != FileIoGuid {
        return;
    }
    let event_id = u16::from(record.EventHeader.EventDescriptor.Opcode);
    if !matches!(
        event_id,
        EVENT_CREATE | EVENT_CLEANUP | EVENT_CLOSE | EVENT_READ | EVENT_WRITE
    ) {
        return;
    }
    // A file event without a payload is exactly as undecodable as a short one,
    // so it is counted rather than dropped.
    let payload = unsafe { etw::payload_of(record) }.unwrap_or_default();
    let version = record.EventHeader.EventDescriptor.Version;
    handle_payload(
        shared,
        record.EventHeader.ProcessId,
        event_id,
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
fn handle_payload(shared: &Shared, pid: u32, event_id: u16, version: u8, payload: &[u8]) {
    match parse_event(event_id, version, payload, KERNEL_FILE_POINTER_BYTES) {
        Some(FileEvent::Create { object, name }) => shared.create(pid, object, name),
        Some(FileEvent::Close { object }) => shared.close(object),
        Some(FileEvent::Transfer {
            object,
            size,
            write,
        }) => shared.transfer(pid, object, size, write),
        None => shared.undecodable(event_id, version, payload.len() as u32),
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

    /// `Create` payload in the provider's v1 template.
    fn create_payload(pointer_bytes: usize, object: u64, name: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(std::iter::repeat_n(0x11u8, pointer_bytes)); // Irp
        payload.extend_from_slice(&object.to_le_bytes()[..pointer_bytes]); // FileObject
        payload.extend_from_slice(&1_000u32.to_le_bytes()); // IssuingThreadId
        payload.extend_from_slice(&0u32.to_le_bytes()); // CreateOptions
        payload.extend_from_slice(&0u32.to_le_bytes()); // CreateAttributes
        payload.extend_from_slice(&3u32.to_le_bytes()); // ShareAccess
        for unit in name.encode_utf16() {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
        payload.extend_from_slice(&0u16.to_le_bytes()); // NUL terminator
        payload
    }

    /// `Read`/`Write` payload in the provider's v1 template.
    fn transfer_payload(pointer_bytes: usize, object: u64, size: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4_096u64.to_le_bytes()); // ByteOffset
        payload.extend(std::iter::repeat_n(0x11u8, pointer_bytes)); // Irp
        payload.extend_from_slice(&object.to_le_bytes()[..pointer_bytes]); // FileObject
        payload.extend(std::iter::repeat_n(0x22u8, pointer_bytes)); // FileKey
        payload.extend_from_slice(&1_000u32.to_le_bytes()); // IssuingThreadId
        payload.extend_from_slice(&size.to_le_bytes()); // IOSize
        payload.extend_from_slice(&0u32.to_le_bytes()); // IOFlags
        payload.extend_from_slice(&0u32.to_le_bytes()); // ExtraFlags
        payload
    }

    /// `Cleanup`/`Close` payload in the provider's v1 template.
    fn close_payload(pointer_bytes: usize, object: u64) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend(std::iter::repeat_n(0x11u8, pointer_bytes)); // Irp
        payload.extend_from_slice(&object.to_le_bytes()[..pointer_bytes]); // FileObject
        payload.extend(std::iter::repeat_n(0x22u8, pointer_bytes)); // FileKey
        payload.extend_from_slice(&1_000u32.to_le_bytes()); // IssuingThreadId
        payload
    }

    fn shared() -> Shared {
        Shared {
            state: Mutex::new(State::default()),
            devices: Mutex::new(DeviceCache::default()),
            live: AtomicBool::new(true),
        }
    }

    /// The callback's work, minus the ETW plumbing: decode, then account.
    fn deliver(shared: &Shared, pid: u32, event_id: u16, payload: &[u8]) {
        handle_payload(shared, pid, event_id, 1, payload);
    }

    /// Drive the REAL callback with a synthetic record of `provider`'s classic
    /// class, the way the kernel logger hands one over: the class GUID in
    /// `ProviderId`, the operation in the opcode, and no manifest event id.
    fn deliver_kernel_record_as(
        shared: &Shared,
        provider: windows::core::GUID,
        opcode: u16,
        version: u8,
        pid: u32,
        payload: &[u8],
    ) {
        assert!(u8::try_from(opcode).is_ok(), "opcodes are one byte");
        // SAFETY: `EVENT_RECORD` is plain data, and a zeroed one is a record
        // with no payload, no context and an all-zero descriptor — every field
        // this test cares about is set below.
        let mut record: EVENT_RECORD = unsafe { std::mem::zeroed() };
        record.EventHeader.ProviderId = provider;
        record.EventHeader.EventDescriptor.Opcode = opcode as u8;
        record.EventHeader.EventDescriptor.Version = version;
        // Deliberately zero: the classic identity carries no event id, and
        // matching on one is what discarded every record that did arrive.
        record.EventHeader.EventDescriptor.Id = 0;
        record.EventHeader.ProcessId = pid;
        record.UserData = payload.as_ptr() as *mut core::ffi::c_void;
        record.UserDataLength = payload.len() as u16;
        record.UserContext = shared as *const Shared as *mut core::ffi::c_void;
        // SAFETY: both pointers stay alive across the call and the callback
        // copies nothing out of them.
        unsafe { on_event(&mut record) };
    }

    fn deliver_kernel_record(shared: &Shared, opcode: u16, version: u8, pid: u32, payload: &[u8]) {
        deliver_kernel_record_as(shared, FileIoGuid, opcode, version, pid, payload);
    }

    fn sample_of(shared: &Shared, pid: u32) -> FileIoSample {
        let state = shared.state.lock().unwrap();
        let files = state.pids.get(&pid);
        FileIoSample {
            running: true,
            since_ms: 0,
            files: files
                .map(|files| {
                    files
                        .files
                        .iter()
                        .map(|(path, bytes)| (path.to_string(), *bytes))
                        .collect()
                })
                .unwrap_or_default(),
            unnamed: files.map_or_else(Remainder::default, |files| files.unnamed),
            omitted: files.map_or_else(Remainder::default, |files| files.omitted),
            events_seen: state.events_seen,
            events_decoded: state.events_decoded,
            pid_read_transfers: files.map_or(0, |files| files.read_transfers),
            pid_write_transfers: files.map_or(0, |files| files.write_transfers),
            cleared_file_objects: state.cleared_file_objects,
            unnamed_creates: state.unnamed_creates,
            dropped_paths: state.dropped_paths,
            events_lost: None,
        }
    }

    fn bytes(sample: &FileIoSample, path: &str) -> Option<FileBytes> {
        sample
            .files
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, bytes)| *bytes)
    }

    /// The kernel delivers per-file events under the classic `FileIo` class, so
    /// that is what the callback has to accept: the class GUID plus one of the
    /// five opcodes, decoded with the manifest's fields.
    #[test]
    fn a_kernel_file_io_record_decodes_from_the_classic_identity() {
        let shared = shared();
        deliver_kernel_record(
            &shared,
            EVENT_CREATE,
            1,
            20,
            &create_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, r"\??\C:\data\a.bin"),
        );
        deliver_kernel_record(
            &shared,
            EVENT_READ,
            1,
            20,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, 4_096),
        );
        deliver_kernel_record(
            &shared,
            EVENT_WRITE,
            1,
            20,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, 512),
        );
        let sample = sample_of(&shared, 20);
        assert_eq!(sample.events_seen, 3);
        assert_eq!(sample.events_decoded, 3);
        assert_eq!(
            bytes(&sample, r"C:\data\a.bin"),
            Some(FileBytes {
                read_bytes: 4_096,
                write_bytes: 512,
            })
        );
        // Close forgets the object, so a later transfer through it is unnamed
        // rather than charged to the file that had the address.
        deliver_kernel_record(
            &shared,
            EVENT_CLOSE,
            1,
            20,
            &close_payload(KERNEL_FILE_POINTER_BYTES, 0x1000),
        );
        deliver_kernel_record(
            &shared,
            EVENT_READ,
            1,
            20,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, 128),
        );
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

    /// The four operations the decoder keeps, driven by records carrying the
    /// identity the `SYSTEM_IO_KW_FILE` enable delivers: the `FileIo` class
    /// GUID plus the classic opcode. Create names the object, Read and Write
    /// add their sizes, Close forgets it — and every one of them counts as
    /// DECODED, so a session that delivers them is measured, not undecodable.
    #[test]
    fn a_system_io_file_record_decodes_create_read_write_close() {
        let shared = shared();
        let pid = 88;
        let object = 0x9000;
        deliver_kernel_record(
            &shared,
            EVENT_CREATE,
            1,
            pid,
            &create_payload(KERNEL_FILE_POINTER_BYTES, object, r"\??\C:\data\d.bin"),
        );
        deliver_kernel_record(
            &shared,
            EVENT_READ,
            1,
            pid,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, object, 1_024),
        );
        deliver_kernel_record(
            &shared,
            EVENT_WRITE,
            1,
            pid,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, object, 2_048),
        );
        let sample = sample_of(&shared, pid);
        assert_eq!(sample.events_seen, 3);
        assert_eq!(sample.events_decoded, 3);
        assert_eq!(
            bytes(&sample, r"C:\data\d.bin"),
            Some(FileBytes {
                read_bytes: 1_024,
                write_bytes: 2_048,
            })
        );
        assert_eq!(sample.state(), FileIoAccountingState::Measured);
        deliver_kernel_record(
            &shared,
            EVENT_CLOSE,
            1,
            pid,
            &close_payload(KERNEL_FILE_POINTER_BYTES, object),
        );
        let sample = sample_of(&shared, pid);
        assert_eq!(sample.events_seen, 4);
        assert_eq!(sample.events_decoded, 4, "Close decodes too");
        assert!(
            !shared.state.lock().unwrap().open.contains_key(&object),
            "Close forgets the file object"
        );
        assert_eq!(
            bytes(&sample, r"C:\data\d.bin"),
            Some(FileBytes {
                read_bytes: 1_024,
                write_bytes: 2_048,
            }),
            "closing does not erase what was measured"
        );
    }

    /// The defect this replaces: a live elevated session reported `undelivered`
    /// with `events_lost` at zero because no record ever arrived. Once one does,
    /// the state must move on — and only a decoded read or write may make it
    /// `measured`, never a Create.
    #[test]
    fn a_delivered_kernel_record_is_no_longer_undelivered() {
        let shared = shared();
        assert_eq!(
            sample_of(&shared, 20).state(),
            FileIoAccountingState::Undelivered
        );
        deliver_kernel_record(
            &shared,
            EVENT_CREATE,
            1,
            20,
            &create_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, r"\??\C:\data\a.bin"),
        );
        assert_eq!(
            sample_of(&shared, 20).state(),
            FileIoAccountingState::Undelivered,
            "a Create names the file but carries no bytes"
        );
        deliver_kernel_record(
            &shared,
            EVENT_READ,
            1,
            20,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, 4_096),
        );
        let sample = sample_of(&shared, 20);
        assert_eq!(sample.state(), FileIoAccountingState::Measured);
        assert_eq!(sample.pid_read_transfers, 1);

        // A record from another class sharing the session is not a file event:
        // it must leave every counter alone instead of making the state
        // `undecodable` or inflating the byte counts.
        deliver_kernel_record_as(
            &shared,
            windows::Win32::System::Diagnostics::Etw::ThreadGuid,
            EVENT_READ,
            1,
            20,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, 0x1000, 8_192),
        );
        assert_eq!(sample_of(&shared, 20), sample);
    }

    /// The record shape the PREFERRED session delivers: a classic `FileIo`
    /// record, whose identity is the class GUID in `ProviderId` and the
    /// operation in the opcode (no manifest event id, no provider enable —
    /// `EVENT_TRACE_FLAG_FILE_IO` is the whole subscription). Driven through the
    /// real callback, the four operations the decoder keeps must add up to a
    /// measured sample.
    #[test]
    fn a_kernel_logger_file_io_record_decodes_into_a_measured_sample() {
        let shared = shared();
        let pid = 4_692;
        let object = 0xff_ff_9a_00_12_34_00_00u64;
        deliver_kernel_record(
            &shared,
            EVENT_CREATE,
            1,
            pid,
            &create_payload(
                KERNEL_FILE_POINTER_BYTES,
                object,
                r"\Device\HarddiskVolume3\Users\me\kernel.dat",
            ),
        );
        deliver_kernel_record(
            &shared,
            EVENT_WRITE,
            1,
            pid,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, object, 8_192),
        );
        deliver_kernel_record(
            &shared,
            EVENT_READ,
            1,
            pid,
            &transfer_payload(KERNEL_FILE_POINTER_BYTES, object, 1_024),
        );
        let sample = sample_of(&shared, pid);
        assert_eq!(
            bytes(&sample, r"C:\Users\me\kernel.dat"),
            Some(FileBytes {
                read_bytes: 1_024,
                write_bytes: 8_192,
            }),
            "the device path is translated exactly as on the fallback session"
        );
        assert_eq!(sample.state(), FileIoAccountingState::Measured);
        assert_eq!(sample.events_seen, 3);
        assert_eq!(sample.events_decoded, 3);
        deliver_kernel_record(
            &shared,
            EVENT_CLEANUP,
            1,
            pid,
            &close_payload(KERNEL_FILE_POINTER_BYTES, object),
        );
        // Cleanup ends the handle's accounting; the measurement stands.
        let sample = sample_of(&shared, pid);
        assert_eq!(sample.events_decoded, 4);
        assert_eq!(
            bytes(&sample, r"C:\Users\me\kernel.dat"),
            Some(FileBytes {
                read_bytes: 1_024,
                write_bytes: 8_192,
            })
        );
        assert!(!shared.state.lock().unwrap().open.contains_key(&object));
    }

    /// The session kind plus the provider enable ARE the subscription: a
    /// manifest session delivers no `FileIo` record however it is enabled, and
    /// the legacy `EVENT_TRACE_FLAG_FILE_IO` shape delivered none either on a
    /// PRIVATE system logger — a live elevated selfcheck reported `undelivered`
    /// with `events_lost` at 0 on both. What is left there is the documented
    /// SystemIo provider path: the group GUID with the file keywords, on a system
    /// logger.
    #[test]
    fn the_fallback_session_enables_the_system_io_provider_with_the_file_keywords() {
        assert_eq!(LOGGER_KIND, LoggerKind::SystemFile);
        assert_eq!(PROVIDERS.len(), 1, "one keyword set, not a broad enable");
        assert_eq!(PROVIDERS[0].guid, SystemIoProviderGuid);
        assert_eq!(
            PROVIDERS[0].match_any_keyword,
            SYSTEM_IO_KW_FILE | SYSTEM_IO_KW_FILENAME
        );
        assert_eq!(
            PROVIDERS[0].match_any_keyword & !(SYSTEM_IO_KW_FILE | SYSTEM_IO_KW_FILENAME),
            0,
            "no other SystemIo keyword may ride this session"
        );
        assert!(
            PROVIDERS[0].match_any_keyword & SYSTEM_IO_KW_FILE != 0,
            "the class keyword stays: it is what makes the transfers arrive"
        );
        // The class the decoder accepts is the FILE class, not the group's
        // disk class: the enable and the record identity are different GUIDs.
        assert_ne!(PROVIDERS[0].guid, FileIoGuid);
    }

    /// The attempt order IS the design, so it is data a test reads.
    ///
    /// The kernel logger comes first because it is the session that actually
    /// delivers the classic `FileIo` class; the private SystemIo session is the
    /// fallback for when another tracing tool owns that one. The kernel attempt
    /// enables NO provider — its `EnableFlags` are the subscription — and it
    /// keeps the global name whatever the role, while the fallback's name is
    /// per role so the two hosts never share a session.
    #[test]
    fn the_kernel_logger_is_tried_before_the_private_system_io_session() {
        for role in [TraceRole::Service, TraceRole::App] {
            let [primary, fallback] = sources(role);
            assert_eq!(primary.kind, LoggerKind::KernelFile);
            assert_eq!(primary.name, KERNEL_LOGGER);
            assert!(
                primary.providers.is_empty(),
                "the kernel logger subscribes through EnableFlags, not a provider"
            );
            assert_eq!(
                primary.providers.len(),
                KERNEL_PROVIDERS.len(),
                "and that is what KERNEL_PROVIDERS says"
            );
            assert_eq!(fallback.kind, LOGGER_KIND);
            assert_eq!(fallback.name, session_name(role));
            assert_ne!(fallback.name, KERNEL_LOGGER);
            assert_eq!(fallback.providers.len(), 1);
            assert_eq!(fallback.providers[0].guid, SystemIoProviderGuid);
        }
        // The two hosts must not share the FALLBACK name; only the kernel
        // logger is global, and that is exactly why it is exclusive.
        assert_ne!(
            sources(TraceRole::Service)[1].name,
            sources(TraceRole::App)[1].name
        );
    }

    /// A refusal from the kernel attempt becomes the state that names an owner,
    /// and no byte count is invented: the snapshot keeps the handle walk's rows
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
        assert_eq!(accounting.unnamed_read_bytes, 0);
        assert_eq!(accounting.omitted_write_bytes, 0);

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

    /// A read-only handle reports bytes in Read and none in Write, and the
    /// reverse for a write-only one. This is the whole product requirement: the
    /// two directions are never mixed up or duplicated.
    #[test]
    fn a_read_only_handle_reports_only_read_bytes() {
        let shared = shared();
        deliver(
            &shared,
            20,
            EVENT_CREATE,
            &create_payload(8, 0x1000, r"\??\C:\data\a.bin"),
        );
        deliver(&shared, 20, EVENT_READ, &transfer_payload(8, 0x1000, 4_096));
        deliver(&shared, 20, EVENT_READ, &transfer_payload(8, 0x1000, 512));
        let sample = sample_of(&shared, 20);
        assert_eq!(
            bytes(&sample, r"C:\data\a.bin"),
            Some(FileBytes {
                read_bytes: 4_608,
                write_bytes: 0,
            })
        );
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
            &create_payload(8, 0x2000, r"\??\C:\data\b.bin"),
        );
        deliver(
            &shared,
            24,
            EVENT_WRITE,
            &transfer_payload(8, 0x2000, 8_192),
        );
        let sample = sample_of(&shared, 24);
        assert_eq!(
            bytes(&sample, r"C:\data\b.bin"),
            Some(FileBytes {
                read_bytes: 0,
                write_bytes: 8_192,
            })
        );
    }

    /// The full lifecycle: the name arrives with Create, both transfers land on
    /// it, and Close forgets the file object so a later reuse cannot inherit it.
    #[test]
    fn create_read_write_close_accumulates_then_forgets() {
        let shared = shared();
        let path = r"\??\C:\data\c.bin";
        deliver(&shared, 8, EVENT_CREATE, &create_payload(8, 0x3000, path));
        deliver(&shared, 8, EVENT_READ, &transfer_payload(8, 0x3000, 1_024));
        deliver(&shared, 8, EVENT_WRITE, &transfer_payload(8, 0x3000, 2_048));
        let sample = sample_of(&shared, 8);
        assert_eq!(
            bytes(&sample, r"C:\data\c.bin"),
            Some(FileBytes {
                read_bytes: 1_024,
                write_bytes: 2_048,
            })
        );
        assert_eq!(
            sample.attributed(),
            FileBytes {
                read_bytes: 1_024,
                write_bytes: 2_048
            }
        );
        deliver(&shared, 8, EVENT_CLOSE, &close_payload(8, 0x3000));
        deliver(&shared, 8, EVENT_READ, &transfer_payload(8, 0x3000, 4_096));
        let sample = sample_of(&shared, 8);
        // The counters keep what was measured; the new read has no name.
        assert_eq!(
            bytes(&sample, r"C:\data\c.bin"),
            Some(FileBytes {
                read_bytes: 1_024,
                write_bytes: 2_048,
            })
        );
        assert_eq!(
            sample.unnamed,
            Remainder {
                read_bytes: 4_096,
                write_bytes: 0
            }
        );
        assert_eq!(
            sample.attributed(),
            FileBytes {
                read_bytes: 5_120,
                write_bytes: 2_048,
            },
            "rows plus remainders are the whole attribution"
        );
    }

    /// A transfer whose FileObject was never seen — the normal case for handles
    /// opened before the session started — is attributed to nobody.
    #[test]
    fn an_unseen_file_object_is_never_charged_to_a_name() {
        let shared = shared();
        deliver(
            &shared,
            30,
            EVENT_READ,
            &transfer_payload(8, 0x4000, 65_536),
        );
        deliver(
            &shared,
            30,
            EVENT_WRITE,
            &transfer_payload(8, 0x4000, 1_024),
        );
        let sample = sample_of(&shared, 30);
        assert!(sample.files.is_empty(), "nothing may be charged");
        assert_eq!(
            sample.unnamed,
            Remainder {
                read_bytes: 65_536,
                write_bytes: 1_024,
            }
        );
    }

    /// Bytes are attributed to the process in the event header, one row per
    /// process: the same file read by two processes is two rows.
    #[test]
    fn bytes_are_attributed_to_the_header_process() {
        let shared = shared();
        let path = r"\??\C:\data\shared.bin";
        deliver(&shared, 40, EVENT_CREATE, &create_payload(8, 0x5000, path));
        deliver(&shared, 40, EVENT_READ, &transfer_payload(8, 0x5000, 4_096));
        // A handle duplicated into another process resolves the same object
        // but belongs to that process's accounting, not to the creator's.
        deliver(&shared, 44, EVENT_READ, &transfer_payload(8, 0x5000, 512));
        assert_eq!(
            bytes(&sample_of(&shared, 40), r"C:\data\shared.bin"),
            Some(FileBytes {
                read_bytes: 4_096,
                write_bytes: 0,
            })
        );
        let other = sample_of(&shared, 44);
        assert!(
            other.files.is_empty(),
            "a foreign pid must not inherit the name"
        );
        assert_eq!(
            other.unnamed,
            Remainder {
                read_bytes: 512,
                write_bytes: 0
            }
        );
        assert_eq!(other.pid_transfers(), 1);
    }

    /// The open-file map is capped, and what it discards is counted rather than
    /// silently lost. A Create that will not be inserted must not pay that
    /// price: clearing the map for a network-share name would turn every file
    /// already open into a remainder.
    #[test]
    fn the_open_file_map_is_capped_and_counts_what_it_drops() {
        let shared = shared();
        for index in 0..MAX_OPEN_FILES {
            let path = format!(r"\??\C:\f{index}");
            deliver(
                &shared,
                8,
                EVENT_CREATE,
                &create_payload(8, 0x10_0000 + index as u64 * 8, &path),
            );
        }
        assert_eq!(shared.state.lock().unwrap().open.len(), MAX_OPEN_FILES);
        // A name that cannot resolve is not inserted, so it does not clear the
        // map either: every live entry survives and only the refusal is
        // counted.
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(8, 0x30_0000, r"\SystemRoot\unresolvable"),
        );
        {
            let state = shared.state.lock().unwrap();
            assert_eq!(state.open.len(), MAX_OPEN_FILES, "nothing was inserted");
            assert_eq!(state.cleared_file_objects, 0, "nothing was cleared");
            assert_eq!(state.unnamed_creates, 1);
            assert!(
                state.open.contains_key(&0x10_0000),
                "the first file is still named"
            );
        }
        // One more Create that DOES resolve hits the cap: the map is cleared
        // and the drop counted.
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(8, 0x20_0000, r"\??\C:\last"),
        );
        let state = shared.state.lock().unwrap();
        assert_eq!(state.open.len(), 1, "the map restarts bounded");
        assert_eq!(state.cleared_file_objects, MAX_OPEN_FILES as u64);
        assert!(state.open.contains_key(&0x20_0000));
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
            &create_payload(8, 0x1000, r"\??\C:\data\budget.bin"),
        );
        {
            let mut state = shared.state.lock().unwrap();
            state.tracked_paths = MAX_TRACKED_FILES;
        }
        deliver(&shared, 8, EVENT_WRITE, &transfer_payload(8, 0x1000, 4_096));
        let sample = sample_of(&shared, 8);
        assert!(
            sample.files.is_empty(),
            "a path with no room must not get a row"
        );
        assert_eq!(
            sample.omitted,
            Remainder {
                read_bytes: 0,
                write_bytes: 4_096
            }
        );
        assert_eq!(sample.dropped_paths, 1);
        assert_eq!(
            sample.attributed().write_bytes,
            4_096,
            "an untracked file is still part of the process total"
        );

        // Below the budget the same transfer tracks normally, and an already
        // tracked path does not consume a second slot.
        {
            let mut state = shared.state.lock().unwrap();
            state.tracked_paths = MAX_TRACKED_FILES - 1;
        }
        deliver(&shared, 8, EVENT_WRITE, &transfer_payload(8, 0x1000, 256));
        let sample = sample_of(&shared, 8);
        assert_eq!(
            bytes(&sample, r"C:\data\budget.bin"),
            Some(FileBytes {
                read_bytes: 0,
                write_bytes: 256
            }),
            "the rejected transfer is not replayed, only this one counts"
        );
        assert_eq!(sample.dropped_paths, 1);
        assert_eq!(
            shared.state.lock().unwrap().tracked_paths,
            MAX_TRACKED_FILES
        );
    }

    /// A pid reused between two samples belongs to a different process, so the
    /// previous process's paths and open file objects must not carry over.
    #[test]
    fn a_reused_pid_starts_a_fresh_bucket() {
        let shared = shared();
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(8, 0x1000, r"\??\C:\old.bin"),
        );
        deliver(&shared, 8, EVENT_READ, &transfer_payload(8, 0x1000, 4_096));
        {
            let mut state = shared.state.lock().unwrap();
            state.bind_epoch(8, 1_000);
            assert_eq!(state.pids.get(&8).unwrap().epoch_s, Some(1_000));
            assert_eq!(state.tracked_paths, 1);
        }
        // Same pid, different creation time: the counters belong to a process
        // that is gone.
        {
            let mut state = shared.state.lock().unwrap();
            state.bind_epoch(8, 2_000);
            assert!(
                !state.pids.contains_key(&8),
                "the previous process's paths are gone"
            );
            assert!(
                !state.open.contains_key(&0x1000),
                "and so are the names it had open"
            );
            assert_eq!(state.tracked_paths, 0);
        }
        // The new process starts from nothing and accumulates its own paths.
        let sample = sample_of(&shared, 8);
        assert!(sample.files.is_empty());
        deliver(&shared, 8, EVENT_READ, &transfer_payload(8, 0x1000, 512));
        let sample = sample_of(&shared, 8);
        assert!(sample.files.is_empty(), "the old name is not inherited");
        assert_eq!(
            sample.unnamed,
            Remainder {
                read_bytes: 512,
                write_bytes: 0
            }
        );
        // Re-binding the same creation time keeps the bucket.
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(8, 0x2000, r"\??\C:\new.bin"),
        );
        {
            let mut state = shared.state.lock().unwrap();
            let before = state.pids.get(&8).unwrap().files.len();
            state.bind_epoch(8, 2_000);
            assert_eq!(state.pids.get(&8).unwrap().files.len(), before);
        }
    }

    /// Per-process tracking is capped too, and a file beyond the cap never
    /// appears as a truncated row.
    #[test]
    fn per_process_files_are_capped_into_the_omitted_remainder() {
        let shared = shared();
        for index in 0..MAX_TRACKED_FILES_PER_PID {
            let path = format!(r"\??\C:\g{index}");
            let object = 0x10_0000 + index as u64 * 8;
            deliver(&shared, 8, EVENT_CREATE, &create_payload(8, object, &path));
            deliver(&shared, 8, EVENT_WRITE, &transfer_payload(8, object, 256));
        }
        let object = 0x90_0000;
        deliver(
            &shared,
            8,
            EVENT_CREATE,
            &create_payload(8, object, r"\??\C:\over"),
        );
        deliver(&shared, 8, EVENT_READ, &transfer_payload(8, object, 4_096));
        let sample = sample_of(&shared, 8);
        assert_eq!(sample.files.len(), MAX_TRACKED_FILES_PER_PID);
        assert_eq!(bytes(&sample, r"C:\over"), None);
        assert_eq!(
            sample.omitted,
            Remainder {
                read_bytes: 4_096,
                write_bytes: 0
            }
        );
        assert_eq!(
            sample.attributed().read_bytes,
            4_096,
            "an omitted file is still part of the process total"
        );
        assert_eq!(sample.dropped_paths, 1);
    }

    /// The payload is written by the kernel, so its pointers are native width
    /// even when the record is flagged as coming from a 32-bit caller. Reading
    /// it four bytes narrow would shift every field and leave that process with
    /// dashes on every row while the session reports Measured.
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
        assert_eq!(
            etw::pointer_bytes(&record),
            4,
            "the caller's width is what the header flag describes"
        );
        assert_eq!(
            KERNEL_FILE_POINTER_BYTES,
            std::mem::size_of::<usize>(),
            "and it is NOT the width of this provider's payloads"
        );

        // The record's payload is native width throughout, and the callback
        // decodes it with the pinned width: the bytes land on the file.
        let shared = shared();
        deliver(
            &shared,
            64,
            EVENT_CREATE,
            &create_payload(KERNEL_FILE_POINTER_BYTES, 0x8000, r"\??\C:\data\wow64.bin"),
        );
        let payload = transfer_payload(KERNEL_FILE_POINTER_BYTES, 0x8000, 4_096);
        handle_payload(&shared, 64, EVENT_READ, 1, &payload);
        let sample = sample_of(&shared, 64);
        assert_eq!(
            bytes(&sample, r"C:\data\wow64.bin"),
            Some(FileBytes {
                read_bytes: 4_096,
                write_bytes: 0
            }),
            "a 32-bit caller's record still names its file"
        );
        assert_eq!(sample.unnamed, Remainder::default());
        // Four-byte pointers would have read the Irp as the object and lost
        // the record entirely.
        assert_ne!(
            parse_event(EVENT_READ, 1, &payload, 4),
            parse_event(EVENT_READ, 1, &payload, KERNEL_FILE_POINTER_BYTES)
        );
    }

    /// A session that delivered nothing is unknown, not a measured zero; a
    /// session whose records all failed to decode is unknown as well.
    #[test]
    fn nothing_arrived_is_undelivered_and_nothing_decoded_is_undecodable() {
        let empty = FileIoSample {
            running: true,
            ..FileIoSample::default()
        };
        assert_eq!(empty.state(), FileIoAccountingState::Undelivered);
        assert_eq!(empty.attributed(), FileBytes::default());

        let broken = FileIoSample {
            running: true,
            events_seen: 4_000,
            events_decoded: 0,
            ..FileIoSample::default()
        };
        assert_eq!(broken.state(), FileIoAccountingState::Undecodable);

        let other_pid = FileIoSample {
            running: true,
            events_seen: 4_000,
            events_decoded: 3_998,
            pid_read_transfers: 0,
            pid_write_transfers: 0,
            ..FileIoSample::default()
        };
        assert_eq!(
            other_pid.state(),
            FileIoAccountingState::Undelivered,
            "a process with no decoded transfer has no measured bytes"
        );

        // A Create alone does not make a byte count measured.
        let created_only = FileIoSample {
            running: true,
            events_seen: 3,
            events_decoded: 3,
            pid_read_transfers: 0,
            pid_write_transfers: 0,
            ..FileIoSample::default()
        };
        assert_eq!(created_only.state(), FileIoAccountingState::Undelivered);

        let measured = FileIoSample {
            running: true,
            events_seen: 3,
            events_decoded: 3,
            pid_read_transfers: 1,
            pid_write_transfers: 1,
            ..FileIoSample::default()
        };
        assert_eq!(measured.state(), FileIoAccountingState::Measured);

        let stopped = FileIoSample {
            running: false,
            events_seen: 3,
            events_decoded: 3,
            pid_read_transfers: 1,
            pid_write_transfers: 1,
            ..FileIoSample::default()
        };
        assert_eq!(stopped.state(), FileIoAccountingState::Unavailable);
    }

    /// Undecodable records are counted, never charged, and the id, version and
    /// payload length of the last rejection are kept so a layout change and a
    /// truncated payload can be told apart from a log line.
    #[test]
    fn undecodable_records_are_counted_but_never_charged() {
        let shared = shared();
        // Zeroing the file object fails the plausibility check.
        deliver(&shared, 8, EVENT_CREATE, &create_payload(8, 0, r"\??\C:\z"));
        // An implausible size.
        deliver(
            &shared,
            8,
            EVENT_READ,
            &transfer_payload(8, 0x1000, u32::MAX),
        );
        // A truncated transfer; its identity is the most recent rejection.
        shared.undecodable(EVENT_READ, 1, 12);
        let state = shared.state.lock().unwrap();
        assert_eq!(state.events_seen, 3);
        assert_eq!(state.events_decoded, 0);
        assert!(state.pids.is_empty() || state.pids.values().all(|p| p.files.is_empty()));
        assert_eq!(state.rejected_event_id, EVENT_READ);
        assert_eq!(state.rejected_version, 1);
        assert_eq!(state.rejected_payload_len, 12);
    }

    /// Both template generations decode, and each is read with its own
    /// offsets: v0 spells the issuing thread as a pointer, which shifts every
    /// following field by four bytes.
    #[test]
    fn both_template_versions_decode_their_own_layout() {
        for pointer_bytes in [4usize, 8] {
            let (object, size, min) = transfer_layout(1, pointer_bytes).unwrap();
            assert_eq!(object, 8 + pointer_bytes);
            assert_eq!(size, 8 + 3 * pointer_bytes + 4);
            assert_eq!(min, 8 + 3 * pointer_bytes + 16);
            let (object, size, min) = transfer_layout(0, pointer_bytes).unwrap();
            assert_eq!(object, 8 + 2 * pointer_bytes);
            assert_eq!(size, 8 + 4 * pointer_bytes);
            assert_eq!(min, 8 + 4 * pointer_bytes + 8);
            assert_eq!(
                create_layout(1, pointer_bytes),
                Some((pointer_bytes, 2 * pointer_bytes + 16))
            );
            assert_eq!(
                create_layout(0, pointer_bytes),
                Some((2 * pointer_bytes, 3 * pointer_bytes + 12))
            );
            assert_eq!(
                close_layout(1, pointer_bytes),
                Some((pointer_bytes, 3 * pointer_bytes + 4))
            );
            assert_eq!(
                close_layout(0, pointer_bytes),
                Some((2 * pointer_bytes, 4 * pointer_bytes))
            );
        }
        // An unknown generation is a gap, not a guess.
        assert_eq!(transfer_layout(2, 8), None);
        assert_eq!(create_layout(2, 8), None);
        assert_eq!(close_layout(2, 8), None);

        // A v0 transfer read with the v1 offsets lands on the FileKey, which is
        // a real aligned pointer — so the misread does NOT fail, it silently
        // yields another file object and another size. That is exactly why the
        // version is taken from the record header instead of being assumed, and
        // the v0 payload decodes correctly with its own layout.
        let mut v0 = Vec::new();
        v0.extend_from_slice(&4_096u64.to_le_bytes()); // ByteOffset
        v0.extend(std::iter::repeat_n(0x11u8, 8)); // Irp
        v0.extend_from_slice(&1_000u64.to_le_bytes()); // ThreadId
        v0.extend_from_slice(&0x7000u64.to_le_bytes()); // FileObject
        v0.extend_from_slice(&0x7000_aaaa_u64.to_le_bytes()); // FileKey
        v0.extend_from_slice(&512u32.to_le_bytes()); // IOSize
        v0.extend_from_slice(&0u32.to_le_bytes()); // IOFlags
        assert_eq!(
            parse_event(EVENT_READ, 0, &v0, 8),
            Some(FileEvent::Transfer {
                object: 0x7000,
                size: 512,
                write: false,
            })
        );
        assert_ne!(
            parse_event(EVENT_READ, 1, &v0, 8),
            parse_event(EVENT_READ, 0, &v0, 8),
            "reading a v0 payload with the v1 offsets is a different record"
        );
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
        let payload = create_payload(8, 0x1000, "C:\\ä.txt");
        let FileEvent::Create { name, .. } = parse_event(EVENT_CREATE, 1, &payload, 8).unwrap()
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
            &create_payload(8, 0x1000, r"\??\C:\a"),
        );
        deliver(&shared, 8, EVENT_READ, &transfer_payload(8, 0x1000, 4_096));
        let state = shared.state.lock().unwrap();
        assert!(state.open.is_empty());
        assert!(state.pids.is_empty());
        assert_eq!(state.events_seen, 0);
    }

    /// Creation time is what tells a consumer how long the counts cover, and
    /// the keys of the sampled map are strings the broker can compare.
    #[test]
    fn a_sample_carries_paths_and_the_elapsed_time() {
        let shared = shared();
        shared.devices.lock().unwrap().loaded = Some((Instant::now(), Vec::new()));
        deliver(
            &shared,
            12,
            EVENT_CREATE,
            &create_payload(8, 0x1000, r"\??\C:\x\y.log"),
        );
        deliver(&shared, 12, EVENT_WRITE, &transfer_payload(8, 0x1000, 700));
        let sample = sample_of(&shared, 12);
        assert_eq!(sample.files.len(), 1);
        assert_eq!(sample.files[0].0, r"C:\x\y.log");
        assert_eq!(sample.pid_transfers(), 1);
        assert_eq!(sample.events_seen, 2);
        assert_eq!(sample.events_decoded, 2);
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
            pid_read_transfers: 9,
            pid_write_transfers: 2,
            // The session had to drop names and had nowhere to put paths; both
            // explain a row that reads unavailable while the state is measured.
            cleared_file_objects: 12,
            unnamed_creates: 3,
            dropped_paths: 4,
            events_lost: Some(1),
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
            &create_payload(8, 0x1000, r"\??\C:\data\a.bin"),
        );
        deliver(
            &shared,
            pid,
            EVENT_READ,
            &transfer_payload(8, 0x1000, 4_096),
        );
        deliver(
            &shared,
            pid,
            EVENT_WRITE,
            &transfer_payload(8, 0x1000, 1_024),
        );
        // A transfer on a file object whose Create was never seen: unnamed.
        deliver(&shared, pid, EVENT_READ, &transfer_payload(8, 0x9000, 512));
        // A file past the session's path budget: omitted, and no row.
        deliver(
            &shared,
            pid,
            EVENT_CREATE,
            &create_payload(8, 0x2000, r"\??\C:\data\b.bin"),
        );
        shared.state.lock().unwrap().tracked_paths = MAX_TRACKED_FILES;
        deliver(
            &shared,
            pid,
            EVENT_WRITE,
            &transfer_payload(8, 0x2000, 2_048),
        );

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
        assert_eq!(accounting.read_bytes, 4_096 + 512);
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
        // 500 (gone.tmp) + 70 (unnamed) read; 5 + 7 written.
        assert_eq!(accounting.unnamed_read_bytes, 570);
        assert_eq!(accounting.unnamed_write_bytes, 12);
        assert_eq!(accounting.omitted_read_bytes, 300);
        assert_eq!(accounting.omitted_write_bytes, 3);
        assert_eq!(accounting.read_ops, 9);
        assert_eq!(accounting.write_ops, 2);
        // The row + the two remainders are the attribution for the process.
        assert_eq!(accounting.read_bytes, 1_000 + 570 + 300);
        assert_eq!(accounting.write_bytes, 20 + 12 + 3);
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
