//! Per-adapter network facts the sampler can't get from sysinfo: hardware
//! description, negotiated link speed, oper status, unicast addresses and
//! Wi-Fi connection details.
//!
//! Sources: `GetAdaptersAddresses` (description/link/oper status, keyed by
//! the adapter's friendly name — the same name sysinfo exposes) and
//! `WlanQueryInterface` (SSID of the active wireless connection, joined via
//! the adapter GUID).

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr},
};

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces,
    WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, wlan_intf_opcode_current_connection,
};
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
};

#[derive(Debug, Clone, Default)]
pub struct AdapterInfo {
    /// Hardware/model description, e.g. "Intel(R) Wi-Fi 6 AX201 160MHz".
    pub desc: String,
    /// Negotiated link speed in bits/s (0 = unknown/down).
    pub link_bps: u64,
    pub oper_up: bool,
    /// SSID of the active wireless connection, when this is Wi-Fi.
    pub ssid: Option<String>,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
    pub signal_quality_pct: Option<u32>,
}

#[derive(Debug, Clone)]
struct WifiInfo {
    ssid: String,
    signal_quality_pct: u32,
}

/// FriendlyName → adapter facts. One `GetAdaptersAddresses` call plus (when a
/// wireless adapter exists) one WLAN enumeration. The sampler caches this
/// metadata because address and WLAN discovery do not belong on every tick.
pub fn adapters() -> HashMap<String, AdapterInfo> {
    let Some(buf) = adapter_addresses() else {
        return HashMap::new();
    };
    let ssids = wifi_ssids_by_guid();

    let mut out = HashMap::new();
    let mut cursor = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !cursor.is_null() {
        let adapter = unsafe { &*cursor };
        cursor = adapter.Next;
        let Ok(name) = (unsafe { adapter.FriendlyName.to_string() }) else {
            continue;
        };
        let desc = unsafe { adapter.Description.to_string() }.unwrap_or_default();
        let oper_up = adapter.OperStatus == IfOperStatusUp;
        // Prefer the transmit speed; fall back to receive for odd drivers.
        let link_bps = if adapter.TransmitLinkSpeed > 0 {
            adapter.TransmitLinkSpeed
        } else {
            adapter.ReceiveLinkSpeed
        };
        let wifi = ssids.get(&format!("{:?}", adapter.NetworkGuid).to_lowercase());
        let (ipv4, ipv6) = preferred_unicast_addresses(adapter.FirstUnicastAddress);
        out.insert(
            name,
            AdapterInfo {
                desc,
                link_bps,
                oper_up,
                ssid: wifi.map(|info| info.ssid.clone()),
                ipv4,
                ipv6,
                signal_quality_pct: wifi.map(|info| info.signal_quality_pct),
            },
        );
    }
    out
}

/// SSID per wireless interface, keyed by the lowercase debug form of the
/// interface GUID (matches `IP_ADAPTER_ADDRESSES_LH::NetworkGuid`).
fn wifi_ssids_by_guid() -> HashMap<String, WifiInfo> {
    unsafe {
        let mut handle = Default::default();
        let mut negotiated = 0u32;
        if WlanOpenHandle(2, None, &mut negotiated, &mut handle) != 0 {
            return HashMap::new();
        }
        let mut out = HashMap::new();
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(handle, None, &mut list) == 0 && !list.is_null() {
            let l = &*list;
            let items =
                std::slice::from_raw_parts(l.InterfaceInfo.as_ptr(), l.dwNumberOfItems as usize);
            for item in items {
                let mut size = 0u32;
                let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
                if WlanQueryInterface(
                    handle,
                    &item.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    None,
                    &mut size,
                    &mut data,
                    None,
                ) == 0
                    && !data.is_null()
                {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    // wlan_interface_state_connected == 1
                    if attrs.isState.0 == 1 {
                        let ssid = &attrs.wlanAssociationAttributes.dot11Ssid;
                        if ssid.uSSIDLength > 0 {
                            let bytes = &ssid.ucSSID[..(ssid.uSSIDLength as usize).min(32)];
                            out.insert(
                                format!("{:?}", item.InterfaceGuid).to_lowercase(),
                                WifiInfo {
                                    ssid: String::from_utf8_lossy(bytes).into_owned(),
                                    signal_quality_pct: attrs
                                        .wlanAssociationAttributes
                                        .wlanSignalQuality
                                        .min(100),
                                },
                            );
                        }
                    }
                    WlanFreeMemory(data);
                }
            }
            WlanFreeMemory(list as *const _ as *mut _);
        }
        let _ = WlanCloseHandle(handle, None);
        out
    }
}

fn preferred_unicast_addresses(
    mut cursor: *mut IP_ADAPTER_UNICAST_ADDRESS_LH,
) -> (Option<String>, Option<String>) {
    let mut ipv4 = None;
    let mut ipv4_fallback = None;
    let mut ipv6 = None;
    let mut ipv6_fallback = None;

    while !cursor.is_null() {
        // SAFETY: the linked list and pointed-to socket addresses remain owned
        // by the `GetAdaptersAddresses` buffer for the duration of this walk.
        let address = unsafe { &*cursor };
        cursor = address.Next;
        let socket = address.Address.lpSockaddr;
        let socket_len = usize::try_from(address.Address.iSockaddrLength).unwrap_or(0);
        if socket.is_null()
            || socket_len
                < std::mem::size_of::<windows::Win32::Networking::WinSock::ADDRESS_FAMILY>()
        {
            continue;
        }
        let family = unsafe { (*socket).sa_family };
        if family == AF_INET && socket_len >= std::mem::size_of::<SOCKADDR_IN>() {
            let sockaddr = unsafe { &*socket.cast::<SOCKADDR_IN>() };
            let octets = unsafe { sockaddr.sin_addr.S_un.S_un_b };
            let candidate = Ipv4Addr::new(octets.s_b1, octets.s_b2, octets.s_b3, octets.s_b4);
            if !candidate.is_unspecified() && !candidate.is_loopback() {
                if candidate.is_link_local() {
                    ipv4_fallback.get_or_insert(candidate);
                } else {
                    ipv4.get_or_insert(candidate);
                }
            }
        } else if family == AF_INET6 && socket_len >= std::mem::size_of::<SOCKADDR_IN6>() {
            let sockaddr = unsafe { &*socket.cast::<SOCKADDR_IN6>() };
            let candidate = Ipv6Addr::from(unsafe { sockaddr.sin6_addr.u.Byte });
            if !candidate.is_unspecified() && !candidate.is_loopback() {
                if candidate.is_unicast_link_local() {
                    ipv6_fallback.get_or_insert(candidate);
                } else {
                    ipv6.get_or_insert(candidate);
                }
            }
        }
    }

    (
        ipv4.or(ipv4_fallback).map(|address| address.to_string()),
        ipv6.or(ipv6_fallback).map(|address| address.to_string()),
    )
}

/// One `GetAdaptersAddresses` call including unicast-address metadata.
fn adapter_addresses() -> Option<Vec<u64>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 15 * 1024;
    loop {
        // The API writes pointer/u64-bearing C structs into this buffer. A
        // u64 backing allocation provides the required alignment; `Vec<u8>`
        // would happen to be aligned on today's allocator but cannot promise it.
        let words = (size as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; words];
        let ret = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
            )
        };
        if ret == ERROR_SUCCESS.0 {
            return Some(buf);
        }
        if ret != ERROR_BUFFER_OVERFLOW.0 || size == 0 {
            return None;
        }
        // Buffer too small: loop retries with the size reported above.
    }
}
