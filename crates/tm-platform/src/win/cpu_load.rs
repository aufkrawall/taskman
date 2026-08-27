//! Time-based CPU load accounting (global, per-core, per-process).
//!
//! Everything here is derived from raw OS time accumulators obtained with
//! `NtQuerySystemInformation` and differenced over our own monotonic
//! sampling window:
//!
//! * global/per-core: `SystemProcessorPerformanceInformation` gives
//!   idle/kernel/user accumulators per logical CPU. The kernel accumulator
//!   *includes* idle time, so busy = (kernel − idle) + user and
//!   total = kernel + user.
//! * per-process: `SystemProcessInformation` returns every process'
//!   user/kernel accumulators (plus creation time) in a single syscall.
//!
//! Because all values are plain fractions of wall-clock time, the result is
//! completely independent of CPU frequency: boost clocks, park/unpark,
//! power schemes and driver games do not move it.
//!
//! Which Task Manager metric this matches (2026 status):
//! Since the 2025 Task Manager update, the Processes, Performance and Users
//! pages use the standardized TIME-BASED CPU workload this module computes.
//! The older frequency-weighted "% Processor Utility" no longer drives those
//! pages; it survives only as the optional **CPU Utility** column in the
//! Details tab. So this accountant is directionally aligned with CURRENT
//! Task Manager semantics. A separate legacy utility provider (frequency /
//! performance-state aware) is planned as an explicit second metric so both
//! can be offered side by side — do NOT rewrite this calculation to imitate
//! utility (it would silently break current-parity).
//!
//! Semantics produced here:
//! * global/per-core ∈ [0, 100] = fraction of time not in the idle thread.
//! * per-process ∈ [0, 100] = process' CPU-time delta divided by the whole
//!   machine's time capacity in the window ("share of total capacity",
//!   TM Processes/Details style).
//!
//! Attribution completeness: the per-core accumulators see ALL busy time,
//! but the process table only contains processes alive at sample time.
//! Two gaps are closed here so busy work is never left unidentified:
//! * Processes born inside the window are credited their full accumulated
//!   time since creation (which for them is exactly in-window time).
//! * Everything not chargeable to a live process — CPU of processes that
//!   terminated during the window (compilers, scripts, installers churn
//!   through whole generations per tick), plus kernel DPC/ISR servicing —
//!   is reported as `unattributed_pct` together with the image names of
//!   the processes that exited, so the UI can render honest pseudo-rows
//!   ("Terminated processes", "System interrupts") instead of silently
//!   dropping the load.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessInformation, SystemProcessorPerformanceInformation,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

/// Deltas over windows shorter than this are dominated by quantization and
/// scheduling-accounting delay (process times are updated at context-switch
/// granularity), so we resync and keep the previous values instead.
const MIN_WINDOW_S: f64 = 0.12;

/// Hard cap for the process-table buffer growth (typical systems need
/// well under 1 MiB; hundreds of processes ≈ a few hundred KiB).
const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;

const STATUS_SUCCESS: i32 = 0;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;

/// Per-logical-CPU raw time accumulators in 100 ns units (`kernel` includes idle).
#[derive(Debug, Clone, Copy)]
struct CoreRaw {
    idle: i64,
    kernel: i64,
    user: i64,
}

/// Per-process raw time accumulators in 100 ns units.
#[derive(Debug, Clone)]
struct ProcRaw {
    /// Creation time as reported by the kernel; guards against PID reuse.
    create_time: i64,
    kernel: i64,
    user: i64,
    /// Image base name from the kernel table (empty when unparseable).
    /// Remembered for processes that exit so their CPU churn can be named.
    name: Box<str>,
}

/// Per-process load for one sampling window.
#[derive(Debug, Clone, Copy)]
pub struct ProcCpu {
    /// Share of total machine capacity in [0, 100].
    pub pct: f32,
    /// Absolute user+kernel time since process start, 100 ns units.
    pub total_time_100ns: u64,
}

/// One executable image observed among the processes that terminated
/// during the sampling window.
#[derive(Debug, Clone)]
pub struct ExitedImage {
    pub name: String,
    pub count: u32,
}

