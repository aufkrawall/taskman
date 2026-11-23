//! DRM adapter discovery and safe sysfs enrichment.
//!
//! Every DRM card can be described from sysfs. Utilization and VRAM remain
//! driver-specific: when those files are absent, the fields stay unavailable
//! and `engines` stays empty. No engine is synthesized from a missing usage
//! counter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tm_core::model::GpuInfo;

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

pub fn gpus() -> Vec<GpuInfo> {
    let nvidia_names = nvidia_models();
    let mut cards = drm_cards();
    cards.sort_by_key(|(name, _)| card_number(name));
    cards
        .into_iter()
        .enumerate()
        .map(|(id, (_, path))| gpu_from_card(id, read_card(&path), &nvidia_names))
        .collect()
}

fn drm_cards() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
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
        .collect()
}

fn card_number(name: &str) -> u32 {
    name.strip_prefix("card")
        .and_then(|number| number.parse().ok())
        .unwrap_or(u32::MAX)
}

fn read_card(card: &Path) -> CardFacts {
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
        busy_pct: read_trim(device.join("gpu_busy_percent"))
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value)),
        vram_used: read_u64(device.join("mem_info_vram_used")),
        vram_total: read_u64(device.join("mem_info_vram_total")),
        gtt_used: read_u64(device.join("mem_info_gtt_used")),
        gtt_total: read_u64(device.join("mem_info_gtt_total")),
        temperature_c: read_temperature(&device),
    }
}

fn gpu_from_card(
    id: usize,
    facts: CardFacts,
    nvidia_names: &HashMap<String, String>,
) -> GpuInfo {
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
    let base = ids.map_or_else(
        || "GPU".to_string(),
        |ids| format!("{vendor} GPU [{ids}]"),
    );
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
    let mut sensors: Vec<PathBuf> = std::fs::read_dir(hwmon)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("temp1_input"))
        .filter(|path| path.exists())
        .collect();
    if sensors.is_empty() {
        sensors = std::fs::read_dir(hwmon)
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
    (-20.0..=200.0)
        .contains(&celsius)
        .then_some(celsius)
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
    let slot = slot.trim_matches(['[', ']']).to_ascii_lowercase();
    let Some((prefix, device_function)) = slot.rsplit_once(':') else {
        return slot;
    };
    let bus = prefix.rsplit_once(':').map_or(prefix, |(_, bus)| bus);
    format!("{bus}:{device_function}")
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
        (candidate.trim() == key && !value.trim().is_empty())
            .then(|| value.trim().to_string())
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
    fn nvidia_proc_name_is_used_without_inventing_usage() {
        let name = "NVIDIA GeForce RTX 5070".to_string();
        let mut names = HashMap::new();
        names.insert(normalize_pci_slot("0000:03:00.1"), name);
        let mut facts = fact();
        facts.driver = Some("nvidia".into());
        facts.vendor_id = Some(0x10de);
        facts.device_id = Some(0x2684);
        let gpu = gpu_from_card(0, facts, &names);
        assert_eq!(
            gpu.name,
            "NVIDIA GeForce RTX 5070 [10de:2684] (nvidia)"
        );
        assert!(gpu.engines.is_empty());
    }

    #[test]
    fn pci_location_accepts_domained_and_domainless_slots() {
        assert_eq!(parse_pci_location("0000:03:00.1"), Some((3, 0, 1)));
        assert_eq!(parse_pci_location("01:1f.7"), Some((1, 31, 7)));
        assert_eq!(parse_pci_location("[0000:03:00.1]"), Some((3, 0, 1)));
        assert_eq!(parse_pci_location("not-a-pci-slot"), None);
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
        assert_eq!(temperature_millidegrees_to_c(-5_000), None);
        assert_eq!(temperature_millidegrees_to_c(i64::MAX), None);
    }
}
