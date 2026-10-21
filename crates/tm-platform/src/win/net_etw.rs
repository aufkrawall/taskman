//! Per-process network bytes from an ETW real-time session.
//!
//! Windows exposes no per-process byte counter through a plain query API; the
//! numbers Task Manager shows come from kernel network events. This module
//! runs a private real-time session on `Microsoft-Windows-Kernel-Network` and
//! accumulates the `size` field of the TCP/UDP data events per payload PID.
//! Session lifetime, orphan reclamation and the privilege rules live in
//! [`super::etw`].
//!
//! ## The thing that is easy to get wrong here
//!
//! **The PID must come from the PAYLOAD, not the event header.** Kernel
//! network events are emitted from arbitrary (often System) context, so
//! `EventHeader.ProcessId` is not the owner of the traffic. Every one of the
//! data events starts with `PID: u32` followed by `size: u32`, for TCP and UDP
//! and for both address families.
//!
//! **Loopback traffic is not network traffic.** Next come `daddr` and `saddr`
//! (4 bytes for the v4 events, 16 for v6; field order per the provider
//! manifest's messages, "%2 bytes transmitted from %4:%6 to %3:%5"). Traffic
//! to 127.0.0.0/8 or ::1 never crosses an adapter, so counting it put a local
//! database or dev server at hundreds of MB/s under a network total of 0 %.
//! Traffic to the machine's own adapter address also stays inside the stack
//! and is not recognized here; that needs the adapter address list.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;
use windows::core::GUID;

use super::etw::{self, LoggerKind, Provider, Session, TraceContext};

pub use super::etw::TraceRole;

/// `Microsoft-Windows-Kernel-Network`.
const KERNEL_NETWORK_GUID: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);

/// Direction of one kernel network data event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Sent,
    Received,
}

/// Map an event id of `Microsoft-Windows-Kernel-Network` to a traffic
/// direction. Only the *data* events carry byte counts; connect/disconnect/
/// retransmit/ACK bookkeeping events are deliberately ignored so bytes are
/// never counted twice.
fn direction_of(event_id: u16) -> Option<Direction> {
    match event_id {
        // TCP v4 / TCP v6 / UDP v4 / UDP v6 "Datasent"
        10 | 26 | 42 | 58 => Some(Direction::Sent),
        // ...and the matching "Datareceived"
        11 | 27 | 43 | 59 => Some(Direction::Received),
        _ => None,
    }
}

/// Whether a data event's traffic stayed on this machine (loopback).
///
/// A payload too short to carry the destination is counted, as it was before
/// addresses were read: dropping it would hide traffic on a guess.
fn is_loopback(event_id: u16, payload: &[u8]) -> bool {
    let v6 = matches!(event_id, 26 | 27 | 58 | 59);
    if v6 {
        let Some(daddr) = payload.get(8..24) else {
            return false;
        };
        let ipv6_loopback = daddr[..15].iter().all(|&b| b == 0) && daddr[15] == 1;
        // ::ffff:127.x.x.x — v4 loopback through a dual-stack socket.
        let mapped_v4_loopback = daddr[..10].iter().all(|&b| b == 0)
            && daddr[10..12] == [0xff, 0xff]
            && daddr[12] == 127;
        ipv6_loopback || mapped_v4_loopback
    } else {
        // Network byte order: the first byte is the first octet.
        payload.get(8).is_some_and(|&octet| octet == 127)
    }
}

/// The `(pid, size)` prefix shared by every kernel network data event.
fn parse_pid_and_size(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 8 {
        return None;
    }
    let pid = u32::from_le_bytes(payload[0..4].try_into().ok()?);
    let size = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    Some((pid, size))
}

/// Cumulative bytes observed for one process since the session started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidBytes {
    pub received: u64,
    pub sent: u64,
}

/// Shared state between the ETW callback thread and the sampler.
struct Shared {
    totals: Mutex<HashMap<u32, PidBytes>>,
    /// Cleared when the session is torn down so a late callback cannot touch
    /// a map the sampler has already stopped reading.
    live: AtomicBool,
}