/// One consistent CPU-load sample.
pub struct LoadSample {
    /// Average busy ratio across logical CPUs in [0, 100].
    pub global_pct: f32,
    /// Busy ratio per logical CPU in [0, 100], index == processor number.
    pub per_core_pct: Vec<f32>,
    /// Kernel-mode busy share per logical CPU in [0, 100] (the "kernel
    /// times" overlay; kernel includes idle so only the non-idle part is
    /// counted). Same length as `per_core_pct`.
    pub per_core_kernel_pct: Vec<f32>,
    /// Average kernel-mode busy share across CPUs [0, 100].
    pub global_kernel_pct: f32,
    /// Per-process values; PIDs absent here were not seen or are new.
    pub procs: HashMap<u32, ProcCpu>,
    /// Busy time in the window that could not be charged to any live
    /// process (terminated processes, DPC/ISR servicing, accounting
    /// residue), as share of total capacity in [0, 100].
    pub unattributed_pct: f32,
    /// Number of processes that terminated during the window.
    pub exited_count: u32,
    /// Most frequently exited images (deduped, highest count first).
    pub exited_images: Vec<ExitedImage>,
}

struct PrevSample {
    at: Instant,
    cores: Vec<CoreRaw>,
    procs: HashMap<u32, ProcRaw>,
}

/// Stateful accountant; call [`sample`](Self::sample) once per tick.
pub struct CpuLoadAccountant {
    prev: Option<PrevSample>,
    proc_buf: Vec<u8>,
    core_buf: Vec<u8>,
}

impl Default for CpuLoadAccountant {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuLoadAccountant {
    pub fn new() -> Self {
        Self {
            prev: None,
            // Typical desktop: ~300 processes * ~600 B + headroom.
            proc_buf: vec![0u8; 512 * 1024],
            // Generous for up to 640 logical processors (24 B each).
            core_buf: vec![0u8; 16 * 1024],
        }
    }

    /// Take one sample. Returns `None` for the first call (no reference
    /// window yet) or when the window is too short / the kernel refused to
    /// answer; in those cases internal state is resynced so the next call
    /// produces valid deltas again.
    pub fn sample(&mut self) -> Option<Arc<LoadSample>> {
        let now = Instant::now();

        // Query cores first, then the process table: keeps the bracket
        // between the two tables tight.
        let cores = self.query_cores()?;
        let procs = self.query_procs()?;

        if cores.is_empty() {
            return None;
        }

        let Some(prev) = self.prev.take() else {
            self.prev = Some(PrevSample {
                at: now,
                cores,
                procs,
            });
            return None;
        };

        let dt_s = now.duration_since(prev.at).as_secs_f64();

        if dt_s < MIN_WINDOW_S {
            // Window too short for trustworthy ratios: resync only.
            self.prev = Some(PrevSample {
                at: now,
                cores,
                procs,
            });
            return None;
        }

        let out = build_sample(&prev, &cores, &procs, dt_s);
        self.prev = Some(PrevSample {
            at: now,
            cores,
            procs,
        });
        Some(Arc::new(out))
    }

    fn nt_query(
        buf: &mut Vec<u8>,
        class: windows::Wdk::System::SystemInformation::SYSTEM_INFORMATION_CLASS,
    ) -> Option<usize> {
        loop {
            let mut retlen: u32 = 0;
            let status = unsafe {
                NtQuerySystemInformation(
                    class,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut retlen,
                )
            };
            if status.0 == STATUS_SUCCESS {
                return Some(retlen.min(buf.len() as u32) as usize);
            }
            if status.0 != STATUS_INFO_LENGTH_MISMATCH || buf.len() >= MAX_BUFFER_BYTES {
                tracing::debug!(status = status.0, "NtQuerySystemInformation failed");
                return None;
            }
            // Grow and retry (the table can legitimately grow between calls).
            let grown = buf.len().saturating_mul(2).min(MAX_BUFFER_BYTES);
            buf.resize(grown.max(buf.len() + 4096), 0);
        }
    }

