//! On-demand file I/O enumeration of one process (Process Properties → I/O).
//!
//! Enumerates open handles for the target process via `NtQueryInformationProcess(51)`.
//! For each handle that represents a disk file (`GetFileType == FILE_TYPE_DISK`),
//! resolves the normalized file path, file size (`EndOfFile`), current read/write
//! position (`SetFilePointerEx`), and granted access rights.

use std::collections::HashMap;

use tm_core::error::{Result, TmError};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_CURRENT, FILE_NAME_NORMALIZED, FILE_STANDARD_INFO, FILE_TYPE_DISK, FileStandardInfo,
    GETFINALPATHNAMEBYHANDLE_FLAGS, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, SetFilePointerEx, VOLUME_NAME_DOS,
};
use windows::Win32::System::Threading as th;

use crate::actions::{
    FileIoAccounting, FileIoAccountingState, ProcessFileIo, ProcessFileIoSnapshot,
};

const PROCESS_HANDLE_INFORMATION: PROCESSINFOCLASS = PROCESSINFOCLASS(51);

const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

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

pub fn list_process_file_io(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<ProcessFileIoSnapshot> {
    super::process_ops::enable_debug_privilege();

    let h_process = super::process_ops::open_process_verified(
        pid,
        th::PROCESS_QUERY_INFORMATION | th::PROCESS_DUP_HANDLE,
        expected_start_epoch_s,
    )?;

    let mut io_counters = th::IO_COUNTERS::default();
    let has_counters = unsafe { th::GetProcessIoCounters(h_process, &mut io_counters) }.is_ok();

    let result = enumerate_process_files(h_process);
    unsafe {
        let _ = CloseHandle(h_process);
    }
    let files = result?;
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
        counter_read_bytes: if has_counters {
            Some(io_counters.ReadTransferCount)
        } else {
            None
        },
        counter_write_bytes: if has_counters {
            Some(io_counters.WriteTransferCount)
        } else {
            None
        },
        events_lost: None,
        handles_complete: true,
    };
    Ok(ProcessFileIoSnapshot { files, accounting })
}