impl TraceContext for Shared {
    fn stop(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

impl Shared {
    fn record(&self, pid: u32, size: u32, dir: Direction) {
        if !self.live.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut totals) = self.totals.lock() else {
            return;
        };
        let entry = totals.entry(pid).or_default();
        match dir {
            Direction::Sent => entry.sent = entry.sent.saturating_add(u64::from(size)),
            Direction::Received => entry.received = entry.received.saturating_add(u64::from(size)),
        }
    }
}

/// A running per-process network trace.
pub struct NetworkUsage {
    session: Session<Shared>,
}

impl NetworkUsage {
    /// Start a real-time session for `role`. Returns `None` when ETW is
    /// unavailable to this token (the common unelevated case), which keeps
    /// per-process network reported as unknown rather than zero.
    pub fn start(role: TraceRole) -> Option<Self> {
        let shared = Arc::new(Shared {
            totals: Mutex::new(HashMap::new()),
            live: AtomicBool::new(true),
        });
        let session = Session::start(
            session_name(role),
            LoggerKind::Manifest,
            &[Provider {
                guid: KERNEL_NETWORK_GUID,
                // 0 means "every event of this provider"; `direction_of` does
                // the real filtering.
                match_any_keyword: 0,
            }],
            shared,
            on_event,
            "tm-net-etw",
        )?;
        tracing::info!("per-process network trace started");
        Some(Self { session })
    }

    /// Whether events are still arriving; see [`Session::is_running`].
    pub fn is_running(&self) -> bool {
        self.session.is_running()
    }

