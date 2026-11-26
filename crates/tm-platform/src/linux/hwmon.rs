//! Sysfs sensor collection: hwmon chips, thermal zones and the RAPL powercap
//! tree.
//!
//! Everything here lands in the reading model of [`crate::linux_sensors`]: one
//! [`SensorReading`] per readable input, already scaled to base units, and one
//! `unreadable` line per path this process was not allowed to read (or that
//! carried no plausible value). A sensor that cannot be read stays absent; it
//! is never a zero (core product invariant).
//!
//! How the model is filled, so a consumer can group the rows:
//!
//! * hwmon: `chip` is the chip's `name` (`coretemp`, `amdgpu`, `nvme`, ...),
//!   `label` its `*_label` (else the attribute stem, `temp1`), and `device` the
//!   last component of the chip's `device` symlink — a PCI slot on most boards.
//! * thermal zone: `chip` is the zone `type` (`x86_pkg_temp`, `acpitz`, ...) and
//!   `label` the zone name (`thermal_zone0`), which is what identifies the row.
//! * RAPL: `chip` is `intel-rapl`, `label` the zone `name` (`package-0`, `core`,
//!   `core`, `dram` — a label that repeats across packages) and `device` the
//!   zone directory (`intel-rapl:0`, `intel-rapl:0:2`), which is what makes a
//!   package and its constraints distinguishable. `value` is a cumulative
//!   counter and `max` the range it wraps at, not a rate.
//!
//! Every walk is bounded. sysfs is kernel-owned, but the core service runs this
//! as root: a malformed or hostile tree must not be able to grow the reading
//! list without limit, follow a symlink loop, or make the collector spin.

use crate::linux_sensors::{
    SensorKind, SensorReading, attribute_order, hwmon_device_label, kind_of_stem, parse_alarm,
    powercap_zone_index, scale_hwmon, sensor_stem, split_attribute, unit_of_kind,
};
use std::path::{Path, PathBuf};

const HWMON_ROOT: &str = "/sys/class/hwmon";
const THERMAL_ROOT: &str = "/sys/class/thermal";
const POWERCAP_ROOT: &str = "/sys/class/powercap";
/// The RAPL provider's directory prefix inside the powercap class.
const RAPL_PREFIX: &str = "intel-rapl:";
/// No board has more chips than this, and the cap keeps a hostile class
/// directory from growing the walk.
const MAX_CHIPS: usize = 64;
/// A chip with more attributes than this is not a sensor chip.
const MAX_ATTRIBUTES_PER_CHIP: usize = 512;
/// One snapshot's ceiling: an unusual board reports a few hundred readings.
const MAX_READINGS: usize = 4096;
/// Diagnostics only, so a denied tree cannot fill the snapshot with reasons.
const MAX_UNREADABLE: usize = 128;

/// Every reading this process can take from `/sys`, plus the paths it could
/// not use and why.
pub(super) fn collect() -> (Vec<SensorReading>, Vec<String>) {
    let mut collector = Collector::default();
    collector.hwmon();
    collector.thermal();
    collector.powercap();
    (collector.readings, collector.unreadable)
}

#[derive(Default)]
struct Collector {
    readings: Vec<SensorReading>,
    unreadable: Vec<String>,
}

impl Collector {
    /// Record a path this poll could not use, with the reason.
    fn note(&mut self, path: &Path, reason: impl std::fmt::Display) {
        if self.unreadable.len() < MAX_UNREADABLE {
            self.unreadable
                .push(format!("{}: {reason}", path.display()));
        }
    }

    /// Every `/sys/class/hwmon/hwmonN` chip.
    fn hwmon(&mut self) {
        let chips = match class_entries(HWMON_ROOT, "hwmon") {
            Ok(chips) => chips,
            Err(error) => {
                self.note(Path::new(HWMON_ROOT), error);
                return;
            }
        };
        for chip in chips {
            self.chip(&chip);
        }
    }

