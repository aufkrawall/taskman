//! DRM adapter discovery and safe sysfs enrichment.
//!
//! Every DRM card can be described from sysfs. Utilization and VRAM remain
//! driver-specific: when those files are absent, the fields stay unavailable
//! and `engines` stays empty. No engine is synthesized from a missing usage
//! counter.
//!
//! The proprietary NVIDIA driver publishes neither `gpu_busy_percent` nor
//! `mem_info_vram_used`, so those adapters get their measurements from NVML
//! (`sensors::gpu_for_slot`) joined on the normalized PCI slot. Sysfs wins per
//! measured pair — a pair sysfs half-measured is replaced whole — and an
//! adapter NVML says nothing about keeps its fields unavailable: no row is
//! invented for a card DRM did not enumerate.

use super::sensors;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tm_core::model::GpuInfo;

/// Adapter identity describes a loaded driver, not a moment, so it is re-read
/// on this bound instead of every tick. A card that appears later (eGPU, driver
/// reload) is picked up at the next expiry; an empty scan stays empty rather
/// than inventing an adapter.
const IDENTITY_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default)]
struct CardFacts {
    driver: Option<String>,
    module_version: Option<String>,
    label: Option<String>,
    pci_slot: Option<String>,
    vendor_id: Option<u16>,
    device_id: Option<u16>,
    busy_pct: Option<f32>,
    vram_used: Option<u64>,
    vram_total: Option<u64>,
    gtt_used: Option<u64>,
    gtt_total: Option<u64>,
    temperature_c: Option<f32>,
}

/// Adapter identity read once per cache window, with the sysfs card path whose
/// live counters are read every tick.
#[derive(Debug, Clone)]
struct CardIdentity {
    path: PathBuf,
    /// The PCI device this card number pointed at when the identity was read.
    /// `/sys/class/drm/cardN` is per-driver probe order and is reused, so the
    /// number alone would pair card A's cached name and VRAM total with card
    /// B's live counters after a removal.
    device: Option<PathBuf>,
    facts: CardFacts,
}

struct CardCache {
    cards: Vec<CardIdentity>,
    nvidia_names: HashMap<String, String>,
    read_at: Instant,
}

impl CardCache {
    /// Whether these identities still describe the cards sysfs lists right now.
    ///
    /// One directory listing plus one symlink read per card per tick catches a
    /// removed card, a newly probed one, a reused card number and a card that
    /// now names another device, so a real change is picked up immediately
    /// instead of at the next TTL expiry. The expensive identity reads stay
    /// behind the TTL. A listing that fails cannot prove the cache wrong, so it
    /// keeps what it has rather than publishing an empty adapter list.
    fn still_accurate(&self) -> bool {
        let Some(current) = drm_cards() else {
            return true;
        };
        let current: Vec<(PathBuf, Option<PathBuf>)> = current
            .into_iter()
            .map(|(_, path)| {
                let device = card_device(&path);
                (path, device)
            })
            .collect();
        let cached: Vec<(PathBuf, Option<PathBuf>)> = self
            .cards
            .iter()
            .map(|card| (card.path.clone(), card.device.clone()))
            .collect();
        crate::linux_telemetry::card_identities_hold(&cached, &current)
    }
}

fn card_device(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path.join("device")).ok()
}

fn card_cache() -> &'static Mutex<Option<CardCache>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<CardCache>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// One entry per DRM card, lowest card number first.
///
/// `measure` reads the counters that change between ticks (utilization, VRAM
/// and GTT use, temperature) for a surface that shows them. Without it the
/// adapters are still listed — as Windows lists the adapters it enumerated —
/// while their measurements stay unavailable.
pub fn gpus(measure: bool) -> Vec<GpuInfo> {
    // Warm the one-second NVML window before the identity cache lock: the first
    // call in a process loads the driver library, and that should not happen
    // while the adapter cache is held. The per-adapter lookups below then hit
    // the same cached enumeration, and none of them waits on a socket — the
    // brokered fallback lives on its own worker thread.
    if measure {
        let _ = sensors::nvml_gpus();
    }
    let mut guard = tm_core::sync::lock(card_cache());
    let stale = guard
        .as_ref()
        .is_none_or(|cache| cache.read_at.elapsed() >= IDENTITY_TTL || !cache.still_accurate());
    if stale {
        // A listing that failed is not "no cards": keep the adapters we already
        // described instead of blanking the GPU page for a whole cache window.
        if let Some(fresh) = scan_cards() {
            *guard = Some(fresh);
        }
    }
    let Some(cache) = guard.as_ref() else {
        return Vec::new();
    };
    cache
        .cards
        .iter()
        .enumerate()
        .map(|(id, card)| gpu_for(id, card, measure, cache))
        .collect()
}

