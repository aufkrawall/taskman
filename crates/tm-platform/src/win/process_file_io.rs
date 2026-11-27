//! On-demand file I/O enumeration of one process (Process Properties → I/O).
//!
//! Enumerates open handles for the target process via `NtQueryInformationProcess(51)`.
//! For each handle that represents a disk file (`GetFileType == FILE_TYPE_DISK`),
//! resolves the normalized file path, file size (`EndOfFile`), current read/write
//! position (`FilePositionInformation`), and granted access rights.
//!
//! Every per-file fact is read from our duplicate of the handle, never from the
//! handle snapshot: the target can close a handle after the snapshot, and the
//! same handle value can then name a different object (seen when a concurrent
//! enumerator's temporary duplicate reused the value of a closed read handle,
//! which made a write-only file look read/write). All queries are read-only, so
//! the target's shared file position is never written.

use std::collections::HashMap;

use tm_core::error::{Result, TmError};
use windows::Wdk::Foundation::{NtQueryObject, ObjectBasicInformation};
use windows::Wdk::Storage::FileSystem::{
    FILE_POSITION_INFORMATION, FilePositionInformation, NtQueryInformationFile,
};
use windows::Wdk::System::SystemInformation::SystemProcessInformation;
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, FILE_STANDARD_INFO, FILE_TYPE_DISK, FileStandardInfo,
    GETFINALPATHNAMEBYHANDLE_FLAGS, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::Win32::System::Threading as th;
use windows::Win32::System::WindowsProgramming::PUBLIC_OBJECT_BASIC_INFORMATION;

use crate::actions::{
    FileIoAccounting, FileIoAccountingState, ProcessFileIo, ProcessFileIoSnapshot,
};

const PROCESS_HANDLE_INFORMATION: PROCESSINFOCLASS = PROCESSINFOCLASS(51);

type HandleInspection<T> = std::result::Result<T, ()>;

const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OBJ_KERNEL_HANDLE: u32 = 0x0000_0200;

