//! Per-adapter network facts the sampler can't get from sysinfo: hardware
//! description, negotiated link speed, oper status and the Wi-Fi SSID.
//!
//! Sources: `GetAdaptersAddresses` (description/link/oper status, keyed by
//! the adapter's friendly name — the same name sysinfo exposes) and
//! `WlanQueryInterface` (SSID of the active wireless connection, joined via
//! the adapter GUID).

use std::collections::HashMap;

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces,
    WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, wlan_intf_opcode_current_connection,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;

#[derive(Debug, Clone, Default)]
pub struct AdapterInfo {
    /// Hardware/model description, e.g. "Intel(R) Wi-Fi 6 AX201 160MHz".
    pub desc: String,
    /// Negotiated link speed in bits/s (0 = unknown/down).
    pub link_bps: u64,
    pub oper_up: bool,
    /// SSID of the active wireless connection, when this is Wi-Fi.
    pub ssid: Option<String>,
}

/// FriendlyName → adapter facts. One `GetAdaptersAddresses` call plus (when a
/// wireless adapter exists) one WLAN enumeration — cheap enough per tick.
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
        let ssid = ssids.get(&format!("{:?}", adapter.NetworkGuid).to_lowercase());
        out.insert(
            name,
            AdapterInfo {
                desc,
                link_bps,
                oper_up,
                ssid: ssid.cloned(),
            },
        );
    }
    out
}

/// SSID per wireless interface, keyed by the lowercase debug form of the
/// interface GUID (matches `IP_ADAPTER_ADDRESSES_LH::NetworkGuid`).
fn wifi_ssids_by_guid() -> HashMap<String, String> {
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
                                String::from_utf8_lossy(bytes).into_owned(),
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

/// One `GetAdaptersAddresses` call with adapter-level flags only.
fn adapter_addresses() -> Option<Vec<u8>> {
    let flags = GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 15 * 1024;
    loop {
        let mut buf = vec![0u8; size as usize];
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
