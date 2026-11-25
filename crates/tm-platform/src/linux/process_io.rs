//! Linux Process Properties file-I/O facts.
//!
//! Procfs can describe the files a process currently has open, but it exposes
//! no per-file transfer totals. Keep those totals unavailable instead of
//! turning a file pointer, file size, or process-wide counter into fabricated
//! attribution. `/proc/<pid>/io` is useful only as an explicit process-wide
//! counter alongside the open-handle snapshot.

use std::collections::HashMap;
use std::fs::{DirEntry, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::actions::{
    FileIoAccounting, FileIoAccountingState, ProcessFileIo, ProcessFileIoSnapshot,
};
use tm_core::error::Result;

const MAX_FDS: usize = 4_096;
const HANDLE_ENUMERATION_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProcessIoCounters {
    read_bytes: Option<u64>,
    write_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FdAccess {
    is_read: bool,
    is_write: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FdInfo {
    offset: Option<u64>,
    access: Option<FdAccess>,
}

struct HandleFact {
    path: String,
    file_size: Option<u64>,
    offset: Option<u64>,
    access_known: bool,
    is_read: bool,
    is_write: bool,
}

struct HandleEnumeration {
    files: Vec<ProcessFileIo>,
    complete: bool,
}

pub(super) fn list_process_file_io(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<ProcessFileIoSnapshot> {
    // The start tick brackets both procfs walks. A recycled PID must not let a
    // partially old handle list and a partially new counter read become one
    // coherent-looking answer. A sampled generation that is not known yet — a
    // process that started inside the current sampling tick — is read without
    // one instead of being refused, exactly as the module, thread and
    // connection reads in `mod.rs` bracket their own walks.
    let before = super::process_start_for_read(pid, expected_start_epoch_s)?;
    let handles = enumerate_open_files(pid)?;
    let counters = read_process_counters(pid);
    super::ensure_process_generation(pid, before)?;

    Ok(snapshot(handles.files, counters, handles.complete))
}

fn snapshot(
    mut files: Vec<ProcessFileIo>,
    counters: ProcessIoCounters,
    handles_complete: bool,
) -> ProcessFileIoSnapshot {
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    ProcessFileIoSnapshot {
        files,
        accounting: FileIoAccounting {
            // No per-file trace exists on Linux. The process-wide kernel
            // counters below are explicit, separate facts and must not be
            // presented as per-file attribution.
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
            counter_read_bytes: counters.read_bytes,
            counter_write_bytes: counters.write_bytes,
            events_lost: None,
            cleared_file_objects: 0,
            unnamed_creates: 0,
            dropped_paths: 0,
            handles_complete,
            traced_since_process_start: false,
            counter_window_read_bytes: None,
            counter_window_write_bytes: None,
        },
    }
}

fn enumerate_open_files(pid: u32) -> Result<HandleEnumeration> {
    let fd_dir = format!("/proc/{pid}/fd");
    let entries = match std::fs::read_dir(&fd_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            // The process-wide counters can still be available even when this
            // user may not inspect another user's descriptors.
            return Ok(HandleEnumeration {
                files: Vec::new(),
                complete: false,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(tm_core::TmError::ProcessNotFound { pid });
        }
        Err(error) => {
            return Err(tm_core::TmError::platform(
                "read /proc/pid/fd",
                error.to_string(),
            ));
        }
    };

    let mut complete = true;
    let mut files = HashMap::new();
    let started = Instant::now();
    let mut inspected = 0usize;
    for entry in entries {
        if inspected >= MAX_FDS || started.elapsed() >= HANDLE_ENUMERATION_BUDGET {
            complete = false;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let fd_name = entry.file_name();
        let fd_name = fd_name.to_string_lossy();
        if fd_name.parse::<u32>().is_err() {
            continue;
        }
        inspected += 1;
        if let Some(fact) = inspect_fd(pid, &fd_name, &entry, &mut complete) {
            merge_handle_fact(&mut files, fact);
        }
    }

    Ok(HandleEnumeration {
        files: files.into_values().collect(),
        complete,
    })
}

fn inspect_fd(
    pid: u32,
    fd_name: &str,
    entry: &DirEntry,
    complete: &mut bool,
) -> Option<HandleFact> {
    let target = match std::fs::read_link(entry.path()) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            *complete = false;
            return None;
        }
    };
    let metadata = match std::fs::metadata(entry.path()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            *complete = false;
            return None;
        }
    };
    let path = reportable_file_target(&target, &metadata)?;

    let fdinfo = match std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd_name}")) {
        Ok(content) => {
            let parsed = parse_fdinfo(&content);
            if parsed.offset.is_none() || parsed.access.is_none() {
                *complete = false;
            }
            parsed
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            *complete = false;
            FdInfo::default()
        }
    };
    // The fd number is not a stable identity. Re-read both the link and the
    // opened object after fdinfo; if the descriptor was closed/reused during
    // this walk, none of the three observations may be combined.
    let target_after = match std::fs::read_link(entry.path()) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            *complete = false;
            return None;
        }
    };
    let metadata_after = match std::fs::metadata(entry.path()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            *complete = false;
            return None;
        }
    };
    if target_after != target
        || metadata_after.dev() != metadata.dev()
        || metadata_after.ino() != metadata.ino()
    {
        *complete = false;
        return None;
    }
    let access_known = fdinfo.access.is_some();
    let access = fdinfo.access.unwrap_or_default();

    Some(HandleFact {
        path,
        file_size: Some(metadata.len()),
        offset: fdinfo.offset,
        access_known,
        is_read: access.is_read,
        is_write: access.is_write,
    })
}

