//! On-demand thread inventory of one process (Process Properties → Threads).
//!
//! The list, states, priorities, times and context-switch counts come from
//! ONE `NtQuerySystemInformation(SystemExtendedProcessInformation)` snapshot:
//! no process or thread handle is needed, so the list also works for the
//! SYSTEM, service and protected processes an unelevated session cannot open —
//! and the process identity is checked against the very record the threads
//! come from. The extended class (57) needs no privilege (unlike class 148).
//!
//! The start address and the cycle counter need a thread handle. The extended
//! record does carry `Win32StartAddress`, but current Windows 11 builds zero
//! it (together with `StartAddress` and the stack bounds) for callers without
//! SeDebugPrivilege — observed on 26300 — so it is only a fallback for an
//! elevated session whose thread opens fail.

use crate::actions::{ProcessThread, ProcessThreads, ThreadState};
use crate::win::cpu_load::{
    CpuLoadAccountant, Offsets, filetime_to_unix_seconds, read_i64, read_u32, read_usize,
};
use tm_core::error::{Result, TmError};
use windows::Wdk::System::SystemInformation::SYSTEM_INFORMATION_CLASS;
use windows::Wdk::System::Threading::{NtQueryInformationThread, ThreadQuerySetWin32StartAddress};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Threading as th;
use windows::Win32::System::WindowsProgramming::{
    SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION,
};

const SYSTEM_EXTENDED_PROCESS_INFORMATION: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(57);

/// `SYSTEM_EXTENDED_THREAD_INFORMATION`, which the extended class emits in
/// place of the plain thread record. Declared (never instantiated) so the
/// compiler lays out the pointer-sized tail for either target width.
#[repr(C)]
#[allow(dead_code)]
struct SystemExtendedThreadInformation {
    thread_info: SYSTEM_THREAD_INFORMATION,
    stack_base: usize,
    stack_limit: usize,
    win32_start_address: usize,
    teb_base: usize,
    reserved2: usize,
    reserved3: usize,
    reserved4: usize,
}

/// Field offsets inside one extended thread record. `SYSTEM_THREAD_INFORMATION`
/// hides KernelTime/UserTime/CreateTime in `Reserved1` (three LARGE_INTEGERs)
/// and ContextSwitches in `Reserved3`; the layout matches Process Hacker's.
struct ThreadOffsets {
    size: usize,
    kernel_time: usize,
    user_time: usize,
    create_time: usize,
    tid: usize,
    priority: usize,
    base_priority: usize,
    context_switches: usize,
    state: usize,
    wait_reason: usize,
    win32_start_address: usize,
}

impl ThreadOffsets {
    fn get() -> Self {
        let reserved1 = std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, Reserved1);
        let client_id = std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, ClientId);
        Self {
            size: std::mem::size_of::<SystemExtendedThreadInformation>(),
            kernel_time: reserved1,
            user_time: reserved1 + 8,
            create_time: reserved1 + 16,
            // CLIENT_ID { UniqueProcess, UniqueThread }.
            tid: client_id + std::mem::size_of::<usize>(),
            priority: std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, Priority),
            base_priority: std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, BasePriority),
            context_switches: std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, Reserved3),
            state: std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, ThreadState),
            wait_reason: std::mem::offset_of!(SYSTEM_THREAD_INFORMATION, WaitReason),
            win32_start_address: std::mem::offset_of!(
                SystemExtendedThreadInformation,
                win32_start_address
            ),
        }
    }
}

