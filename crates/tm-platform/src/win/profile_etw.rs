//! On-demand sampled instruction-pointer attribution for the Modules page.
//!
//! The system profile provider emits a sampled-profile record carrying the
//! instruction pointer that was executing when a processor was sampled. For
//! the Modules page we only need the page containing that pointer: matching
//! it against the target process's current module ranges answers "did this
//! module's code execute in the window?" without suspending a live game or
//! walking another process's threads.
//!
//! Sampled-profile records do not reliably carry a process id. Their payload
//! carries the TID, and the system thread provider supplies thread start/stop
//! plus rundown events. The trace is seeded from a one-shot ToolHelp thread
//! snapshot first, so long-lived render threads predating the session are
//! immediately attributable. A header PID is accepted only as a fallback for
//! a newly created thread whose start event was missed; a known thread's
//! owner always wins, and samples from another process are never charged to
//! the target.
//!
//! This is deliberately not a symbolizing profiler and does not collect call
//! stacks. A sample whose page lies in no currently loaded module is reported
//! as an unattributed remainder. A window with no usable sample stream, an
//! unknown ETW loss count, or an exhausted page budget must stay visible as
//! unavailable/lower-bound evidence rather than being presented as exact CPU
//! time.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tm_core::error::{Result, TmError};
use windows::Win32::System::Diagnostics::Etw::{
    EVENT_RECORD, PerfInfoGuid, SYSTEM_PROCESS_KW_THREAD, SYSTEM_PROFILE_KW_GENERAL,
    SystemProcessProviderGuid, SystemProfileProviderGuid, ThreadGuid,
};

use super::etw::{
    self, LoggerKind, Provider, Session, TraceContext, TraceRole, plausible_client_id,
};
use crate::actions::{ProcessModule, ProcessModuleCodeSamples, ProcessModuleCodeWindow};

/// Legacy `SampledProfile` event type. For MOF-defined ETW events, the event
/// type lives in `EVENT_DESCRIPTOR.Opcode`, not the manifest-only `Id` field.
/// The payload starts with a pointer-sized instruction pointer followed by the
/// sampled TID.
const SAMPLE_PROFILE_EVENT_TYPE: u8 = 46;

const OPCODE_THREAD_START: u8 = 1;
const OPCODE_THREAD_STOP: u8 = 2;
const OPCODE_THREAD_DC_START: u8 = 3;
const OPCODE_THREAD_DC_STOP: u8 = 4;

/// Page keys are sufficient for module attribution and drastically reduce the
/// cardinality of the per-window map. Exceeding this means the window cannot
/// be presented as a complete sample set.
const MAX_SAMPLE_PAGES: usize = 131_072;

/// Keep the system profile session alive across a few missed UI ticks, then
/// stop it so closing Process Properties cannot leave a machine-wide trace
/// running forever.
const PROFILE_TRACE_IDLE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SampledProfile {
    instruction_pointer: u64,
    thread_id: u32,
}

/// Decode the stable prefix of `SampledProfile`.
///
/// The payload begins with a pointer-sized `InstructionPointer`, followed by
/// `ThreadId`. Later versions add count, priority and DPC/ISR flags; none of
/// those are needed for module attribution. The handle-table alignment check
/// on the TID also rejects most malformed reads, while ETW's architecture flag
/// remains the authoritative pointer-width source.
fn parse_sampled_profile(payload: &[u8], pointer_bytes: usize) -> Option<SampledProfile> {
    let thread_offset = pointer_bytes;
    if !matches!(pointer_bytes, 4 | 8) || payload.len() < thread_offset + 4 {
        return None;
    }
    let instruction_pointer = if pointer_bytes == 8 {
        u64::from_le_bytes(payload[0..8].try_into().ok()?)
    } else {
        u64::from(u32::from_le_bytes(payload[0..4].try_into().ok()?))
    };
    let thread_id = u32::from_le_bytes(payload[thread_offset..thread_offset + 4].try_into().ok()?);
    (instruction_pointer != 0 && plausible_client_id(thread_id)).then_some(SampledProfile {
        instruction_pointer,
        thread_id,
    })
}

/// Parse the `(ProcessID, ThreadID)` prefix shared by classic Thread events.
fn parse_thread_event(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let tid = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    (plausible_client_id(pid) && plausible_client_id(tid)).then_some((pid, tid))
}

struct State {
    target_pid: u32,
    target_threads: HashSet<u32>,
    pages: HashMap<u64, u64>,
    events_seen: u64,
    events_decoded: u64,
    rejected_payload_len: u16,
    overflowed: bool,
}