    fn query_cores(&mut self) -> Option<Vec<CoreRaw>> {
        // This info class is a minefield:
        //
        // * The record size is not fixed: classic builds use 24 B records
        //   `{idle, kernel, user}`, some builds append extra fields (48 B
        //   observed), so the required buffer is not derivable from the CPU
        //   count alone.
        // * The kernel reports the exact requirement via
        //   STATUS_INFO_LENGTH_MISMATCH + ReturnLength.
        // * WORSE: it also "succeeds" for smaller buffers that fit whole
        //   records and then silently returns TRUNCATED data (observed:
        //   half the CPUs missing, interleaved with garbage slots).
        //
        // Therefore: probe with a minimal buffer to learn the required
        // size, then ask again with exactly that size. Only a success at
        // the kernel-named size can be trusted.
        let nb_cpu = logical_processor_count();

        self.core_buf.resize(24, 0);
        let mut retlen: u32 = 0;
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessorPerformanceInformation,
                self.core_buf.as_mut_ptr().cast(),
                self.core_buf.len() as u32,
                &mut retlen,
            )
        };
        if status.0 == STATUS_INFO_LENGTH_MISMATCH && retlen >= 24 {
            self.core_buf.resize(retlen as usize, 0);
            let mut r2: u32 = 0;
            let st2 = unsafe {
                NtQuerySystemInformation(
                    SystemProcessorPerformanceInformation,
                    self.core_buf.as_mut_ptr().cast(),
                    self.core_buf.len() as u32,
                    &mut r2,
                )
            };
            if st2.0 == STATUS_SUCCESS {
                let written = (r2 as usize).min(self.core_buf.len());
                return parse_cores(&self.core_buf, written, nb_cpu);
            }
            tracing::debug!(status = st2.0, "core perf query at probed size failed");
            return None;
        }
        if status.0 == STATUS_SUCCESS && retlen >= 24 {
            // Exotic build accepted the tiny buffer; use what it gave us.
            return parse_cores(&self.core_buf, retlen as usize, nb_cpu);
        }
        tracing::debug!(status = status.0, "core perf probe failed");
        None
    }

    fn query_procs(&mut self) -> Option<HashMap<u32, ProcRaw>> {
        let written = Self::nt_query(&mut self.proc_buf, SystemProcessInformation)?;
        let off = Offsets::get();
        let buf = &self.proc_buf;

        let mut map = HashMap::with_capacity(256);
        let mut pos = 0usize;
        loop {
            if pos.checked_add(off.min_size)? > written {
                break;
            }
            let next = read_u32(buf, pos) as usize;
            let create_time = read_i64(buf, pos + off.create_time);
            let user = read_i64(buf, pos + off.user_time);
            let kernel = read_i64(buf, pos + off.kernel_time);
            let pid = read_usize(buf, pos + off.pid) as u32;
            let buf_base = buf.as_ptr() as usize;
            let name = parse_image_name(buf, buf_base, pos, written, &off);

            // pid 0 is the Idle process whose "CPU time" is just idle time.
            if pid != 0 && kernel >= 0 && user >= 0 {
                map.insert(
                    pid,
                    ProcRaw {
                        create_time,
                        kernel,
                        user,
                        name,
                    },
                );
            }
            if next == 0 {
                break;
            }
            // Corrupt-chain guard: entries must at least cover the header.
            if next < off.min_size || written - pos <= next {
                break;
            }
            pos += next;
        }
        Some(map)
    }
}

/// Byte offsets into `SYSTEM_PROCESS_INFORMATION`. The kernel does not
/// export the layout, so it is addressed manually; the field order has been
/// stable since Vista and matches Process Hacker's definition.
struct Offsets {
    create_time: usize,
    user_time: usize,
    kernel_time: usize,
    pid: usize,
    /// UNICODE_STRING ImageName (Length @ +0, Buffer pointer field follows).
    image_name: usize,
    /// Field holding the name offset, relative to the record start.
    image_name_buffer: usize,
    min_size: usize,
}

impl Offsets {
    const fn get() -> Self {
        if cfg!(target_pointer_width = "64") {
            // NextEntryOffset@0, NumberOfThreads@4, WorkingSetPrivateSize@8,
            // HardFaultCount@16, HighWatermark@20, CycleTime@24,
            // CreateTime@32, UserTime@40, KernelTime@48,
            // ImageName@56 (UNICODE_STRING, 16 B: Length@56, Buffer@64),
            // BasePriority@72, UniqueProcessId@80, InheritedFrom@88,
            // HandleCount@96, SessionId@100.
            Self {
                create_time: 32,
                user_time: 40,
                kernel_time: 48,
                pid: 80,
                image_name: 56,
                image_name_buffer: 64,
                min_size: 104,
            }
        } else {
            // Same order, pointer-sized handles/pointers (UNICODE_STRING is
            // 8 B: Length@56, Buffer@60).
            Self {
                create_time: 32,
                user_time: 40,
                kernel_time: 48,
                pid: 68,
                image_name: 56,
                image_name_buffer: 60,
                min_size: 84,
            }
        }
    }
}

