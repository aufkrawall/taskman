//! Linux adapter metadata from sysfs, procfs and `getifaddrs(3)`.
//!
//! No external command is required. Missing sysfs/procfs facts remain empty
//! or `None`; in particular, Wi-Fi SSID and signal are not guessed from an
//! interface name.

use std::collections::HashMap;
use std::ffi::CStr;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct NetMeta {
    pub desc: String,
    pub kind: String,
    pub oper_up: bool,
    pub link_bps: u64,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
}

/// What an interface *is*: the model/driver description and its class. It only
/// changes when the interface is replaced, so it is read once per interface
/// instead of on every tick.
#[derive(Debug, Clone, Default)]
struct AdapterIdentity {
    /// `device` symlink target the identity was read for, so a NIC unplugged and
    /// replugged under the same interface name is described again.
    device: Option<PathBuf>,
    desc: String,
    kind: String,
}

fn identity_cache() -> &'static Mutex<HashMap<String, AdapterIdentity>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, AdapterIdentity>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv6Address {
    address: Ipv6Addr,
    flags: u8,
}

/// Current state of every interface: name, description, link state, addresses.
///
/// The description and class come from [`AdapterIdentity`]; operstate, carrier,
/// link speed and addresses are read on every call, so no column loses
/// freshness to the split.
pub fn collect() -> HashMap<String, NetMeta> {
    let ipv4 = ipv4_addresses();
    let ipv6 = std::fs::read_to_string("/proc/net/if_inet6")
        .map(|text| parse_if_inet6(&text))
        .unwrap_or_default();
    let mut identities = tm_core::sync::lock(identity_cache());
    let mut out = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() {
                continue;
            }
            let base = entry.path();
            let device = std::fs::read_link(base.join("device")).ok();
            let cached = identities
                .get(&name)
                .filter(|known| known.device == device)
                .cloned();
            let identity = cached.unwrap_or_else(|| {
                let fresh = AdapterIdentity {
                    device,
                    desc: adapter_description(&base),
                    kind: adapter_kind(&base, &name),
                };
                identities.insert(name.clone(), fresh.clone());
                fresh
            });
            let addresses = ipv4.get(&name);
            out.insert(
                name.clone(),
                NetMeta {
                    desc: identity.desc,
                    kind: identity.kind,
                    oper_up: adapter_oper_up(&base, &name),
                    link_bps: adapter_link_bps(&base),
                    ipv4: preferred_ipv4(addresses.into_iter().flatten().copied()),
                    ipv6: preferred_ipv6(ipv6.get(&name).into_iter().flatten().copied()),
                },
            );
        }
    } else {
        // A listing that failed is not "no interfaces". Blanking every description
        // and pruning the cached names would outlast the failure, so the identities
        // already measured are published as the only facts left — the same way the
        // DRM card scan keeps a list it cannot re-read. The live fields stay
        // unclaimed because they cannot be read without the interface directory.
        tracing::debug!("could not list /sys/class/net; keeping the known adapter identities");
        return identities
            .iter()
            .map(|(name, identity)| {
                (
                    name.clone(),
                    NetMeta {
                        desc: identity.desc.clone(),
                        kind: identity.kind.clone(),
                        ..NetMeta::default()
                    },
                )
            })
            .collect();
    }
    // Interfaces that are gone take their cached identity with them.
    identities.retain(|name, _| out.contains_key(name));
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
    // A USB NIC's `device` link names its `usb_interface` (`…/2-1/2-1:1.0`),
    // where none of the marketing strings above exist; they live on the
    // `usb_device` that is its parent (`…/2-1`). One level further up is the
    // xHCI root hub, which carries `idVendor` and a `product` of its own, so
    // reading there would describe every USB NIC as "xHCI Host Controller".
    // `idVendor` is what identifies the parent as a USB device, so a PCI parent
    // — whose `product` would name the bridge, not the NIC — is never consulted.
    let usb_device = device.join("..");
    if usb_device.join("idVendor").is_file() {
        for candidate in ["product", "manufacturer"] {
            if let Some(value) = read_trim(usb_device.join(candidate)).filter(|v| !v.is_empty()) {
                return value;
            }
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

fn preferred_ipv6(addresses: impl Iterator<Item = Ipv6Address>) -> Option<String> {
    let mut best: Option<(u8, Ipv6Addr)> = None;
    for address in addresses
        .filter(|address| !address.address.is_unspecified() && !address.address.is_loopback())
    {
        let Some(rank) = ipv6_address_rank(address) else {
            continue;
        };
        if best.is_none_or(|(best_rank, _)| rank < best_rank) {
            best = Some((rank, address.address));
        }
    }
    best.map(|(_, address)| address.to_string())
}

fn ipv6_address_rank(address: Ipv6Address) -> Option<u8> {
    const IFA_F_TENTATIVE: u8 = 0x40;
    const IFA_F_DADFAILED: u8 = 0x08;
    const IFA_F_OPTIMISTIC: u8 = 0x04;
    const IFA_F_DEPRECATED: u8 = 0x20;
    const IFA_F_TEMPORARY: u8 = 0x01;

    if address.flags & (IFA_F_TENTATIVE | IFA_F_DADFAILED | IFA_F_OPTIMISTIC) != 0 {
        return None;
    }
    let stability_rank = if address.flags & IFA_F_DEPRECATED != 0 {
        2
    } else if address.flags & IFA_F_TEMPORARY != 0 {
        1
    } else {
        0
    };
    Some(stability_rank * 2 + u8::from(is_ipv6_link_local(address.address)))
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

/// `/proc/net/if_inet6` rows are:
/// `address ifindex prefixlen scope flags interface`.
fn parse_if_inet6(text: &str) -> HashMap<String, Vec<Ipv6Address>> {
    let mut out: HashMap<String, Vec<Ipv6Address>> = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let Some(address) = parse_ipv6_hex(fields[0]) else {
            continue;
        };
        let Some(flags) = u8::from_str_radix(fields[4], 16).ok() else {
            continue;
        };
        out.entry(fields[5].to_string())
            .or_default()
            .push(Ipv6Address { address, flags });
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

    /// A throwaway directory tree, so a sysfs layout can be pinned without the
    /// machine's own devices.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("tm-linux-netmeta-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn link(&self, relative: &str, target: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, &path).unwrap();
            path
        }

        fn mkdir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// `/sys/class/net/<if>/device` is a link to the `usb_interface`, so the
    /// interface's marketing name is its PARENT. The grandparent is the xHCI
    /// root hub, which carries `idVendor` and a `product` of its own — reading
    /// there names every USB NIC after the host controller.
    #[test]
    fn a_usb_adapter_is_named_after_its_device_not_the_root_hub() {
        let tree = Tree::new("usb-nic");
        // The host controller: a decoy `idVendor` and `product` one level above
        // the device the interface belongs to.
        tree.write("usb2/idVendor", "1d6b\n");
        tree.write("usb2/product", "xHCI Host Controller\n");
        // The adapter itself.
        tree.write("usb2/2-1/idVendor", "0b95\n");
        tree.write(
            "usb2/2-1/product",
            "AX88179A USB 3.2 Gen1 Ethernet Adapter\n",
        );
        tree.write("usb2/2-1/2-1:1.0/uevent", "DRIVER=ax88179_178a\n");
        // `net/eth0/device` → the interface, exactly as sysfs links it.
        tree.link("net/eth0/device", "../../usb2/2-1/2-1:1.0");

        assert_eq!(
            adapter_description(&tree.root.join("net/eth0")),
            "AX88179A USB 3.2 Gen1 Ethernet Adapter"
        );
    }

    /// A PCI device has no `idVendor` file and no marketing strings of its own,
    /// so the description stays the driver plus hardware id — the parent PCI
    /// bridge must never be described as the adapter.
    #[test]
    fn a_pci_adapter_keeps_the_driver_and_hardware_id_description() {
        let tree = Tree::new("pci-nic");
        tree.write("0000:00:1c.0/product", "PCIe Root Port\n");
        tree.write(
            "0000:00:1c.0/0000:03:00.0/uevent",
            "DRIVER=e1000e\nPCI_ID=8086:15B8\n",
        );
        tree.mkdir("net/eth0");
        tree.link("net/eth0/device", "../../0000:00:1c.0/0000:03:00.0");

        assert_eq!(
            adapter_description(&tree.root.join("net/eth0")),
            "e1000e (8086:15B8)"
        );
    }

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
        assert_eq!(
            parsed["eth0"][0].address,
            "2001:db8::1".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(parsed["lo"][0].address, Ipv6Addr::LOCALHOST);
    }

    #[test]
    fn preferred_ipv6_uses_stable_global_before_link_local_and_skips_loopback() {
        let address = |text: &str, flags| Ipv6Address {
            address: text.parse().unwrap(),
            flags,
        };
        let link_local = address("fe80::1", 0);
        let global = address("2001:db8::10", 0);
        assert_eq!(
            preferred_ipv6(
                [
                    Ipv6Address {
                        address: Ipv6Addr::LOCALHOST,
                        flags: 0,
                    },
                    link_local,
                    global,
                ]
                .into_iter()
            ),
            Some(global.address.to_string())
        );
        assert_eq!(
            preferred_ipv6(
                [
                    link_local,
                    Ipv6Address {
                        address: Ipv6Addr::LOCALHOST,
                        flags: 0,
                    },
                ]
                .into_iter()
            ),
            Some(link_local.address.to_string())
        );
        assert_eq!(
            preferred_ipv6(
                [Ipv6Address {
                    address: Ipv6Addr::LOCALHOST,
                    flags: 0,
                }]
                .into_iter()
            ),
            None
        );
    }

    #[test]
    fn preferred_ipv6_rejects_tentative_dad_failed_and_optimistic_addresses() {
        let address = |text: &str, flags| Ipv6Address {
            address: text.parse().unwrap(),
            flags,
        };
        assert_eq!(
            preferred_ipv6(
                [
                    address("2001:db8::1", 0x40),
                    address("2001:db8::2", 0x08),
                    address("2001:db8::3", 0x04),
                ]
                .into_iter()
            ),
            None
        );
    }

    #[test]
    fn deprecated_address_is_only_a_fallback() {
        let address = |text: &str, flags| Ipv6Address {
            address: text.parse().unwrap(),
            flags,
        };
        let stable_link = address("fe80::1", 0);
        let deprecated_global = address("2001:db8::10", 0x20);
        assert_eq!(
            preferred_ipv6([deprecated_global, stable_link].into_iter()),
            Some(stable_link.address.to_string())
        );
        assert_eq!(
            preferred_ipv6([deprecated_global].into_iter()),
            Some(deprecated_global.address.to_string())
        );
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