impl State {
    fn new(target_pid: u32, target_threads: HashSet<u32>) -> Self {
        Self {
            target_pid,
            target_threads,
            pages: HashMap::new(),
            events_seen: 0,
            events_decoded: 0,
            rejected_payload_len: 0,
            overflowed: false,
        }
    }

    fn record_sample(&mut self, header_pid: u32, sample: Option<SampledProfile>) {
        self.events_seen = self.events_seen.saturating_add(1);
        let Some(sample) = sample else {
            return;
        };
        self.events_decoded = self.events_decoded.saturating_add(1);
        let known_target_thread = self.target_threads.contains(&sample.thread_id);
        let header_names_target = header_pid == self.target_pid;
        if !known_target_thread && !header_names_target {
            return;
        }
        if !known_target_thread {
            // A thread created after the seed whose start record was missed
            // still becomes attributable when a sample's own header names the
            // target process. Track it so the UI can distinguish "no target
            // thread observed" from a usable stream.
            self.target_threads.insert(sample.thread_id);
        }
        let page = sample.instruction_pointer & !0xFFF;
        if !self.pages.contains_key(&page) && self.pages.len() >= MAX_SAMPLE_PAGES {
            self.overflowed = true;
            return;
        }
        let count = self.pages.entry(page).or_default();
        *count = count.saturating_add(1);
    }

    fn track_thread(&mut self, pid: u32, tid: u32) {
        if pid == self.target_pid {
            self.target_threads.insert(tid);
        } else {
            // This TID no longer belongs to the target.
            self.target_threads.remove(&tid);
        }
    }

    fn forget_thread(&mut self, tid: u32) {
        self.target_threads.remove(&tid);
    }
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
    fn record_sample(&self, header_pid: u32, sample: Option<SampledProfile>, payload_len: u16) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.record_sample(header_pid, sample);
        if sample.is_none() {
            state.rejected_payload_len = payload_len;
        }
    }

    fn track_thread(&self, pid: u32, tid: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.track_thread(pid, tid);
        }
    }

    fn forget_thread(&self, tid: u32) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.forget_thread(tid);
        }
    }
}

/// Attribute sampled pages to module ranges.
///
/// Module end addresses are exclusive and checked with `checked_add`; a sample
/// outside every range remains in `target_samples - attributed_samples`.
fn samples_by_module(
    pages: &HashMap<u64, u64>,
    modules: &[ProcessModule],
) -> (u64, Vec<ProcessModuleCodeSamples>) {
    let mut ranges: Vec<(u64, u64, usize)> = modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.size_bytes != 0)
        .filter_map(|(index, module)| {
            module
                .base_address
                .checked_add(module.size_bytes)
                .map(|end| (module.base_address, end, index))
        })
        .collect();
    ranges.sort_unstable_by_key(|range| range.0);
    let mut counts = vec![0u64; modules.len()];
    for (&page, &samples) in pages {
        let candidate = ranges.partition_point(|range| range.0 <= page);
        let Some(&(_, end, index)) = candidate
            .checked_sub(1)
            .and_then(|position| ranges.get(position))
        else {
            continue;
        };
        if page < end {
            counts[index] = counts[index].saturating_add(samples);
        }
    }
    let mut attributed = 0u64;
    let entries = modules
        .iter()
        .zip(counts)
        .filter(|(_, samples)| *samples != 0)
        .map(|(module, samples)| {
            attributed = attributed.saturating_add(samples);
            ProcessModuleCodeSamples {
                base_address: module.base_address,
                samples,
            }
        })
        .collect();
    (attributed, entries)
}

struct ProcessModuleTrace {
    session: Session<Shared>,
    window_start: Instant,
    previous_events_lost: Option<u64>,
}