#[repr(C)]
struct ProcessHandleSnapshotInformation {
    number_of_handles: usize,
    reserved: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessHandleTableEntryInfo {
    handle_value: usize,
    handle_count: usize,
    pointer_count: usize,
    granted_access: u32,
    object_type_index: u32,
    handle_attributes: u32,
    reserved: u32,
}

/// Strip verbatim prefixes: `\\?\C:\foo` -> `C:\foo`, `\\?\UNC\server\share` -> `\\server\share`.
pub(crate) fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

fn is_access_denied_or_system(err: &TmError, pid: u32) -> bool {
    if pid <= 4 {
        return true;
    }
    match err {
        TmError::Platform { detail, .. } => {
            detail.contains("0x80070005")
                || detail.to_lowercase().contains("access is denied")
                || detail.to_lowercase().contains("zugriff verweigert")
        }
        _ => false,
    }
}

fn query_kernel_process_io(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<(Option<u64>, Option<u64>)> {
    let mut buf = vec![0u8; 512 * 1024];
    let written =
        crate::win::cpu_load::CpuLoadAccountant::nt_query(&mut buf, SystemProcessInformation)
            .ok_or_else(|| TmError::platform("NtQuerySystemInformation", "query failed"))?;
    let off = crate::win::cpu_load::Offsets::get();
    let mut pos = 0usize;
    loop {
        if pos.checked_add(off.min_size).is_none() || pos + off.min_size > written {
            break;
        }
        let proc_pid = crate::win::cpu_load::read_usize(&buf, pos + off.pid) as u32;
        let next = crate::win::cpu_load::read_u32(&buf, pos) as usize;
        let record_end = if next == 0 {
            written
        } else if next >= off.min_size {
            match pos.checked_add(next) {
                Some(end) if end <= written => end,
                _ => break,
            }
        } else {
            break;
        };
        if proc_pid == pid {
            let create_time = crate::win::cpu_load::read_i64(&buf, pos + off.create_time);
            if let Some(expected) = expected_start_epoch_s {
                let actual = crate::win::cpu_load::filetime_to_unix_ticks(create_time);
                if pid > 4
                    && expected > 0
                    && !actual
                        .is_some_and(|ts| tm_core::model::creation_timestamps_match(expected, ts))
                {
                    return Err(TmError::ProcessNotFound { pid });
                }
            }
            if let Some(raw_io) =
                crate::win::cpu_load::read_raw_io_counters(&buf, pos, record_end, &off)
            {
                return Ok((Some(raw_io.read_bytes), Some(raw_io.write_bytes)));
            }
            return Ok((None, None));
        }
        if next == 0 {
            break;
        }
        pos += next;
    }
    Err(TmError::ProcessNotFound { pid })
}

/// Every process's creation time (100-ns Unix ticks) and kernel read/write
/// transfer counters, from one `SystemProcessInformation` snapshot.
///
/// The per-file trace takes this just before its session starts, so that for
/// a process that already existed it knows what the process's own counters
/// said when tracing began — the start of the window its per-file counts
/// cover. One kernel query for the whole machine, no handle per process.
/// A process whose counters cannot be read is simply absent.
pub(super) fn snapshot_io_counters() -> HashMap<u32, (i64, u64, u64)> {
    let mut counters = HashMap::new();
    let mut buf = vec![0u8; 512 * 1024];
    let Some(written) =
        crate::win::cpu_load::CpuLoadAccountant::nt_query(&mut buf, SystemProcessInformation)
    else {
        return counters;
    };
    let off = crate::win::cpu_load::Offsets::get();
    let mut pos = 0usize;
    loop {
        if pos
            .checked_add(off.min_size)
            .is_none_or(|end| end > written)
        {
            break;
        }
        let pid = crate::win::cpu_load::read_usize(&buf, pos + off.pid) as u32;
        let next = crate::win::cpu_load::read_u32(&buf, pos) as usize;
        let record_end = if next == 0 {
            written
        } else if next >= off.min_size {
            match pos.checked_add(next) {
                Some(end) if end <= written => end,
                _ => break,
            }
        } else {
            break;
        };
        let created = crate::win::cpu_load::filetime_to_unix_ticks(crate::win::cpu_load::read_i64(
            &buf,
            pos + off.create_time,
        ));
        if let (Some(created), Some(io)) = (
            created,
            crate::win::cpu_load::read_raw_io_counters(&buf, pos, record_end, &off),
        ) {
            counters.insert(pid, (created, io.read_bytes, io.write_bytes));
        }
        if next == 0 {
            break;
        }
        pos += next;
    }
    counters
}

fn query_process_io_fallback(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<(Option<u64>, Option<u64>)> {
    if pid > 4
        && let Ok(h) = super::process_ops::open_process_verified(
            pid,
            th::PROCESS_QUERY_LIMITED_INFORMATION,
            expected_start_epoch_s,
        )
    {
        let mut io_counters = th::IO_COUNTERS::default();
        let ok = unsafe { th::GetProcessIoCounters(h, &mut io_counters) }.is_ok();
        unsafe {
            let _ = CloseHandle(h);
        }
        if ok {
            return Ok((
                Some(io_counters.ReadTransferCount),
                Some(io_counters.WriteTransferCount),
            ));
        }
    }
    query_kernel_process_io(pid, expected_start_epoch_s)
}

pub fn list_process_file_io(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<ProcessFileIoSnapshot> {
    super::process_ops::enable_debug_privilege();

    let open_result = if pid <= 4 {
        Err(TmError::platform(
            "OpenProcess",
            "system process is protected",
        ))
    } else {
        super::process_ops::open_process_verified(
            pid,
            th::PROCESS_QUERY_INFORMATION | th::PROCESS_DUP_HANDLE,
            expected_start_epoch_s,
        )
    };

    let (files, counter_read_bytes, counter_write_bytes, handles_complete) = match open_result {
        Ok(h_process) => {
            let mut io_counters = th::IO_COUNTERS::default();
            let has_counters =
                unsafe { th::GetProcessIoCounters(h_process, &mut io_counters) }.is_ok();
            let (cr, cw) = if has_counters {
                (
                    Some(io_counters.ReadTransferCount),
                    Some(io_counters.WriteTransferCount),
                )
            } else {
                (None, None)
            };
            let result = enumerate_process_files(h_process);
            unsafe {
                let _ = CloseHandle(h_process);
            }
            let result = result?;
            (result.files, cr, cw, result.complete)
        }
        Err(error) => {
            if is_access_denied_or_system(&error, pid) {
                let (cr, cw) = query_process_io_fallback(pid, expected_start_epoch_s)?;
                (Vec::new(), cr, cw, false)
            } else {
                return Err(error);
            }
        }
    };

    let accounting = FileIoAccounting {
        state: FileIoAccountingState::Unavailable,
        since_ms: 0,
        read_bytes: 0,
        write_bytes: 0,
        read_ops: 0,
        write_ops: 0,
        unnamed_read_bytes: 0,
        unnamed_write_bytes: 0,
        omitted_read_bytes: 0,
        omitted_write_bytes: 0,
        counter_read_bytes,
        counter_write_bytes,
        events_lost: None,
        cleared_file_objects: 0,
        unnamed_creates: 0,
        dropped_paths: 0,
        handles_complete,
        traced_since_process_start: false,
        counter_window_read_bytes: None,
        counter_window_write_bytes: None,
    };
    Ok(ProcessFileIoSnapshot { files, accounting })
}

struct FileEnumeration {
    files: Vec<ProcessFileIo>,
    complete: bool,
}

fn enumerate_process_files(h_process: HANDLE) -> Result<FileEnumeration> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut written: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                h_process,
                PROCESS_HANDLE_INFORMATION,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut written,
            )
        };
        if status.0 == 0 {
            buf.truncate(written as usize);
            break;
        }
        // STATUS_INFO_LENGTH_MISMATCH (0xC0000004) or STATUS_BUFFER_OVERFLOW (0x80000005)
        if status.0 == 0xC000_0004u32 as i32 || status.0 == 0x8000_0005u32 as i32 {
            let next_size = if written > buf.len() as u32 {
                written as usize + 16384
            } else {
                buf.len() * 2
            };
            if next_size > 16 * 1024 * 1024 {
                return Err(TmError::platform(
                    "handle enumeration",
                    "handle table exceeds budget",
                ));
            }
            buf.resize(next_size, 0);
        } else {
            return Err(TmError::platform(
                "NtQueryInformationProcess(ProcessHandleInformation)",
                format!("status {:#x}", status.0),
            ));
        }
    }

