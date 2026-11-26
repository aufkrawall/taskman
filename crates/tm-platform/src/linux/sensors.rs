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
//! When the driver refuses the GUI token's own NVML queries, the snapshot falls
//! back to the readings the root broker takes. That request runs on its own
//! worker thread: a broker that accepts and never answers costs one deadline
//! per window there and nothing on the sampling path, so a sample can never
//! wait on a socket.

use super::core_service::client::{self, ClientError, StatusProbe};
use super::{hwmon, nvml};
use crate::linux_core_service::{Request, Response};
use crate::linux_sensors::{
    BrokerAttempt, BrokerGate, BrokerLiveness, GpuSensors, SensorSnapshot, normalize_pci_slot,
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often the broker fallback asks the service.
///
/// Its own window, not the NVML cache's: that cache exists so one tick asks the
/// driver once, while this window paces a background worker whose request may
/// take the client's whole deadline. Two seconds keeps an NVIDIA column fresh
/// without making the service answer a request per tick.
const BROKER_TTL: Duration = Duration::from_secs(2);

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
/// publish (utilization, VRAM use, temperature). It never waits for the core
/// service: when the driver refuses this token, the answer is whatever the
/// background worker has stored so far.
pub fn nvml_gpus() -> Vec<GpuSensors> {
    let direct = nvml::sample();
    if !direct.denied {
        return direct.gpus;
    }
    // The driver refused this token's queries — what a user outside the `video`
    // group and a container without the device node both look like. The broker
    // runs as root and can take the same readings, so its worker is asked for
    // them; the answer arrives on a later tick, and a refusal, a failure or an
    // absent broker leaves the direct readings in place, because the fallback
    // must never disable the path that works.
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

/// The fallback's shared state: the last broker answer and how far asking got.
struct BrokerReadings {
    gpus: Vec<GpuSensors>,
    gate: BrokerGate,
    /// Whether the worker was started, so one process runs one worker and a
    /// failed spawn is not retried on every tick.
    started: bool,
    /// Whether this run of failures was already reported, so an absent broker
    /// costs one log line instead of one per window.
    reported: bool,
}

fn broker_readings() -> &'static Mutex<BrokerReadings> {
    static READINGS: OnceLock<Mutex<BrokerReadings>> = OnceLock::new();
    READINGS.get_or_init(|| {
        Mutex::new(BrokerReadings {
            gpus: Vec::new(),
            gate: BrokerGate::Ask,
            started: false,
            reported: false,
        })
    })
}

/// The last GPU readings the broker produced, starting its worker on first use.
///
/// An elevated process asks nobody: it is (or is running as) the broker, so its
/// own poll cannot be denied NVML and a request to itself would occupy its own
/// worker.
///
/// This does no I/O. The lock is taken only to read the stored answer and to
/// start the worker once, so a broker that accepts and never answers delays no
/// sample and cannot queue an action behind a sampling tick.
fn brokered_gpus() -> Vec<GpuSensors> {
    if super::is_elevated() {
        return Vec::new();
    }
    let (start, gpus) = {
        let mut readings = tm_core::sync::lock(broker_readings());
        let start = !readings.started;
        readings.started = true;
        (start, readings.gpus.clone())
    };
    if start {
        start_broker_worker();
    }
    gpus
}

/// Start the one worker that asks the broker for sensor readings.
///
/// It lives for the process: it owns the only sensor traffic on the client's
/// single connection, is never joined (a sampling thread must not wait for a
/// socket at exit either), and a failed spawn disables the fallback instead of
/// letting a sampling tick ask inline.
fn start_broker_worker() {
    if std::thread::Builder::new()
        .name("tm-sensors-broker".to_string())
        .spawn(broker_worker)
        .is_err()
    {
        let mut readings = tm_core::sync::lock(broker_readings());
        readings.gate = BrokerGate::Disabled;
        tracing::warn!("could not start the GPU sensor broker worker; keeping direct GPU readings");
    }
}

/// Ask the broker for GPU readings, once per [`BROKER_TTL`], for the process.
///
/// The sleep at the end of each round is the broker's polling window, not a
/// race workaround: the client bounds the request itself, so a wedged broker
/// costs this thread one deadline and nothing else. The decision for each window
/// — one observation, one optional request, one folded outcome — is
/// [`BrokerGate::step`], so it is testable where the tests run.
///
/// The lock is never held across the request, so a sampling tick only ever
/// waits for a clone of the stored answer.
fn broker_worker() {
    // What the previous window's request produced, folded into this window's
    // decision together with a fresh observation.
    let mut attempt: Option<BrokerAttempt> = None;
    loop {
        let ask = {
            // The gate's liveness input is observed here, by the owner of the
            // gate: `client::status()` is a cache read that starts the client's
            // single background probe, so a broker nobody probes is never
            // mistaken for one that is gone. A probe still in flight yields no
            // observation, which leaves the gate exactly as it is.
            let liveness = liveness_of(&client::status());
            let mut readings = tm_core::sync::lock(broker_readings());
            let (gate, ask) = readings.gate.step(liveness, attempt);
            readings.gate = gate;
            ask
        };
        attempt = ask.then(ask_broker);
        std::thread::sleep(BROKER_TTL);
    }
}

/// Ask the broker once, keep what it answered, and report what the attempt was.
fn ask_broker() -> BrokerAttempt {
    let outcome = client::request(&Request::Sensors);
    let attempt = attempt_of(&outcome);
    let mut readings = tm_core::sync::lock(broker_readings());
    match outcome {
        Ok(Response::Sensors(snapshot)) => {
            readings.gpus = snapshot.gpus;
            readings.reported = false;
        }
        Ok(_) => {
            readings.reported = true;
            tracing::debug!("the core service answered a sensor poll with another payload");
        }
        Err(error) => {
            // One line per outage or decision change: a broker that stays away
            // must not fill the log at window rate, while a refusal is news.
            if !readings.reported || attempt == BrokerAttempt::Refused {
                readings.reported = true;
                tracing::debug!(?error, "the core service supplied no GPU sensors");
            }
        }
    }
    attempt
}

/// What one request to the broker produced, as the gate understands it.
///
/// Only a request that did not reach a broker is transient; a refusal or an
/// unusable answer came from the broker and is final.
fn attempt_of(outcome: &Result<Response, ClientError>) -> BrokerAttempt {
    match outcome {
        Ok(Response::Sensors(_)) => BrokerAttempt::Answered,
        Ok(_) => BrokerAttempt::Refused,
        Err(error) if error.may_fall_back() => BrokerAttempt::Unreachable,
        Err(_) => BrokerAttempt::Refused,
    }
}

/// Whether the status probe proves anything about the broker.
///
/// An answered probe proves it is there whatever it said, including a refusal;
/// only an answer that reached nobody means the service is gone. A probe that is
/// still pending proves neither, so it yields no observation at all — reading it
/// as "gone" is what made a latched gate re-ask and re-latch.
fn liveness_of(probe: &StatusProbe) -> Option<BrokerLiveness> {
    match probe {
        StatusProbe::Pending => None,
        StatusProbe::Answered(Ok(_)) => Some(BrokerLiveness::Reachable),
        StatusProbe::Answered(Err(error)) if error.may_fall_back() => {
            Some(BrokerLiveness::Unreachable)
        }
        StatusProbe::Answered(Err(_)) => Some(BrokerLiveness::Reachable),
    }
}

/// Wall-clock stamp of the poll; `0` means "never taken".
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}
