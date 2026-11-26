//! Per-process GPU engine time and memory: DRM client fdinfo for the open
//! drivers, NVML for the proprietary NVIDIA driver.
//!
//! Parsing and the engine accountant are platform-neutral
//! ([`crate::linux_gpu_clients`]); this module finds the DRM descriptors,
//! reads them, asks NVML, and folds both into the process rows and the
//! adapters' engine lists.
//!
//! Finding DRM descriptors means walking `/proc/<pid>/fd`, so the walk is
//! cached per process and repeated only every [`FD_RESCAN`] (or at once for a
//! new process, or one whose cached descriptor stopped being a DRM client).
//! A steady tick reads only the handful of fdinfo files of GPU clients.

use super::nvml;
use crate::linux_gpu_clients::{self as clients, DrmClient, EngineAccountant};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};
use tm_core::model::{GpuEngine, GpuEngineShare, GpuInfo, ProcessEntry};

/// How long one process' list of DRM descriptors is trusted.
const FD_RESCAN: Duration = Duration::from_secs(5);

struct FdCache {
    start_ticks: Option<i64>,
    scanned_at: Instant,
    /// Descriptor numbers that pointed at `/dev/dri/*` at the last scan.
    drm_fds: Vec<u32>,
}

#[derive(Default)]
pub(super) struct ProcessGpu {
    fds: HashMap<u32, FdCache>,
    accountant: EngineAccountant,
}

/// One process' GPU use, keyed by adapter PCI slot.
#[derive(Debug, Default, Clone)]
struct Usage {
    /// (pci slot, engine type) → busy percent.
    shares: BTreeMap<(String, String), f32>,
    vram_bytes: Option<u64>,
    shared_bytes: Option<u64>,
}

impl ProcessGpu {
    pub(super) fn forget(&mut self) {
        self.fds.clear();
        self.accountant.forget();
    }

    /// Fill the per-process GPU columns and, where the adapter has no engine
    /// data of its own, the adapters' engine lists.
    ///
    /// A process is "covered" when some source could have seen its GPU use:
    /// every process when NVML answered its per-process queries (the driver
    /// enumerates GPU users system-wide), and a process whose descriptors this
    /// token can read for the DRM path. A covered process with no GPU use is
    /// a measured 0 %; an uncovered one keeps `None`.
    pub(super) fn apply(
        &mut self,
        processes: &mut [ProcessEntry],
        gpus: &mut [GpuInfo],
        interval_s: f64,
        now: Instant,
    ) {
        let live: HashSet<u32> = processes.iter().map(|p| p.pid).collect();
        self.fds.retain(|pid, _| live.contains(pid));

        let mut drm_clients: Vec<(u32, DrmClient)> = Vec::new();
        let mut readable: HashSet<u32> = HashSet::new();
        for process in processes.iter() {
            if let Some(clients) = self.read_clients(process, now) {
                readable.insert(process.pid);
                drm_clients.extend(clients.into_iter().map(|c| (process.pid, c)));
            }
        }
        let mut usage: HashMap<u32, Usage> = HashMap::new();
        for ((pid, pdev, engine), pct) in self.accountant.window(&drm_clients, interval_s) {
            let slot = usage.entry(pid).or_default();
            *slot.shares.entry((pdev, engine)).or_insert(0.0) += pct;
        }
        let mut seen_clients: HashSet<(u32, u64)> = HashSet::new();
        for (pid, client) in &drm_clients {
            if let Some(id) = client.client_id
                && !seen_clients.insert((*pid, id))
            {
                continue;
            }
            let slot = usage.entry(*pid).or_default();
            if let Some(bytes) = client.vram_bytes {
                slot.vram_bytes = Some(slot.vram_bytes.unwrap_or(0).saturating_add(bytes));
            }
            if let Some(bytes) = client.shared_bytes {
                slot.shared_bytes = Some(slot.shared_bytes.unwrap_or(0).saturating_add(bytes));
            }
        }

        let nvml = nvml::process_usage();
        if let Some((nvidia, _)) = &nvml {
            for reading in nvidia {
                let slot = usage.entry(reading.pid).or_default();
                for (engine, pct) in [
                    ("3D", reading.sm_pct),
                    ("VideoEncode", reading.enc_pct),
                    ("VideoDecode", reading.dec_pct),
                ] {
                    if let Some(pct) = pct {
                        slot.shares
                            .insert((reading.pci_slot.clone(), engine.to_string()), pct);
                    }
                }
                if let Some(bytes) = reading.vram_bytes {
                    slot.vram_bytes = Some(slot.vram_bytes.unwrap_or(0).max(bytes));
                }
            }
        }
        let nvml_covers_all = nvml.is_some();

        let adapter_of = |slot: &str| -> Option<usize> {
            let location = parse_slot(slot)?;
            gpus.iter()
                .find(|gpu| gpu.pci_location == Some(location))
                .map(|gpu| gpu.id)
        };
        let mut engine_ids: Vec<String> = Vec::new();
        let mut engine_id = |name: &str| -> u32 {
            match engine_ids.iter().position(|known| known == name) {
                Some(index) => index as u32,
                None => {
                    engine_ids.push(name.to_string());
                    (engine_ids.len() - 1) as u32
                }
            }
        };
        // (adapter, engine type) → summed process load, for the adapter
        // engine graphs.
        let mut adapter_engines: BTreeMap<(usize, String), f32> = BTreeMap::new();
        for process in processes.iter_mut() {
            let covered = nvml_covers_all || readable.contains(&process.pid);
            let Some(found) = usage.remove(&process.pid) else {
                if covered {
                    process.gpu_util_pct = Some(0.0);
                    process.gpu_dedicated_bytes = Some(0);
                    process.gpu_mem_bytes = Some(0);
                }
                continue;
            };
            let mut engines = Vec::new();
            let mut best: Option<(usize, &str, f32)> = None;
            for ((slot, engine), pct) in &found.shares {
                let Some(adapter) = adapter_of(slot) else {
                    continue;
                };
                let pct = pct.clamp(0.0, 100.0);
                *adapter_engines
                    .entry((adapter, engine.clone()))
                    .or_insert(0.0) += pct;
                if pct > 0.0 {
                    engines.push(GpuEngineShare {
                        adapter,
                        engine: engine_id(engine),
                        util_pct: pct,
                    });
                }
                if best.is_none_or(|(_, _, current)| pct > current) {
                    best = Some((adapter, engine.as_str(), pct));
                }
            }
            process.gpu_util_pct = Some(best.map_or(0.0, |(_, _, pct)| pct));
            process.gpu_engine_label = best
                .filter(|(_, _, pct)| *pct > 0.0)
                .map(|(adapter, engine, _)| format!("GPU {adapter} - {engine}"));
            process.gpu_engines = engines;
            process.gpu_dedicated_bytes = found.vram_bytes.or(covered.then_some(0));
            process.gpu_mem_bytes = process.gpu_dedicated_bytes;
            process.gpu_shared_bytes = found.shared_bytes;
        }

        for gpu in gpus.iter_mut() {
            if !gpu.engines.is_empty() {
                continue;
            }
            let mut engines: Vec<GpuEngine> = adapter_engines
                .iter()
                .filter(|((adapter, _), _)| *adapter == gpu.id)
                .map(|((_, name), pct)| GpuEngine {
                    name: name.clone(),
                    util_pct: pct.min(100.0),
                })
                .collect();
            // NVIDIA: the adapter's own counters are the authority for the
            // engines NVML measures adapter-wide; per-process samples can miss
            // short-lived clients.
            if let Some((_, codecs)) = &nvml
                && let Some(location) = gpu.pci_location
                && let Some(codec) = codecs
                    .iter()
                    .find(|codec| parse_slot(&codec.pci_slot) == Some(location))
            {
                let mut set = |name: &str, pct: Option<f32>| {
                    let Some(pct) = pct else {
                        return;
                    };
                    match engines.iter_mut().find(|engine| engine.name == name) {
                        Some(engine) => engine.util_pct = pct,
                        None => engines.push(GpuEngine {
                            name: name.to_string(),
                            util_pct: pct,
                        }),
                    }
                };
                set("3D", gpu.util_pct_available.then_some(gpu.util_pct));
                set("VideoEncode", codec.enc_pct);
                set("VideoDecode", codec.dec_pct);
            }
            gpu.engines = engines;
        }
    }