impl ProcessModuleTrace {
    fn start(role: TraceRole, pid: u32) -> Option<Self> {
        let target_threads = super::threads_map::thread_owners()
            .into_iter()
            .filter_map(|(tid, owner)| (owner == pid).then_some(tid))
            .collect();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(pid, target_threads)),
            live: AtomicBool::new(true),
        });
        let session = Session::start(
            session_name(role),
            LoggerKind::SystemProfile,
            &[
                Provider {
                    guid: SystemProfileProviderGuid,
                    match_any_keyword: SYSTEM_PROFILE_KW_GENERAL,
                },
                // Thread start/stop and the rundown of threads that existed
                // before profiling began.
                Provider {
                    guid: SystemProcessProviderGuid,
                    match_any_keyword: SYSTEM_PROCESS_KW_THREAD,
                },
            ],
            shared,
            on_event,
            "tm-profile-etw",
        )?;
        tracing::info!(pid, "per-process module code-sample trace started");
        Some(Self {
            session,
            window_start: Instant::now(),
            previous_events_lost: None,
        })
    }

    fn is_running(&self) -> bool {
        self.session.is_running()
    }

    fn take_window(&mut self, modules: &[ProcessModule]) -> Result<ProcessModuleCodeWindow> {
        let now = Instant::now();
        let window_ms = now
            .saturating_duration_since(self.window_start)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        // Drain and query loss while callbacks cannot add another sample.
        // Querying loss first left a small window in which newly delivered
        // samples could be counted without the losses that displaced them.
        let (
            pages,
            events_seen,
            events_decoded,
            rejected_payload_len,
            tracked_threads,
            overflowed,
            cumulative_lost,
            next_window_start,
        ) = {
            let mut state = match self.session.shared().state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            let pages = std::mem::take(&mut state.pages);
            let tracked_threads = state.target_threads.len().min(u32::MAX as usize) as u32;
            let rejected = state.rejected_payload_len;
            state.rejected_payload_len = 0;
            let cumulative_lost = self.session.events_lost();
            let next_window_start = Instant::now();
            (
                pages,
                std::mem::take(&mut state.events_seen),
                std::mem::take(&mut state.events_decoded),
                rejected,
                tracked_threads,
                std::mem::take(&mut state.overflowed),
                cumulative_lost,
                next_window_start,
            )
        };
        self.window_start = next_window_start;
        let events_lost = cumulative_lost.map(|cumulative| {
            let previous = self.previous_events_lost.unwrap_or(0);
            cumulative.saturating_sub(previous)
        });
        self.previous_events_lost = cumulative_lost;
        if events_seen > 0 && events_decoded == 0 {
            tracing::warn!(
                events_seen,
                rejected_payload_len,
                "module profile trace decoded no sampled-profile records"
            );
        }
        if overflowed {
            return Err(TmError::platform(
                "module code samples",
                "sampled page cardinality exceeded the bounded per-window budget",
            ));
        }
        let target_samples = pages
            .values()
            .fold(0u64, |total, samples| total.saturating_add(*samples));
        let (attributed_samples, module_samples) = samples_by_module(&pages, modules);
        Ok(ProcessModuleCodeWindow {
            window_ms,
            events_seen,
            events_decoded,
            events_lost,
            target_samples,
            attributed_samples,
            tracked_threads,
            modules: module_samples,
        })
    }
}

fn pointer_bytes(record: &EVENT_RECORD) -> usize {
    const EVENT_HEADER_FLAG_32_BIT_HEADER: u16 = 0x0020;
    if record.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
        4
    } else {
        8
    }
}

/// ETW record callback. The hot path is allocation-free except for the
/// page-count map's first insertion for a page.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(shared) = (unsafe { etw::context_of::<Shared>(record) }) else {
        return;
    };
    let provider = record.EventHeader.ProviderId;
    if provider == SystemProfileProviderGuid || provider == PerfInfoGuid {
        if record.EventHeader.EventDescriptor.Opcode != SAMPLE_PROFILE_EVENT_TYPE {
            return;
        }
        let sample = unsafe { etw::payload_of(record) }
            .and_then(|payload| parse_sampled_profile(payload, pointer_bytes(record)));
        shared.record_sample(record.EventHeader.ProcessId, sample, record.UserDataLength);
        return;
    }
    if provider != ThreadGuid {
        return;
    }
    let Some(payload) = (unsafe { etw::payload_of(record) }) else {
        return;
    };
    let Some((pid, tid)) = parse_thread_event(payload) else {
        return;
    };
    match record.EventHeader.EventDescriptor.Opcode {
        OPCODE_THREAD_START | OPCODE_THREAD_DC_START => shared.track_thread(pid, tid),
        OPCODE_THREAD_STOP | OPCODE_THREAD_DC_STOP => shared.forget_thread(tid),
        _ => {}
    }
}

struct ActiveTrace {
    trace: ProcessModuleTrace,
    role: TraceRole,
    pid: u32,
    expected_start_epoch_s: i64,
    generation: u64,
    last_request: Instant,
}

fn active_trace() -> &'static Mutex<Option<ActiveTrace>> {
    static SLOT: std::sync::OnceLock<Mutex<Option<ActiveTrace>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

