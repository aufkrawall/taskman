//! Per-adapter network facts the sampler can't get from sysinfo: hardware
//! description, negotiated link speed, oper status and unicast addresses.
//!
//! Source: `GetAdaptersAddresses` (description/link/oper status, keyed by
//! the adapter's friendly name — the same name sysinfo exposes). None of it
//! is consent-gated.
//!
//! Wi-Fi connection details (SSID, signal strength) live in [`wifi_details`]
//! instead of this walk. Since the Windows 11 Wi-Fi/location privacy
//! changes they are precise-location data obtained through
//! `WlanQueryInterface`, which Windows gates behind a one-time location
//! consent prompt. Keeping that call out of the routine metadata walk is
//! what makes "never touch location without an explicit opt-in" auditable
//! at a glance: [`wifi_details`] is the only caller of the gated API.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr},
};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
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
    /// Join key for [`WifiDetails::by_guid`]: the WLAN interface GUID in
    /// [`interface_guid_key`] form (lowercase, without braces).
    pub interface_guid: String,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}

/// Connection facts of one wireless interface.
#[derive(Debug, Clone)]
pub struct WifiInfo {
    pub ssid: String,
    pub signal_quality_pct: u32,
}

/// Result of one [`wifi_details`] collection.
#[derive(Debug, Clone, Default)]
pub struct WifiDetails {
    /// Per-interface facts keyed by [`AdapterInfo::interface_guid`].
    pub by_guid: HashMap<String, WifiInfo>,
    /// True when Windows refused the query with `ERROR_ACCESS_DENIED`:
    /// precise-location consent is missing or was declined. The FIRST
    /// refusal is what makes Windows show its one-time location permission
    /// prompt, so callers must never retry this eagerly.
    pub access_denied: bool,
}

/// FriendlyName → adapter facts. One `GetAdaptersAddresses` call. The
/// sampler caches this metadata because address discovery does not belong
/// on every tick.
pub fn adapters() -> HashMap<String, AdapterInfo> {
    let Some(buf) = adapter_addresses() else {
        return HashMap::new();
    };

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
        let link_bps = link_speed_bps(adapter.TransmitLinkSpeed, adapter.ReceiveLinkSpeed);
        // `AdapterName` is the interface GUID ("{...}"), the identity WLAN
        // reports as `InterfaceGuid`; it is the join key for `wifi_details`.
        // (`NetworkGuid` names the connected network profile instead, so
        // joining on it never matched and the SSID and signal rows never
        // appeared.)
        let interface_guid = unsafe { adapter.AdapterName.to_string() }
            .map(|guid| interface_guid_key(&guid))
            .unwrap_or_default();
        let (ipv4, ipv6) = preferred_unicast_addresses(adapter.FirstUnicastAddress);
        out.insert(
            name,
            AdapterInfo {
                desc,
                link_bps,
                oper_up,
                interface_guid,
                ipv4,
                ipv6,
            },
        );
    }
    out
}

/// NDIS reports an unknown link speed as all-ones (`NDIS_LINK_SPEED_UNKNOWN`).
/// Virtual adapters that are up but have no physical link (Wi-Fi Direct,
/// some VPN miniports) report exactly that.
const LINK_SPEED_UNKNOWN: u64 = u64::MAX;

/// Negotiated link speed in bits/s, 0 when unknown.
///
/// Prefers the transmit speed and falls back to receive for odd drivers. The
/// all-ones sentinel must not pass as a measurement: it rendered as an
/// 18-million-Tbit/s link and divided every utilization figure to zero.
fn link_speed_bps(transmit: u64, receive: u64) -> u64 {
    [transmit, receive]
        .into_iter()
        .find(|speed| *speed != 0 && *speed != LINK_SPEED_UNKNOWN)
        .unwrap_or(0)
}

/// Join key for an interface GUID: lowercase, without braces. Accepts both
/// the `AdapterName` form ("{6B29FC40-...}") and the debug form of a `GUID`.
fn interface_guid_key(guid: &str) -> String {
    guid.trim_matches(['{', '}']).to_ascii_lowercase()
}

