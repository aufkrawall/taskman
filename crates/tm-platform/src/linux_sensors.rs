//! Sensor readings shared by the Linux sampler, the root core service and the
//! GUI.
//!
//! Platform-neutral on purpose (like `linux_telemetry`): every parser here
//! takes the file name or the raw text it inspects, so the sysfs conversions
//! and the NVML ABI shapes are unit-testable on the Windows development host.
//!
//! The collector in [`crate::linux::sensors`] fills these types from
//! `/sys/class/hwmon`, `/sys/class/thermal`, the RAPL powercap tree and NVML.
//! A reading that cannot be taken stays absent — never a fabricated zero
//! (core product invariant).

use serde::{Deserialize, Serialize};
use tm_core::model::GpuInfo;

/// One complete sensor poll.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SensorSnapshot {
    /// Unix epoch milliseconds of the poll. `0` means "never taken".
    #[serde(default)]
    pub taken_at_unix_ms: i64,
    /// One entry per readable hwmon attribute.
    #[serde(default)]
    pub readings: Vec<SensorReading>,
    /// NVML GPU telemetry, one entry per adapter NVML enumerated.
    #[serde(default)]
    pub gpus: Vec<GpuSensors>,
    /// Paths the poll was not allowed to read (or that failed to parse).
    /// Diagnostics only: an unreadable sensor is unavailable, not zero.
    #[serde(default)]
    pub unreadable: Vec<String>,
}

impl SensorSnapshot {
    pub fn is_empty(&self) -> bool {
        self.readings.is_empty() && self.gpus.is_empty()
    }

    /// Readings of one kind, lowest chip/label first.
    pub fn of_kind(&self, kind: SensorKind) -> impl Iterator<Item = &SensorReading> {
        self.readings
            .iter()
            .filter(move |reading| reading.kind == kind)
    }

    /// The GPU NVML described for a PCI slot, matched on the normalized slot.
    pub fn gpu_for_pci_slot(&self, slot: &str) -> Option<&GpuSensors> {
        let slot = normalize_pci_slot(slot);
        self.gpus
            .iter()
            .find(|gpu| gpu.pci_slot.as_deref() == Some(slot.as_str()))
    }
}

/// What an hwmon attribute measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SensorKind {
    Temperature,
    Fan,
    Voltage,
    Current,
    Power,
    /// Cumulative energy counter (hwmon `energy*_input`, RAPL `energy_uj`), in
    /// joules: the total counted so far, NOT the consumption of the sample
    /// window. A wrapping counter publishes its range in
    /// [`SensorReading::max`], so a consumer subtracts two samples modulo that
    /// range instead of reading the value as a rate.
    Energy,
    Humidity,
    Intrusion,
    Other,
}

impl SensorKind {
    /// The hwmon file-name prefix that carries this kind.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Temperature => "temp",
            Self::Fan => "fan",
            Self::Voltage => "in",
            Self::Current => "curr",
            Self::Power => "power",
            Self::Energy => "energy",
            Self::Humidity => "humidity",
            Self::Intrusion => "intrusion",
            Self::Other => "",
        }
    }
}

/// Unit of a scaled reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SensorUnit {
    Celsius,
    Rpm,
    Volt,
    Ampere,
    Watt,
    Joule,
    Percent,
    Unknown,
}

impl SensorUnit {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Rpm => "RPM",
            Self::Volt => "V",
            Self::Ampere => "A",
            Self::Watt => "W",
            Self::Joule => "J",
            Self::Percent => "%",
            Self::Unknown => "",
        }
    }
}

/// One hwmon attribute, already converted to base units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorReading {
    /// hwmon chip name (`k10temp`, `amdgpu`, `nvme`, `coretemp`, ...).
    pub chip: String,
    /// Device the chip belongs to (PCI slot, nvme controller, ...) when the
    /// chip's `device` symlink resolves to one.
    pub device: Option<String>,
    /// Kernel-supplied label when the chip exposes one, else the attribute
    /// name (`temp1_input` -> `temp1`).
    pub label: String,
    /// Sysfs path the value came from. Useful for diagnostics and for a
    /// caller that wants to re-read one sensor.
    pub path: String,
    pub kind: SensorKind,
    pub value: f64,
    pub unit: SensorUnit,
    /// `*_max`, in the same unit.
    pub max: Option<f64>,
    /// `*_crit`, in the same unit.
    pub crit: Option<f64>,
    /// (`*_alarm`, `*_crit_alarm`) when the chip reports them.
    pub alarm: Option<bool>,
}

/// NVML telemetry for one NVIDIA adapter.
///
/// Every field is optional because NVML reports per-adapter capability: a
/// laptop dGPU may expose no fan, and a vGPU exposes no serial/PCI ids.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuSensors {
    /// NVML device index (its enumeration order, not a DRM card number).
    pub index: u32,
    /// Normalized PCI slot (`0000:01:00.0`), the join key with DRM.
    pub pci_slot: Option<String>,
    pub name: Option<String>,
    pub driver_version: Option<String>,
    /// Adapter-wide GPU utilization in percent.
    pub utilization_pct: Option<f32>,
    /// Memory-controller utilization in percent.
    pub memory_utilization_pct: Option<f32>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    /// GPU die temperature in °C (NVML sensor 0).
    pub temperature_c: Option<f32>,
    /// Memory-junction temperature in °C (NVML sensor 1), where supported.
    pub memory_temperature_c: Option<f32>,
    pub fan_pct: Option<f32>,
    pub power_w: Option<f32>,
    /// Enforced power limit in W.
    pub power_limit_w: Option<f32>,
    pub graphics_clock_mhz: Option<u32>,
    pub memory_clock_mhz: Option<u32>,
    pub pcie_link_generation: Option<u32>,
    pub pcie_link_width: Option<u32>,
    /// Raw `nvmlDeviceGetCurrentClocksThrottleReasons` bits; `None` when the
    /// driver refused the query. Bit meanings are driver-version specific, so
    /// the raw mask is carried and no state is invented.
    pub throttle_reasons: Option<u64>,
}

