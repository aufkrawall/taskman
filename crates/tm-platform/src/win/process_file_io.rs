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
        handles_complete,
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

    let header = unsafe { &*buf.as_ptr().cast::<ProcessHandleSnapshotInformation>() };
    let max_entries = (buf.len() - header_size) / entry_size;
    let count = header.number_of_handles.min(max_entries);

    let entries_ptr = unsafe {
        buf.as_ptr()
            .add(header_size)
            .cast::<ProcessHandleTableEntryInfo>()
    };
    let current_proc = unsafe { th::GetCurrentProcess() };

    let mut map: HashMap<String, ProcessFileIo> = HashMap::new();
    let mut path_buf = vec![0u16; 1024];
    let mut complete = count == header.number_of_handles;

    for i in 0..count {
        let entry = unsafe { *entries_ptr.add(i) };
        if entry.handle_value == 0 || entry.handle_attributes & OBJ_KERNEL_HANDLE != 0 {
            continue;
        }
        let facts = match inspect_handle(h_process, current_proc, entry.handle_value, &mut path_buf)
        {
            Ok(Some(facts)) => facts,
            Ok(None) => continue,
            Err(()) => {
                complete = false;
                continue;
            }
        };
        if !facts.complete {
            complete = false;
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

/// Duplicates `handle_value` out of `h_process` and inspects the duplicate
/// read-only. Returns `None` for handles that are gone, are not disk files,
/// are directories, or have no resolvable path.
fn inspect_handle(
    h_process: HANDLE,
    current_proc: HANDLE,
    handle_value: usize,
    path_buf: &mut [u16],
) -> HandleInspection<Option<HandleFacts>> {
    let source = HANDLE(handle_value as *mut core::ffi::c_void);
    // Query the local handle table entry before asking PROCESS_DUP_HANDLE to
    // copy it. Most of a real process's table is non-files; protected or
    // non-duplicable special handles must not turn a complete file walk into
    // an incomplete one.
    if unsafe { GetFileType(source) } != FILE_TYPE_DISK {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

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

    #[test]
    fn non_existent_process_returns_error() {
        let result = list_process_file_io(999_999_999, None);
        assert!(result.is_err());
    }
}
