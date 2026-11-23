//! Linux adapter metadata from sysfs, procfs and `getifaddrs(3)`.
//!
//! No external command is required. Missing sysfs/procfs facts remain empty
//! or `None`; in particular, Wi-Fi SSID and signal are not guessed from an
//! interface name.

use std::collections::HashMap;
use std::ffi::CStr;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct NetMeta {
    pub desc: String,
    pub kind: String,
    pub oper_up: bool,
    pub link_bps: u64,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}

pub fn collect() -> HashMap<String, NetMeta> {
    let ipv4 = ipv4_addresses();
    let ipv6 = std::fs::read_to_string("/proc/net/if_inet6")
        .map(|text| parse_if_inet6(&text))
        .unwrap_or_default();
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let base = entry.path();
        let addresses = ipv4.get(&name);
        out.insert(
            name.clone(),
            NetMeta {
                desc: adapter_description(&base),
                kind: adapter_kind(&base, &name),
                oper_up: adapter_oper_up(&base, &name),
                link_bps: adapter_link_bps(&base),
                ipv4: preferred_ipv4(addresses.into_iter().flatten().copied()),
                ipv6: preferred_ipv6(ipv6.get(&name).into_iter().flatten().copied()),
            },
        );
    }
    out
}

fn adapter_kind(base: &Path, name: &str) -> String {
    if name == "lo" || read_trim(base.join("type")).as_deref() == Some("772") {
        "Loopback"
    } else if base.join("wireless").exists() || base.join("phy80211").exists() {
        "Wi-Fi"
    } else if base.join("device").exists() {
        "Ethernet"
    } else {
        "Virtual"
    }
    .to_string()
}

fn adapter_oper_up(base: &Path, name: &str) -> bool {
    let operstate = read_trim(base.join("operstate")).unwrap_or_default();
    let carrier = read_trim(base.join("carrier")).map(|value| value == "1");
    matches!(operstate.as_str(), "up" | "unknown")
        && (carrier == Some(true) || (name == "lo" && operstate == "up"))
}

fn adapter_link_bps(base: &Path) -> u64 {
    read_trim(base.join("speed"))
        .and_then(|speed| speed.parse::<u64>().ok())
        .filter(|&mbps| mbps > 0 && mbps < 1_000_000)
        .map_or(0, |mbps| mbps.saturating_mul(1_000_000))
}

/// Prefer an explicitly advertised model/label. PCI and USB sysfs do not
/// generally carry a marketing name, so fall back to the stable driver and
/// hardware IDs instead of guessing one from the interface name.
fn adapter_description(base: &Path) -> String {
    let device = base.join("device");
    for candidate in ["label", "product", "model"] {
        if let Some(value) = read_trim(device.join(candidate)).filter(|v| !v.is_empty()) {
            return value;
        }
    }
    let Some(uevent) = std::fs::read_to_string(device.join("uevent")).ok() else {
        return String::new();
    };
    describe_uevent(&uevent)
}

