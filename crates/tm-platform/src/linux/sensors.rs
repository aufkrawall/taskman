//! Linux sensor collection: `/sys/class/hwmon`, `/sys/class/thermal`, RAPL
//! powercap and NVML.
//!
//! The parsers and the reading types live in [`crate::linux_sensors`], which
//! compiles and unit-tests on every host. This module is the part that touches
//! sysfs and the NVIDIA driver, and it is used from two sides:
//!
//! * the GUI's own sampler, for what an ordinary user can read (hwmon is
//!   world-readable on every mainstream distribution), and
//! * the root core service, which polls the same tree plus whatever needs
//!   privilege (RAPL energy counters, NVML device handles for a user outside
//!   the `video` group).
//!
//! This file is a stub in the foundation commit: the sensor task fills it in.
//! The public surface below is fixed.

use crate::linux_sensors::{GpuSensors, SensorSnapshot};

/// Poll every sensor this process can read.
///
/// Must not fail as a whole: a chip that cannot be read is recorded in
/// `unreadable`, and a sensor that has no value stays absent.
pub fn snapshot() -> SensorSnapshot {
    SensorSnapshot::default()
}

/// Direct (unelevated) NVML query for every adapter, from a short-lived cache.
///
/// `drm.rs` calls this once per tick to fill the NVIDIA fields sysfs does not
/// publish (utilization, VRAM use, temperature).
pub fn nvml_gpus() -> Vec<GpuSensors> {
    Vec::new()
}

/// NVML readings for one PCI slot, already normalized.
pub fn gpu_for_slot(_slot: &str) -> Option<GpuSensors> {
    None
}

/// Whether NVML (`libnvidia-ml.so.1`) could be loaded at all.
pub fn nvml_available() -> bool {
    false
}