pub fn list_process_threads(
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<ProcessThreads> {
    let mut buf = vec![0u8; 1024 * 1024];
    let written = CpuLoadAccountant::nt_query(&mut buf, SYSTEM_EXTENDED_PROCESS_INFORMATION)
        .ok_or_else(|| {
            TmError::platform(
                "NtQuerySystemInformation(SystemExtendedProcessInformation)",
                "query failed",
            )
        })?;
    let parsed = parse_process_threads(&buf[..written], pid, expected_start_epoch_s)?;
    Ok(ProcessThreads {
        process_base_priority: parsed.process_base_priority,
        threads: parsed
            .threads
            .into_iter()
            .map(
                |ParsedThread {
                     mut thread,
                     created_raw,
                 }| {
                    if let Some(read) = read_thread_handle(pid, thread.tid, created_raw) {
                        thread.cycles = read.cycles;
                        thread.start_address = read.start_address.or(thread.start_address);
                    }
                    thread
                },
            )
            .collect(),
    })
}

/// A parsed thread plus its raw creation FILETIME, kept for the identity
/// check of the per-thread handle opened afterwards.
struct ParsedThread {
    thread: ProcessThread,
    created_raw: i64,
}

struct Parsed {
    process_base_priority: i32,
    threads: Vec<ParsedThread>,
}

/// Find `pid`'s record in an extended process table and decode its threads.
/// A creation-time mismatch is `ProcessNotFound` (the PID was recycled), and a
/// record whose thread array does not fit is an error rather than a short list.
fn parse_process_threads(
    buf: &[u8],
    pid: u32,
    expected_start_epoch_s: Option<i64>,
) -> Result<Parsed> {
    let off = Offsets::get();
    let toff = ThreadOffsets::get();
    let corrupt = || {
        TmError::platform(
            "SystemExtendedProcessInformation",
            "process table did not parse",
        )
    };
    let mut pos = 0usize;
    loop {
        if pos.checked_add(off.min_size).ok_or_else(corrupt)? > buf.len() {
            return Err(corrupt());
        }
        let next = read_u32(buf, pos) as usize;
        let record_end = if next == 0 {
            buf.len()
        } else if next >= off.min_size {
            pos.checked_add(next)
                .filter(|end| *end <= buf.len())
                .ok_or_else(corrupt)?
        } else {
            return Err(corrupt());
        };
        if read_usize(buf, pos + off.pid) as u32 == pid {
            let created = filetime_to_unix_seconds(read_i64(buf, pos + off.create_time));
            if let Some(expected) = expected_start_epoch_s
                && created != Some(expected)
            {
                return Err(TmError::ProcessNotFound { pid });
            }
            let count = read_u32(buf, pos + 4) as usize;
            let first = pos + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>();
            let end = count
                .checked_mul(toff.size)
                .and_then(|bytes| first.checked_add(bytes))
                .ok_or_else(corrupt)?;
            if end > record_end {
                return Err(corrupt());
            }
            let threads = (0..count)
                .map(|index| parse_thread(buf, first + index * toff.size, &toff))
                .collect();
            return Ok(Parsed {
                process_base_priority: read_u32(buf, pos + off.base_priority) as i32,
                threads,
            });
        }
        if next == 0 {
            return Err(TmError::ProcessNotFound { pid });
        }
        pos = record_end;
    }
}

fn parse_thread(buf: &[u8], base: usize, off: &ThreadOffsets) -> ParsedThread {
    let created_raw = read_i64(buf, base + off.create_time);
    let start = read_usize(buf, base + off.win32_start_address) as u64;
    ParsedThread {
        thread: ProcessThread {
            tid: read_usize(buf, base + off.tid) as u32,
            start_address: (start != 0).then_some(start),
            priority: read_u32(buf, base + off.priority) as i32,
            base_priority: read_u32(buf, base + off.base_priority) as i32,
            context_switches: read_u32(buf, base + off.context_switches),
            kernel_time_100ns: u64::try_from(read_i64(buf, base + off.kernel_time)).unwrap_or(0),
            user_time_100ns: u64::try_from(read_i64(buf, base + off.user_time)).unwrap_or(0),
            created_epoch_s: filetime_to_unix_seconds(created_raw),
            state: ThreadState::from_raw(read_u32(buf, base + off.state)),
            wait_reason: read_u32(buf, base + off.wait_reason),
            cycles: None,
        },
        created_raw,
    }
}

struct HandleRead {
    cycles: Option<u64>,
    start_address: Option<u64>,
}

/// Cycles and start address of one thread, read through a handle that is
/// proven to be the snapshot's thread: same owning process AND same creation
/// time. A TID recycled between the snapshot and the open yields `None`,
/// never another thread's values.
fn read_thread_handle(pid: u32, tid: u32, created_raw: i64) -> Option<HandleRead> {
    // The start address wants full query access; the limited right still
    // reaches the cycle counter where the full one is refused.
    let handle = unsafe { th::OpenThread(th::THREAD_QUERY_INFORMATION, false, tid) }
        .or_else(|_| unsafe { th::OpenThread(th::THREAD_QUERY_LIMITED_INFORMATION, false, tid) })
        .ok()?;
    let result = (|| {
        if unsafe { th::GetProcessIdOfThread(handle) } != pid {
            return None;
        }
        let mut create = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { th::GetThreadTimes(handle, &mut create, &mut exit, &mut kernel, &mut user) }
            .ok()?;
        let raw = (i64::from(create.dwHighDateTime) << 32) | i64::from(create.dwLowDateTime);
        if raw != created_raw {
            return None;
        }
        let mut cycles = 0u64;
        let cycles = unsafe {
            windows::Win32::System::WindowsProgramming::QueryThreadCycleTime(handle, &mut cycles)
        }
        .ok()
        .map(|()| cycles);
        let mut start = 0usize;
        let status = unsafe {
            NtQueryInformationThread(
                handle,
                ThreadQuerySetWin32StartAddress,
                (&mut start as *mut usize).cast(),
                std::mem::size_of::<usize>() as u32,
                std::ptr::null_mut(),
            )
        };
        Some(HandleRead {
            cycles,
            start_address: (status.is_ok() && start != 0).then_some(start as u64),
        })
    })();
    let _ = unsafe { CloseHandle(handle) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic one-process extended table: a process record
    /// followed by `threads` extended thread records.
    fn table(pid: u32, create_time: i64, tids: &[u32]) -> Vec<u8> {
        let off = Offsets::get();
        let toff = ThreadOffsets::get();
        let head = std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>();
        let mut buf = vec![0u8; head + tids.len() * toff.size];
        buf[4..8].copy_from_slice(&(tids.len() as u32).to_ne_bytes());
        buf[off.create_time..off.create_time + 8].copy_from_slice(&create_time.to_ne_bytes());
        let n = std::mem::size_of::<usize>();
        buf[off.pid..off.pid + n].copy_from_slice(&(pid as usize).to_ne_bytes());
        buf[off.base_priority..off.base_priority + 4].copy_from_slice(&8u32.to_ne_bytes());
        for (index, tid) in tids.iter().enumerate() {
            let base = head + index * toff.size;
            buf[base + toff.tid..base + toff.tid + n]
                .copy_from_slice(&(*tid as usize).to_ne_bytes());
            buf[base + toff.context_switches..base + toff.context_switches + 4]
                .copy_from_slice(&(tid * 10).to_ne_bytes());
            buf[base + toff.state..base + toff.state + 4].copy_from_slice(&5u32.to_ne_bytes());
            buf[base + toff.win32_start_address..base + toff.win32_start_address + n]
                .copy_from_slice(&0x7FF0_0000usize.to_ne_bytes());
        }
        buf
    }

    // 2024-01-01T00:00:00Z as a FILETIME.
    const CREATED: i64 = 133_485_408_000_000_000;
    const CREATED_EPOCH: i64 = 1_704_067_200;

    #[test]
    fn threads_decode_from_the_matching_record() {
        let buf = table(42, CREATED, &[7, 9]);
        let parsed = parse_process_threads(&buf, 42, Some(CREATED_EPOCH)).expect("parsed");
        assert_eq!(parsed.process_base_priority, 8);
        let tids: Vec<u32> = parsed.threads.iter().map(|t| t.thread.tid).collect();
        assert_eq!(tids, [7, 9]);
        assert_eq!(parsed.threads[1].thread.context_switches, 90);
        assert_eq!(parsed.threads[0].thread.state, ThreadState::Waiting);
        assert_eq!(parsed.threads[0].thread.start_address, Some(0x7FF0_0000));
        assert_eq!(parsed.threads[0].created_raw, 0);
    }

    #[test]
    fn a_recycled_pid_is_not_found() {
        let buf = table(42, CREATED, &[7]);
        assert!(matches!(
            parse_process_threads(&buf, 42, Some(CREATED_EPOCH + 1)),
            Err(TmError::ProcessNotFound { pid: 42 })
        ));
        assert!(matches!(
            parse_process_threads(&buf, 43, None),
            Err(TmError::ProcessNotFound { pid: 43 })
        ));
    }

    #[test]
    fn a_truncated_thread_array_is_an_error_not_a_short_list() {
        let mut buf = table(42, CREATED, &[7, 9]);
        buf.truncate(buf.len() - 1);
        assert!(parse_process_threads(&buf, 42, Some(CREATED_EPOCH)).is_err());
    }

    /// Live kernel: the current thread must appear in our own process's
    /// list with a start address and a readable cycle counter, and a parked
    /// thread must reach a state where two snapshots show it never ran in
    /// between — the evidence the "dormant" mark rests on.
    #[test]
    fn live_table_lists_this_process_threads_and_parked_threads_stay_still() {
        let pid = std::process::id();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let parked = std::thread::spawn(move || {
            ready_tx.send(unsafe { th::GetCurrentThreadId() }).unwrap();
            let _ = stop_rx.recv();
        });
        let parked_tid = ready_rx.recv().expect("parked thread id");
        let me = unsafe { th::GetCurrentThreadId() };

        let first = list_process_threads(pid, None).expect("own threads");
        let current = first
            .threads
            .iter()
            .find(|t| t.tid == me)
            .expect("current thread listed");
        assert!(current.start_address.is_some());
        assert!(current.cycles.is_some_and(|c| c > 0));

        // The parked thread may still be on its way into `recv`; poll
        // (bounded) until two consecutive snapshots agree it did not run.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut previous = None;
        loop {
            let snapshot = list_process_threads(pid, None).expect("own threads");
            let parked_now = snapshot
                .threads
                .iter()
                .find(|t| t.tid == parked_tid)
                .expect("parked thread listed")
                .clone();
            if let Some(before) = previous.replace(parked_now.clone())
                && parked_now.state == ThreadState::Waiting
                && before == parked_now
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "parked thread never held still"
            );
            std::thread::yield_now();
        }

        stop_tx.send(()).unwrap();
        parked.join().unwrap();
    }
}