    /// The DRM clients `process` holds, or `None` when its descriptors cannot
    /// be read (another user's process without root).
    fn read_clients(&mut self, process: &ProcessEntry, now: Instant) -> Option<Vec<DrmClient>> {
        let pid = process.pid;
        let stale = self.fds.get(&pid).is_none_or(|cache| {
            cache.start_ticks != process.boot_start_ticks
                || now.duration_since(cache.scanned_at) >= FD_RESCAN
        });
        if stale {
            let drm_fds = scan_drm_fds(pid)?;
            self.fds.insert(
                pid,
                FdCache {
                    start_ticks: process.boot_start_ticks,
                    scanned_at: now,
                    drm_fds,
                },
            );
        }
        let cache = self.fds.get_mut(&pid)?;
        let mut out = Vec::with_capacity(cache.drm_fds.len());
        let mut changed = false;
        for fd in &cache.drm_fds {
            match std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))
                .ok()
                .and_then(|text| clients::parse_fdinfo(&text))
            {
                Some(client) => out.push(client),
                None => changed = true,
            }
        }
        if changed {
            // A cached descriptor was closed or reused: rescan next tick.
            cache.scanned_at = now.checked_sub(FD_RESCAN).unwrap_or(cache.scanned_at);
        }
        Some(out)
    }
}

/// Descriptor numbers of `pid` that refer to a DRM device node.
fn scan_drm_fds(pid: u32) -> Option<Vec<u32>> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut fds = Vec::new();
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        if target.starts_with("/dev/dri/")
            && let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse().ok())
        {
            fds.push(fd);
        }
    }
    Some(fds)
}

/// `0000:06:00.0` → (bus, device, function), the `GpuInfo::pci_location` form.
fn parse_slot(slot: &str) -> Option<(u32, u32, u32)> {
    let mut parts = slot.rsplitn(3, ':');
    let device_function = parts.next()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let (device, function) = device_function.split_once('.')?;
    Some((
        bus,
        u32::from_str_radix(device, 16).ok()?,
        u32::from_str_radix(function, 16).ok()?,
    ))
}