/// SSID and signal quality per wireless interface, keyed by
/// [`AdapterInfo::interface_guid`].
///
/// PRIVACY — this is the ONLY function in the program that touches a
/// Windows location-gated API. Since the Windows 11 Wi-Fi/location privacy
/// changes, `WlanQueryInterface(wlan_intf_opcode_current_connection)`
/// requires precise-location consent: the first unconsented call triggers
/// the one-time system location permission prompt, and every call registers
/// in the "location in use" tray activity and recent-activity list. Callers
/// MUST gate this behind the user's explicit opt-in
/// (`TelemetryDemand::WIFI_DETAILS`, derived from `Settings.wifi_details`)
/// and throttle it — see `sampler::collect_wifi`. There is no consent-free
/// alternative: Windows withholds SSID and signal from every API, including
/// the WinRT `GetConnectedSsid`/`GetSignalBars` routes, without location
/// consent.
pub fn wifi_details() -> WifiDetails {
    use windows::Win32::NetworkManagement::WiFi::{
        WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces,
        WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, wlan_intf_opcode_current_connection,
    };

    unsafe {
        let mut handle = Default::default();
        let mut negotiated = 0u32;
        if WlanOpenHandle(2, None, &mut negotiated, &mut handle) != 0 {
            return WifiDetails::default();
        }
        let mut out = WifiDetails::default();
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(handle, None, &mut list) == 0 && !list.is_null() {
            let l = &*list;
            let items =
                std::slice::from_raw_parts(l.InterfaceInfo.as_ptr(), l.dwNumberOfItems as usize);
            for item in items {
                let mut size = 0u32;
                let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
                let ret = WlanQueryInterface(
                    handle,
                    &item.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    None,
                    &mut size,
                    &mut data,
                    None,
                );
                if ret == 0 && !data.is_null() {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    // wlan_interface_state_connected == 1
                    if attrs.isState.0 == 1 {
                        let ssid = &attrs.wlanAssociationAttributes.dot11Ssid;
                        if ssid.uSSIDLength > 0 {
                            let bytes = &ssid.ucSSID[..(ssid.uSSIDLength as usize).min(32)];
                            out.by_guid.insert(
                                interface_guid_key(&format!("{:?}", item.InterfaceGuid)),
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
                } else if ret == ERROR_ACCESS_DENIED.0 {
                    out.access_denied = true;
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
fn adapter_addresses() -> Option<super::aligned::AlignedBuf> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 15 * 1024;
    loop {
        // The API writes pointer/u64-bearing C structs into this buffer, so
        // the allocation must carry the structures' alignment (see
        // `aligned`): a plain `Vec<u8>` cannot promise it.
        let mut buf = super::aligned::AlignedBuf::zeroed(size as usize);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The WLAN interface GUID and the adapter's `AdapterName` must meet on
    /// one key, whatever braces and case either side uses.
    #[test]
    fn wlan_and_adapter_guids_share_a_key() {
        let guid = windows::core::GUID::from_u128(0x6b29fc40_ca47_1067_b31d_00dd010662da);
        assert_eq!(
            interface_guid_key(&format!("{guid:?}")),
            interface_guid_key("{6B29FC40-CA47-1067-B31D-00DD010662DA}")
        );
    }

    #[test]
    fn unknown_link_speed_is_not_a_measurement() {
        assert_eq!(link_speed_bps(LINK_SPEED_UNKNOWN, LINK_SPEED_UNKNOWN), 0);
        assert_eq!(link_speed_bps(0, 0), 0);
        assert_eq!(link_speed_bps(LINK_SPEED_UNKNOWN, 100_000_000), 100_000_000);
        assert_eq!(link_speed_bps(1_000_000_000, 100_000_000), 1_000_000_000);
        assert_eq!(link_speed_bps(0, 300_000_000), 300_000_000);
    }
}