fn describe_uevent(uevent: &str) -> String {
    let mut driver = None;
    let mut pci_id = None;
    let mut product = None;
    for line in uevent.lines() {
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        match key {
            "DRIVER" if !value.is_empty() => driver = Some(value),
            "PCI_ID" if !value.is_empty() => pci_id = Some(value),
            "PRODUCT" if !value.is_empty() => product = Some(value),
            _ => {}
        }
    }
    match (driver, pci_id.or(product)) {
        (Some(driver), Some(id)) => format!("{driver} ({id})"),
        (Some(driver), None) => driver.to_string(),
        (None, Some(id)) => id.to_string(),
        (None, None) => String::new(),
    }
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

fn preferred_ipv4(addresses: impl Iterator<Item = Ipv4Addr>) -> Option<String> {
    let mut fallback = None;
    for address in addresses.filter(|address| !address.is_unspecified() && !address.is_loopback()) {
        if address.is_link_local() {
            fallback.get_or_insert(address);
        } else {
            return Some(address.to_string());
        }
    }
    fallback.map(|address| address.to_string())
}

fn preferred_ipv6(addresses: impl Iterator<Item = Ipv6Addr>) -> Option<String> {
    let mut fallback = None;
    for address in addresses.filter(|address| !address.is_unspecified() && !address.is_loopback()) {
        if is_ipv6_link_local(address) {
            fallback.get_or_insert(address);
        } else {
            return Some(address.to_string());
        }
    }
    fallback.map(|address| address.to_string())
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

/// `/proc/net/if_inet6` rows are:
/// `address ifindex prefixlen scope flags interface`.
fn parse_if_inet6(text: &str) -> HashMap<String, Vec<Ipv6Addr>> {
    let mut out: HashMap<String, Vec<Ipv6Addr>> = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let Some(address) = parse_ipv6_hex(fields[0]) else {
            continue;
        };
        out.entry(fields[5].to_string()).or_default().push(address);
    }
    out
}

fn parse_ipv6_hex(text: &str) -> Option<Ipv6Addr> {
    if text.len() != 32 {
        return None;
    }
    let value = u128::from_str_radix(text, 16).ok()?;
    Some(Ipv6Addr::from(value))
}

fn ipv4_addresses() -> HashMap<String, Vec<Ipv4Addr>> {
    let mut out: HashMap<String, Vec<Ipv4Addr>> = HashMap::new();
    let mut first: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut first) } != 0 {
        return out;
    }
    let mut cursor = first;
    while !cursor.is_null() {
        let interface = unsafe { &*cursor };
        cursor = interface.ifa_next;
        if interface.ifa_addr.is_null() || interface.ifa_name.is_null() {
            continue;
        }
        let family = unsafe { (*interface.ifa_addr).sa_family as i32 };
        if family != libc::AF_INET {
            continue;
        }
        let name = unsafe { CStr::from_ptr(interface.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let sockaddr = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
        let address = Ipv4Addr::from(sockaddr.sin_addr.s_addr.to_ne_bytes());
        out.entry(name).or_default().push(address);
    }
    unsafe { libc::freeifaddrs(first) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uevent_description_preserves_driver_and_hardware_id() {
        assert_eq!(
            describe_uevent("DRIVER=e1000e\nPCI_ID=8086:15B8\nPCI_SUBSYS_ID=17AA:2238\n"),
            "e1000e (8086:15B8)"
        );
        assert_eq!(
            describe_uevent("DRIVER=ax88179_178a\nPRODUCT=0b95/1790/100\n"),
            "ax88179_178a (0b95/1790/100)"
        );
        assert_eq!(describe_uevent("INTERFACE=eth0\n"), "");
    }

    #[test]
    fn if_inet6_keeps_each_interface_addresses() {
        let text = "\
20010db8000000000000000000000001 02 40 00 80 eth0
fe800000000000000000000000000001 02 40 20 80 eth0
00000000000000000000000000000001 01 80 10 80 lo
";
        let parsed = parse_if_inet6(text);
        assert_eq!(parsed["eth0"].len(), 2);
        assert_eq!(parsed["eth0"][0], "2001:db8::1".parse().unwrap());
        assert_eq!(parsed["lo"][0], Ipv6Addr::LOCALHOST);
    }

    #[test]
    fn preferred_ipv6_uses_global_before_link_local_and_skips_loopback() {
        let link_local: Ipv6Addr = "fe80::1".parse().unwrap();
        let global: Ipv6Addr = "2001:db8::10".parse().unwrap();
        assert_eq!(
            preferred_ipv6([Ipv6Addr::LOCALHOST, link_local, global].into_iter()),
            Some(global.to_string())
        );
        assert_eq!(
            preferred_ipv6([link_local, Ipv6Addr::LOCALHOST].into_iter()),
            Some(link_local.to_string())
        );
        assert_eq!(preferred_ipv6([Ipv6Addr::LOCALHOST].into_iter()), None);
    }

    #[test]
    fn preferred_ipv4_uses_routable_before_link_local_and_skips_loopback() {
        let link_local: Ipv4Addr = "169.254.20.10".parse().unwrap();
        let routable: Ipv4Addr = "192.0.2.10".parse().unwrap();
        assert_eq!(
            preferred_ipv4([Ipv4Addr::LOCALHOST, routable, link_local].into_iter()),
            Some(routable.to_string())
        );
        assert_eq!(
            preferred_ipv4([link_local, Ipv4Addr::LOCALHOST].into_iter()),
            Some(link_local.to_string())
        );
    }
}
