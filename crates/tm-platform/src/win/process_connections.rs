//! On-demand TCP/UDP endpoint list of one process (Process Properties →
//! Network), from the IP Helper owner-PID tables. These need no process
//! handle and no privilege, so they cover service and SYSTEM processes too.

use crate::actions::{ConnectionProtocol, ProcessConnection, TcpState};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use tm_core::error::{Result, TmError};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
    MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

/// Upper bound for one table. A host with this many sockets has bigger
/// problems than an incomplete inspector; refusing beats unbounded growth.
const MAX_TABLE_BYTES: usize = 64 * 1024 * 1024;

pub fn list_process_connections(pid: u32) -> Result<Vec<ProcessConnection>> {
    let mut out = Vec::new();
    for row in table::<MIB_TCPROW_OWNER_PID>(Kind::Tcp4)? {
        if row.dwOwningPid == pid {
            let state = TcpState::from_raw(row.dwState);
            out.push(ProcessConnection {
                protocol: ConnectionProtocol::Tcp,
                local: v4(row.dwLocalAddr, row.dwLocalPort),
                remote: remote_of(state, v4(row.dwRemoteAddr, row.dwRemotePort)),
                state: Some(state),
            });
        }
    }
    for row in table::<MIB_TCP6ROW_OWNER_PID>(Kind::Tcp6)? {
        if row.dwOwningPid == pid {
            let state = TcpState::from_raw(row.dwState);
            out.push(ProcessConnection {
                protocol: ConnectionProtocol::Tcp6,
                local: v6(row.ucLocalAddr, row.dwLocalPort, row.dwLocalScopeId),
                remote: remote_of(
                    state,
                    v6(row.ucRemoteAddr, row.dwRemotePort, row.dwRemoteScopeId),
                ),
                state: Some(state),
            });
        }
    }
    for row in table::<MIB_UDPROW_OWNER_PID>(Kind::Udp4)? {
        if row.dwOwningPid == pid {
            out.push(ProcessConnection {
                protocol: ConnectionProtocol::Udp,
                local: v4(row.dwLocalAddr, row.dwLocalPort),
                remote: None,
                state: None,
            });
        }
    }
    for row in table::<MIB_UDP6ROW_OWNER_PID>(Kind::Udp6)? {
        if row.dwOwningPid == pid {
            out.push(ProcessConnection {
                protocol: ConnectionProtocol::Udp6,
                local: v6(row.ucLocalAddr, row.dwLocalPort, row.dwLocalScopeId),
                remote: None,
                state: None,
            });
        }
    }
    Ok(out)
}

/// A listener's "remote" is the all-zero placeholder, not a peer.
fn remote_of(state: TcpState, remote: SocketAddr) -> Option<SocketAddr> {
    (state != TcpState::Listen).then_some(remote)
}

/// Ports are stored in network byte order in the low 16 bits.
fn port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// IPv4 addresses are stored in network byte order, i.e. their in-memory
/// bytes are the dotted quad.
fn v4(addr: u32, raw_port: u32) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::from(addr.to_ne_bytes())),
        port(raw_port),
    )
}

fn v6(addr: [u8; 16], raw_port: u32, scope: u32) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::from(addr),
        port(raw_port),
        0,
        scope,
    ))
}

#[derive(Clone, Copy)]
enum Kind {
    Tcp4,
    Tcp6,
    Udp4,
    Udp6,
}

/// Fetch one owner-PID table as typed rows. The table layout is
/// `{ DWORD dwNumEntries; ROW table[]; }`, with the rows aligned to their own
/// alignment after the count.
fn table<Row: Copy>(kind: Kind) -> Result<Vec<Row>> {
    let call = |buf: Option<*mut core::ffi::c_void>, size: &mut u32| unsafe {
        match kind {
            Kind::Tcp4 => GetExtendedTcpTable(
                buf,
                size,
                false,
                AF_INET.0.into(),
                TCP_TABLE_OWNER_PID_ALL,
                0,
            ),
            Kind::Tcp6 => GetExtendedTcpTable(
                buf,
                size,
                false,
                AF_INET6.0.into(),
                TCP_TABLE_OWNER_PID_ALL,
                0,
            ),
            Kind::Udp4 => {
                GetExtendedUdpTable(buf, size, false, AF_INET.0.into(), UDP_TABLE_OWNER_PID, 0)
            }
            Kind::Udp6 => {
                GetExtendedUdpTable(buf, size, false, AF_INET6.0.into(), UDP_TABLE_OWNER_PID, 0)
            }
        }
    };
    // u64 storage keeps the buffer 8-byte aligned for every row type.
    let mut storage: Vec<u64> = Vec::new();
    let mut size = 0u32;
    loop {
        let status = call(
            (!storage.is_empty()).then(|| storage.as_mut_ptr().cast()),
            &mut size,
        );
        if status == NO_ERROR.0 {
            break;
        }
        if status != ERROR_INSUFFICIENT_BUFFER.0 || size as usize > MAX_TABLE_BYTES {
            return Err(TmError::platform(
                "GetExtendedTcpTable/GetExtendedUdpTable",
                std::io::Error::from_raw_os_error(status as i32).to_string(),
            ));
        }
        // The table can grow between the size probe and the fetch; the
        // headroom keeps that race from looping.
        let words = (size as usize + 1024).div_ceil(8);
        storage.resize(words, 0);
        size = (words * 8) as u32;
    }
    let bytes = (size as usize).min(storage.len() * 8);
    if bytes < 4 {
        return Ok(Vec::new());
    }
    let base = storage.as_ptr().cast::<u8>();
    let count = unsafe { base.cast::<u32>().read_unaligned() } as usize;
    let first = 4usize.next_multiple_of(std::mem::align_of::<Row>());
    let fits = count
        .checked_mul(std::mem::size_of::<Row>())
        .and_then(|rows| rows.checked_add(first))
        .is_some_and(|end| end <= bytes);
    if !fits {
        return Err(TmError::platform(
            "GetExtendedTcpTable/GetExtendedUdpTable",
            "table larger than its buffer",
        ));
    }
    Ok((0..count)
        .map(|index| unsafe {
            base.add(first + index * std::mem::size_of::<Row>())
                .cast::<Row>()
                .read_unaligned()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_order_decodes_like_the_wire() {
        // 127.0.0.1:8080 as the table stores it.
        let addr = u32::from_ne_bytes([127, 0, 0, 1]);
        let raw_port = u32::from(8080u16.to_be());
        assert_eq!(v4(addr, raw_port), "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn listeners_have_no_remote_peer() {
        let zero = v4(0, 0);
        assert_eq!(remote_of(TcpState::Listen, zero), None);
        assert_eq!(remote_of(TcpState::Established, zero), Some(zero));
    }

    /// Live: a listener this test owns must be attributed to this process
    /// with its exact port and state.
    #[test]
    fn live_table_attributes_an_own_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let local = listener.local_addr().unwrap();
        let mine = list_process_connections(std::process::id()).expect("table");
        assert!(
            mine.iter().any(|c| c.protocol == ConnectionProtocol::Tcp
                && c.local == local
                && c.state == Some(TcpState::Listen)
                && c.remote.is_none()),
            "own listener {local} missing from {mine:?}"
        );
        let socket = std::net::UdpSocket::bind("[::1]:0").expect("bind udp6");
        let udp = socket.local_addr().unwrap();
        let mine = list_process_connections(std::process::id()).expect("table");
        assert!(
            mine.iter()
                .any(|c| c.protocol == ConnectionProtocol::Udp6 && c.local.port() == udp.port()),
            "own UDP endpoint {udp} missing from {mine:?}"
        );
    }
}
