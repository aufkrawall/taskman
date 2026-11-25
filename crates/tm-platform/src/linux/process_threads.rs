//! Per-thread evidence from `/proc/<pid>/task`.
//!
//! Linux procfs does not expose a portable thread entry address or cumulative
//! CPU cycles, so those fields stay unavailable. The thread group leader's
//! start tick is re-read before and after the walk; both it and the expected
//! sampled generation must match the task records belonging to `pid`.

use crate::actions::{ProcessThread, ProcessThreads, ThreadState};
use tm_core::error::{Result, TmError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskStat {
    tid: u32,
    state: u8,
    user_ticks: u64,
    kernel_ticks: u64,
    priority: i32,
    nice: i32,
    start_ticks: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TaskStatus {
    tgid: Option<u32>,
    voluntary_switches: Option<u64>,
    nonvoluntary_switches: Option<u64>,
}

impl TaskStatus {
    fn context_switches(self) -> Option<u64> {
        self.voluntary_switches?
            .checked_add(self.nonvoluntary_switches?)
    }
}

/// Parse `/proc/<pid>/task/<tid>/stat`.
///
/// Field two is the command name and may contain spaces, `)` bytes and
/// non-UTF-8 data. Fixed fields therefore begin only after the final `)`.
fn parse_task_stat(input: &[u8]) -> Option<TaskStat> {
    let open = input.iter().position(|&byte| byte == b'(')?;
    let close = input.iter().rposition(|&byte| byte == b')')?;
    if close <= open {
        return None;
    }
    let tid = std::str::from_utf8(&input[..open])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let fields = input[close + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let parse_u64 = |index: usize| {
        std::str::from_utf8(fields.get(index).copied()?)
            .ok()?
            .parse::<u64>()
            .ok()
    };
    let parse_i32 = |index: usize| {
        std::str::from_utf8(fields.get(index).copied()?)
            .ok()?
            .parse::<i32>()
            .ok()
    };
    Some(TaskStat {
        tid,
        state: *fields.first()?.first()?,
        // Field 14 (utime) and field 15 (stime); index zero is field 3.
        user_ticks: parse_u64(11)?,
        kernel_ticks: parse_u64(12)?,
        // Field 18 (priority), 19 (nice) and 22 (starttime).
        priority: parse_i32(15)?,
        nice: parse_i32(16)?,
        start_ticks: i64::try_from(parse_u64(19)?).ok()?,
    })
}

fn parse_task_status(text: &str) -> TaskStatus {
    let mut status = TaskStatus::default();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Tgid:") {
            status.tgid = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
            status.voluntary_switches = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            status.nonvoluntary_switches = value.trim().parse().ok();
        }
    }
    status
}

fn linux_thread_state(state: u8) -> ThreadState {
    match state {
        // Linux TASK_RUNNING covers both on-CPU and runnable-in-queue.
        b'R' => ThreadState::Runnable,
        b'S' => ThreadState::Sleeping,
        b'D' | b'W' => ThreadState::Waiting,
        b'T' | b't' => ThreadState::Stopped,
        b'Z' | b'X' | b'x' => ThreadState::Terminated,
        b'I' => ThreadState::Idle,
        b'P' => ThreadState::Parked,
        other => ThreadState::Other(u32::from(other)),
    }
}

fn ticks_to_100ns(ticks: u64, clk_tck: u64) -> Option<u64> {
    if clk_tck == 0 {
        return None;
    }
    u64::try_from(u128::from(ticks) * 10_000_000 / u128::from(clk_tck)).ok()
}

fn read_task_stat(pid: u32, tid: u32) -> Result<Option<TaskStat>> {
    let path = format!("/proc/{pid}/task/{tid}/stat");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(TmError::platform(
                "read /proc/pid/task/tid/stat",
                format!("{path}: {error}"),
            ));
        }
    };
    let stat = parse_task_stat(&bytes).ok_or_else(|| {
        TmError::platform(
            "parse /proc/pid/task/tid/stat",
            format!("{path}: malformed task record"),
        )
    })?;
    Ok((stat.tid == tid).then_some(stat))
}