    let header_size = std::mem::size_of::<ProcessHandleSnapshotInformation>();
    let entry_size = std::mem::size_of::<ProcessHandleTableEntryInfo>();
    if buf.len() < header_size {
        return Ok(FileEnumeration {
            files: Vec::new(),
            complete: false,
        });
    }

    // The snapshot arrives in a byte buffer, so its records are only
    // byte-aligned: they are read out unaligned instead of by reference, which
    // would require an alignment `Vec<u8>` does not guarantee.
    let header = unsafe {
        buf.as_ptr()
            .cast::<ProcessHandleSnapshotInformation>()
            .read_unaligned()
    };
    let max_entries = (buf.len() - header_size) / entry_size;
    let count = header.number_of_handles.min(max_entries);

    let entries_ptr = unsafe {
        buf.as_ptr()
            .add(header_size)
            .cast::<ProcessHandleTableEntryInfo>()
    };
    let current_proc = unsafe { th::GetCurrentProcess() };
    // `inspect_handle` may only ask `GetFileType` about the handle when the
    // table being walked is this process's own; see `may_probe_local_handle_table`.
    let probe_locally =
        may_probe_local_handle_table(unsafe { th::GetProcessId(h_process) }, unsafe {
            th::GetCurrentProcessId()
        });

    let mut map: HashMap<String, ProcessFileIo> = HashMap::new();
    let mut path_buf = vec![0u16; 1024];
    // `complete` describes the ENUMERATION, not every handle inside it: it says
    // the snapshot covered the whole table. Individual handles that cannot be
    // duplicated or read are counted separately, because every real process
    // holds protected objects (and files opened without query rights); clearing
    // the flag for those made the I/O page report itself unavailable for every
    // process on the machine.
    let complete = count == header.number_of_handles;
    let mut unreadable = 0usize;

    for i in 0..count {
        let entry = unsafe { entries_ptr.add(i).read_unaligned() };
        if entry.handle_value == 0 || entry.handle_attributes & OBJ_KERNEL_HANDLE != 0 {
            continue;
        }
        let facts = match inspect_handle(
            h_process,
            current_proc,
            entry.handle_value,
            probe_locally,
            &mut path_buf,
        ) {
            Ok(Some(facts)) => facts,
            Ok(None) => continue,
            Err(()) => {
                // A handle this process will not let us duplicate is normal: it
                // is not a file we can report, and it is not a gap in the
                // enumeration either.
                unreadable += 1;
                continue;
            }
        };
        if !facts.complete {
            // The file was identified; only some of its details could not be
            // read, and those fields are already `None`.
            unreadable += 1;
        }
        let (is_read, is_write) = access_flags(facts.granted_access);

        let file_entry = map
            .entry(facts.path.clone())
            .or_insert_with(|| ProcessFileIo {
                path: facts.path,
                read_bytes: None,
                write_bytes: None,
                read_ops: None,
                write_ops: None,
                file_size: facts.file_size,
                offset: facts.offset,
                access_known: facts.granted_access.is_some(),
                is_read: false,
                is_write: false,
                open_handles: 0,
                idle_ms: None,
            });

        file_entry.open_handles = file_entry.open_handles.saturating_add(1);
        file_entry.access_known |= facts.granted_access.is_some();
        file_entry.is_read |= is_read;
        file_entry.is_write |= is_write;
        if let Some(pos_val) = facts.offset {
            file_entry.offset = Some(file_entry.offset.map_or(pos_val, |prev| prev.max(pos_val)));
        }
        if let Some(sz) = facts.file_size {
            file_entry.file_size = Some(file_entry.file_size.map_or(sz, |prev| prev.max(sz)));
        }
    }