fn reportable_file_target(target: &Path, metadata: &Metadata) -> Option<String> {
    // Names such as `socket:[123]`, `pipe:[123]`, and `anon_inode:...` are not
    // filesystem paths. Procfs can still stat deleted regular files through
    // the fd link, preserving their path marker, size, and descriptor facts.
    (target.is_absolute() && metadata.file_type().is_file())
        .then(|| target.to_string_lossy().into_owned())
}

fn parse_fdinfo(content: &str) -> FdInfo {
    let mut offset = None;
    let mut access = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "pos" => offset = value.trim().parse().ok(),
            "flags" => {
                access = u32::from_str_radix(value.trim(), 8)
                    .ok()
                    .and_then(access_from_flags);
            }
            _ => {}
        }
    }
    FdInfo { offset, access }
}

fn access_from_flags(flags: u32) -> Option<FdAccess> {
    if flags & libc::O_PATH as u32 != 0 {
        // O_PATH descriptors name an object but cannot read or write its data.
        return Some(FdAccess {
            is_read: false,
            is_write: false,
        });
    }
    match flags & 0o3 {
        0 => Some(FdAccess {
            is_read: true,
            is_write: false,
        }),
        1 => Some(FdAccess {
            is_read: false,
            is_write: true,
        }),
        2 => Some(FdAccess {
            is_read: true,
            is_write: true,
        }),
        _ => None,
    }
}

fn merge_handle_fact(files: &mut HashMap<String, ProcessFileIo>, fact: HandleFact) {
    let HandleFact {
        path,
        file_size,
        offset,
        access_known,
        is_read,
        is_write,
    } = fact;
    let file = files.entry(path.clone()).or_insert_with(|| ProcessFileIo {
        path,
        read_bytes: None,
        write_bytes: None,
        read_ops: None,
        write_ops: None,
        file_size,
        offset,
        access_known: false,
        is_read: false,
        is_write: false,
        open_handles: 0,
    });
    file.open_handles = file.open_handles.saturating_add(1);
    file.access_known |= access_known;
    file.is_read |= is_read;
    file.is_write |= is_write;
    if let Some(offset) = offset {
        file.offset = Some(file.offset.map_or(offset, |current| current.max(offset)));
    }
    if let Some(size) = file_size {
        file.file_size = Some(file.file_size.map_or(size, |current| current.max(size)));
    }
}

fn read_process_counters(pid: u32) -> ProcessIoCounters {
    std::fs::read_to_string(format!("/proc/{pid}/io"))
        .ok()
        .map_or_else(ProcessIoCounters::default, |content| {
            parse_process_counters(&content)
        })
}