    /// One hwmon chip: its name, its device, and every input it publishes.
    fn chip(&mut self, dir: &Path) {
        let name_path = dir.join("name");
        let chip = match std::fs::read_to_string(&name_path) {
            Ok(text) => text.trim().to_string(),
            Err(error) => {
                self.note(&name_path, error);
                return;
            }
        };
        if chip.is_empty() {
            // Without its name the readings would carry an empty chip column,
            // so the whole chip is unusable rather than half-labelled.
            self.note(&name_path, "empty chip name");
            return;
        }
        let device = std::fs::read_link(dir.join("device"))
            .ok()
            .and_then(|target| hwmon_device_label(&target.to_string_lossy()));
        let files = match attribute_files(dir) {
            Ok(files) => files,
            Err(error) => {
                self.note(dir, error);
                return;
            }
        };
        for file in files {
            self.attribute(dir, &chip, device.as_deref(), &file);
        }
    }

    /// One attribute group: the sample plus its `_max`/`_crit`/`_alarm`
    /// siblings and its `_label`.
    fn attribute(&mut self, dir: &Path, chip: &str, device: Option<&str>, file: &str) {
        let Some((stem, attribute)) = split_attribute(file) else {
            return;
        };
        let Some(kind) = kind_of_stem(stem) else {
            return;
        };
        // Every kind publishes its sample as `<stem>_input`; an intrusion sensor
        // has no input at all, so its alarm flag is the sample.
        let from_alarm = kind == SensorKind::Intrusion;
        let source = if from_alarm { "alarm" } else { "input" };
        if attribute != source {
            return;
        }
        // A tree at the ceiling is truncated, not reported as a reason: the cap
        // exists so a hostile sysfs cannot exhaust memory.
        if self.readings.len() >= MAX_READINGS {
            return;
        }
        let path = dir.join(file);
        let raw = match read_number::<i64>(&path) {
            Ok(raw) => raw,
            Err(error) => {
                self.note(&path, error);
                return;
            }
        };
        let Some(value) = scale_hwmon(kind, raw) else {
            // A driver sentinel (`i64::MAX`, a negative temperature, a humidity
            // past 100 %) is not a sample this model can carry.
            self.note(&path, "no plausible value");
            return;
        };
        let limit = |suffix: &str| {
            read_number::<i64>(&dir.join(format!("{stem}_{suffix}")))
                .ok()
                .and_then(|raw| scale_hwmon(kind, raw))
        };
        let alarm = if from_alarm {
            Some(raw != 0)
        } else {
            read_alarm(&dir.join(format!("{stem}_alarm")))
                .or_else(|| read_alarm(&dir.join(format!("{stem}_crit_alarm"))))
        };
        let label = std::fs::read_to_string(dir.join(format!("{stem}_label")))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| stem.to_string());
        self.readings.push(SensorReading {
            chip: chip.to_string(),
            device: device.map(str::to_string),
            label,
            path: path.display().to_string(),
            kind,
            value,
            unit: unit_of_kind(kind),
            max: limit("max"),
            crit: limit("crit"),
            alarm,
        });
    }

    /// `/sys/class/thermal/thermal_zone*`: one temperature each.
    fn thermal(&mut self) {
        let zones = match class_entries(THERMAL_ROOT, "thermal_zone") {
            Ok(zones) => zones,
            Err(error) => {
                self.note(Path::new(THERMAL_ROOT), error);
                return;
            }
        };
        for zone in zones {
            if self.readings.len() >= MAX_READINGS {
                return;
            }
            let path = zone.join("temp");
            let raw = match read_number::<i64>(&path) {
                Ok(raw) => raw,
                Err(error) => {
                    self.note(&path, error);
                    continue;
                }
            };
            let Some(value) = scale_hwmon(SensorKind::Temperature, raw) else {
                self.note(&path, "no plausible value");
                continue;
            };
            let zone_name = zone
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "thermal_zone".to_string());
            // The type names the driver-side sensor (`x86_pkg_temp`, `acpitz`);
            // the zone name is what identifies this row.
            let chip = std::fs::read_to_string(zone.join("type"))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "thermal".to_string());
            self.readings.push(SensorReading {
                chip,
                device: None,
                label: zone_name,
                path: path.display().to_string(),
                kind: SensorKind::Temperature,
                value,
                unit: unit_of_kind(SensorKind::Temperature),
                max: None,
                crit: None,
                alarm: None,
            });
        }
    }

    /// `/sys/class/powercap/intel-rapl:*`: cumulative energy counters.
    ///
    /// The tree nests a package's own counter beside its constraints
    /// (`intel-rapl:0:2` is `dram`), and a zone's `name` repeats across
    /// packages, so the zone directory travels in `device`. Each zone also
    /// publishes the range its counter wraps at, which becomes the reading's
    /// `max`. The counters are root-only on current kernels, so an unelevated
    /// sampler records the denial and reports them absent instead of 0 J.
    fn powercap(&mut self) {
        let zones = match rapl_zones() {
            Ok(zones) => zones,
            Err(error) => {
                self.note(Path::new(POWERCAP_ROOT), error);
                return;
            }
        };
        for zone in zones {
            if self.readings.len() >= MAX_READINGS {
                return;
            }
            let path = zone.join("energy_uj");
            let raw = match read_number::<u64>(&path) {
                Ok(raw) => raw,
                Err(error) => {
                    self.note(&path, error);
                    continue;
                }
            };
            let Some(value) = energy_joules(raw) else {
                self.note(&path, "no plausible value");
                continue;
            };
            let zone_name = zone
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = std::fs::read_to_string(zone.join("name"))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| zone_name.clone());
            self.readings.push(SensorReading {
                chip: "intel-rapl".to_string(),
                device: Some(zone_name),
                label,
                path: path.display().to_string(),
                kind: SensorKind::Energy,
                value,
                unit: unit_of_kind(SensorKind::Energy),
                // Without the range a consumer cannot tell a wrap from a
                // counter that jumped backwards.
                max: read_number::<u64>(&zone.join("max_energy_range_uj"))
                    .ok()
                    .and_then(energy_joules),
                crit: None,
                alarm: None,
            });
        }
    }
}