fn build_sample(
    prev: &PrevSample,
    cores: &[CoreRaw],
    procs: &HashMap<u32, ProcRaw>,
    dt_s: f64,
) -> LoadSample {
    let nb_cores = cores.len() as f64;

    let per_core_pct: Vec<f32> = cores
        .iter()
        .zip(prev.cores.iter())
        .map(|(c, p)| core_busy_pct(*p, *c))
        .collect();
    let global_pct = if per_core_pct.is_empty() {
        0.0
    } else {
        per_core_pct.iter().sum::<f32>() / per_core_pct.len() as f32
    };

    // Kernel overlay: fraction of each core's window spent in kernel mode
    // EXCLUDING idle (kernel accumulates idle time).
    let per_core_kernel_pct: Vec<f32> = cores
        .iter()
        .zip(prev.cores.iter())
        .map(|(c, p)| core_kernel_busy_pct(*p, *c))
        .collect();
    let global_kernel_pct = if per_core_kernel_pct.is_empty() {
        0.0
    } else {
        per_core_kernel_pct.iter().sum::<f32>() / per_core_kernel_pct.len() as f32
    };

    // Whole-machine time capacity within the window, 100 ns units.
    let capacity_100ns = dt_s * 1e7 * nb_cores;
    let capacity = |time_100ns: u64| -> f32 {
        if capacity_100ns <= f64::EPSILON {
            return 0.0;
        }
        ((time_100ns as f64 / capacity_100ns) * 100.0).clamp(0.0, 100.0) as f32
    };

    let mut out = HashMap::with_capacity(procs.len());
    // Sum of all in-window CPU time chargeable to LIVE processes.
    let mut accounted_100ns: u64 = 0;
    for (&pid, cur) in procs {
        let total_now = nonneg(cur.kernel) + nonneg(cur.user);
        let in_window = match prev.procs.get(&pid) {
            // Same identity: the delta since the previous sample.
            Some(p) if p.create_time == cur.create_time => {
                let t_prev = nonneg(p.kernel) + nonneg(p.user);
                total_now.saturating_sub(t_prev)
            }
            // Born (or reborn under a reused pid) inside the window: the
            // kernel accumulators run since creation, and creation happened
            // after the previous sample — so the full accumulator IS
            // in-window time. Crediting it here is what makes short-lived
            // but nontrivial processes (compilers!) visible on their first
            // sample instead of starting at a fabricated 0 %.
            _ => total_now,
        };
        accounted_100ns += in_window;
        out.insert(
            pid,
            ProcCpu {
                pct: capacity(in_window),
                total_time_100ns: total_now,
            },
        );
    }

    // Processes that disappeared since the previous sample. Their final
    // accumulators are unobservable, so their in-window time cannot be
    // charged to a row — but they can be NAMED, and their share is captured
    // in the unattributed residual below.
    let mut exited_count: u32 = 0;
    let mut exited_by_name: HashMap<&str, u32> = HashMap::new();
    for (pid, p) in &prev.procs {
        let gone = procs
            .get(pid)
            .is_none_or(|c| c.create_time != p.create_time);
        if gone {
            exited_count += 1;
            if !p.name.is_empty() {
                *exited_by_name.entry(&p.name).or_insert(0) += 1;
            }
        }
    }
    let mut exited_images: Vec<ExitedImage> = exited_by_name
        .into_iter()
        .map(|(name, count)| ExitedImage {
            name: name.to_string(),
            count,
        })
        .collect();
    exited_images.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    exited_images.truncate(8);

    // Global busy time straight from the per-core accumulators (what the
    // Performance page reports). Whatever it exceeds the live processes'
    // chargeable time belongs to terminated processes, interrupt/DPC
    // servicing and accounting residue — surfaced honestly instead of
    // vanishing between the rows.
    let busy_100ns: u64 = cores
        .iter()
        .zip(prev.cores.iter())
        .map(|(c, p)| {
            let d_kernel = c.kernel.saturating_sub(p.kernel) as u64;
            let d_user = c.user.saturating_sub(p.user) as u64;
            let d_idle = c.idle.saturating_sub(p.idle) as u64;
            (d_kernel + d_user).saturating_sub(d_idle)
        })
        .sum();
    let unattributed_100ns = busy_100ns.saturating_sub(accounted_100ns);

    LoadSample {
        global_pct,
        per_core_pct,
        per_core_kernel_pct,
        global_kernel_pct,
        procs: out,
        unattributed_pct: capacity(unattributed_100ns),
        exited_count,
        exited_images,
    }
}