fn parse_process_counters(content: &str) -> ProcessIoCounters {
    let mut counters = ProcessIoCounters::default();
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            // These are the kernel's storage-layer byte totals, matching the
            // non-cache-inclusive counter carried by the other backends. The
            // syscall-byte fields (`rchar`/`wchar`) and operation totals are a
            // different measurement and must not be substituted here.
            "read_bytes" => counters.read_bytes = value.trim().parse().ok(),
            "write_bytes" => counters.write_bytes = value.trim().parse().ok(),
            _ => {}
        }
    }
    counters
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;

    #[test]
    fn proc_io_parsing_keeps_only_the_explicit_process_wide_storage_counters() {
        let counters = parse_process_counters(
            "rchar: 11\n\
             wchar: 22\n\
             syscr: 3\n\
             syscw: 4\n\
             read_bytes: 4096\n\
             write_bytes: 8192\n\
             cancelled_write_bytes: 5\n",
        );
        assert_eq!(
            counters,
            ProcessIoCounters {
                read_bytes: Some(4096),
                write_bytes: Some(8192),
            }
        );

        let incomplete = parse_process_counters("read_bytes: nope\nwchar: 22\n");
        assert_eq!(incomplete.read_bytes, None);
        assert_eq!(incomplete.write_bytes, None);
    }

    #[test]
    fn fdinfo_separates_position_from_the_measured_access_mode() {
        let read = parse_fdinfo("pos:\t42\nflags:\t0100000\n");
        assert_eq!(read.offset, Some(42));
        assert_eq!(
            read.access,
            Some(FdAccess {
                is_read: true,
                is_write: false,
            })
        );

        let path_only = parse_fdinfo(&format!("pos:\t7\nflags:\t{:o}\n", libc::O_PATH));
        assert_eq!(
            path_only.access,
            Some(FdAccess {
                is_read: false,
                is_write: false,
            })
        );
    }

    #[test]
    fn handle_shaping_groups_facts_without_inventing_per_file_io() {
        let mut files = HashMap::new();
        merge_handle_fact(
            &mut files,
            HandleFact {
                path: "/tmp/report".into(),
                file_size: Some(100),
                offset: Some(25),
                access_known: true,
                is_read: true,
                is_write: false,
            },
        );
        merge_handle_fact(
            &mut files,
            HandleFact {
                path: "/tmp/report".into(),
                file_size: Some(120),
                offset: Some(75),
                access_known: true,
                is_read: false,
                is_write: true,
            },
        );

        let file = files.get("/tmp/report").expect("grouped file");
        assert_eq!(file.open_handles, 2);
        assert_eq!(file.file_size, Some(120));
        assert_eq!(file.offset, Some(75));
        assert!(file.is_read);
        assert!(file.is_write);
        assert_eq!(file.read_bytes, None);
        assert_eq!(file.write_bytes, None);
        assert_eq!(file.read_ops, None);
        assert_eq!(file.write_ops, None);
    }

    #[test]
    fn snapshot_keeps_process_counters_separate_from_unavailable_file_io() {
        let snapshot = snapshot(
            Vec::new(),
            ProcessIoCounters {
                read_bytes: Some(4096),
                write_bytes: Some(8192),
            },
            true,
        );
        assert_eq!(
            snapshot.accounting.state,
            FileIoAccountingState::Unavailable
        );
        assert_eq!(snapshot.accounting.counter_read_bytes, Some(4096));
        assert_eq!(snapshot.accounting.counter_write_bytes, Some(8192));
        assert_eq!(snapshot.accounting.read_bytes, 0);
        assert_eq!(snapshot.accounting.write_bytes, 0);
    }

    #[test]
    fn deleted_regular_files_are_reportable_but_other_descriptor_types_are_not() {
        let path = std::env::temp_dir().join(format!(
            "tm-linux-process-io-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"open handle").unwrap();
        let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        std::fs::remove_file(&path).unwrap();

        let deleted_target = std::fs::read_link(&fd_path).unwrap();
        let deleted_metadata = std::fs::metadata(&fd_path).unwrap();
        assert!(deleted_target.to_string_lossy().ends_with(" (deleted)"));
        assert!(reportable_file_target(&deleted_target, &deleted_metadata).is_some());

        let device = std::path::Path::new("/dev/null");
        assert!(
            reportable_file_target(device, &std::fs::metadata(device).unwrap()).is_none(),
            "character devices are not files that this view can size or attribute"
        );
        assert!(
            reportable_file_target(std::path::Path::new("socket:[1]"), &deleted_metadata).is_none(),
            "anonymous descriptors have no filesystem path"
        );
    }
}
