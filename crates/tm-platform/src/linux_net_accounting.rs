//! Per-process network accounting for the Linux backend: decoding of
//! `NETLINK_SOCK_DIAG` TCP dumps and the per-socket byte-delta accountant.
//!
//! Linux keeps no per-process network counters. What it does keep, and hands
//! to any unprivileged caller through `sock_diag` (the interface behind
//! `ss -ti`), is a pair of lifetime byte counters on every TCP socket:
//! `tcpi_bytes_acked` (payload the peer acknowledged — sent) and
//! `tcpi_bytes_received`. Differencing those per socket and summing the
//! sockets each process owns gives a per-process TCP rate without tracing,
//! eBPF or root.
//!
//! What this cannot see, deliberately left unattributed rather than guessed:
//! UDP (QUIC/HTTP-3 included — the kernel keeps no UDP byte counters per
//! socket), traffic of sockets that closed inside a window after the previous
//! dump, and sockets of processes whose descriptors this token may not read.
//!
//! Platform-neutral on purpose (byte decoding and bookkeeping only) so its
//! tests run on every host.

use std::collections::HashMap;

/// `NLMSG_ERROR`, `NLMSG_DONE` from `<linux/netlink.h>`.
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
/// `SOCK_DIAG_BY_FAMILY` from `<linux/sock_diag.h>`.
pub const SOCK_DIAG_BY_FAMILY: u16 = 20;
/// `INET_DIAG_INFO`: the attribute carrying `struct tcp_info`.
const INET_DIAG_INFO: u16 = 2;
const NLMSG_HDR_LEN: usize = 16;
/// `struct inet_diag_msg`: family, state, timer, retrans (4) + sockid (48) +
/// expires, rqueue, wqueue, uid, inode (20).
const INET_DIAG_MSG_LEN: usize = 72;
/// Offsets inside `struct tcp_info` (`<linux/tcp.h>`, stable since 4.2).
const TCPI_BYTES_ACKED: usize = 120;
const TCPI_BYTES_RECEIVED: usize = 128;

/// One TCP socket's lifetime counters, as one dump reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpSocket {
    /// The kernel's per-socket cookie: unique for the socket's lifetime and
    /// never reused the way an inode number can be.
    pub cookie: u64,
    /// Socket inode, which is what `/proc/<pid>/fd/N -> socket:[inode]`
    /// names, i.e. how a socket is joined to the process holding it.
    pub inode: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Outcome of decoding one `recv` buffer of a dump.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DumpChunk {
    pub sockets: Vec<TcpSocket>,
    /// `NLMSG_DONE` seen: the dump is complete.
    pub done: bool,
    /// `NLMSG_ERROR` with a non-zero code: the dump failed.
    pub error: Option<i32>,
}

/// The `SOCK_DIAG_BY_FAMILY` dump request for every TCP socket of one address
/// family, asking for `tcp_info`.
pub fn tcp_dump_request(family: u8, sequence: u32) -> Vec<u8> {
    const NLM_F_REQUEST: u16 = 0x1;
    const NLM_F_DUMP: u16 = 0x300;
    const IPPROTO_TCP: u8 = 6;
    let total = NLMSG_HDR_LEN + 56;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_ne_bytes());
    out.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    out.extend_from_slice(&sequence.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    // inet_diag_req_v2: family, protocol, ext, pad, states, sockid (zeroed).
    out.push(family);
    out.push(IPPROTO_TCP);
    out.push(1 << (INET_DIAG_INFO - 1));
    out.push(0);
    out.extend_from_slice(&u32::MAX.to_ne_bytes());
    out.resize(total, 0);
    out
}

/// Decode every netlink message in one `recv` buffer.
///
/// A message that is truncated, or a socket record too short to hold the
/// fields read here, ends decoding of that buffer instead of producing
/// garbage counters. A socket whose `tcp_info` predates the byte counters
/// (pre-4.2 kernels) is skipped: it cannot be measured.
pub fn decode_dump(buffer: &[u8]) -> DumpChunk {
    let mut chunk = DumpChunk::default();
    let mut offset = 0usize;
    while offset + NLMSG_HDR_LEN <= buffer.len() {
        let length = u32_at(buffer, offset) as usize;
        let kind = u16_at(buffer, offset + 4);
        if length < NLMSG_HDR_LEN || offset + length > buffer.len() {
            break;
        }
        let body = &buffer[offset + NLMSG_HDR_LEN..offset + length];
        match kind {
            NLMSG_DONE => {
                chunk.done = true;
                break;
            }
            NLMSG_ERROR => {
                let code = if body.len() >= 4 {
                    i32::from_ne_bytes([body[0], body[1], body[2], body[3]])
                } else {
                    -1
                };
                if code != 0 {
                    chunk.error = Some(code);
                    chunk.done = true;
                    break;
                }
            }
            SOCK_DIAG_BY_FAMILY => {
                if let Some(socket) = decode_socket(body) {
                    chunk.sockets.push(socket);
                }
            }
            _ => {}
        }
        offset += align4(length);
    }
    chunk
}