static TRACE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Drain one sample window for an exact process. The caller must have just
/// discovered `modules` through an identity-checked process read; this method
/// reuses those current ranges only for attribution and never starts a trace
/// from a PID alone.
pub(crate) fn sample_process_module_code(
    role: TraceRole,
    pid: u32,
    expected_start_epoch_s: i64,
    modules: &[ProcessModule],
) -> Result<Option<ProcessModuleCodeWindow>> {
    let mut slot = match active_trace().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    let same_target = slot.as_ref().is_some_and(|active| {
        active.role == role
            && active.pid == pid
            && active.expected_start_epoch_s == expected_start_epoch_s
            && active.trace.is_running()
    });
    if same_target {
        let active = slot.as_mut().expect("same_target checked above");
        active.last_request = Instant::now();
        return active.trace.take_window(modules).map(Some);
    }

    // A different process, a recycled PID, a role change, or an ETW session
    // that ended underneath us: replace the trace. Drop stops the old fixed
    // session before the new one tries to claim it.
    *slot = None;
    let Some(trace) = ProcessModuleTrace::start(role, pid) else {
        return Ok(None);
    };
    let generation = TRACE_GENERATION.fetch_add(1, Ordering::Relaxed);
    let mut active = ActiveTrace {
        trace,
        role,
        pid,
        expected_start_epoch_s,
        generation,
        last_request: Instant::now(),
    };
    let window = active.trace.take_window(modules)?;
    *slot = Some(active);
    spawn_trace_watchdog(generation);
    Ok(Some(window))
}