/// Enumerate the adapters, once per [`IDENTITY_TTL`] window.
///
/// `None` when `/sys/class/drm` cannot be listed: that is "unknown", not "no GPU".
fn scan_cards() -> Option<CardCache> {
    let mut cards = drm_cards()?;
    cards.sort_by_key(|(name, _)| card_number(name));
    Some(CardCache {
        cards: cards
            .into_iter()
            .map(|(_, path)| CardIdentity {
                device: card_device(&path),
                facts: read_identity(&path),
                path,
            })
            .collect(),
        nvidia_names: nvidia_models(),
        read_at: Instant::now(),
    })
}

/// One adapter row: cached identity plus, when measured, this tick's counters.
fn gpu_for(id: usize, card: &CardIdentity, measure: bool, cache: &CardCache) -> GpuInfo {
    let facts = if measure {
        read_live(card)
    } else {
        card.facts.clone()
    };
    let slot = facts.pci_slot.clone();
    let nvidia = is_nvidia_card(&facts);
    let gpu = gpu_from_card(id, facts, &cache.nvidia_names);
    if !measure {
        // Identity only: an unmeasured row has no counters to fill, and asking
        // the driver for them would be a measurement nobody asked for.
        return gpu;
    }
    // One adapter's NVML readings, from the cached enumeration: no adapter that
    // DRM did not enumerate can appear here.
    let readings = slot.as_deref().and_then(sensors::gpu_for_slot);
    crate::linux_sensors::with_nvml(gpu, slot.as_deref(), nvidia, readings.as_slice())
}

/// Whether this adapter uses the proprietary NVIDIA driver, which publishes no
/// utilization or VRAM counter through sysfs.
///
/// The vendor id covers a card whose driver link could not be read; a driver
/// that does answer a counter keeps it, because the overlay only fills fields
/// that are still unavailable.
fn is_nvidia_card(facts: &CardFacts) -> bool {
    facts.driver.as_deref() == Some("nvidia") || facts.vendor_id == Some(0x10de)
}

/// The cards sysfs lists, lowest name order. `None` when the directory cannot be
/// read, which the caller must not confuse with "this host has no DRM".
fn drm_cards() -> Option<Vec<(String, PathBuf)>> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let number = name.strip_prefix("card")?;
                if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
                    return None;
                }
                Some((name, entry.path()))
            })
            .collect(),
    )
}

fn card_number(name: &str) -> u32 {
    name.strip_prefix("card")
        .and_then(|number| number.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Static per-card facts: driver, module version, label, PCI ids/slot and the
/// VRAM/GTT totals, all of which describe the loaded driver.
fn read_identity(card: &Path) -> CardFacts {
    let device = card.join("device");
    let uevent = std::fs::read_to_string(device.join("uevent")).unwrap_or_default();
    let (vendor_id, device_id) = parse_pci_id(&uevent);
    let label = ["label", "product", "model"]
        .into_iter()
        .find_map(|name| read_trim(device.join(name)).filter(|value| !value.is_empty()));
    CardFacts {
        driver: driver_name(&device),
        module_version: read_trim(device.join("driver/module/version"))
            .filter(|version| !version.is_empty()),
        label,
        pci_slot: parse_uevent_value(&uevent, "PCI_SLOT_NAME")
            .or_else(|| pci_slot_from_path(&device)),
        vendor_id,
        device_id,
        vram_total: read_u64(device.join("mem_info_vram_total")),
        gtt_total: read_u64(device.join("mem_info_gtt_total")),
        ..Default::default()
    }
}

/// The counters that change between ticks, on top of the cached identity.
fn read_live(card: &CardIdentity) -> CardFacts {
    let device = card.path.join("device");
    CardFacts {
        busy_pct: read_trim(device.join("gpu_busy_percent"))
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value)),
        vram_used: read_u64(device.join("mem_info_vram_used")),
        gtt_used: read_u64(device.join("mem_info_gtt_used")),
        temperature_c: read_temperature(&device),
        ..card.facts.clone()
    }
}