/// hwmon attribute file names are `<kind><index>_<attribute>`.
///
/// Splits `temp2_crit` into (`temp2`, `crit`), which is what the collector
/// needs to pair an input with its limits and label.
pub fn split_attribute(file_name: &str) -> Option<(&str, &str)> {
    let (stem, attribute) = file_name.rsplit_once('_')?;
    if stem.is_empty() || attribute.is_empty() {
        return None;
    }
    Some((stem, attribute))
}

/// The sensor stem of one hwmon file name: `temp1_input` -> `temp1`.
pub fn sensor_stem(file_name: &str) -> Option<&str> {
    let (stem, attribute) = split_attribute(file_name)?;
    matches!(
        attribute,
        "input" | "max" | "min" | "crit" | "crit_hyst" | "lcrit" | "alarm" | "crit_alarm" | "label"
    )
    .then_some(stem)
}

/// hwmon file-name prefix -> sensor kind, for the stems the collector keeps.
pub fn kind_of_stem(stem: &str) -> Option<SensorKind> {
    for (prefix, kind) in [
        ("temp", SensorKind::Temperature),
        ("fan", SensorKind::Fan),
        ("in", SensorKind::Voltage),
        ("curr", SensorKind::Current),
        ("power", SensorKind::Power),
        ("energy", SensorKind::Energy),
        ("humidity", SensorKind::Humidity),
        ("intrusion", SensorKind::Intrusion),
    ] {
        let Some(rest) = stem.strip_prefix(prefix) else {
            continue;
        };
        if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(kind);
        }
    }
    None
}

/// Sort key of one hwmon attribute file: kind, then the hardware index, then
/// the attribute, so `temp1_input` precedes `temp1_label` and `temp2_input`
/// precedes `temp10_input`.
///
/// A snapshot ordered by this key lists every chip the way the kernel numbers
/// its sensors instead of the way the filesystem happens to return dirents.
pub fn attribute_order(file: &str) -> (SensorKind, u32, String) {
    let Some((stem, attribute)) = split_attribute(file) else {
        return (SensorKind::Other, u32::MAX, file.to_string());
    };
    let kind = kind_of_stem(stem).unwrap_or(SensorKind::Other);
    let index = stem
        .strip_prefix(kind.prefix())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(u32::MAX);
    (kind, index, attribute.to_string())
}

/// Convert one raw hwmon integer to base units.
///
/// hwmon publishes fixed-point integers: millidegrees Celsius, millivolts,
/// milliamperes, microwatts, microjoules. A driver that has no valid sample
/// may expose a sentinel (`i64::MAX`, a large negative, or a temperature far
/// outside any real die), so each kind is range-checked here and an implausible
/// value becomes unavailable instead of a bogus reading.
pub fn scale_hwmon(kind: SensorKind, raw: i64) -> Option<f64> {
    let value = raw as f64;
    let scaled = match kind {
        SensorKind::Temperature => value / 1000.0,
        SensorKind::Fan => value,
        SensorKind::Voltage => value / 1000.0,
        SensorKind::Current => value / 1000.0,
        SensorKind::Power => value / 1_000_000.0,
        SensorKind::Energy => value / 1_000_000.0,
        SensorKind::Humidity => value / 1000.0,
        SensorKind::Intrusion => value,
        SensorKind::Other => return None,
    };
    if !scaled.is_finite() {
        return None;
    }
    let plausible = match kind {
        SensorKind::Temperature => (-20.0..=200.0).contains(&scaled),
        // 0xffff is how a chip says "there is no tachometer", and the fastest
        // server fans spin at about 25,000 RPM, so a higher answer is a
        // sentinel rather than a measurement.
        SensorKind::Fan => (0.0..=30_000.0).contains(&scaled),
        SensorKind::Voltage => (-5.0..=30.0).contains(&scaled),
        SensorKind::Current => (-200.0..=200.0).contains(&scaled),
        SensorKind::Power => (0.0..=10_000.0).contains(&scaled),
        SensorKind::Energy => (0.0..=1e15).contains(&scaled),
        SensorKind::Humidity => (0.0..=100.0).contains(&scaled),
        SensorKind::Intrusion => true,
        SensorKind::Other => false,
    };
    plausible.then_some(scaled)
}

/// Unit a kind is scaled to by [`scale_hwmon`].
pub fn unit_of_kind(kind: SensorKind) -> SensorUnit {
    match kind {
        SensorKind::Temperature => SensorUnit::Celsius,
        SensorKind::Fan => SensorUnit::Rpm,
        SensorKind::Voltage => SensorUnit::Volt,
        SensorKind::Current => SensorUnit::Ampere,
        SensorKind::Power => SensorUnit::Watt,
        SensorKind::Energy => SensorUnit::Joule,
        SensorKind::Humidity => SensorUnit::Percent,
        SensorKind::Intrusion => SensorUnit::Unknown,
        SensorKind::Other => SensorUnit::Unknown,
    }
}