fn enumerate_process_files(h_process: HANDLE) -> Result<Vec<ProcessFileIo>> {
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
        return Ok(Vec::new());
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
    let path_flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let mut path_buf = vec![0u16; 1024];

    for i in 0..count {
        let entry = unsafe { *entries_ptr.add(i) };
        if entry.handle_value == 0 {
            continue;
        }

        let mut dup_handle = HANDLE::default();
        let ok = unsafe {
            DuplicateHandle(
                h_process,
                HANDLE(entry.handle_value as *mut core::ffi::c_void),
                current_proc,
                &mut dup_handle,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok.is_err() || dup_handle.is_invalid() {
            continue;
        }

        // Only disk files; strictly avoid named pipes, sockets, or devices that could hang.
        let file_type = unsafe { GetFileType(dup_handle) };
        if file_type != FILE_TYPE_DISK {
            unsafe {
                let _ = CloseHandle(dup_handle);
            }
            continue;
        }

        let len =
            unsafe { GetFinalPathNameByHandleW(dup_handle, &mut path_buf, path_flags) } as usize;
        if len == 0 || len >= path_buf.len() {
            unsafe {
                let _ = CloseHandle(dup_handle);
            }
            continue;
        }

        let raw_path = String::from_utf16_lossy(&path_buf[..len]);
        let clean_path = strip_verbatim(&raw_path);

        // Query file position if available.
        let mut pos: i64 = 0;
        let pos_ok =
            unsafe { SetFilePointerEx(dup_handle, 0, Some(&mut pos), FILE_CURRENT) }.is_ok();
        let current_pos = if pos_ok && pos >= 0 {
            Some(pos as u64)
        } else {
            None
        };

        // Query file standard information (size, directory).
        let mut std_info = FILE_STANDARD_INFO::default();
        let std_ok = unsafe {
            GetFileInformationByHandleEx(
                dup_handle,
                FileStandardInfo,
                (&raw mut std_info).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        }
        .is_ok();

        let (file_size, is_dir) = if std_ok {
            (Some(std_info.EndOfFile.max(0) as u64), std_info.Directory)
        } else {
            (None, false)
        };

        unsafe {
            let _ = CloseHandle(dup_handle);
        }

        if is_dir {
            continue;
        }

        let is_read = (entry.granted_access & (FILE_READ_DATA | GENERIC_READ)) != 0;
        let is_write =
            (entry.granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE)) != 0;

        let file_entry = map
            .entry(clean_path.clone())
            .or_insert_with(|| ProcessFileIo {
                path: clean_path,
                read_bytes: None,
                write_bytes: None,
                read_ops: None,
                write_ops: None,
                file_size,
                offset: current_pos,
                is_read: false,
                is_write: false,
                open_handles: 0,
            });

        file_entry.open_handles = file_entry.open_handles.saturating_add(1);
        file_entry.is_read |= is_read;
        file_entry.is_write |= is_write;
        if let Some(pos_val) = current_pos {
            file_entry.offset = Some(file_entry.offset.map_or(pos_val, |prev| prev.max(pos_val)));
        }
        if let Some(sz) = file_size {
            file_entry.file_size = Some(file_entry.file_size.map_or(sz, |prev| prev.max(sz)));
        }
    }

    // Attribute observed handle positions based strictly on granted access rights.
    // Never attribute file_size as written bytes, and never duplicate positions across both.
    for file_entry in map.values_mut() {
        if file_entry.is_read && !file_entry.is_write {
            file_entry.read_bytes = file_entry.offset;
            file_entry.write_bytes = Some(0);
        } else if file_entry.is_write && !file_entry.is_read {
            file_entry.read_bytes = Some(0);
            file_entry.write_bytes = file_entry.offset;
        } else {
            // Read and write: cannot guess the split between read and write.
            file_entry.read_bytes = None;
            file_entry.write_bytes = None;
        }
    }

    Ok(map.into_values().collect())
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
        let test_path =
            std::env::temp_dir().join(format!("test_file_io_{}.bin", std::process::id()));

        // Write some bytes and keep the handle open
        let mut file = std::fs::File::create(&test_path).unwrap();
        file.write_all(&[42u8; 4096]).unwrap();
        file.flush().unwrap();

        let pid = std::process::id();
        let snapshot = list_process_file_io(pid, None).expect("query current process files");

        let found = snapshot
            .files
            .iter()
            .find(|f| f.path.ends_with(&format!("test_file_io_{}.bin", pid)));
        assert!(found.is_some(), "opened temporary file must be discovered");
        let found = found.unwrap();
        assert!(found.is_write);
        assert!(found.write_bytes.is_some_and(|w| w >= 4096));
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
            .find(|f| f.path.ends_with(&format!("test_file_io_{}.bin", pid)));
        assert!(found2.is_some(), "read file must be discovered");
        let found2 = found2.unwrap();
        assert!(found2.is_read);
        assert!(found2.read_bytes.is_some_and(|r| r >= 1024));

        drop(read_file);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn write_only_idle_handle_does_not_attribute_file_size() {
        let test_path =
            std::env::temp_dir().join(format!("test_file_idle_{}.bin", std::process::id()));

        // Create an 8 MiB file on disk
        std::fs::write(&test_path, vec![7u8; 8 * 1024 * 1024]).unwrap();

        // Open write-only without writing anything
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&test_path)
            .unwrap();

        let pid = std::process::id();
        let snapshot = list_process_file_io(pid, None).expect("query current process files");
        let found = snapshot
            .files
            .iter()
            .find(|f| f.path.ends_with(&format!("test_file_idle_{}.bin", pid)));
        assert!(found.is_some(), "opened write-only file must be discovered");
        let found = found.unwrap();
        assert!(found.is_write);
        assert_eq!(found.file_size, Some(8 * 1024 * 1024));
        // Crucial invariant: 0 bytes were written, so write_bytes must be 0, NEVER 8 MB!
        assert_eq!(found.write_bytes, Some(0));

        drop(file);
        let _ = std::fs::remove_file(&test_path);
    }

    #[test]
    fn read_write_handle_does_not_fabricate_split() {
        let test_path =
            std::env::temp_dir().join(format!("test_file_rw_{}.bin", std::process::id()));

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&test_path)
            .unwrap();

        let pid = std::process::id();
        let snapshot = list_process_file_io(pid, None).expect("query current process files");
        let found = snapshot
            .files
            .iter()
            .find(|f| f.path.ends_with(&format!("test_file_rw_{}.bin", pid)));
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
    fn non_existent_process_returns_error() {
        let result = list_process_file_io(999_999_999, None);
        assert!(result.is_err());
    }
}
