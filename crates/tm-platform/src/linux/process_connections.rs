//! TCP/UDP endpoints owned by one process from its socket file descriptors
//! and its network namespace's `/proc/<pid>/net` tables.

use crate::actions::{ConnectionProtocol, ProcessConnection, TcpState};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use tm_core::error::{Result, TmError};

const TABLES: [(&str, ConnectionProtocol); 4] = [
    ("tcp", ConnectionProtocol::Tcp),
    ("tcp6", ConnectionProtocol::Tcp6),
    ("udp", ConnectionProtocol::Udp),
    ("udp6", ConnectionProtocol::Udp6),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcNetSocket {
    inode: u64,
    connection: ProcessConnection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NetNamespace {
    device: u64,
    inode: u64,
}

fn net_namespace(pid: u32) -> Result<NetNamespace> {
    let path = format!("/proc/{pid}/ns/net");
    let metadata = std::fs::metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TmError::ProcessNotFound { pid }
        } else {
            TmError::platform("read /proc/pid/ns/net", format!("{path}: {error}"))
        }
    })?;
    Ok(NetNamespace {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn socket_inode_link(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn process_socket_inodes(pid: u32) -> Result<HashSet<u64>> {
    let fd_path = format!("/proc/{pid}/fd");
    let entries = std::fs::read_dir(&fd_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TmError::ProcessNotFound { pid }
        } else {
            TmError::platform("read /proc/pid/fd", format!("{fd_path}: {error}"))
        }
    })?;
    let mut inodes = HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            TmError::platform("enumerate /proc/pid/fd", format!("{fd_path}: {error}"))
        })?;
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(TmError::platform(
                    "read /proc/pid/fd link",
                    format!("{}: {error}", entry.path().display()),
                ));
            }
        };
        if let Some(inode) = target.to_str().and_then(socket_inode_link) {
            inodes.insert(inode);
        }
    }
    Ok(inodes)
}

fn parse_ipv4(hex: &str) -> Option<Ipv4Addr> {
    if hex.len() != 8 {
        return None;
    }
    Some(Ipv4Addr::from(
        u32::from_str_radix(hex, 16)?.to_le_bytes(),
    ))
}

fn parse_ipv6(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut octets = [0u8; 16];
    for (index, chunk) in hex.as_bytes().chunks_exact(8).enumerate() {
        let word = std::str::from_utf8(chunk).ok()?;
        octets[index * 4..index * 4 + 4]
            .copy_from_slice(&u32::from_str_radix(word, 16)?.to_le_bytes());
    }
    Some(Ipv6Addr::from(octets))
}

fn parse_proc_address(value: &str, ipv6: bool) -> Option<(IpAddr, u16)> {
    let (address, port) = value.split_once(':')?;
    let ip = if ipv6 {
        IpAddr::V6(parse_ipv6(address)?)
    } else {
        IpAddr::V4(parse_ipv4(address)?)
    };
    Some((ip, u16::from_str_radix(port, 16).ok()?))
}

fn linux_tcp_state(raw: u32) -> TcpState {
    match raw {
        1 => TcpState::Established,
        2 => TcpState::SynSent,
        3 | 12 => TcpState::SynReceived,
        4 => TcpState::FinWait1,
        5 => TcpState::FinWait2,
        6 => TcpState::TimeWait,
        7 => TcpState::Closed,
        8 => TcpState::CloseWait,
        9 => TcpState::LastAck,
        10 => TcpState::Listen,
        11 => TcpState::Closing,
        other => TcpState::Other(other),
    }
}

fn remote_of(state: TcpState, remote: SocketAddr) -> Option<SocketAddr> {
    (state != TcpState::Listen && remote.port() != 0 && !remote.ip().is_unspecified())
        .then_some(remote)
}