fn gpu_from_card(id: usize, facts: CardFacts, nvidia_names: &HashMap<String, String>) -> GpuInfo {
    let nvidia_name = facts
        .pci_slot
        .as_deref()
        .and_then(|slot| nvidia_names.get(&normalize_pci_slot(slot)))
        .map(String::as_str);
    let name = gpu_name(&facts, nvidia_name);
    let vram_used = facts.vram_used.unwrap_or(0);
    let vram_total = facts.vram_total.unwrap_or(0);
    GpuInfo {
        id,
        name,
        driver_version: facts.module_version.unwrap_or_default(),
        driver_date_epoch_s: None,
        directx_version: String::new(),
        pci_location: facts.pci_slot.as_deref().and_then(parse_pci_location),
        util_pct_available: facts.busy_pct.is_some(),
        util_pct: facts.busy_pct.unwrap_or(0.0),
        dedicated_mem_available: facts.vram_used.is_some() && facts.vram_total.is_some(),
        shared_mem_available: facts.gtt_used.is_some() && facts.gtt_total.is_some(),
        mem_used_bytes: vram_used,
        mem_total_bytes: vram_total,
        dedicated_used_bytes: vram_used,
        shared_used_bytes: facts.gtt_used.unwrap_or(0),
        shared_total_bytes: facts.gtt_total.unwrap_or(0),
        hardware_reserved_bytes: 0,
        temperature_c: facts.temperature_c,
        luid: None,
        // `gpu_busy_percent` is an adapter-wide counter, not a named engine
        // instance. Keep it in `util_pct`; there is no engine data here.
        engines: Vec::new(),
    }
}

fn gpu_name(facts: &CardFacts, nvidia_name: Option<&str>) -> String {
    if let Some(label) = facts.label.as_deref().filter(|value| !value.is_empty()) {
        let name = with_pci_ids(label, facts);
        return with_driver(&name, facts.driver.as_deref());
    }
    if let Some(name) = nvidia_name {
        let name = with_pci_ids(name, facts);
        return with_driver(&name, facts.driver.as_deref());
    }

    let vendor = facts.vendor_id.map(vendor_name).unwrap_or("GPU");
    let ids = facts
        .vendor_id
        .zip(facts.device_id)
        .map(|(vendor, device)| format!("{vendor:04x}:{device:04x}"));
    let base = ids.map_or_else(|| "GPU".to_string(), |ids| format!("{vendor} GPU [{ids}]"));
    with_driver(&with_pci_ids(&base, facts), facts.driver.as_deref())
}

fn with_pci_ids(name: &str, facts: &CardFacts) -> String {
    let Some(ids) = facts
        .vendor_id
        .zip(facts.device_id)
        .map(|(vendor, device)| format!("{vendor:04x}:{device:04x}"))
    else {
        return name.to_string();
    };
    if name.to_ascii_lowercase().contains(&ids) {
        name.to_string()
    } else {
        format!("{name} [{ids}]")
    }
}

fn with_driver(name: &str, driver: Option<&str>) -> String {
    match driver.filter(|driver| !driver.is_empty()) {
        Some(driver) if !name.contains(driver) => format!("{name} ({driver})"),
        _ => name.to_string(),
    }
}

fn vendor_name(vendor_id: u16) -> &'static str {
    match vendor_id {
        0x1002 => "AMD",
        0x1010 => "ImgTec",
        0x10de => "NVIDIA",
        0x13b5 => "Arm",
        0x1414 => "Microsoft",
        0x1ae0 => "Google",
        0x1af4 => "Virtio",
        0x15ad => "VMware",
        0x8086 => "Intel",
        _ => "PCI",
    }
}

fn driver_name(device: &Path) -> Option<String> {
    std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
}

fn read_temperature(device: &Path) -> Option<f32> {
    let hwmon = device.join("hwmon");
    let mut sensors: Vec<PathBuf> = std::fs::read_dir(&hwmon)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("temp1_input"))
        .filter(|path| path.exists())
        .collect();
    if sensors.is_empty() {
        sensors = std::fs::read_dir(&hwmon)
            .ok()?
            .flatten()
            .flat_map(|entry| {
                let path = entry.path();
                let mut inputs = std::fs::read_dir(path)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|file| file.path())
                    .filter(|file| {
                        file.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with("temp") && name.ends_with("_input")
                            })
                    })
                    .collect::<Vec<_>>();
                inputs.sort();
                inputs
            })
            .collect();
    }
    sensors.sort();
    sensors.into_iter().find_map(|path| {
        read_trim(path)
            .and_then(|value| value.parse::<i64>().ok())
            .and_then(temperature_millidegrees_to_c)
    })
}