fn decode_socket(body: &[u8]) -> Option<TcpSocket> {
    if body.len() < INET_DIAG_MSG_LEN {
        return None;
    }
    // sockid starts at 4: sport(2) dport(2) src(16) dst(16) if(4) cookie(8).
    let cookie_lo = u32_at(body, 4 + 40) as u64;
    let cookie_hi = u32_at(body, 4 + 44) as u64;
    let inode = u32_at(body, 68) as u64;
    let mut offset = INET_DIAG_MSG_LEN;
    while offset + 4 <= body.len() {
        let length = u16_at(body, offset) as usize;
        let kind = u16_at(body, offset + 2);
        if length < 4 || offset + length > body.len() {
            return None;
        }
        if kind == INET_DIAG_INFO {
            let info = &body[offset + 4..offset + length];
            if info.len() < TCPI_BYTES_RECEIVED + 8 {
                return None;
            }
            return Some(TcpSocket {
                cookie: cookie_lo | (cookie_hi << 32),
                inode,
                bytes_sent: u64_at(info, TCPI_BYTES_ACKED),
                bytes_received: u64_at(info, TCPI_BYTES_RECEIVED),
            });
        }
        offset += align4(length);
    }
    None
}

/// Bytes one socket moved in one window, joined to its inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketDelta {
    pub inode: u64,
    pub sent: u64,
    pub received: u64,
}

/// Per-socket byte deltas between consecutive dumps.
#[derive(Debug, Default)]
pub struct TcpAccountant {
    previous: HashMap<u64, (u64, u64)>,
    primed: bool,
}

impl TcpAccountant {
    /// The deltas since the previous dump, or `None` on the first dump (no
    /// window exists yet — a rate is unknown, not zero).
    ///
    /// A socket first seen after the first dump was opened inside this window,
    /// so its whole lifetime count belongs to it. A counter that went
    /// backwards cannot happen for one cookie; it is treated as no traffic
    /// rather than a wrapped huge value.
    pub fn window(&mut self, sockets: &[TcpSocket]) -> Option<Vec<SocketDelta>> {
        let mut next = HashMap::with_capacity(sockets.len());
        let mut deltas = Vec::new();
        for socket in sockets {
            let (sent_before, received_before) =
                self.previous.get(&socket.cookie).copied().unwrap_or((0, 0));
            let sent = socket.bytes_sent.saturating_sub(sent_before);
            let received = socket.bytes_received.saturating_sub(received_before);
            if sent > 0 || received > 0 {
                deltas.push(SocketDelta {
                    inode: socket.inode,
                    sent,
                    received,
                });
            }
            next.insert(socket.cookie, (socket.bytes_sent, socket.bytes_received));
        }
        self.previous = next;
        if !self.primed {
            self.primed = true;
            return None;
        }
        Some(deltas)
    }

    /// Drop the baseline, so the next dump starts a new window instead of
    /// attributing a whole off period to one tick.
    pub fn forget(&mut self) {
        self.previous.clear();
        self.primed = false;
    }
}

/// Sum socket deltas per owning process. Deltas whose inode no process is
/// known to hold are returned separately (their owner must be looked up).
pub fn attribute(
    deltas: &[SocketDelta],
    owners: &HashMap<u64, u32>,
) -> (HashMap<u32, (u64, u64)>, Vec<u64>) {
    let mut per_pid: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut unowned = Vec::new();
    for delta in deltas {
        match owners.get(&delta.inode) {
            Some(pid) => {
                let slot = per_pid.entry(*pid).or_default();
                slot.0 = slot.0.saturating_add(delta.received);
                slot.1 = slot.1.saturating_add(delta.sent);
            }
            None => unowned.push(delta.inode),
        }
    }
    (per_pid, unowned)
}

fn align4(length: usize) -> usize {
    (length + 3) & !3
}

fn u16_at(buffer: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes([buffer[offset], buffer[offset + 1]])
}

fn u32_at(buffer: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes([
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
    ])
}