/// Parse one `/proc/<pid>/net/{tcp,tcp6,udp,udp6}` table. Address words are
/// stored in host byte order in the kernel's textual representation, which is
/// little-endian on every Linux target this project ships.
fn parse_net_table<R: BufRead>(
    reader: R,
    protocol: ConnectionProtocol,
    inodes: Option<&HashSet<u64>>,
) -> Vec<ProcNetSocket> {
    let ipv6 = matches!(protocol, ConnectionProtocol::Tcp6 | ConnectionProtocol::Udp6);
    let tcp = matches!(protocol, ConnectionProtocol::Tcp | ConnectionProtocol::Tcp6);
    let mut sockets = Vec::new();
    for line in reader.lines().skip(1) {
        let Ok(line) = line else {
            continue;
        };
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() < 10 {
            continue;
        }
        let Some((local_ip, local_port)) = parse_proc_address(fields[1], ipv6) else {
            continue;
        };
        let Ok(inode) = fields[9].parse::<u64>() else {
            continue;
        };
        if inode == 0 {
            continue;
        }
        if inodes.is_some_and(|inodes| !inodes.contains(&inode)) {
            continue;
        }
        let state = if tcp {
            let Ok(raw) = u32::from_str_radix(fields[3], 16) else {
                continue;
            };
            Some(linux_tcp_state(raw))
        } else {
            None
        };
        let remote = match state {
            Some(state) => {
                let Some((ip, port)) = parse_proc_address(fields[2], ipv6) else {
                    continue;
                };
                let remote = SocketAddr::new(ip, port);
                let Some(remote) = remote_of(state, remote) else {
                    // Listeners and placeholder peers are the only TCP rows
                    // allowed to omit a peer. An established row with a
                    // malformed address must not masquerade as a listener.
                    if state == TcpState::Listen {
                        None
                    } else {
                        continue;
                    }
                };
                Some(remote)
            }
            None => None,
        };
        sockets.push(ProcNetSocket {
            inode,
            connection: ProcessConnection {
                protocol,
                local: SocketAddr::new(local_ip, local_port),
                remote,
                state,
            },
        });
    }
    sockets
}

fn connection_protocol_rank(protocol: ConnectionProtocol) -> u8 {
    match protocol {
        ConnectionProtocol::Tcp => 0,
        ConnectionProtocol::Tcp6 => 1,
        ConnectionProtocol::Udp => 2,
        ConnectionProtocol::Udp6 => 3,
    }
}

