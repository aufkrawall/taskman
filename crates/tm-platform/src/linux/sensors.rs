//! Linux sensor collection: `/sys/class/hwmon`, `/sys/class/thermal`, RAPL
//! powercap and NVML.
//!
//! The parsers and the reading types live in [`crate::linux_sensors`], which
//! compiles and unit-tests on every host; the sysfs walk is in `hwmon` and the
//! dynamic NVML binding in `nvml`. This module assembles one snapshot and
//! decides whose GPU readings it carries:
//!
//! * the GUI's own sampler takes what an ordinary user can read (hwmon is
//!   world-readable on every mainstream distribution, and NVML works wherever
//!   `/dev/nvidiactl` is), and
//! * the root core service polls the same tree with privilege, which is where
//!   RAPL energy counters and an NVML handle for a user outside the `video`
//!   group come from.
//!
//! When the driver refuses the GUI token's own NVML queries, the snapshot asks
//! the core service for the same readings — at most once per cache window, with
//! the client's own bounded deadline, and never in a way that could disable the
//! direct path.

use super::core_service::client;
use super::{hwmon, nvml};
use crate::linux_core_service::{Request, Response};
use crate::linux_sensors::{GpuSensors, SensorSnapshot, normalize_pci_slot};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the broker may be asked for GPU readings.
///
/// One sampling tick asks once; asking a stopped or wedged broker on every tick
/// would make each tick wait for its deadline. The window matches the direct
/// NVML cache, so a tick that sees fresh direct readings never asks and a tick
/// that does not asks at most once.
const BROKER_TTL: Duration = nvml::CACHE_TTL;

/// Poll every sensor this process can read.
///
/// Must not fail as a whole: a chip that cannot be read is recorded in
/// `unreadable`, and a sensor that has no value stays absent.
pub fn snapshot() -> SensorSnapshot {
    let (readings, unreadable) = hwmon::collect();
    SensorSnapshot {
        taken_at_unix_ms: now_unix_ms(),
        readings,
        gpus: nvml_gpus(),
        unreadable,
    }
}

/// Direct (unelevated) NVML query for every adapter, from a short-lived cache.
///
/// `drm.rs` calls this once per tick to fill the NVIDIA fields sysfs does not
/// publish (utilization, VRAM use, temperature).
pub fn nvml_gpus() -> Vec<GpuSensors> {
    let direct = nvml::sample();
    if !direct.denied {
        return direct.gpus;
    }
    // The driver refused this token's queries — what a user outside the `video`
    // group and a container without the device node both look like. The broker
    // runs as root and can take the same readings; a refusal, a failure or an
    // absent broker leaves the direct answer in place, because the fallback must
    // never disable the path that works.
    let brokered = brokered_gpus();
    if brokered.is_empty() {
        direct.gpus
    } else {
        brokered
    }
}

/// NVML readings for one PCI slot, already normalized.
pub fn gpu_for_slot(slot: &str) -> Option<GpuSensors> {
    let slot = normalize_pci_slot(slot);
    nvml_gpus()
        .into_iter()
        .find(|gpu| gpu.pci_slot.as_deref() == Some(slot.as_str()))
}

/// Whether NVML (`libnvidia-ml.so.1`) could be loaded and initialized.
///
/// A library that loads but cannot initialize cannot answer a single query, so
/// it reports unavailable; both answers are permanent for this process.
pub fn nvml_available() -> bool {
    nvml::available()
}

/// One window's answer from the broker.
struct BrokerCache {
    gpus: Vec<GpuSensors>,
    asked_at: Instant,
}

fn broker_cache() -> &'static Mutex<Option<BrokerCache>> {
    static CACHE: OnceLock<Mutex<Option<BrokerCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// GPU readings from the root broker, asked for at most once per window.
///
/// An elevated process asks nobody: it is (or is running as) the broker, so its
/// own poll cannot be denied NVML and a request to itself would wait on its own
/// worker. The client bounds the request with its own deadline, so this cannot
/// park the sampler on a wedged socket.
fn brokered_gpus() -> Vec<GpuSensors> {
    if super::is_elevated() {
        return Vec::new();
    }
    let mut guard = tm_core::sync::lock(broker_cache());
    if let Some(cache) = guard.as_ref()
        && cache.asked_at.elapsed() < BROKER_TTL
    {
        return cache.gpus.clone();
    }
    let gpus = match client::request(&Request::Sensors) {
        Ok(Response::Sensors(snapshot)) => snapshot.gpus,
        Ok(_) => {
            tracing::debug!("the core service answered a sensor poll with another response");
            Vec::new()
        }
        Err(error) => {
            // One line per window: a stopped broker must not fill the log at
            // sampling rate.
            tracing::debug!(?error, "the core service supplied no GPU sensors");
            Vec::new()
        }
    };
    *guard = Some(BrokerCache {
        gpus: gpus.clone(),
        asked_at: Instant::now(),
    });
    gpus
}

/// Wall-clock stamp of the poll; `0` means "never taken".
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}