/// Decode an hwmon alarm attribute: `0` clear, `1` asserted.
///
/// Chips publish the flag as a small integer. Anything else is not an alarm
/// this model can render, so it stays unknown instead of guessing a state.
pub fn parse_alarm(text: &str) -> Option<bool> {
    match text.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// The device a hwmon chip sits on, from its `device` symlink target.
///
/// `/sys/class/hwmon/hwmon3/device -> ../../../0000:01:00.0` names the PCI
/// device the chip belongs to; a platform chip links to something like
/// `/sys/devices/platform/coretemp.0`. Only the last component is kept: that is
/// the part a user recognizes, and the rest is kernel topology. A target
/// without one (`.`, `..`, `/`) yields `None`.
pub fn hwmon_device_label(target: &str) -> Option<String> {
    let target = target.trim().trim_end_matches('/');
    let label = target.rsplit('/').next()?;
    (!matches!(label, "" | "." | "..")).then(|| label.to_string())
}

/// Normalize a PCI slot for joining NVML (`00000000:01:00.0`), DRM's
/// `/proc/driver/nvidia/gpus/<slot>` (`0000:01:00.0`) and a hwmon device
/// path (`0000:01:00.0`).
pub fn normalize_pci_slot(slot: &str) -> String {
    let slot = slot.trim().trim_matches(['[', ']']).to_ascii_lowercase();
    let Some((domain, rest)) = slot.split_once(':') else {
        return slot;
    };
    // A four-digit domain is the kernel's form; NVML publishes eight hex
    // digits. Both collapse to the low four digits, so the join works across
    // PCI domains up to 0xffff (more than Linux assigns).
    //
    // The domain is driver text, not necessarily ASCII: `nvml_text` only
    // requires valid UTF-8, so `len() - 4` can land inside a multi-byte
    // character. `get` answers `None` there and the whole domain is kept, which
    // cannot join wrongly.
    let domain = if domain.len() > 4 {
        domain.get(domain.len() - 4..).unwrap_or(domain)
    } else {
        domain
    };
    format!("{domain}:{rest}")
}

/// Read the driver's text out of a fixed NVML buffer.
///
/// NVML fills caller-owned buffers with NUL-terminated text. A buffer with
/// nothing before its terminator, or with bytes that are not UTF-8, yields
/// `None` rather than an empty or garbled name. A buffer the driver filled
/// completely carries no terminator; what it wrote is still readable.
pub fn nvml_text(buffer: &[u8]) -> Option<String> {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let text = std::str::from_utf8(&buffer[..end]).ok()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// NVML reports utilization, memory-controller load and fan speed as integer
/// percent.
///
/// A measured idle adapter really is 0 %, so zero stays a sample; above 100 is
/// a driver quirk no field can carry.
pub fn nvml_percent(raw: u32) -> Option<f32> {
    nvml_bounded(raw, 100).map(|pct| pct as f32)
}

/// NVML power counters are milliwatts; the model carries watts.
pub fn nvml_milliwatts_to_watts(raw: u32) -> Option<f32> {
    let watts = raw as f64 / 1000.0;
    (watts <= 2_000.0).then_some(watts as f32)
}

/// NVML temperatures are unsigned degrees Celsius.
///
/// No GPU sensor reports 0 °C: drivers that cannot sample the die answer 0, so
/// that value (and anything past a junction limit) stays unavailable.
pub fn nvml_temperature_c(raw: u32) -> Option<f32> {
    (1..=150).contains(&raw).then_some(raw as f32)
}

/// NVML clocks are MHz, and 0 is a real answer (a gated clock).
pub fn nvml_clock_mhz(raw: u32) -> Option<u32> {
    nvml_bounded(raw, 10_000)
}

/// Sort key of one powercap zone name: `0` is `(0, None)`, `0:2` is
/// `(0, Some(2))`.
///
/// The RAPL tree nests a package's own counter (`intel-rapl:0`) beside its
/// constraints (`intel-rapl:0:2` is `dram`), and the suffixes share the package
/// number. The pair keeps every zone of one package together in the kernel's
/// order: the package's own counter comes first (`None` sorts before any
/// constraint, and it cannot tie with constraint 0), then its constraints in
/// index order. A name with a third segment or a non-numeric one is not a zone
/// this model reads.
pub fn powercap_zone_index(suffix: &str) -> Option<(u32, Option<u32>)> {
    let mut segments = suffix.split(':');
    let package: u32 = segments.next()?.parse().ok()?;
    let constraint = match segments.next() {
        Some(segment) => Some(segment.parse().ok()?),
        None => None,
    };
    segments.next().is_none().then_some((package, constraint))
}

/// A small NVML counter, or `None` when it is outside the range the field can
/// physically mean (a link width or generation saturated to `u32::MAX` is a
/// driver sentinel, not a link).
pub fn nvml_bounded(raw: u32, max: u32) -> Option<u32> {
    (raw <= max).then_some(raw)
}

/// Fill the adapter measurements sysfs does not publish from NVML readings.
///
/// The join key is [`normalize_pci_slot`], which folds NVML's eight-digit PCI
/// domain onto the kernel's four, so a card in one domain can never borrow the
/// readings of a card with the same bus in another. Sysfs wins per measured
/// pair: a pair sysfs half-measured (a VRAM total with no used counter) is
/// replaced whole, because NVML's pair is a measurement while sysfs's half is
/// not. An adapter NVML does not describe keeps every field unavailable, and an
/// adapter DRM did not enumerate is never created here.
///
/// `nvidia` is the adapter whose driver publishes no counter at all; any other
/// adapter is consulted only when its utilization counter is missing.
pub fn with_nvml(
    mut gpu: GpuInfo,
    slot: Option<&str>,
    nvidia: bool,
    nvml: &[GpuSensors],
) -> GpuInfo {
    // Nothing to join, or nothing this adapter is missing.
    let needs_nvml = nvidia || !gpu.util_pct_available;
    if nvml.is_empty() || !needs_nvml {
        return gpu;
    }
    let Some(slot) = slot else {
        return gpu;
    };
    let slot = normalize_pci_slot(slot);
    let Some(readings) = nvml
        .iter()
        .find(|readings| readings.pci_slot.as_deref() == Some(slot.as_str()))
    else {
        return gpu;
    };
    if !gpu.util_pct_available
        && let Some(pct) = readings.utilization_pct
    {
        gpu.util_pct = pct;
        gpu.util_pct_available = true;
    }
    if !gpu.dedicated_mem_available
        && let (Some(used), Some(total)) = (readings.memory_used_bytes, readings.memory_total_bytes)
        && total > 0
        && used <= total
    {
        gpu.mem_used_bytes = used;
        gpu.mem_total_bytes = total;
        gpu.dedicated_used_bytes = used;
        gpu.dedicated_mem_available = true;
    }
    if gpu.temperature_c.is_none() {
        gpu.temperature_c = readings.temperature_c;
    }
    gpu
}

/// Whether the brokered sensor fallback may ask the service for readings.
///
/// The fallback exists for the readings this token cannot take (an NVIDIA user
/// outside the `video` group). Asking is paced by a background worker, and this
/// gate is what keeps that worker from turning a decision into a poll: a
/// transport failure is transient and retried, while a refusal is an answer
/// about this client that does not change by asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerGate {
    /// Ask the broker.
    Ask,
    /// The broker answered and refused (or answered something unusable).
    /// Asking resumes only after the broker has been seen unreachable and then
    /// reachable again, because a restarted or reinstalled service is a
    /// different peer than the one that refused.
    Refused {
        /// Whether the liveness probe has reported the broker gone since the
        /// refusal.
        saw_unreachable: bool,
    },
    /// The fallback cannot run in this process (its worker could not be
    /// started). Asking stays off for the process's lifetime: there is nothing
    /// left that could ask.
    Disabled,
}

impl BrokerGate {
    /// Fold one liveness observation into the gate.
    ///
    /// A broker that has been seen gone and then answers again is a different
    /// peer — restarted, reinstalled, or started after the GUI — so a refusal
    /// recorded before that no longer applies.
    ///
    /// `liveness` is `None` when the probe is still pending, which is no
    /// observation at all: it neither proves the broker gone (that would let a
    /// latched gate re-ask and re-latch) nor proves it back.
    pub fn observe(self, liveness: Option<BrokerLiveness>) -> Self {
        match (self, liveness) {
            (Self::Ask | Self::Disabled, _) => self,
            (
                Self::Refused {
                    saw_unreachable: true,
                },
                Some(BrokerLiveness::Reachable),
            ) => Self::Ask,
            (Self::Refused { saw_unreachable }, liveness) => Self::Refused {
                saw_unreachable: saw_unreachable || liveness == Some(BrokerLiveness::Unreachable),
            },
        }
    }

    /// Whether a request may be sent now.
    pub fn may_ask(self) -> bool {
        matches!(self, Self::Ask)
    }

    /// One window of the brokered fallback, as one pure step.
    ///
    /// `liveness` is this window's observation of the broker (`None` while the
    /// probe is still pending), and `last` is what the previous window's request
    /// produced (`None` when it did not ask). The result is the gate to carry
    /// into the next window and whether this window should ask at all.
    ///
    /// A request that never reached a broker is transient, so the next window
    /// asks again. A refusal latches: it is a decision about this client, and
    /// repeating it every window would only spam the log until the broker is seen
    /// gone and back (or the process restarts).
    pub fn step(
        self,
        liveness: Option<BrokerLiveness>,
        last: Option<BrokerAttempt>,
    ) -> (Self, bool) {
        // A fallback that cannot run keeps its state whatever the wire says.
        if self == Self::Disabled {
            return (Self::Disabled, false);
        }
        let gate = self.observe(liveness);
        let gate = match last {
            None | Some(BrokerAttempt::Answered | BrokerAttempt::Unreachable) => gate,
            Some(BrokerAttempt::Refused) => Self::Refused {
                saw_unreachable: false,
            },
        };
        (gate, gate.may_ask())
    }
}

/// Whether the broker answered a liveness observation.
///
/// What it answered does not matter: an answered probe, even a refusal, proves
/// the service is there, while a probe that reached nobody proves it is not. A
/// probe that is still pending is neither, and is passed to [`BrokerGate`] as
/// `None` rather than as a value here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerLiveness {
    /// The broker answered.
    Reachable,
    /// No broker answered.
    Unreachable,
}