fn temperature_millidegrees_to_c(raw: i64) -> Option<f32> {
    let celsius = raw as f32 / 1000.0;
    // A driver that has no valid sample may expose a sentinel or all-ones.
    (-20.0..=200.0).contains(&celsius).then_some(celsius)
}

fn parse_pci_id(uevent: &str) -> (Option<u16>, Option<u16>) {
    let Some(value) = parse_uevent_value(uevent, "PCI_ID") else {
        return (None, None);
    };
    let Some((vendor, device)) = value.split_once(':') else {
        return (None, None);
    };
    (
        u16::from_str_radix(vendor, 16).ok(),
        u16::from_str_radix(device, 16).ok(),
    )
}

fn parse_uevent_value(uevent: &str, key: &str) -> Option<String> {
    uevent.lines().find_map(|line| {
        let (candidate, value) = line.split_once('=')?;
        (candidate == key && !value.is_empty()).then(|| value.to_string())
    })
}

fn pci_slot_from_path(device: &Path) -> Option<String> {
    std::fs::canonicalize(device)
        .ok()?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.contains(':'))
}

fn normalize_pci_slot(slot: &str) -> String {
    // The domain stays part of the key. A multi-segment host lists two cards as
    // `0000:03:00.0` and `0001:03:00.0`, and folding both to `03:00.0` made one
    // card's model name label the other. Both sides of the join carry it: a PCI
    // device's `PCI_SLOT_NAME` and `/proc/driver/nvidia/gpus/`'s `Bus Location`.
    slot.trim_matches(['[', ']']).to_ascii_lowercase()
}

fn parse_pci_location(slot: &str) -> Option<(u32, u32, u32)> {
    let clean = slot.trim_matches(['[', ']']);
    let (bus_part, device_function) = clean.rsplit_once(':')?;
    let bus = match bus_part.rsplit_once(':') {
        Some((domain, bus))
            if !domain.is_empty()
                && domain.chars().all(|ch| ch.is_ascii_hexdigit())
                && !bus.is_empty()
                && bus.chars().all(|ch| ch.is_ascii_hexdigit()) =>
        {
            // The published location carries no domain, so a non-zero one would
            // name two different segments' devices by the same bus/device/
            // function. No location is honest; an ambiguous one is not.
            if u32::from_str_radix(domain, 16).ok()? != 0 {
                return None;
            }
            bus
        }
        Some(_) => return None,
        None if bus_part.chars().all(|ch| ch.is_ascii_hexdigit()) => bus_part,
        None => return None,
    };
    let (device, function) = device_function.split_once('.')?;
    let bus = u32::from_str_radix(bus, 16).ok()?;
    let device = u32::from_str_radix(device, 16).ok()?;
    let function = u32::from_str_radix(function, 16).ok()?;
    Some((bus, device, function))
}

fn nvidia_models() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc/driver/nvidia/gpus") else {
        return out;
    };
    for entry in entries.flatten() {
        let info = std::fs::read_to_string(entry.path().join("information")).unwrap_or_default();
        let Some(model) = nvidia_property(&info, "Model") else {
            continue;
        };
        let slot = nvidia_property(&info, "Bus Location")
            .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
        out.insert(normalize_pci_slot(&slot), model);
    }
    out
}

