//! Per-process TCP throughput from `NETLINK_SOCK_DIAG` and socket ownership.
//!
//! The decoding and the per-socket accountant live in the platform-neutral
//! [`crate::linux_net_accounting`]; this module owns the netlink socket and
//! the `/proc/<pid>/fd` walk that joins a socket inode to its process.
//!
//! Ownership is cached: the descriptor walk only runs when a socket carrying
//! traffic has an inode no known process holds (a new connection), so a steady
//! state costs two netlink dumps per tick and no procfs walk at all.

use super::process_connections::process_socket_inodes;
use crate::linux_net_accounting::{self as accounting, TcpAccountant, TcpSocket};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A dump that does not finish within this long is abandoned for the tick.
const RECV_TIMEOUT_MS: libc::suseconds_t = 500_000;

/// Bytes each process received and sent in the last window.
pub(super) type Window = HashMap<u32, (u64, u64)>;

#[derive(Default)]
pub(super) struct ProcessNet {
    accountant: TcpAccountant,
    /// Socket inode → owning pid, from the last descriptor walk.
    owners: HashMap<u64, u32>,
    sequence: u32,
    failure_logged: bool,
}

impl ProcessNet {
    /// The window's per-process TCP bytes, or `None` when the window cannot be
    /// measured (first dump, or the dump failed) — every process is then
    /// unmeasured, not idle.
    pub(super) fn sample(&mut self, live_pids: &HashSet<u32>) -> Option<Window> {
        let sockets = match self.dump() {
            Ok(sockets) => sockets,
            Err(error) => {
                if !std::mem::replace(&mut self.failure_logged, true) {
                    tracing::warn!(%error, "sock_diag TCP dump failed; per-process network is unavailable");
                }
                self.accountant.forget();
                return None;
            }
        };
        let deltas = self.accountant.window(&sockets)?;
        let (mut per_pid, unowned) = accounting::attribute(&deltas, &self.owners);
        if !unowned.is_empty() {
            self.rescan_owners(live_pids, &sockets);
            let late: Vec<_> = deltas
                .iter()
                .filter(|delta| unowned.contains(&delta.inode))
                .copied()
                .collect();
            let (found, _) = accounting::attribute(&late, &self.owners);
            for (pid, (received, sent)) in found {
                let slot = per_pid.entry(pid).or_default();
                slot.0 = slot.0.saturating_add(received);
                slot.1 = slot.1.saturating_add(sent);
            }
        }
        Some(per_pid)
    }

    /// Stop measuring: the next sample starts a fresh window.
    pub(super) fn forget(&mut self) {
        self.accountant.forget();
        self.owners.clear();
    }

    /// Re-learn which process holds each live TCP socket.
    ///
    /// Only descriptors this token may read are visible, so other users'
    /// processes stay unattributed (unless TaskMan runs as root).
    fn rescan_owners(&mut self, live_pids: &HashSet<u32>, sockets: &[TcpSocket]) {
        let wanted: HashSet<u64> = sockets.iter().map(|socket| socket.inode).collect();
        let mut owners = HashMap::with_capacity(wanted.len());
        let mut pids: Vec<u32> = live_pids.iter().copied().collect();
        // Lowest pid first, so a socket inherited across fork is attributed
        // deterministically (to the parent that normally created it).
        pids.sort_unstable();
        for pid in pids {
            let Ok(inodes) = process_socket_inodes(pid) else {
                continue;
            };
            for inode in inodes {
                if wanted.contains(&inode) {
                    owners.entry(inode).or_insert(pid);
                }
            }
        }
        self.owners = owners;
    }

    fn dump(&mut self) -> std::io::Result<Vec<TcpSocket>> {
        let socket = open_sock_diag()?;
        let mut sockets = Vec::new();
        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            self.sequence = self.sequence.wrapping_add(1);
            let request = accounting::tcp_dump_request(family, self.sequence);
            // SAFETY: `socket` is a live netlink socket and `request` a fully
            // initialized buffer of the length passed.
            let sent = unsafe {
                libc::send(
                    socket.as_raw_fd(),
                    request.as_ptr().cast(),
                    request.len(),
                    0,
                )
            };
            if sent < 0 {
                return Err(std::io::Error::last_os_error());
            }
            receive_dump(&socket, &mut sockets)?;
        }
        Ok(sockets)
    }
}

fn open_sock_diag() -> std::io::Result<OwnedFd> {
    // SAFETY: no pointer arguments; returns an owned descriptor or -1.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor owned by nobody else.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    let timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: RECV_TIMEOUT_MS,
    };
    // SAFETY: `timeout` is a valid `timeval` for the duration of the call.
    let set = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&timeout as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if set < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(socket)
}

fn receive_dump(socket: &OwnedFd, out: &mut Vec<TcpSocket>) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: `buffer` is writable for its whole length.
        let received = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if received < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if received == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "sock_diag closed the dump early",
            ));
        }
        let chunk = accounting::decode_dump(&buffer[..received as usize]);
        out.extend(chunk.sockets);
        if let Some(code) = chunk.error {
            return Err(std::io::Error::from_raw_os_error(-code));
        }
        if chunk.done {
            return Ok(());
        }
    }
}