    // A handle position is a seek offset, not transferred bytes. No per-file
    // byte/operation source is available in this enumeration, so every row
    // keeps those fields unavailable; the process-wide counters above remain
    // separate facts.
    if unreadable > 0 {
        tracing::debug!(
            unreadable,
            count,
            "some handles could not be inspected; the file list is still complete"
        );
    }
    Ok(FileEnumeration {
        files: map.into_values().collect(),
        complete,
    })
}

/// What one open disk-file handle says about itself, all read from the same
/// duplicated handle so the facts always describe one object.
struct HandleFacts {
    path: String,
    offset: Option<u64>,
    file_size: Option<u64>,
    /// `None` when the object could not be queried; never taken from the
    /// handle snapshot, whose entry may describe a since-closed handle.
    granted_access: Option<u32>,
    /// False when the handle identified a file but one of its optional facts
    /// (type/size/position/access) could not be read.
    complete: bool,
}

/// Read/write flags for a handle's granted access. Unknown access sets
/// neither, so the attribution reports the split as unavailable instead of
/// guessing.
fn access_flags(granted_access: Option<u32>) -> (bool, bool) {
    let access = granted_access.unwrap_or(0);
    (
        (access & (FILE_READ_DATA | GENERIC_READ)) != 0,
        (access & (FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE)) != 0,
    )
}

/// Whether a handle taken from the enumerated process may be type-probed with
/// `GetFileType` in this process's own handle table.
///
/// `GetFileType` resolves its argument in the CALLING process's table, so the
/// answer only describes the enumerated process when that process is this one.
/// For any other process the same numeric value names nothing here, or names an
/// unrelated local object, and using that answer as the type of the target's
/// handle silently drops the file. An unknown PID (0) is never this process.
fn may_probe_local_handle_table(enumerated_pid: u32, self_pid: u32) -> bool {
    enumerated_pid != 0 && enumerated_pid == self_pid
}

/// Duplicates `handle_value` out of `h_process` and inspects the duplicate
/// read-only. Returns `None` for handles that are gone, are not disk files,
/// are directories, or have no resolvable path.
///
/// `probe_locally` is [`may_probe_local_handle_table`]: it enables a cheap
/// pre-copy type probe that is only valid for this process's own handles.
fn inspect_handle(
    h_process: HANDLE,
    current_proc: HANDLE,
    handle_value: usize,
    probe_locally: bool,
    path_buf: &mut [u16],
) -> HandleInspection<Option<HandleFacts>> {
    let source = HANDLE(handle_value as *mut core::ffi::c_void);
    // Most of a real process's table is non-files, and a protected or
    // non-duplicable special handle must not turn a complete file walk into an
    // incomplete one, so skip what is already known not to be a disk file.
    // Only this process's table can answer that (see the doc above).
    if probe_locally && unsafe { GetFileType(source) } != FILE_TYPE_DISK {
        return Ok(None);
    }
    let mut dup = HANDLE::default();
    let ok = unsafe {
        DuplicateHandle(
            h_process,
            source,
            current_proc,
            &mut dup,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok.is_err() || dup.is_invalid() {
        return Err(());
    }
    let facts = inspect_duplicated_handle(dup, path_buf);
    unsafe {
        let _ = CloseHandle(dup);
    }
    facts
}

fn inspect_duplicated_handle(
    dup: HANDLE,
    path_buf: &mut [u16],
) -> HandleInspection<Option<HandleFacts>> {
    // Only disk files; strictly avoid named pipes, sockets, or devices that could hang.
    if unsafe { GetFileType(dup) } != FILE_TYPE_DISK {
        return Ok(None);
    }

    let path_flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let len = unsafe { GetFinalPathNameByHandleW(dup, path_buf, path_flags) } as usize;
    if len == 0 || len >= path_buf.len() {
        return Err(());
    }
    let path = strip_verbatim(&String::from_utf16_lossy(&path_buf[..len]));

    let mut std_info = FILE_STANDARD_INFO::default();
    let std_ok = unsafe {
        GetFileInformationByHandleEx(
            dup,
            FileStandardInfo,
            (&raw mut std_info).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    }
    .is_ok();
    if std_ok && std_info.Directory {
        return Ok(None);
    }
    let file_size = std_ok.then(|| std_info.EndOfFile.max(0) as u64);
    let offset = query_file_position(dup);
    let granted_access = query_granted_access(dup);

    Ok(Some(HandleFacts {
        path,
        offset,
        file_size,
        granted_access,
        complete: std_ok && offset.is_some() && granted_access.is_some(),
    }))
}

/// Current byte offset of the file object behind `handle`. Query only:
/// `SetFilePointerEx(0, FILE_CURRENT)` reads the position and then writes it
/// back, and the file object (with its position) is shared with the target
/// process, so a target write landing in between would be rewound.
fn query_file_position(handle: HANDLE) -> Option<u64> {
    let mut iosb = IO_STATUS_BLOCK::default();
    let mut info = FILE_POSITION_INFORMATION::default();
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut iosb,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_POSITION_INFORMATION>() as u32,
            FilePositionInformation,
        )
    };
    (status.is_ok() && info.CurrentByteOffset >= 0).then_some(info.CurrentByteOffset as u64)
}

/// Access granted to `handle` itself. A duplicate made with
/// `DUPLICATE_SAME_ACCESS` carries exactly the source handle's access.
fn query_granted_access(handle: HANDLE) -> Option<u32> {
    let mut info = PUBLIC_OBJECT_BASIC_INFORMATION::default();
    let status = unsafe {
        NtQueryObject(
            Some(handle),
            ObjectBasicInformation,
            Some((&raw mut info).cast()),
            std::mem::size_of::<PUBLIC_OBJECT_BASIC_INFORMATION>() as u32,
            None,
        )
    };
    status.is_ok().then_some(info.GrantedAccess)
}

/// `SystemExtendedHandleInformation`: every handle on the machine with the
/// kernel address of the object it refers to. Not in the `windows` crate's
/// enum, so spelled by value.
const SYSTEM_EXTENDED_HANDLE_INFORMATION:
    windows::Wdk::System::SystemInformation::SYSTEM_INFORMATION_CLASS =
    windows::Wdk::System::SystemInformation::SYSTEM_INFORMATION_CLASS(64);

/// Header of a `SystemExtendedHandleInformation` answer.
#[repr(C)]
struct SystemHandleInformationEx {
    number_of_handles: usize,
    reserved: usize,
}

/// One `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemHandleTableEntryInfoEx {
    object: usize,
    unique_process_id: usize,
    handle_value: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
}