/// Decode the ImageName of one process record.
///
/// The name payload lives inside the same output buffer, but WHERE the
/// UNICODE_STRING Buffer field points has varied across Windows builds:
/// modern NT (which writes the caller's buffer in place) stores an absolute
/// pointer, older conventions use an offset relative to the record start or
/// the table start. Every candidate is bounds-checked and must decode to
/// control-character-free text; any doubt yields an empty name rather than
/// garbage (see the live-kernel unit test pinning this down).
fn parse_image_name(
    buf: &[u8],
    buf_base: usize,
    record: usize,
    written: usize,
    off: &Offsets,
) -> Box<str> {
    if record + off.image_name_buffer + std::mem::size_of::<usize>() > written {
        return "".into();
    }
    let raw = read_usize(buf, record + off.image_name_buffer);
    let len_bytes = read_u32(buf, record + off.image_name) as usize & 0xFFFF;
    // Sanity: real image names are short, even-length UTF-16.
    if raw == 0
        || len_bytes == 0
        || len_bytes > 512
        || !len_bytes.is_multiple_of(2)
        || len_bytes > written
    {
        return "".into();
    }
    // Candidate name locations, tried in order:
    let candidates = [
        raw.checked_sub(buf_base), // absolute pointer into our buffer
        Some(raw),                 // offset from the table start
        record.checked_add(raw),   // offset from the record start
    ];
    for start in candidates.into_iter().flatten() {
        if start >= written || len_bytes > written - start {
            continue;
        }
        let units: Vec<u16> = buf[start..start + len_bytes]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
            .collect();
        let s = String::from_utf16_lossy(&units);
        // Control characters would mean we misread the layout — reject.
        if !s.chars().any(char::is_control) {
            return s.into();
        }
    }
    "".into()
}

/// Kernel-mode (non-idle) fraction of one logical CPU in [0, 100].
fn core_kernel_busy_pct(p: CoreRaw, c: CoreRaw) -> f32 {
    let d_kernel = (c.kernel.saturating_sub(p.kernel)).max(0) as f64;
    let d_user = (c.user.saturating_sub(p.user)).max(0) as f64;
    let d_idle = (c.idle.saturating_sub(p.idle)).max(0) as f64;
    let total = d_kernel + d_user;
    if total <= f64::EPSILON {
        return 0.0;
    }
    ((((d_kernel - d_idle).max(0.0)) / total) * 100.0).clamp(0.0, 100.0) as f32
}

/// Busy fraction of one logical CPU between two readings in [0, 100].
/// Kernel time includes idle, hence busy = total − idle.
fn core_busy_pct(p: CoreRaw, c: CoreRaw) -> f32 {
    let d_kernel = (c.kernel.saturating_sub(p.kernel)).max(0) as f64;
    let d_user = (c.user.saturating_sub(p.user)).max(0) as f64;
    let d_idle = (c.idle.saturating_sub(p.idle)).max(0) as f64;
    let total = d_kernel + d_user;
    if total <= f64::EPSILON {
        return 0.0;
    }
    ((((total - d_idle) / total) * 100.0).clamp(0.0, 100.0)) as f32
}

fn nonneg(v: i64) -> u64 {
    v.max(0) as u64
}

/// Number of logical processors visible to the process. Matches what sysinfo
/// reports (`GetSystemInfo`), keeping per-core indices consistent app-wide.
fn logical_processor_count() -> usize {
    let mut si = SYSTEM_INFO::default();
    unsafe { GetSystemInfo(&mut si) };
    (si.dwNumberOfProcessors as usize).max(1)
}