pub(super) fn list_process_connections(pid: u32) -> Result<Vec<ProcessConnection>> {
    let namespace_before = net_namespace(pid)?;
    let inodes_before = process_socket_inodes(pid)?;
    if inodes_before.is_empty() {
        return Ok(Vec::new());
    }

    let mut sockets = Vec::new();
    let mut seen_inodes = HashSet::new();
    for (table_name, protocol) in TABLES {
        let path = format!("/proc/{pid}/net/{table_name}");
        let table = match std::fs::File::open(&path) {
            Ok(table) => table,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(TmError::platform(
                    "read /proc/pid/net",
                    format!("{path}: {error}"),
                ));
            }
        };
        for socket in parse_net_table(BufReader::new(table), protocol, Some(&inodes_before)) {
            if seen_inodes.insert(socket.inode) {
                sockets.push(socket);
            }
        }
    }
    let namespace_after = net_namespace(pid)?;
    let inodes_after = process_socket_inodes(pid)?;
    if namespace_before != namespace_after {
        return Err(TmError::platform(
            "list process connections",
            "the process moved to another network namespace during inspection",
        ));
    }
    let stable: HashSet<u64> = inodes_before
        .intersection(&inodes_after)
        .copied()
        .collect();
    sockets.retain(|socket| {
        // The inode is the kernel socket object identity while it is live.
        // Retaining only sockets present before and after the table scan
        // prevents a descriptor reused during the walk from inheriting a
        // different endpoint.
        stable.contains(&socket.inode)
    });
    let mut connections: Vec<ProcessConnection> =
        sockets.into_iter().map(|socket| socket.connection).collect();
    connections.sort_by(|a, b| {
        connection_protocol_rank(a.protocol)
            .cmp(&connection_protocol_rank(b.protocol))
            .then_with(|| a.local.cmp(&b.local))
            .then_with(|| a.remote.cmp(&b.remote))
    });
    Ok(connections)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    #[test]
    fn proc_net_decodes_ipv4_ports_and_tcp_states() {
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 0200000A:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 4242 1\n\
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4243 1\n";
        let rows = parse_net_table(Cursor::new(text), ConnectionProtocol::Tcp, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].connection.local, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(rows[0].connection.remote, Some("10.0.0.2:443".parse().unwrap()));
        assert_eq!(rows[0].connection.state, Some(TcpState::Established));
        assert_eq!(rows[1].connection.state, Some(TcpState::Listen));
        assert_eq!(rows[1].connection.remote, None);
    }

    #[test]
    fn proc_net_decodes_ipv6_words_and_placeholder_peers() {
        let text = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n\
   0: 00000000000000000000000001000000:1F90 00000000000000000000000001000000:0050 01 00000000:00000000 00:00000000 00000000 1000 0 99 1\n\
   1: 00000000000000000000000001000000:1F91 00000000000000000000000000000000:0000 01 00000000:00000000 00:00000000 00000000 1000 0 100 1\n";
        let rows = parse_net_table(Cursor::new(text), ConnectionProtocol::Tcp6, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].connection.local, "[::1]:8080".parse().unwrap());
        assert_eq!(rows[0].connection.remote, Some("[::1]:80".parse().unwrap()));
        assert_eq!(rows[0].connection.state, Some(TcpState::Established));
        assert_eq!(rows[1].connection.remote, None);
    }

    #[test]
    fn udp_endpoints_have_no_remote_or_state() {
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 777 2\n";
        let rows = parse_net_table(Cursor::new(text), ConnectionProtocol::Udp, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].connection.local, "0.0.0.0:53".parse().unwrap());
        assert_eq!(rows[0].connection.remote, None);
        assert_eq!(rows[0].connection.state, None);
    }

    #[test]
    fn malformed_tcp_peer_is_omitted_instead_of_becoming_a_listener_shape() {
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 BADADDR:01BB 01 00000000:00000000 00:00000000 00000000  1000 0 4242 1\n";
        assert!(parse_net_table(
            Cursor::new(text),
            ConnectionProtocol::Tcp,
            None
        )
        .is_empty());
    }

    #[test]
    fn table_scan_filters_unrelated_socket_inodes_before_allocating_rows() {
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 0200000A:01BB 01 00000000:00000000 00:00000000 00000000  1000 0 4242 1\n\
   1: 0100007F:1F91 0200000A:01BB 01 00000000:00000000 00:00000000 00000000  1000 0 4243 1\n";
        let wanted = HashSet::from([4242]);
        let rows = parse_net_table(
            BufReader::new(Cursor::new(text)),
            ConnectionProtocol::Tcp,
            Some(&wanted),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].inode, 4242);
    }

    #[test]
    fn linux_tcp_states_include_new_syn_recv_and_unknown_values() {
        for (raw, expected) in [
            (1, TcpState::Established),
            (2, TcpState::SynSent),
            (3, TcpState::SynReceived),
            (4, TcpState::FinWait1),
            (5, TcpState::FinWait2),
            (6, TcpState::TimeWait),
            (7, TcpState::Closed),
            (8, TcpState::CloseWait),
            (9, TcpState::LastAck),
            (10, TcpState::Listen),
            (11, TcpState::Closing),
            (12, TcpState::SynReceived),
        ] {
            assert_eq!(linux_tcp_state(raw), expected, "state {raw:#x}");
        }
        assert_eq!(linux_tcp_state(99), TcpState::Other(99));
    }

    #[test]
    fn only_socket_links_name_an_inode() {
        assert_eq!(socket_inode_link("socket:[987]"), Some(987));
        assert_eq!(socket_inode_link("socket:[]"), None);
        assert_eq!(socket_inode_link("/tmp/file"), None);
    }

    #[test]
    fn current_process_connections_find_a_bound_tcp_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let local = listener.local_addr().expect("listener address");
        let connections =
            list_process_connections(std::process::id()).expect("current process connections");
        assert!(
            connections.iter().any(|connection| {
                connection.protocol == ConnectionProtocol::Tcp
                    && connection.local == local
                    && connection.remote.is_none()
                    && connection.state == Some(TcpState::Listen)
            }),
            "own listener {local} missing from {connections:?}"
        );
    }
}