/// One handle of the system handle table: who holds it, its value, its type
/// and the kernel address of the object it refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandleObject {
    pid: usize,
    handle_value: usize,
    object_type_index: u16,
    object: u64,
}

/// The whole system handle table, or `None` when it could not be read.
///
/// Object addresses come back as zero for a caller without `SeDebugPrivilege`
/// on current Windows builds, so callers enable it first and treat a zero as
/// "unknown", never as an address.
fn system_handle_objects() -> Option<Vec<HandleObject>> {
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let written = crate::win::cpu_load::CpuLoadAccountant::nt_query(
        &mut buf,
        SYSTEM_EXTENDED_HANDLE_INFORMATION,
    )?;
    parse_handle_objects(buf.get(..written)?)
}

/// Decode a `SystemExtendedHandleInformation` answer; split out so the record
/// arithmetic is testable without a privileged query.
fn parse_handle_objects(buf: &[u8]) -> Option<Vec<HandleObject>> {
    let header_size = std::mem::size_of::<SystemHandleInformationEx>();
    let entry_size = std::mem::size_of::<SystemHandleTableEntryInfoEx>();
    if buf.len() < header_size {
        return None;
    }
    // SAFETY: bounds checked above; the read is unaligned because a byte
    // buffer guarantees no alignment.
    let header = unsafe {
        buf.as_ptr()
            .cast::<SystemHandleInformationEx>()
            .read_unaligned()
    };
    let count = header
        .number_of_handles
        .min((buf.len() - header_size) / entry_size);
    let mut handles = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: `count` was clamped to the entries the buffer holds.
        let entry = unsafe {
            buf.as_ptr()
                .add(header_size + index * entry_size)
                .cast::<SystemHandleTableEntryInfoEx>()
                .read_unaligned()
        };
        handles.push(HandleObject {
            pid: entry.unique_process_id,
            handle_value: entry.handle_value,
            object_type_index: entry.object_type_index,
            object: entry.object as u64,
        });
    }
    Some(handles)
}