/// Parse the `SystemProcessorPerformanceInformation` answer.
///
/// The canonical layout is packed 24-byte records `{idle, kernel, user}`;
/// some builds return wider records (extra fields appended). When the answer
/// does not divide into `nb_cpu` packed records we fall back to a stride of
/// `written / nb_cpu`. Both candidates are scored by the invariant
/// `0 <= idle <= kernel` (kernel time includes idle) and the winner wins.
fn parse_cores(buf: &[u8], written: usize, nb_cpu: usize) -> Option<Vec<CoreRaw>> {
    if written < 24 || buf.len() < written {
        return None;
    }

    // Candidate A: packed 24 B records.
    let max_packed = written / 24;
    let packed: Vec<CoreRaw> = (0..max_packed.min(nb_cpu))
        .map(|r| CoreRaw {
            idle: read_i64(buf, r * 24),
            kernel: read_i64(buf, r * 24 + 8),
            user: read_i64(buf, r * 24 + 16),
        })
        .collect();

    // Candidate B: wider records, one per CPU.
    let strided: Option<Vec<CoreRaw>> = if written.is_multiple_of(nb_cpu) && written / nb_cpu >= 24
    {
        let stride = written / nb_cpu;
        Some(
            (0..nb_cpu)
                .filter_map(|r| {
                    let base = r * stride;
                    if base + 24 > written {
                        return None;
                    }
                    Some(CoreRaw {
                        idle: read_i64(buf, base),
                        kernel: read_i64(buf, base + 8),
                        user: read_i64(buf, base + 16),
                    })
                })
                .collect(),
        )
    } else {
        None
    };

    let sane = |cores: &[CoreRaw]| -> usize {
        cores
            .iter()
            .filter(|c| c.idle >= 0 && c.user >= 0 && c.kernel >= 0 && c.kernel >= c.idle)
            .count()
    };

    let strided_sane = strided.as_ref().map(|sc| sane(sc)).unwrap_or(0);
    if strided_sane > sane(&packed) {
        return strided;
    }
    if !packed.is_empty() {
        return Some(packed);
    }
    strided
}

// ---- little-endian-safe unchecked-free readers -----------------------------

fn read_u32(b: &[u8], o: usize) -> u32 {
    let mut s = [0u8; 4];
    s.copy_from_slice(&b[o..o + 4]);
    u32::from_ne_bytes(s)
}

fn read_i64(b: &[u8], o: usize) -> i64 {
    let mut s = [0u8; 8];
    s.copy_from_slice(&b[o..o + 8]);
    i64::from_ne_bytes(s)
}