    /// Current cumulative byte counters, exactly as observed.
    pub fn totals(&self) -> HashMap<u32, PidBytes> {
        match self.session.shared().totals.lock() {
            Ok(totals) => totals.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Current cumulative byte counters, pruned to the processes that still
    /// exist. Pruning here bounds the map and stops a recycled PID from
    /// inheriting a dead process's totals across a gap.
    ///
    /// An EMPTY `live_pids` means "the caller could not enumerate processes",
    /// never "nothing is alive": pruning against it would wipe every counter
    /// and report a system with no traffic at all.
    pub fn totals_pruned(
        &self,
        live_pids: &std::collections::HashSet<u32>,
    ) -> HashMap<u32, PidBytes> {
        let Ok(mut totals) = self.session.shared().totals.lock() else {
            return HashMap::new();
        };
        if live_pids.is_empty() {
            return totals.clone();
        }
        totals.retain(|pid, _| live_pids.contains(pid));
        totals.clone()
    }
}

/// ETW record callback. Kept minimal and allocation-free on the hot path.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(dir) = direction_of(record.EventHeader.EventDescriptor.Id) else {
        return;
    };
    let Some(shared) = (unsafe { etw::context_of::<Shared>(record) }) else {
        return;
    };
    let Some(payload) = (unsafe { etw::payload_of(record) }) else {
        return;
    };
    let Some((pid, size)) = parse_pid_and_size(payload) else {
        return;
    };
    if is_loopback(record.EventHeader.EventDescriptor.Id, payload) {
        return;
    }
    shared.record(pid, size, dir);
}

/// Fixed session name per role. See [`super::etw`]: this MUST NOT include the
/// pid, or an orphaned session can never be reclaimed.
fn session_name(role: TraceRole) -> &'static str {
    match role {
        TraceRole::Service => "TaskMan-Net-Service",
        TraceRole::App => "TaskMan-Net-App",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_data_events_carry_bytes() {
        // TCP/UDP, IPv4/IPv6 send and receive.
        for id in [10, 26, 42, 58] {
            assert_eq!(direction_of(id), Some(Direction::Sent), "id {id}");
        }
        for id in [11, 27, 43, 59] {
            assert_eq!(direction_of(id), Some(Direction::Received), "id {id}");
        }
        // Connect/disconnect/retransmit/ACK bookkeeping must not be counted,
        // or every retransmitted byte would be billed twice.
        for id in [0, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 44, 60] {
            assert_eq!(direction_of(id), None, "id {id} must be ignored");
        }
    }

    #[test]
    fn payload_prefix_is_pid_then_size() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4321u32.to_le_bytes());
        payload.extend_from_slice(&1460u32.to_le_bytes());
        payload.extend_from_slice(&[0xAA; 24]); // addresses/ports we ignore
        assert_eq!(parse_pid_and_size(&payload), Some((4321, 1460)));
    }

    fn v4_payload(daddr: [u8; 4], saddr: [u8; 4]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4321u32.to_le_bytes());
        payload.extend_from_slice(&1460u32.to_le_bytes());
        payload.extend_from_slice(&daddr);
        payload.extend_from_slice(&saddr);
        payload.extend_from_slice(&443u16.to_be_bytes());
        payload.extend_from_slice(&50000u16.to_be_bytes());
        payload.extend_from_slice(&[0; 16]); // seqnum, connid, ...
        payload
    }

    fn v6_payload(daddr: [u8; 16]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&4321u32.to_le_bytes());
        payload.extend_from_slice(&1460u32.to_le_bytes());
        payload.extend_from_slice(&daddr);
        payload.extend_from_slice(&daddr); // saddr
        payload.extend_from_slice(&[0; 20]);
        payload
    }

    /// Loopback never crosses an adapter, so it is not network traffic: a
    /// local dev server read hundreds of MB/s under a network total of 0 %.
    #[test]
    fn loopback_traffic_is_recognized_for_both_families() {
        for id in [10, 11, 42, 43] {
            assert!(is_loopback(id, &v4_payload([127, 0, 0, 1], [127, 0, 0, 1])));
            assert!(is_loopback(id, &v4_payload([127, 4, 5, 6], [127, 0, 0, 1])));
            assert!(!is_loopback(
                id,
                &v4_payload([192, 168, 1, 20], [192, 168, 1, 10])
            ));
            assert!(!is_loopback(id, &v4_payload([8, 8, 8, 8], [10, 0, 0, 2])));
        }
        let mut lo6 = [0u8; 16];
        lo6[15] = 1;
        let mut mapped = [0u8; 16];
        mapped[10..12].copy_from_slice(&[0xff, 0xff]);
        mapped[12..16].copy_from_slice(&[127, 0, 0, 1]);
        let mut public = [0u8; 16];
        public[..4].copy_from_slice(&[0x2a, 0x00, 0x14, 0x50]);
        public[15] = 1;
        for id in [26, 27, 58, 59] {
            assert!(is_loopback(id, &v6_payload(lo6)), "::1");
            assert!(is_loopback(id, &v6_payload(mapped)), "::ffff:127.0.0.1");
            assert!(!is_loopback(id, &v6_payload(public)));
            assert!(!is_loopback(id, &v6_payload([0; 16])), ":: is not loopback");
        }
        // Too short to carry an address: counted, never guessed away.
        assert!(!is_loopback(10, &4321u64.to_le_bytes()));
        assert!(!is_loopback(
            26,
            &v4_payload([127, 0, 0, 1], [127, 0, 0, 1])[..20]
        ));
    }

    #[test]
    fn truncated_payloads_are_rejected_instead_of_read() {
        assert_eq!(parse_pid_and_size(&[]), None);
        assert_eq!(parse_pid_and_size(&[1, 2, 3, 4, 5, 6, 7]), None);
    }

    #[test]
    fn totals_accumulate_per_direction_and_pid() {
        let shared = Shared {
            totals: Mutex::new(HashMap::new()),
            live: AtomicBool::new(true),
        };
        shared.record(7, 100, Direction::Sent);
        shared.record(7, 40, Direction::Sent);
        shared.record(7, 900, Direction::Received);
        shared.record(9, 5, Direction::Received);
        let totals = shared.totals.lock().unwrap();
        assert_eq!(
            totals[&7],
            PidBytes {
                received: 900,
                sent: 140
            }
        );
        assert_eq!(
            totals[&9],
            PidBytes {
                received: 5,
                sent: 0
            }
        );
    }

    /// A late callback after teardown must not resurrect the map.
    #[test]
    fn records_are_dropped_once_the_session_is_gone() {
        let shared = Shared {
            totals: Mutex::new(HashMap::new()),
            live: AtomicBool::new(false),
        };
        shared.record(7, 100, Direction::Sent);
        assert!(shared.totals.lock().unwrap().is_empty());
    }

    /// The two roles must never share a session name, or one host stopping its
    /// orphan would kill the other's live trace.
    /// The crash teardown stops sessions BY NAME from a fixed list, because a
    /// process that aborted cannot tell it what it had open. A rename here
    /// that does not reach that list leaves a kernel trace running on the
    /// user's machine after every crash, until they reboot.
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
            assert!(!session_name(role).contains(|c: char| c.is_ascii_digit()));
        }
    }
}