/// Name the open FILE objects of `pid` by their kernel address.
///
/// The per-file trace identifies a file by its `FILE_OBJECT` address and only
/// learns a name from a Create it saw. A handle opened BEFORE the trace started
/// has no Create in it, so every read and write through it would stay unnamed
/// for as long as the handle lives — a database, a log, a VM image opened an
/// hour ago. The system handle table carries that same address for every
/// handle, so it can name those objects after the fact.
///
/// Only objects in `wanted` are looked at; `None` asks for every file handle of
/// the process. Each candidate handle is duplicated and named exactly the way
/// the file list names its rows ([`inspect_handle`]: disk files only, so a pipe
/// cannot hang the query), so a name here compares equal to its row.
///
/// The answer is VERIFIED, not assumed: between reading the table and
/// duplicating, the target can close a handle and the value can be reused for a
/// different object, which would charge that object's bytes to the wrong file.
/// So the table is read a second time with the duplicates open, and a name is
/// kept only when this process's duplicate refers to the very object the
/// target's handle did. Unverifiable answers are dropped.
///
/// Empty when the process is gone or reused (`expected_start_epoch_s`), cannot
/// be opened for handle duplication, or the table carries no object addresses
/// (no `SeDebugPrivilege`).
pub(super) fn name_file_objects(
    pid: u32,
    expected_start_epoch_s: i64,
    wanted: Option<&std::collections::HashSet<u64>>,
) -> Vec<(u64, String)> {
    if pid <= 4 || wanted.is_some_and(|wanted| wanted.is_empty()) {
        return Vec::new();
    }
    super::process_ops::enable_debug_privilege();
    // A handle of our own to a file whose type index we can find in the table:
    // the File type's index differs between Windows builds, so it is read from
    // the table rather than hard-coded.
    let Ok(probe) = std::env::current_exe().and_then(std::fs::File::open) else {
        return Vec::new();
    };
    let probe_value = {
        use std::os::windows::io::AsRawHandle;
        probe.as_raw_handle() as usize
    };
    let self_pid = std::process::id() as usize;
    let Some(table) = system_handle_objects() else {
        return Vec::new();
    };
    let Some(file_type) = table
        .iter()
        .find(|entry| entry.pid == self_pid && entry.handle_value == probe_value)
        .map(|entry| entry.object_type_index)
    else {
        return Vec::new();
    };
    let candidates: Vec<HandleObject> = table
        .into_iter()
        .filter(|entry| {
            entry.pid == pid as usize
                && entry.object_type_index == file_type
                && entry.object != 0
                && wanted.is_none_or(|wanted| wanted.contains(&entry.object))
        })
        .collect();
    drop(probe);
    if candidates.is_empty() {
        return Vec::new();
    }
    let Ok(h_process) = super::process_ops::open_process_verified(
        pid,
        th::PROCESS_QUERY_LIMITED_INFORMATION | th::PROCESS_DUP_HANDLE,
        Some(expected_start_epoch_s),
    ) else {
        return Vec::new();
    };
    let current_proc = unsafe { th::GetCurrentProcess() };
    let mut path_buf = vec![0u16; 1024];
    // (object the target's handle named, our duplicate, its path)
    let mut named: Vec<(u64, HANDLE, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        if !seen.insert(candidate.object) {
            // Two handles to one file object name it once.
            continue;
        }
        let source = HANDLE(candidate.handle_value as *mut core::ffi::c_void);
        let mut dup = HANDLE::default();
        let ok = unsafe {
            DuplicateHandle(
                h_process,
                source,
                current_proc,
                &mut dup,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok.is_err() || dup.is_invalid() {
            continue;
        }
        match inspect_duplicated_handle(dup, &mut path_buf) {
            Ok(Some(facts)) => named.push((candidate.object, dup, facts.path)),
            _ => unsafe {
                let _ = CloseHandle(dup);
            },
        }
    }
    unsafe {
        let _ = CloseHandle(h_process);
    }
    // The verification read: our duplicates must refer to the objects the
    // target's handles named when the table was first read.
    let verified: std::collections::HashMap<usize, u64> = system_handle_objects()
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.pid == self_pid)
        .map(|entry| (entry.handle_value, entry.object))
        .collect();
    let mut answer = Vec::with_capacity(named.len());
    let mut stale = 0usize;
    for (object, dup, path) in named {
        if verified.get(&(dup.0 as usize)) == Some(&object) {
            answer.push((object, path));
        } else {
            stale += 1;
        }
        unsafe {
            let _ = CloseHandle(dup);
        }
    }
    if stale > 0 {
        tracing::debug!(
            pid,
            stale,
            "file handles changed while they were named; those names were dropped"
        );
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// The `SystemExtendedHandleInformation` records are 40 bytes each on
    /// x64 after a two-word header, and a count the buffer cannot hold is
    /// clamped rather than read past the end.
    #[test]
    fn handle_table_records_decode_at_their_native_layout() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3usize.to_ne_bytes()); // claims three...
        buf.extend_from_slice(&0usize.to_ne_bytes());
        for (object, pid, handle, type_index) in [
            (0xffff_d007_ed04_9be0u64, 22_832usize, 0x208usize, 41u16),
            (0, 4, 0x4, 7),
        ] {
            buf.extend_from_slice(&(object as usize).to_ne_bytes());
            buf.extend_from_slice(&pid.to_ne_bytes());
            buf.extend_from_slice(&handle.to_ne_bytes());
            buf.extend_from_slice(&0x0012_019fu32.to_ne_bytes()); // GrantedAccess
            buf.extend_from_slice(&0u16.to_ne_bytes()); // CreatorBackTraceIndex
            buf.extend_from_slice(&type_index.to_ne_bytes());
            buf.extend_from_slice(&0u32.to_ne_bytes()); // HandleAttributes
            buf.extend_from_slice(&0u32.to_ne_bytes()); // Reserved
        }
        // ...but holds two.
        let handles = parse_handle_objects(&buf).expect("a header");
        assert_eq!(
            handles,
            vec![
                HandleObject {
                    pid: 22_832,
                    handle_value: 0x208,
                    object_type_index: 41,
                    object: 0xffff_d007_ed04_9be0,
                },
                HandleObject {
                    pid: 4,
                    handle_value: 0x4,
                    object_type_index: 7,
                    object: 0,
                },
            ]
        );
        assert_eq!(parse_handle_objects(&buf[..8]), None, "no whole header");
    }

    /// The counter snapshot sees this process, with the creation time the
    /// identity checks use and counters that only grow.
    #[test]
    fn the_counter_snapshot_includes_this_process() {
        use std::io::Write as _;
        let pid = std::process::id();
        let before = snapshot_io_counters();
        let (created, _, written_before) = *before.get(&pid).expect("this process is listed");
        let own = super::super::own_start_epoch_for_test();
        assert!(
            tm_core::model::creation_timestamps_match(own, created),
            "{own} vs {created}"
        );
        let path = std::env::temp_dir().join(format!("tm_counter_snapshot_{pid}.bin"));
        std::fs::File::create(&path)
            .and_then(|mut file| file.write_all(&[1u8; 4_096]))
            .expect("write");
        let _ = std::fs::remove_file(&path);
        let after = snapshot_io_counters();
        assert!(after.get(&pid).expect("still listed").2 >= written_before + 4_096);
    }

    /// Naming asks nothing of a process it was told nothing about, and never
    /// touches the System process.
    #[test]
    fn naming_file_objects_short_circuits() {
        let none = std::collections::HashSet::new();
        assert!(name_file_objects(std::process::id(), 1, Some(&none)).is_empty());
        assert!(name_file_objects(4, 1, None).is_empty());
    }

    #[test]
    fn strip_verbatim_handles_all_prefixes() {
        assert_eq!(
            strip_verbatim(r"\\?\C:\Windows\System32"),
            r"C:\Windows\System32"
        );
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\file.txt"),
            r"\\server\share\file.txt"
        );
        assert_eq!(strip_verbatim(r"C:\plain\path"), r"C:\plain\path");
    }

    #[test]
    fn current_process_file_io_discovers_opened_files() {
        let tag = format!("{}_{:?}", std::process::id(), std::thread::current().id());
        let test_name = format!("test_file_io_{tag}.bin");
        let test_path = std::env::temp_dir().join(&test_name);

        // Write some bytes and keep the handle open
        let mut file = std::fs::File::create(&test_path).unwrap();
        file.write_all(&[42u8; 4096]).unwrap();
        file.flush().unwrap();

        let pid = std::process::id();
        let snapshot = list_process_file_io(pid, None).expect("query current process files");

        let found = snapshot.files.iter().find(|f| f.path.ends_with(&test_name));
        assert!(found.is_some(), "opened temporary file must be discovered");
        let found = found.unwrap();
        assert!(found.is_write);
        assert_eq!(found.write_bytes, None);
        assert_eq!(found.file_size, Some(4096));

        drop(file);

        // Now open for reading and read some bytes
        let mut read_file = std::fs::File::open(&test_path).unwrap();
        let mut buf = [0u8; 1024];
        read_file.read_exact(&mut buf).unwrap();

        let snapshot2 = list_process_file_io(pid, None).expect("query current process files");
        let found2 = snapshot2
            .files
            .iter()
            .find(|f| f.path.ends_with(&test_name));
        assert!(found2.is_some(), "read file must be discovered");
        let found2 = found2.unwrap();
        assert!(found2.is_read);
        assert_eq!(found2.read_bytes, None);

        drop(read_file);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn handle_facts_come_from_the_inspected_handle() {
        use std::os::windows::io::AsRawHandle;

        let tag = format!("{}_{:?}", std::process::id(), std::thread::current().id());
        let test_name = format!("test_file_facts_{tag}.bin");
        let test_path = std::env::temp_dir().join(&test_name);
        let mut file = std::fs::File::create(&test_path).unwrap();
        file.write_all(&[9u8; 300]).unwrap();

        // The access must be the handle's own (write-only here), not whatever
        // a possibly stale snapshot entry recorded for the same handle value.
        let current = unsafe { th::GetCurrentProcess() };
        let mut path_buf = vec![0u16; 1024];
        let facts = inspect_handle(
            current,
            current,
            file.as_raw_handle() as usize,
            true,
            &mut path_buf,
        )
        .expect("open disk file inspection")
        .expect("open disk file must be inspectable");
        assert!(facts.path.ends_with(&test_name));
        assert_eq!(access_flags(facts.granted_access), (false, true));
        assert_eq!(facts.offset, Some(300));
        assert_eq!(facts.file_size, Some(300));

        // Reading the position is a query: the target's position is untouched.
        file.write_all(&[9u8; 10]).unwrap();
        let facts = inspect_handle(
            current,
            current,
            file.as_raw_handle() as usize,
            true,
            &mut path_buf,
        )
        .expect("open disk file inspection")
        .expect("open disk file must be inspectable");
        assert_eq!(facts.offset, Some(310));

        drop(file);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn unknown_access_sets_neither_read_nor_write() {
        assert_eq!(access_flags(None), (false, false));
        assert_eq!(access_flags(Some(GENERIC_READ)), (true, false));
        assert_eq!(access_flags(Some(FILE_APPEND_DATA)), (false, true));
    }

    /// Regression: the pre-copy `GetFileType` short-circuit used to run against
    /// the target's handle values in THIS process's table, so every file of a
    /// foreign process was dropped unless a local handle happened to sit at the
    /// same value and be a disk file. The probe may only ever describe our own
    /// table. A second process is needed to observe the end-to-end effect; it
    /// belongs in `tests/integration.rs`, which this module cannot reach.
    #[test]
    fn only_own_process_handles_are_type_probed_locally() {
        assert!(
            may_probe_local_handle_table(4242, 4242),
            "the enumerated process's own handles may be probed in our table"
        );
        assert!(
            !may_probe_local_handle_table(4241, 4242),
            "another process's handle value names nothing in our table"
        );
        assert!(
            !may_probe_local_handle_table(0, 4242),
            "an unknown PID must not be treated as this process"
        );
    }

    #[test]
    fn write_only_idle_handle_does_not_attribute_file_size() {
        let tag = format!("{}_{:?}", std::process::id(), std::thread::current().id());
        let test_name = format!("test_file_idle_{tag}.bin");
        let test_path = std::env::temp_dir().join(&test_name);

        // Create an 8 MiB file on disk
        std::fs::write(&test_path, vec![7u8; 8 * 1024 * 1024]).unwrap();

        // Open write-only without writing anything
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_path)
            .unwrap();

        let pid = std::process::id();
        let snapshot = list_process_file_io(pid, None).expect("query current process files");
        let found = snapshot.files.iter().find(|f| f.path.ends_with(&test_name));
        assert!(found.is_some(), "opened write-only file must be discovered");
        let found = found.unwrap();
        assert!(found.is_write);
        assert_eq!(found.file_size, Some(8 * 1024 * 1024));
        // A seek position is never a transfer total.
        assert_eq!(found.write_bytes, None);

        drop(file);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn read_write_handle_does_not_fabricate_split() {
        let tag = format!("{}_{:?}", std::process::id(), std::thread::current().id());
        let test_name = format!("test_file_rw_{tag}.bin");
        let test_path = std::env::temp_dir().join(&test_name);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&test_path)
            .unwrap();

        let pid = std::process::id();
        let snapshot = list_process_file_io(pid, None).expect("query current process files");
        let found = snapshot.files.iter().find(|f| f.path.ends_with(&test_name));
        assert!(found.is_some(), "opened read/write file must be discovered");
        let found = found.unwrap();
        assert!(found.is_read);
        assert!(found.is_write);
        // Neither read nor write is fabricated; offset is tracked
        assert!(found.read_bytes.is_none());
        assert!(found.write_bytes.is_none());
        assert_eq!(found.offset, Some(0));

        drop(file);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn system_process_file_io_succeeds_with_kernel_counters() {
        let snapshot = list_process_file_io(4, None).expect("system process file io");
        assert!(snapshot.files.is_empty());
        assert!(!snapshot.accounting.handles_complete);
        assert!(snapshot.accounting.counter_read_bytes.is_some());
    }

    /// The local walk has no per-file byte source and must keep saying so: a
    /// Kernel-File trace needs elevation, which this path does not have. If a
    /// caller ever sees anything but `Unavailable` here, the numbers came from
    /// somewhere they should not have.
    #[test]
    fn the_local_walk_reports_per_file_bytes_as_unavailable() {
        let snapshot =
            list_process_file_io(std::process::id(), None).expect("query current process files");
        assert_eq!(
            snapshot.accounting.state,
            FileIoAccountingState::Unavailable
        );
        assert!(snapshot.files.iter().all(|file| file.read_bytes.is_none()
            && file.write_bytes.is_none()
            && file.read_ops.is_none()
            && file.write_ops.is_none()));
        assert_eq!(snapshot.accounting.read_bytes, 0);
        assert_eq!(snapshot.accounting.unnamed_read_bytes, 0);
        assert_eq!(snapshot.accounting.omitted_read_bytes, 0);
    }

    #[test]
    fn non_existent_process_returns_error() {
        let result = list_process_file_io(999_999_999, None);
        assert!(result.is_err());
    }
}