fn read_task_status(pid: u32, tid: u32) -> Option<TaskStatus> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/status"))
        .ok()
        .map(|text| parse_task_status(&text))
}

fn check_root_identity(pid: u32, expected_start_ticks: Option<i64>, root: TaskStat) -> Result<()> {
    if root.tid != pid || expected_start_ticks.is_some_and(|expected| root.start_ticks != expected)
    {
        return Err(TmError::ProcessNotFound { pid });
    }
    Ok(())
}

pub(super) fn list_process_threads(
    pid: u32,
    expected_start_ticks: Option<i64>,
) -> Result<ProcessThreads> {
    let root = read_task_stat(pid, pid)?.ok_or(TmError::ProcessNotFound { pid })?;
    check_root_identity(pid, expected_start_ticks, root)?;

    let task_path = format!("/proc/{pid}/task");
    let entries = std::fs::read_dir(&task_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TmError::ProcessNotFound { pid }
        } else {
            TmError::platform("enumerate /proc/pid/task", format!("{task_path}: {error}"))
        }
    })?;
    let mut tids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            TmError::platform("enumerate /proc/pid/task", format!("{task_path}: {error}"))
        })?;
        if let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        {
            tids.push(tid);
        }
    }
    // The leader is part of the thread group even if the directory read raced
    // its entry. Its record and process generation were already validated.
    tids.push(pid);
    tids.sort_unstable();
    tids.dedup();

    let clk_tck = super::linux_clk_tck()
        .ok_or_else(|| TmError::platform("read clock ticks", "CLK_TCK is unavailable"))?;
    let btime_s = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|text| super::parse_proc_stat_btime(&text));

    let mut threads = Vec::with_capacity(tids.len());
    for tid in tids {
        let Some(stat) = read_task_stat(pid, tid)? else {
            // Threads routinely exit during the walk.
            continue;
        };
        if stat.start_ticks < root.start_ticks {
            // A record older than its process cannot belong to this task
            // group, even if its TID path was just reused.
            continue;
        }
        let status = read_task_status(pid, tid);
        if status
            .and_then(|status| status.tgid)
            .is_some_and(|tgid| tgid != pid)
        {
            // Positive evidence that this TID now names another task group:
            // nothing read through this path may be attributed to our process.
            continue;
        }
        // The second read exists only to prove that the status record belongs to
        // the same generation as the stat. A changed or vanished generation
        // therefore invalidates the status-derived counter alone: the first stat
        // is one self-consistent record of this process' thread, so its own facts
        // still stand instead of dropping the row from a list that has no way to
        // say it is incomplete.
        let revalidated = read_task_stat(pid, tid)?
            .is_some_and(|after| after.tid == stat.tid && after.start_ticks == stat.start_ticks);
        let status = if revalidated { status } else { None };
        let context_switches = status.and_then(TaskStatus::context_switches);
        let kernel_time_100ns = ticks_to_100ns(stat.kernel_ticks, clk_tck).ok_or_else(|| {
            TmError::platform(
                "read thread CPU time",
                format!("thread {tid}: clock-tick conversion overflowed"),
            )
        })?;
        let user_time_100ns = ticks_to_100ns(stat.user_ticks, clk_tck).ok_or_else(|| {
            TmError::platform(
                "read thread CPU time",
                format!("thread {tid}: clock-tick conversion overflowed"),
            )
        })?;
        threads.push(ProcessThread {
            tid,
            // Procfs exposes neither the entry routine nor cumulative CPU
            // cycles. The module and thread inventories still provide state,
            // priority, CPU time and context-switch evidence.
            start_address: None,
            priority: stat.priority,
            base_priority: stat.nice,
            nice: Some(stat.nice),
            context_switches,
            kernel_time_100ns,
            user_time_100ns,
            created_epoch_s: btime_s
                .and_then(|btime| super::linux_start_timestamp(btime, stat.start_ticks, clk_tck)),
            state: linux_thread_state(stat.state),
            // Linux wait channels are names/addresses, not Windows
            // KWAIT_REASON identifiers.
            wait_reason: None,
            cycles: None,
        });
    }

    let final_root = read_task_stat(pid, pid)?.ok_or(TmError::ProcessNotFound { pid })?;
    if final_root.start_ticks != root.start_ticks {
        return Err(TmError::ProcessNotFound { pid });
    }
    threads.sort_by_key(|thread| thread.tid);
    Ok(ProcessThreads {
        process_base_priority: root.nice,
        threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_stat_survives_spaces_parens_and_non_utf8_in_comm() {
        let stat = b"4242 (worker (with) \xff parens) S 41 4242 4242 0 -1 4194304 10 0 2 0 123 456 0 0 20 -5 7 0 98765 1000 50 0 0 0 0 0 0 0 0 0 0 77";
        let parsed = parse_task_stat(stat).expect("task stat");
        assert_eq!(parsed.tid, 4242);
        assert_eq!(parsed.state, b'S');
        assert_eq!(parsed.user_ticks, 123);
        assert_eq!(parsed.kernel_ticks, 456);
        assert_eq!(parsed.priority, 20);
        assert_eq!(parsed.nice, -5);
        assert_eq!(parsed.start_ticks, 98_765);
    }

    #[test]
    fn linux_thread_states_are_mapped_without_windows_names() {
        assert_eq!(linux_thread_state(b'R'), ThreadState::Runnable);
        assert_eq!(linux_thread_state(b'S'), ThreadState::Sleeping);
        assert_eq!(linux_thread_state(b'D'), ThreadState::Waiting);
        assert_eq!(linux_thread_state(b'T'), ThreadState::Stopped);
        assert_eq!(linux_thread_state(b'Z'), ThreadState::Terminated);
        assert_eq!(linux_thread_state(b'I'), ThreadState::Idle);
        assert_eq!(linux_thread_state(b'P'), ThreadState::Parked);
        assert_eq!(
            linux_thread_state(b'?'),
            ThreadState::Other(u32::from(b'?'))
        );
    }

    #[test]
    fn switch_counts_require_both_kernel_counters() {
        let complete = parse_task_status(
            "Tgid:\t4242\nvoluntary_ctxt_switches:\t7\nnonvoluntary_ctxt_switches:\t3\n",
        );
        assert_eq!(complete.tgid, Some(4242));
        assert_eq!(complete.context_switches(), Some(10));

        let partial = parse_task_status("voluntary_ctxt_switches:\t7\n");
        assert_eq!(partial.context_switches(), None);
    }

    #[test]
    fn unavailable_ticks_and_cycles_do_not_become_fabricated_values() {
        assert_eq!(ticks_to_100ns(12, 0), None);
        assert_eq!(ticks_to_100ns(1, 100), Some(100_000));
    }

    #[test]
    fn root_identity_rejects_a_recycled_pid_and_never_accepts_a_different_tid() {
        let root = TaskStat {
            tid: 42,
            state: b'S',
            user_ticks: 0,
            kernel_ticks: 0,
            priority: 20,
            nice: 0,
            start_ticks: 100,
        };
        assert!(check_root_identity(42, Some(100), root).is_ok());
        assert!(check_root_identity(42, Some(99), root).is_err());
        assert!(check_root_identity(7, None, root).is_err());
    }

    #[test]
    fn current_process_exposes_its_leader_without_fabricated_thread_details() {
        let snapshot =
            list_process_threads(std::process::id(), None).expect("current process threads");
        let leader = snapshot
            .threads
            .iter()
            .find(|thread| thread.tid == std::process::id())
            .expect("thread-group leader");
        assert_eq!(leader.start_address, None);
        assert_eq!(leader.cycles, None);
        assert_eq!(leader.wait_reason, None);
        assert_eq!(leader.nice, Some(leader.base_priority));
        assert_eq!(snapshot.process_base_priority, leader.base_priority);
    }
}