/// What one window's request to the broker produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerAttempt {
    /// The broker answered with readings.
    Answered,
    /// The broker answered and refused, or answered something unusable.
    Refused,
    /// The request never reached a broker.
    Unreachable,
}

/// Decode NVML's inline `busId` buffer into a normalized PCI slot.
///
/// The buffer carries ASCII like `00000000:01:00.0`; anything that does not
/// have that exact shape yields `None` rather than a wrong join.
pub fn nvml_bus_id_to_pci_slot(buffer: &[u8]) -> Option<String> {
    let text = nvml_text(buffer)?;
    let normalized = normalize_pci_slot(&text);
    // `normalize_pci_slot` only strips a domain; a bus id must still look like
    // one (`dddd:dd:dd.d`).
    let (domain, rest) = normalized.split_once(':')?;
    let (bus, function) = rest.split_once('.')?;
    let (device, _) = bus.split_once(':')?;
    let shape_ok = domain.len() == 4
        && device.len() == 2
        && function.len() == 1
        && domain.chars().all(|ch| ch.is_ascii_hexdigit())
        && device.chars().all(|ch| ch.is_ascii_hexdigit())
        && function.chars().all(|ch| ch.is_ascii_digit());
    shape_ok.then_some(normalized)
}

// ------------------------------------------------------------- NVML ABI

/// NVML device utilization (`nvmlUtilization_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NvmlUtilization {
    /// GPU (SM) utilization in percent.
    pub gpu: u32,
    /// Memory controller utilization in percent.
    pub memory: u32,
}

/// NVML memory totals (`nvmlMemory_t`), in bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NvmlMemory {
    pub total: u64,
    pub free: u64,
    pub used: u64,
}

/// NVML PCI information (`nvmlPciInfo_t`, filled by
/// `nvmlDeviceGetPciInfo_v3`).
///
/// The layout is the driver's, not ours: the legacy 16-byte id comes first,
/// then five `unsigned int` fields, then the 32-byte modern id. Getting this
/// order wrong would silently join a GPU to the wrong DRM card, so the field
/// order is pinned by a size/offset test below.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NvmlPciInfo {
    pub bus_id_legacy: [u8; 16],
    pub domain: u32,
    pub bus: u32,
    pub device: u32,
    pub pci_device_id: u32,
    pub pci_sub_system_id: u32,
    pub bus_id: [u8; 32],
}

impl NvmlPciInfo {
    /// The modern bus id when the driver filled it, else the legacy buffer.
    pub fn slot(&self) -> Option<String> {
        nvml_bus_id_to_pci_slot(&self.bus_id)
            .or_else(|| nvml_bus_id_to_pci_slot(&self.bus_id_legacy))
    }
}