fn u64_at(buffer: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buffer[offset..offset + 8]);
    u64::from_ne_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `SOCK_DIAG_BY_FAMILY` message as the kernel lays it out.
    fn socket_message(cookie: u64, inode: u32, sent: u64, received: u64) -> Vec<u8> {
        let mut body = vec![0u8; INET_DIAG_MSG_LEN];
        body[44..48].copy_from_slice(&(cookie as u32).to_ne_bytes());
        body[48..52].copy_from_slice(&((cookie >> 32) as u32).to_ne_bytes());
        body[68..72].copy_from_slice(&inode.to_ne_bytes());
        let mut info = vec![0u8; 232];
        info[TCPI_BYTES_ACKED..TCPI_BYTES_ACKED + 8].copy_from_slice(&sent.to_ne_bytes());
        info[TCPI_BYTES_RECEIVED..TCPI_BYTES_RECEIVED + 8].copy_from_slice(&received.to_ne_bytes());
        // An unrelated attribute first, to prove attributes are walked.
        body.extend_from_slice(&8u16.to_ne_bytes());
        body.extend_from_slice(&1u16.to_ne_bytes());
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&((info.len() + 4) as u16).to_ne_bytes());
        body.extend_from_slice(&INET_DIAG_INFO.to_ne_bytes());
        body.extend_from_slice(&info);
        message(SOCK_DIAG_BY_FAMILY, &body)
    }

    fn message(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((NLMSG_HDR_LEN + body.len()) as u32).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(&[0u8; 10]);
        out.extend_from_slice(body);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    #[test]
    fn a_dump_decodes_cookie_inode_and_both_byte_counters() {
        let mut buffer = socket_message(0x1_0000_0002, 4242, 1000, 5000);
        buffer.extend(socket_message(7, 99, 1, 2));
        buffer.extend(message(NLMSG_DONE, &[0u8; 4]));
        let chunk = decode_dump(&buffer);
        assert!(chunk.done);
        assert_eq!(chunk.error, None);
        assert_eq!(
            chunk.sockets,
            [
                TcpSocket {
                    cookie: 0x1_0000_0002,
                    inode: 4242,
                    bytes_sent: 1000,
                    bytes_received: 5000
                },
                TcpSocket {
                    cookie: 7,
                    inode: 99,
                    bytes_sent: 1,
                    bytes_received: 2
                },
            ]
        );
    }

    #[test]
    fn errors_and_truncation_never_produce_counters() {
        let error = message(NLMSG_ERROR, &(-13i32).to_ne_bytes());
        let chunk = decode_dump(&error);
        assert_eq!(chunk.error, Some(-13));
        assert!(chunk.done);

        let mut truncated = socket_message(1, 2, 3, 4);
        truncated.truncate(truncated.len() - 20);
        assert!(decode_dump(&truncated).sockets.is_empty());

        // A pre-4.2 tcp_info (no byte counters) is unmeasurable, not zero.
        let mut body = vec![0u8; INET_DIAG_MSG_LEN];
        body.extend_from_slice(&((104 + 4) as u16).to_ne_bytes());
        body.extend_from_slice(&INET_DIAG_INFO.to_ne_bytes());
        body.extend_from_slice(&[0u8; 104]);
        assert!(
            decode_dump(&message(SOCK_DIAG_BY_FAMILY, &body))
                .sockets
                .is_empty()
        );
    }

    #[test]
    fn the_request_asks_for_tcp_info_on_every_state() {
        let request = tcp_dump_request(2, 9);
        assert_eq!(request.len(), 72);
        assert_eq!(u32_at(&request, 0), 72);
        assert_eq!(u16_at(&request, 4), SOCK_DIAG_BY_FAMILY);
        assert_eq!(u32_at(&request, 8), 9);
        assert_eq!(&request[16..20], &[2, 6, 2, 0]);
        assert_eq!(u32_at(&request, 20), u32::MAX);
    }

    #[test]
    fn deltas_need_a_baseline_and_credit_sockets_born_in_the_window() {
        let socket = |cookie, inode, sent, received| TcpSocket {
            cookie,
            inode,
            bytes_sent: sent,
            bytes_received: received,
        };
        let mut accountant = TcpAccountant::default();
        assert_eq!(accountant.window(&[socket(1, 10, 500, 900)]), None);
        let deltas = accountant
            .window(&[socket(1, 10, 600, 900), socket(2, 20, 50, 70)])
            .unwrap();
        assert_eq!(
            deltas,
            [
                SocketDelta {
                    inode: 10,
                    sent: 100,
                    received: 0
                },
                SocketDelta {
                    inode: 20,
                    sent: 50,
                    received: 70
                },
            ]
        );
        // An idle socket produces no delta at all.
        assert!(
            accountant
                .window(&[socket(1, 10, 600, 900)])
                .unwrap()
                .is_empty()
        );
        accountant.forget();
        assert_eq!(accountant.window(&[socket(1, 10, 9_999, 9_999)]), None);
    }

    #[test]
    fn deltas_sum_per_owner_and_report_unknown_inodes() {
        let deltas = [
            SocketDelta {
                inode: 10,
                sent: 1,
                received: 2,
            },
            SocketDelta {
                inode: 11,
                sent: 3,
                received: 4,
            },
            SocketDelta {
                inode: 12,
                sent: 5,
                received: 6,
            },
        ];
        let owners: HashMap<u64, u32> = [(10, 100), (11, 100)].into();
        let (per_pid, unowned) = attribute(&deltas, &owners);
        assert_eq!(per_pid[&100], (6, 4));
        assert_eq!(unowned, [12]);
    }
}