fn nvidia_property(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate.trim() == key && !value.trim().is_empty()).then(|| value.trim().to_string())
    })
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact() -> CardFacts {
        CardFacts {
            driver: Some("amdgpu".into()),
            module_version: Some("6.8".into()),
            pci_slot: Some("0000:03:00.1".into()),
            vendor_id: Some(0x1002),
            device_id: Some(0x73df),
            vram_used: Some(512 << 20),
            vram_total: Some(8 << 30),
            gtt_used: Some(64 << 20),
            gtt_total: Some(2 << 30),
            temperature_c: Some(51.0),
            ..Default::default()
        }
    }

    #[test]
    fn a_busy_counter_is_measured_without_inventing_an_engine() {
        let gpu = gpu_from_card(
            0,
            CardFacts {
                busy_pct: Some(0.0),
                ..fact()
            },
            &HashMap::new(),
        );
        assert!(gpu.util_pct_available);
        assert_eq!(gpu.util_pct, 0.0);
        assert!(gpu.engines.is_empty());
    }

    #[test]
    fn absent_utilization_does_not_invent_an_idle_engine() {
        let gpu = gpu_from_card(1, fact(), &HashMap::new());
        assert!(!gpu.util_pct_available);
        assert_eq!(gpu.util_pct, 0.0);
        assert!(gpu.engines.is_empty());
        assert_eq!(gpu.name, "AMD GPU [1002:73df] (amdgpu)");
        assert_eq!(gpu.pci_location, Some((3, 0, 1)));
        assert_eq!(gpu.mem_total_bytes, 8 << 30);
        assert_eq!(gpu.shared_total_bytes, 2 << 30);
        assert_eq!(gpu.temperature_c, Some(51.0));
    }

    #[test]
    fn utilization_without_memory_files_keeps_memory_unavailable() {
        let gpu = gpu_from_card(
            2,
            CardFacts {
                busy_pct: Some(12.0),
                ..Default::default()
            },
            &HashMap::new(),
        );
        assert!(gpu.util_pct_available);
        assert!(!gpu.dedicated_mem_available);
        assert!(!gpu.shared_mem_available);
        assert_eq!(gpu.dedicated_used_bytes, 0);
        assert_eq!(gpu.shared_used_bytes, 0);
    }

    #[test]
    fn nvidia_proc_name_is_used_without_inventing_usage() {
        let name = "NVIDIA GeForce RTX 5070".to_string();
        let mut names = HashMap::new();
        names.insert(normalize_pci_slot("0000:03:00.1"), name);
        let mut facts = fact();
        facts.driver = Some("nvidia".into());
        facts.vendor_id = Some(0x10de);
        facts.device_id = Some(0x2684);
        let gpu = gpu_from_card(0, facts, &names);
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 5070 [10de:2684] (nvidia)");
        assert!(gpu.engines.is_empty());
    }

    #[test]
    fn pci_location_accepts_zero_domain_and_domainless_slots_only() {
        assert_eq!(parse_pci_location("0000:03:00.1"), Some((3, 0, 1)));
        assert_eq!(parse_pci_location("01:1f.7"), Some((1, 31, 7)));
        assert_eq!(parse_pci_location("[0000:03:00.1]"), Some((3, 0, 1)));
        assert_eq!(parse_pci_location("not-a-pci-slot"), None);
        // The location has no domain, so a non-zero segment would publish two
        // different devices under the same bus/device/function.
        assert_eq!(parse_pci_location("0001:03:00.0"), None);
        assert_eq!(parse_pci_location("10000:03:00.0"), None);
    }

    #[test]
    fn pci_ids_and_nvidia_properties_parse_without_placeholder_values() {
        assert_eq!(
            parse_pci_id("DRIVER=nvidia\nPCI_ID=10DE:2684\n"),
            (Some(0x10de), Some(0x2684))
        );
        assert_eq!(parse_pci_id("DRIVER=simpledrm\n"), (None, None));
        let information = "\
Model           : NVIDIA GeForce RTX 5070
IRQ             : 100
Bus Location    : 0000:01:00.0
";
        assert_eq!(
            nvidia_property(information, "Model").as_deref(),
            Some("NVIDIA GeForce RTX 5070")
        );
        assert_eq!(
            nvidia_property(information, "Bus Location").as_deref(),
            Some("0000:01:00.0")
        );
    }

    #[test]
    fn temperature_rejects_driver_sentinels() {
        assert_eq!(temperature_millidegrees_to_c(51_250), Some(51.25));
        // A measured sub-zero die is still a measurement: the range check
        // exists to drop sentinels, not plausible readings (a machine in an
        // unheated room really does report a few degrees below zero).
        assert_eq!(temperature_millidegrees_to_c(-5_000), Some(-5.0));
        // The sentinels a driver actually publishes for "no sample": the
        // all-ones of an uninitialised counter and absolute zero.
        assert_eq!(temperature_millidegrees_to_c(i64::MAX), None);
        assert_eq!(temperature_millidegrees_to_c(-273_150), None);
        assert_eq!(temperature_millidegrees_to_c(500_000), None);
    }
}