/// NVML temperature sensor ids.
pub const NVML_TEMPERATURE_GPU: u32 = 0;
/// NVML temperature sensor id for the memory junction where supported.
pub const NVML_TEMPERATURE_MEMORY: u32 = 1;

/// NVML clock ids used for the two clocks a GPU page shows.
pub const NVML_CLOCK_GRAPHICS: u32 = 0;
pub const NVML_CLOCK_MEM: u32 = 2;

/// NVML return code for success.
pub const NVML_SUCCESS: i32 = 0;
/// NVML return code for "not supported by this device/driver". An ordinary
/// per-adapter answer: no fan, no PCI id, no memory-junction sensor.
pub const NVML_ERROR_NOT_SUPPORTED: i32 = 3;
/// NVML return code for "this token may not query the device". The one failure
/// a root broker can supply readings for.
pub const NVML_ERROR_NO_PERMISSION: i32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_names_split_into_stem_and_attribute() {
        assert_eq!(split_attribute("temp1_input"), Some(("temp1", "input")));
        assert_eq!(split_attribute("temp1_crit"), Some(("temp1", "crit")));
        assert_eq!(split_attribute("no_underscore"), None);
        assert_eq!(split_attribute("_input"), None);
        assert_eq!(split_attribute("temp1_"), None);
    }

    #[test]
    fn sensor_stems_accept_inputs_limits_and_labels_only() {
        for name in [
            "temp1_input",
            "fan2_input",
            "in0_max",
            "power1_crit",
            "temp3_label",
        ] {
            assert!(sensor_stem(name).is_some(), "{name}");
        }
        for name in ["temp1_offset", "name", "uevent", "temp1_input_extra"] {
            assert!(sensor_stem(name).is_none(), "{name}");
        }
    }

    #[test]
    fn stem_kinds_require_a_numeric_suffix() {
        assert_eq!(kind_of_stem("temp1"), Some(SensorKind::Temperature));
        assert_eq!(kind_of_stem("fan2"), Some(SensorKind::Fan));
        assert_eq!(kind_of_stem("in0"), Some(SensorKind::Voltage));
        assert_eq!(kind_of_stem("curr3"), Some(SensorKind::Current));
        assert_eq!(kind_of_stem("power1"), Some(SensorKind::Power));
        assert_eq!(kind_of_stem("energy1"), Some(SensorKind::Energy));
        assert_eq!(kind_of_stem("humidity1"), Some(SensorKind::Humidity));
        assert_eq!(kind_of_stem("temp"), None);
        assert_eq!(kind_of_stem("tempx"), None);
        assert_eq!(kind_of_stem("inlet1"), None);
    }

    #[test]
    fn hwmon_values_scale_into_base_units() {
        assert_eq!(scale_hwmon(SensorKind::Temperature, 51_250), Some(51.25));
        assert_eq!(scale_hwmon(SensorKind::Fan, 1450), Some(1450.0));
        assert_eq!(scale_hwmon(SensorKind::Voltage, 1_050), Some(1.05));
        assert_eq!(scale_hwmon(SensorKind::Current, 2_500), Some(2.5));
        assert_eq!(scale_hwmon(SensorKind::Power, 25_000_000), Some(25.0));
        assert_eq!(scale_hwmon(SensorKind::Energy, 4_000_000), Some(4.0));
    }

    #[test]
    fn hwmon_sentinels_are_unavailable_not_zero() {
        assert_eq!(scale_hwmon(SensorKind::Temperature, i64::MAX), None);
        assert_eq!(scale_hwmon(SensorKind::Temperature, -273_000), None);
        assert_eq!(scale_hwmon(SensorKind::Fan, -1), None);
        // 0xffff is "this chip has no tachometer": 65,535 RPM is not a fan.
        assert_eq!(scale_hwmon(SensorKind::Fan, 0xffff), None);
        assert_eq!(scale_hwmon(SensorKind::Fan, 30_001), None);
        assert_eq!(scale_hwmon(SensorKind::Fan, 25_000), Some(25_000.0));
        assert_eq!(scale_hwmon(SensorKind::Voltage, 1_000_000), None);
        assert_eq!(scale_hwmon(SensorKind::Power, -1_000_000), None);
        assert_eq!(scale_hwmon(SensorKind::Humidity, 250_000), None);
    }

    #[test]
    fn units_follow_the_kind() {
        assert_eq!(unit_of_kind(SensorKind::Temperature), SensorUnit::Celsius);
        assert_eq!(unit_of_kind(SensorKind::Power), SensorUnit::Watt);
        assert_eq!(unit_of_kind(SensorKind::Intrusion), SensorUnit::Unknown);
    }

    #[test]
    fn attribute_order_numbers_sensors_numerically() {
        let mut files = [
            "temp10_input",
            "temp2_label",
            "fan1_input",
            "temp2_input",
            "temp1_input",
        ];
        files.sort_by_key(|file| attribute_order(file));
        assert_eq!(
            files,
            [
                "temp1_input",
                "temp2_input",
                "temp2_label",
                "temp10_input",
                "fan1_input",
            ]
        );
        // A name outside the model sorts last instead of panicking.
        assert_eq!(
            attribute_order("uevent"),
            (SensorKind::Other, u32::MAX, "uevent".to_string())
        );
    }

    #[test]
    fn hwmon_alarm_flags_need_a_real_binary_value() {
        assert_eq!(parse_alarm("1\n"), Some(true));
        assert_eq!(parse_alarm("0\n"), Some(false));
        assert_eq!(parse_alarm(""), None);
        assert_eq!(parse_alarm("2"), None);
        assert_eq!(parse_alarm("alarm"), None);
    }

    #[test]
    fn hwmon_device_labels_keep_the_last_path_component() {
        assert_eq!(
            hwmon_device_label("../../../0000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            hwmon_device_label("/sys/devices/platform/coretemp.0/").as_deref(),
            Some("coretemp.0")
        );
        assert_eq!(
            hwmon_device_label("../../../nvme/nvme0").as_deref(),
            Some("nvme0")
        );
        // Nothing to name: the chip's own class directory, an empty target, or
        // a relative step upward.
        for target in ["", "/", ".", ".."] {
            assert_eq!(hwmon_device_label(target), None, "{target}");
        }
    }

    #[test]
    fn pci_slots_normalize_across_kernel_and_nvml_forms() {
        assert_eq!(normalize_pci_slot("0000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_pci_slot("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_pci_slot("[0000:0A:00.0]"), "0000:0a:00.0");
        assert_eq!(normalize_pci_slot("00010000:01:00.0"), "0000:01:00.0");
    }

    #[test]
    fn a_multi_byte_domain_is_kept_instead_of_panicking() {
        // NVML text is only required to be UTF-8, so a driver string can put the
        // low-four-digit cut inside a character. The whole domain is kept, which
        // at worst fails to join; it must not panic or produce a broken string.
        assert_eq!(normalize_pci_slot("0é000:01:00.0"), "0é000:01:00.0");
        assert_eq!(normalize_pci_slot("éé:01:00.0"), "éé:01:00.0");
        // A cut that lands on a character boundary still folds as before.
        assert_eq!(normalize_pci_slot("é0000:01:00.0"), "0000:01:00.0");
    }

    #[test]
    fn nvml_bus_ids_need_a_real_slot_shape() {
        let mut buffer = [0u8; 32];
        buffer[..16].copy_from_slice(b"00000000:01:00.0");
        assert_eq!(
            nvml_bus_id_to_pci_slot(&buffer).as_deref(),
            Some("0000:01:00.0")
        );

        assert_eq!(nvml_bus_id_to_pci_slot(&[0u8; 32]), None);
        assert_eq!(nvml_bus_id_to_pci_slot(b"garbage"), None);
        let mut junk = [0u8; 32];
        junk[..8].copy_from_slice(b"1:2:3.4\0");
        assert_eq!(nvml_bus_id_to_pci_slot(&junk), None);
    }

    #[test]
    fn nvml_buffer_text_stops_at_the_terminator() {
        let mut name = [0u8; 16];
        name[..14].copy_from_slice(b" NVIDIA GeForc");
        assert_eq!(nvml_text(&name).as_deref(), Some("NVIDIA GeForc"));
        assert_eq!(nvml_text(&[0u8; 16]), None);
        assert_eq!(nvml_text(&[0xff, 0, 0]), None);
        // A driver that fills the whole buffer leaves no terminator; what it
        // wrote is still the name.
        assert_eq!(nvml_text(b"580.95.05").as_deref(), Some("580.95.05"));
    }

    #[test]
    fn nvml_counters_stay_within_what_the_field_can_mean() {
        assert_eq!(nvml_percent(0), Some(0.0));
        assert_eq!(nvml_percent(100), Some(100.0));
        assert_eq!(nvml_percent(101), None);
        assert_eq!(nvml_percent(u32::MAX), None);
        assert_eq!(nvml_milliwatts_to_watts(0), Some(0.0));
        assert_eq!(nvml_milliwatts_to_watts(250_000), Some(250.0));
        assert_eq!(nvml_milliwatts_to_watts(u32::MAX), None);
        assert_eq!(nvml_temperature_c(51), Some(51.0));
        // 0 °C is what a driver without a die sample answers, not a reading.
        assert_eq!(nvml_temperature_c(0), None);
        assert_eq!(nvml_temperature_c(u32::MAX), None);
        assert_eq!(nvml_clock_mhz(0), Some(0));
        assert_eq!(nvml_clock_mhz(2_640), Some(2_640));
        assert_eq!(nvml_clock_mhz(u32::MAX), None);
        assert_eq!(nvml_bounded(16, 16), Some(16));
        assert_eq!(nvml_bounded(17, 16), None);
    }

    #[test]
    fn nvml_pci_info_layout_is_pinned() {
        // The driver writes this struct; a field-order change would shift the
        // bus id and join a GPU to the wrong DRM card.
        assert_eq!(std::mem::size_of::<NvmlPciInfo>(), 16 + 20 + 32);
        assert_eq!(std::mem::align_of::<NvmlPciInfo>(), 4);
        assert_eq!(std::mem::size_of::<NvmlUtilization>(), 8);
        assert_eq!(std::mem::size_of::<NvmlMemory>(), 24);
    }

    #[test]
    fn pci_info_prefers_the_modern_bus_id() {
        let mut info = NvmlPciInfo::default();
        // A 16-byte NUL-padded buffer: the literal is 12 bytes plus 4 NULs.
        info.bus_id_legacy[..16].copy_from_slice(b"0000:02:00.0\0\0\0\0");
        info.bus_id[..16].copy_from_slice(b"00000000:01:00.0");
        assert_eq!(info.slot().as_deref(), Some("0000:01:00.0"));
    }

    #[test]
    fn snapshot_joins_gpu_readings_by_slot() {
        let snapshot = SensorSnapshot {
            gpus: vec![GpuSensors {
                pci_slot: Some("0000:01:00.0".into()),
                index: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(snapshot.gpu_for_pci_slot("00000000:01:00.0").is_some());
        assert!(snapshot.gpu_for_pci_slot("0000:02:00.0").is_none());
        assert!(!snapshot.is_empty());
    }

    /// One NVML reading for `slot`, as the enumeration shapes it.
    fn nvml_reading(slot: &str) -> GpuSensors {
        GpuSensors {
            index: 0,
            pci_slot: Some(normalize_pci_slot(slot)),
            utilization_pct: Some(37.0),
            memory_total_bytes: Some(12 << 30),
            memory_used_bytes: Some(3 << 30),
            temperature_c: Some(63.0),
            ..Default::default()
        }
    }

    /// The row an adapter with sysfs counters gets from DRM: measured pairs are
    /// available, the rest is not.
    fn gpu_row() -> GpuInfo {
        GpuInfo {
            id: 0,
            name: "NVIDIA GPU [10de:2684] (nvidia)".into(),
            driver_version: "580.95.05".into(),
            driver_date_epoch_s: None,
            directx_version: String::new(),
            pci_location: Some((3, 0, 1)),
            util_pct_available: false,
            util_pct: 0.0,
            dedicated_mem_available: false,
            shared_mem_available: false,
            mem_used_bytes: 0,
            mem_total_bytes: 0,
            dedicated_used_bytes: 0,
            shared_used_bytes: 0,
            shared_total_bytes: 0,
            hardware_reserved_bytes: 0,
            temperature_c: None,
            luid: None,
            engines: Vec::new(),
        }
    }

    #[test]
    fn nvml_fills_the_counters_the_nvidia_driver_does_not_publish() {
        let gpu = with_nvml(
            gpu_row(),
            Some("0000:03:00.1"),
            true,
            &[nvml_reading("0000:03:00.1")],
        );
        assert!(gpu.util_pct_available);
        assert_eq!(gpu.util_pct, 37.0);
        assert!(gpu.dedicated_mem_available);
        assert_eq!(gpu.mem_used_bytes, 3 << 30);
        assert_eq!(gpu.mem_total_bytes, 12 << 30);
        assert_eq!(gpu.dedicated_used_bytes, 3 << 30);
        assert_eq!(gpu.temperature_c, Some(63.0));
        // An adapter-wide utilization is not an engine instance.
        assert!(gpu.engines.is_empty());
    }

    #[test]
    fn sysfs_counters_win_over_nvml_where_the_driver_publishes_them() {
        // nouveau answers `gpu_busy_percent`, so its own sample is shown even
        // with NVML answering for the same card.
        let mut gpu = gpu_row();
        gpu.util_pct_available = true;
        gpu.util_pct = 12.0;
        gpu.dedicated_mem_available = true;
        gpu.mem_used_bytes = 512 << 20;
        gpu.mem_total_bytes = 8 << 30;
        gpu.dedicated_used_bytes = 512 << 20;
        gpu.temperature_c = Some(51.0);
        let gpu = with_nvml(
            gpu,
            Some("0000:03:00.1"),
            true,
            &[nvml_reading("0000:03:00.1")],
        );
        assert!(gpu.util_pct_available);
        assert_eq!(gpu.util_pct, 12.0);
        assert_eq!(gpu.mem_used_bytes, 512 << 20);
        assert_eq!(gpu.mem_total_bytes, 8 << 30);
        assert_eq!(gpu.temperature_c, Some(51.0));
    }

    #[test]
    fn a_half_measured_vram_pair_is_replaced_whole() {
        // gpu_from_card publishes a total it read without a used counter; the
        // pair stays unavailable, and NVML's complete pair replaces both.
        let mut gpu = gpu_row();
        gpu.mem_total_bytes = 8 << 30;
        let gpu = with_nvml(
            gpu,
            Some("0000:03:00.1"),
            true,
            &[nvml_reading("0000:03:00.1")],
        );
        assert!(gpu.dedicated_mem_available);
        assert_eq!(gpu.mem_total_bytes, 12 << 30);
        assert_eq!(gpu.mem_used_bytes, 3 << 30);
        assert_eq!(gpu.dedicated_used_bytes, 3 << 30);
    }

    #[test]
    fn nvml_silence_leaves_the_measurements_unavailable() {
        // NVML enumerated nothing this tick.
        let gpu = with_nvml(gpu_row(), Some("0000:03:00.1"), true, &[]);
        assert!(!gpu.util_pct_available);
        assert_eq!(gpu.util_pct, 0.0);
        assert!(!gpu.dedicated_mem_available);
        assert_eq!(gpu.mem_used_bytes, 0);
        assert_eq!(gpu.temperature_c, None);

        // NVML answered, but only for another card: the join must not borrow
        // that adapter's numbers.
        let gpu = with_nvml(
            gpu_row(),
            Some("0000:03:00.1"),
            true,
            &[nvml_reading("0000:02:00.0")],
        );
        assert!(!gpu.util_pct_available);
        assert_eq!(gpu.util_pct, 0.0);
        assert!(!gpu.dedicated_mem_available);
        assert_eq!(gpu.temperature_c, None);
    }

    #[test]
    fn a_card_in_another_pci_domain_does_not_join() {
        // The same bus, device and function in segment 1 is a different card;
        // folding the domain away would have the two share one adapter's
        // readings.
        let gpu = with_nvml(
            gpu_row(),
            Some("0000:03:00.1"),
            true,
            &[nvml_reading("00000001:03:00.1")],
        );
        assert!(!gpu.util_pct_available);
        assert!(!gpu.dedicated_mem_available);
        assert_eq!(gpu.temperature_c, None);
        // The same reading joins the card that really sits in segment 1.
        let gpu = with_nvml(
            gpu_row(),
            Some("0001:03:00.1"),
            true,
            &[nvml_reading("00000001:03:00.1")],
        );
        assert!(gpu.util_pct_available);
    }

    #[test]
    fn nvml_memory_needs_a_total_to_be_a_measurement() {
        let nvml = GpuSensors {
            pci_slot: Some("0000:03:00.1".into()),
            memory_used_bytes: Some(1 << 30),
            ..Default::default()
        };
        let gpu = with_nvml(gpu_row(), Some("0000:03:00.1"), true, &[nvml]);
        assert!(!gpu.dedicated_mem_available);
        assert_eq!(gpu.mem_used_bytes, 0);
        assert_eq!(gpu.mem_total_bytes, 0);
    }

    #[test]
    fn an_adapter_without_a_pci_slot_cannot_be_joined() {
        let gpu = with_nvml(gpu_row(), None, true, &[nvml_reading("0000:03:00.1")]);
        assert!(!gpu.util_pct_available);
        assert!(!gpu.dedicated_mem_available);
    }

    #[test]
    fn a_broker_refusal_stops_the_fallback_until_it_restarts() {
        // First window: nothing was attempted yet, so it asks.
        let (gate, ask) = BrokerGate::Ask.step(Some(BrokerLiveness::Unreachable), None);
        assert!(ask, "a window that never asked must ask");
        // The request reached nobody: transient, so the next window asks again.
        let (gate, ask) = gate.step(
            BrokerLiveness::Unreachable,
            Some(BrokerAttempt::Unreachable),
        );
        assert!(ask);
        let (gate, ask) = gate.step(
            BrokerLiveness::Unreachable,
            Some(BrokerAttempt::Unreachable),
        );
        assert!(ask);
        assert_eq!(gate, BrokerGate::Ask);
        // Readings arrived: nothing latches.
        let (gate, ask) = gate.step(
            Some(BrokerLiveness::Reachable),
            Some(BrokerAttempt::Answered),
        );
        assert!(ask);
        // The broker refused: the very next window must not ask, and ten more
        // windows of a reachable broker must not reopen it.
        let (gate, ask) = gate.step(
            Some(BrokerLiveness::Reachable),
            Some(BrokerAttempt::Refused),
        );
        assert!(!ask, "a refusal must latch before the next ask");
        let mut latched = gate;
        for _ in 0..10 {
            let (next, ask) = latched.step(Some(BrokerLiveness::Reachable), None);
            assert!(!ask, "a refused policy does not change by asking again");
            latched = next;
        }
        // It went away: still latched, but remembered.
        let (latched, ask) = latched.step(Some(BrokerLiveness::Unreachable), None);
        assert!(!ask);
        // It answered again: a different peer, so ask once more.
        let (gate, ask) = latched.step(Some(BrokerLiveness::Reachable), None);
        assert!(ask, "a restarted broker reopens the fallback");
        assert_eq!(gate, BrokerGate::Ask);
    }

    #[test]
    fn an_unusable_answer_latches_without_being_retried() {
        // The broker answered a sensor poll with another payload: not a
        // transport failure, so the next window stays quiet.
        let (gate, ask) = BrokerGate::Ask.step(
            Some(BrokerLiveness::Reachable),
            Some(BrokerAttempt::Refused),
        );
        assert!(!ask);
        assert_eq!(
            gate,
            BrokerGate::Refused {
                saw_unreachable: false
            }
        );
        // A window that did not ask keeps whatever it had.
        let (gate, ask) = BrokerGate::Ask.step(Some(BrokerLiveness::Reachable), None);
        assert!(ask);
        assert_eq!(gate, BrokerGate::Ask);
    }

    #[test]
    fn a_pending_probe_is_not_an_observation() {
        // "Still finding out" is neither reachable nor unreachable: it must not
        // count as the broker being gone, which is what let a latched gate
        // re-ask and re-latch on every window.
        let (gate, ask) = BrokerGate::Ask.step(None, None);
        assert!(ask, "a pending probe leaves an asking gate asking");
        assert_eq!(gate, BrokerGate::Ask);
        // A latch survives any number of pending windows.
        let latched = BrokerGate::Refused {
            saw_unreachable: false,
        };
        let (gate, ask) = latched.step(None, None);
        assert!(!ask);
        assert_eq!(gate, latched);
        // Pending is not the "gone" half of a restart either: a gate that already
        // saw the broker leave stays latched until a probe really answers.
        let saw_gone = BrokerGate::Refused {
            saw_unreachable: true,
        };
        let (gate, ask) = saw_gone.step(None, None);
        assert!(!ask, "a pending probe cannot reopen a latch");
        assert_eq!(gate, saw_gone);
    }

    #[test]
    fn a_fallback_that_could_not_start_never_asks() {
        // The worker thread could not be spawned, so there is nothing left that
        // could ask: no observation and no outcome reopens it.
        let (gate, ask) = BrokerGate::Disabled.step(Some(BrokerLiveness::Reachable), None);
        assert!(!ask);
        let (gate, ask) = gate.step(Some(BrokerLiveness::Unreachable), None);
        assert!(!ask);
        let (gate, ask) = gate.step(
            Some(BrokerLiveness::Reachable),
            Some(BrokerAttempt::Answered),
        );
        assert!(!ask);
        assert_eq!(gate, BrokerGate::Disabled);
    }

    #[test]
    fn powercap_zones_include_the_package_constraints() {
        assert_eq!(powercap_zone_index("0"), Some((0, None)));
        assert_eq!(powercap_zone_index("0:2"), Some((0, Some(2))));
        assert_eq!(powercap_zone_index("1:0"), Some((1, Some(0))));
        // Not a zone this model reads.
        assert_eq!(powercap_zone_index("0:0:1"), None);
        assert_eq!(powercap_zone_index("package-0"), None);
        assert_eq!(powercap_zone_index(""), None);
        assert_eq!(powercap_zone_index("0:"), None);
        // A package's own counter comes first, then its constraints in index
        // order, and it cannot tie with constraint 0.
        let mut zones: [(u32, Option<u32>); 4] = [(1, None), (0, Some(2)), (0, None), (0, Some(1))];
        zones.sort();
        assert_eq!(zones, [(0, None), (0, Some(1)), (0, Some(2)), (1, None)]);
    }

    #[test]
    fn snapshot_serializes_without_fabricated_zeros() {
        let snapshot = SensorSnapshot {
            taken_at_unix_ms: 1_700_000_000_000,
            readings: vec![SensorReading {
                chip: "k10temp".into(),
                device: None,
                label: "Tctl".into(),
                path: "/sys/class/hwmon/hwmon0/temp1_input".into(),
                kind: SensorKind::Temperature,
                value: 47.5,
                unit: SensorUnit::Celsius,
                max: None,
                crit: Some(95.0),
                alarm: None,
            }],
            ..Default::default()
        };
        let text = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: SensorSnapshot = serde_json::from_str(&text).expect("parse");
        assert_eq!(decoded, snapshot);
        assert!(text.contains("\"value\":47.5"));
        assert!(!text.contains("\"fan"));
    }
}