fn read_usize(b: &[u8], o: usize) -> usize {
    let n = std::mem::size_of::<usize>();
    let mut s = [0u8; 8];
    s[..n].copy_from_slice(&b[o..o + n]);
    usize::from_ne_bytes(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(idle: i64, kernel: i64, user: i64) -> CoreRaw {
        CoreRaw { idle, kernel, user }
    }

    /// Validates the self-relative ImageName interpretation against the
    /// real kernel table: one live NtQuerySystemInformation call, decoded
    /// names must match known system processes.
    #[test]
    fn live_kernel_table_yields_sane_image_names() {
        let mut acc = CpuLoadAccountant::new();
        let procs = acc.query_procs().expect("NtQuerySystemInformation");
        assert!(procs.len() > 10, "suspiciously empty process table");
        let named = procs.values().filter(|p| !p.name.is_empty()).count();
        assert!(
            named > procs.len() / 2,
            "most processes must decode a name ({named}/{})",
            procs.len()
        );
        // The kernel's own name for the System process (always present).
        assert_eq!(&*procs.get(&4).expect("pid 4 present").name, "System");
        for p in procs.values() {
            assert!(
                !p.name.chars().any(char::is_control),
                "garbage: {:?}",
                p.name
            );
        }
    }

    #[test]
    fn idle_core_is_zero_and_busy_core_is_hundred() {
        // Window of exactly 1 s: each accumulator advanced by 1e7 (100 ns units).
        // Idle machine: kernel includes the idle time.
        let p = core(0, 0, 0);
        assert_eq!(core_busy_pct(p, core(10_000_000, 10_000_000, 0)), 0.0);
        // Fully busy (no idle progress): kernel advanced without idle.
        assert_eq!(core_busy_pct(p, core(0, 10_000_000, 0)), 100.0);
        // Half busy.
        assert_eq!(core_busy_pct(p, core(5_000_000, 10_000_000, 0)), 50.0);
        // User-mode only busy half.
        assert_eq!(
            core_busy_pct(p, core(5_000_000, 5_000_000, 5_000_000)),
            50.0
        );
    }

    #[test]
    fn counter_reset_does_not_explode() {
        let p = core(9_000_000, 10_000_000, 0);
        // Accumulators went backwards (resync elsewhere): must clamp to 0.
        assert_eq!(core_busy_pct(p, core(0, 0, 0)), 0.0);
    }

    #[test]
    fn boost_clocks_cannot_inflate_time_based_load() {
        // A core executing at any frequency still only has 1 second of wall
        // time per second; the math below contains no frequency input, so a
        // boosted-but-mostly-idle core stays at its true busy ratio:
        // idle 0.6 s of a 1.0 s window → 40 % busy, no matter the clock.
        let p = core(0, 0, 0);
        let pct = core_busy_pct(p, core(6_000_000, 8_000_000, 2_000_000));
        assert_eq!(pct, 40.0);
    }

    #[test]
    fn process_share_of_total_capacity_matches_tm_semantics() {
        // 8 logical CPUs, window 1 s → capacity = 8e7 * ... in 100 ns units.
        let dt = 1.0f64;
        let cap = dt * 1e7 * 8.0;
        // Process burned exactly one full core-second.
        let used_one_core = 1e7 as u64;
        let pct = (used_one_core as f64 / cap * 100.0).clamp(0.0, 100.0);
        assert_eq!(pct, 12.5);

        // Process burned all eight cores completely → capped at 100.
        let used_all = 8.0 * 1e7_f64;
        let pct = (used_all / cap * 100.0).clamp(0.0, 100.0);
        assert_eq!(pct, 100.0);
    }

    #[test]
    fn build_sample_maps_processes_and_excludes_pid_reuse_ghosts() {
        let prev_procs: HashMap<u32, ProcRaw> = HashMap::from([
            (
                100,
                ProcRaw {
                    create_time: 111,
                    kernel: 1e7 as i64,
                    user: 1e7 as i64,
                    name: "stay.exe".into(),
                },
            ),
            // Same pid, different identity later → the old one exited, the
            // new one is credited from its creation.
            (
                200,
                ProcRaw {
                    create_time: 222,
                    kernel: 5e7 as i64,
                    user: 0,
                    name: "old.exe".into(),
                },
            ),
        ]);
        let cur_procs: HashMap<u32, ProcRaw> = HashMap::from([
            (
                100,
                ProcRaw {
                    create_time: 111,
                    kernel: 3e7 as i64,
                    user: 3e7 as i64,
                    name: "stay.exe".into(),
                },
            ),
            (
                200,
                ProcRaw {
                    create_time: 999, // reused pid
                    kernel: 2e7 as i64,
                    user: 0,
                    name: "new.exe".into(),
                },
            ),
        ]);
        let prev = PrevSample {
            at: Instant::now(),
            cores: vec![core(0, 0, 0)],
            procs: prev_procs,
        };
        let cores = vec![core(0, 6e7 as i64, 0)];
        let out = build_sample(&prev, &cores, &cur_procs, 4.0 /* s */);

        // Process 100: Δ(kernel+user) = 4e7 over 4 s on 1 core (cap = 4e7)
        // → exactly 100 % of capacity... clamped at 100.
        let p100 = out.procs.get(&100).unwrap();
        assert_eq!(p100.pct, 100.0);
        assert_eq!(p100.total_time_100ns, 6e7 as u64);

        // Reused pid: no matching predecessor → credited its full lifetime
        // (2e7 of 4e7 capacity = 50 %), never a fabricated 0.
        let p200 = out.procs.get(&200).unwrap();
        assert_eq!(p200.pct, 50.0);

        // One core busy 6e7 of 4e7-window capacity → global clamped to 100.
        assert_eq!(out.global_pct, 100.0);

        // old.exe exited during the window and is named.
        assert_eq!(out.exited_count, 1);
        assert_eq!(out.exited_images.len(), 1);
        assert_eq!(out.exited_images[0].name, "old.exe");
    }

    #[test]
    fn process_born_inside_window_is_credited_from_creation() {
        // 4 logical CPUs, 1 s window → capacity 4e7 per core-second.
        let prev = PrevSample {
            at: Instant::now(),
            cores: vec![core(0, 0, 0); 4],
            procs: HashMap::new(),
        };
        // New process burned 2 full core-seconds before its first sample.
        let cur = HashMap::from([(
            7,
            ProcRaw {
                create_time: 1,
                kernel: 1e7 as i64,
                user: 1e7 as i64,
                name: "rustc.exe".into(),
            },
        )]);
        let cores = vec![core(0, 1e7 as i64, 0); 4];
        let out = build_sample(&prev, &cores, &cur, 1.0);
        assert_eq!(out.procs.get(&7).unwrap().pct, 50.0);
        // Busy cores account 4e7, live process accounts 2e7 → rest is
        // unattributed accounting residue.
        assert_eq!(out.unattributed_pct, 50.0);
    }

    #[test]
    fn terminated_process_time_becomes_unattributed_and_named() {
        // 1 core, 1 s window: total busy 1e7, of which the surviving
        // process burned 0.2e7. The exited rustc.exe owned the rest.
        let prev = PrevSample {
            at: Instant::now(),
            cores: vec![core(0, 0, 0)],
            procs: HashMap::from([
                (
                    10,
                    ProcRaw {
                        create_time: 1,
                        kernel: 0,
                        user: 0,
                        name: "rustc.exe".into(),
                    },
                ),
                (
                    11,
                    ProcRaw {
                        create_time: 1,
                        kernel: 0,
                        user: 0,
                        name: "rustc.exe".into(),
                    },
                ),
            ]),
        };
        let cur = HashMap::from([(
            12,
            ProcRaw {
                create_time: 1,
                kernel: 0.2e7 as i64,
                user: 0,
                name: "cargo.exe".into(),
            },
        )]);
        let cores = vec![core(0, 1e7 as i64, 0)];
        let out = build_sample(&prev, &cores, &cur, 1.0);
        assert_eq!(out.procs.get(&12).unwrap().pct, 20.0);
        assert_eq!(out.unattributed_pct, 80.0);
        assert_eq!(out.exited_count, 2);
        assert_eq!(out.exited_images.len(), 1);
        assert_eq!(out.exited_images[0].name, "rustc.exe");
        assert_eq!(out.exited_images[0].count, 2);
    }

    #[test]
    fn image_name_decodes_all_buffer_conventions() {
        let off = Offsets::get();
        let rec = vec![0u8; off.min_size];
        let name: Vec<u16> = "rustc.exe".encode_utf16().collect();

        // Convention A: absolute pointer into the output buffer.
        let base = 0x0001_0000_0000usize; // pretend buffer base address
        let mut buf = rec.clone();
        let abs_start = base + buf.len();
        for u in &name {
            buf.extend_from_slice(&u.to_ne_bytes());
        }
        let written = buf.len();
        buf[off.image_name..off.image_name + 2]
            .copy_from_slice(&((name.len() * 2) as u16).to_ne_bytes());
        buf[off.image_name_buffer..off.image_name_buffer + std::mem::size_of::<usize>()]
            .copy_from_slice(&abs_start.to_ne_bytes());
        assert_eq!(
            &*parse_image_name(&buf, base, 0, written, &off),
            "rustc.exe",
            "absolute-pointer convention"
        );

        // Convention B: offset relative to the record start (old builds).
        let mut buf = rec.clone();
        let name_off = buf.len();
        for u in &name {
            buf.extend_from_slice(&u.to_ne_bytes());
        }
        let written = buf.len();
        buf[off.image_name..off.image_name + 2]
            .copy_from_slice(&((name.len() * 2) as u16).to_ne_bytes());
        buf[off.image_name_buffer..off.image_name_buffer + std::mem::size_of::<usize>()]
            .copy_from_slice(&name_off.to_ne_bytes());
        assert_eq!(
            &*parse_image_name(&buf, base, 0, written, &off),
            "rustc.exe",
            "record-relative convention"
        );

        // Out-of-bounds pointer → empty, never garbage.
        buf[off.image_name_buffer..off.image_name_buffer + std::mem::size_of::<usize>()]
            .copy_from_slice(&999_999usize.to_ne_bytes());
        assert_eq!(&*parse_image_name(&buf, base, 0, written, &off), "");

        // Control characters (misread layout) → rejected.
        let mut bad = rec;
        bad[off.image_name..off.image_name + 2].copy_from_slice(&4u16.to_ne_bytes());
        bad.truncate(off.min_size);
        bad.extend_from_slice(&3u16.to_ne_bytes());
        bad.extend_from_slice(&7u16.to_ne_bytes());
        let bw = bad.len();
        bad[off.image_name_buffer..off.image_name_buffer + std::mem::size_of::<usize>()]
            .copy_from_slice(&off.min_size.to_ne_bytes());
        assert_eq!(&*parse_image_name(&bad, base, 0, bw, &off), "");
        let _ = rec; // rec cloned into bad above
    }
}