fn spawn_trace_watchdog(generation: u64) {
    let _ = std::thread::Builder::new()
        .name("tm-profile-idle".into())
        .spawn(move || {
            loop {
                let wait = {
                    let mut slot = match active_trace().lock() {
                        Ok(slot) => slot,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let Some(active) = slot.as_ref() else {
                        return;
                    };
                    if active.generation != generation {
                        return;
                    }
                    let idle = active.last_request.elapsed();
                    if idle >= PROFILE_TRACE_IDLE {
                        *slot = None;
                        tracing::info!("per-process module code-sample trace stopped (idle)");
                        return;
                    }
                    PROFILE_TRACE_IDLE - idle
                };
                std::thread::sleep(wait);
            }
        });
}

fn session_name(role: TraceRole) -> &'static str {
    match role {
        TraceRole::Service => "TaskMan-Profile-Service",
        TraceRole::App => "TaskMan-Profile-App",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, base: u64, size: u64) -> ProcessModule {
        ProcessModule {
            name: name.into(),
            path: format!(r"C:\Test\{name}"),
            base_address: base,
            size_bytes: size,
        }
    }

    fn sample_payload(pointer_bytes: usize, ip: u64, tid: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        if pointer_bytes == 8 {
            payload.extend_from_slice(&ip.to_le_bytes());
        } else {
            payload.extend_from_slice(&(ip as u32).to_le_bytes());
        }
        payload.extend_from_slice(&tid.to_le_bytes());
        payload.extend_from_slice(&[0; 12]); // count/priority/flags/rank
        payload
    }

    #[test]
    fn sampled_profile_decodes_both_pointer_widths() {
        assert_eq!(
            parse_sampled_profile(&sample_payload(8, 0x7FF0_1122_3344, 0x1234), 8),
            Some(SampledProfile {
                instruction_pointer: 0x7FF0_1122_3344,
                thread_id: 0x1234,
            })
        );
        assert_eq!(
            parse_sampled_profile(&sample_payload(4, 0x1122_3344, 0x1234), 4),
            Some(SampledProfile {
                instruction_pointer: 0x1122_3344,
                thread_id: 0x1234,
            })
        );
        assert_eq!(parse_sampled_profile(&[0; 20], 8), None);
    }

    #[test]
    fn sampled_profile_is_routed_by_its_mof_opcode() {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(42, HashSet::from([0x1234]))),
            live: AtomicBool::new(true),
        });
        let context = Arc::into_raw(Arc::clone(&shared));
        let mut payload = sample_payload(8, 0x7FF0_1122_3344, 0x1234);
        let mut record = EVENT_RECORD {
            EventHeader: windows::Win32::System::Diagnostics::Etw::EVENT_HEADER {
                ProviderId: PerfInfoGuid,
                ProcessId: 42,
                ..Default::default()
            },
            UserContext: context.cast_mut().cast(),
            UserData: payload.as_mut_ptr().cast(),
            UserDataLength: payload.len() as u16,
            ..Default::default()
        };
        record.EventHeader.EventDescriptor.Opcode = SAMPLE_PROFILE_EVENT_TYPE;
        record.EventHeader.EventDescriptor.Id = 0;
        unsafe {
            on_event(&mut record);
        }
        assert_eq!(
            shared.state.lock().unwrap().pages.get(&0x7FF0_1122_3000),
            Some(&1)
        );

        // A manifest-style Id is not the MOF event type and must not be
        // mistaken for a sample.
        record.EventHeader.EventDescriptor.Opcode = 0;
        record.EventHeader.EventDescriptor.Id = u16::from(SAMPLE_PROFILE_EVENT_TYPE);
        unsafe {
            on_event(&mut record);
        }
        assert_eq!(shared.state.lock().unwrap().events_seen, 1);
        unsafe {
            drop(Arc::from_raw(context));
        }
    }

    #[test]
    fn pages_are_attributed_at_exact_module_boundaries() {
        let modules = vec![
            module("first.dll", 0x1000, 0x1000),
            module("second.dll", 0x2000, 0x1000),
            module("third.dll", 0x3000, 0x1000),
        ];
        let pages = HashMap::from([
            (0x1000, 2),
            (0x1000 + 0x900, 3), // still first.dll
            (0x2000, 5),         // second.dll start (inclusive)
            (0x2FF0, 7),         // second.dll
            (0x3000, 11),        // third.dll start
            (0x3FF0, 13),        // third.dll final page
            (0x4000, 17),        // one page beyond every range
        ]);
        let (attributed, entries) = samples_by_module(&pages, &modules);
        assert_eq!(attributed, 41);
        assert_eq!(
            entries,
            vec![
                ProcessModuleCodeSamples {
                    base_address: 0x1000,
                    samples: 5,
                },
                ProcessModuleCodeSamples {
                    base_address: 0x2000,
                    samples: 12,
                },
                ProcessModuleCodeSamples {
                    base_address: 0x3000,
                    samples: 24,
                },
            ]
        );
    }

    #[test]
    fn only_target_threads_or_an_exact_target_header_are_charged() {
        let shared = Shared {
            state: Mutex::new(State::new(42, HashSet::from([100, 104]))),
            live: AtomicBool::new(true),
        };
        let sample = |ip, tid| {
            Some(SampledProfile {
                instruction_pointer: ip,
                thread_id: tid,
            })
        };
        shared.record_sample(0, sample(0x1000, 100), 20); // known target thread
        shared.record_sample(42, sample(0x2000, 108), 20); // missed thread start
        shared.record_sample(0, sample(0x3000, 112), 20); // unrelated process
        let state = shared.state.lock().unwrap();
        assert_eq!(state.events_seen, 3);
        assert_eq!(state.events_decoded, 3);
        assert_eq!(state.pages.get(&0x1000), Some(&1));
        assert_eq!(state.pages.get(&0x2000), Some(&1));
        assert!(!state.pages.contains_key(&0x3000));
        assert!(state.target_threads.contains(&108));
    }

    #[test]
    fn thread_rundown_follows_the_target_process() {
        let shared = Shared {
            state: Mutex::new(State::new(42, HashSet::new())),
            live: AtomicBool::new(true),
        };
        shared.track_thread(42, 100);
        shared.track_thread(84, 100);
        shared.track_thread(42, 104);
        assert_eq!(
            shared.state.lock().unwrap().target_threads,
            HashSet::from([104])
        );
        shared.forget_thread(104);
        assert!(shared.state.lock().unwrap().target_threads.is_empty());
    }

    #[test]
    fn undecodable_records_are_counted_but_not_charged() {
        let shared = Shared {
            state: Mutex::new(State::new(42, HashSet::from([100]))),
            live: AtomicBool::new(true),
        };
        shared.record_sample(42, None, 7);
        let state = shared.state.lock().unwrap();
        assert_eq!(state.events_seen, 1);
        assert_eq!(state.events_decoded, 0);
        assert_eq!(state.rejected_payload_len, 7);
        assert!(state.pages.is_empty());
    }

    #[test]
    fn the_page_budget_turns_an_overflowing_window_into_an_error() {
        let mut state = State::new(42, HashSet::from([100]));
        for index in 0..MAX_SAMPLE_PAGES {
            state.pages.insert((index as u64) * 0x1000, 1);
        }
        state.record_sample(
            42,
            Some(SampledProfile {
                instruction_pointer: (MAX_SAMPLE_PAGES as u64 + 1) * 0x1000,
                thread_id: 100,
            }),
        );
        assert!(state.overflowed);
    }

    #[test]
    fn every_profile_session_name_is_known_to_crash_teardown() {
        for role in [TraceRole::Service, TraceRole::App] {
            let name = session_name(role);
            assert!(
                crate::win::etw::SESSION_NAMES.contains(&name),
                "{name} is missing from etw::SESSION_NAMES"
            );
            assert!(name.contains("Profile"));
        }
        assert_ne!(
            session_name(TraceRole::Service),
            session_name(TraceRole::App)
        );
    }
}