/// One RAPL microjoule counter in joules.
fn energy_joules(raw: u64) -> Option<f64> {
    i64::try_from(raw)
        .ok()
        .and_then(|raw| scale_hwmon(SensorKind::Energy, raw))
}

/// The `intel-rapl` zones of `/sys/class/powercap`, grouped by package.
///
/// A package's own counter and its constraints share the package number, and
/// the `(package, constraint)` pair keeps them together in the kernel's order.
fn rapl_zones() -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(POWERCAP_ROOT)?.flatten() {
        if out.len() >= MAX_CHIPS {
            break;
        }
        if rapl_zone_index(&entry.path()).is_none() {
            continue;
        }
        out.push(entry.path());
    }
    out.sort_by_key(|path| rapl_zone_index(path).unwrap_or((u32::MAX, u32::MAX)));
    Ok(out)
}

/// The `(package, constraint)` pair of one powercap directory.
fn rapl_zone_index(path: &Path) -> Option<(u32, u32)> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(RAPL_PREFIX))
        .and_then(powercap_zone_index)
}

/// The bounded `prefix` + digits entries of one sysfs class directory, in
/// hardware order.
///
/// A listing that fails is an error so the caller can record the reason: an
/// unreadable class directory is not "this machine has no sensor".
fn class_entries(root: &str, prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)?.flatten() {
        if out.len() >= MAX_CHIPS {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(number) = name.strip_prefix(prefix) else {
            continue;
        };
        if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        out.push(entry.path());
    }
    out.sort_by_key(|path| entry_number(path, prefix));
    Ok(out)
}

/// The numeric suffix of a `hwmonN` / `thermal_zoneN` / `intel-rapl:N` entry, so
/// a snapshot orders chips the way the kernel numbers them.
fn entry_number(path: &Path, prefix: &str) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(prefix))
        .and_then(|number| number.parse().ok())
        .unwrap_or(u32::MAX)
}

/// The sensor attribute names of one chip, bounded and in hardware order.
///
/// Only names carrying a recognized attribute are kept: `name`, `uevent` and
/// driver-specific extras are not inputs.
fn attribute_files(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut files: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        if files.len() >= MAX_ATTRIBUTES_PER_CHIP {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if sensor_stem(name).is_none() {
            continue;
        }
        files.push(name.to_string());
    }
    files.sort_by_key(|file| attribute_order(file));
    Ok(files)
}

/// One sysfs integer. The kernel terminates every attribute with a newline.
fn read_number<T: std::str::FromStr>(path: &Path) -> std::io::Result<T> {
    let text = std::fs::read_to_string(path)?;
    text.trim()
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not an integer"))
}

/// Read an hwmon alarm flag.
fn read_alarm(path: &Path) -> Option<bool> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| parse_alarm(&text))
}
