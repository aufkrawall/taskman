//! Per-process GPU engine time and memory: DRM client fdinfo for the open
//! drivers, NVML for the proprietary NVIDIA driver.
//!
//! Parsing and the engine accountant are platform-neutral
//! ([`crate::linux_gpu_clients`]); this module finds the DRM descriptors,
//! reads them, asks NVML, and folds both into the process rows and the
//! adapters' engine lists.
//!
//! Finding DRM descriptors means walking `/proc/<pid>/fd`, so the walk is
//! cached per process and repeated at most every [`FD_RESCAN`], and then only
//! when the process' descriptor count moved (or every [`FD_RESCAN_STEADY`]);
//! at once for a new process, or one whose cached descriptor stopped being a
//! DRM client. A steady tick reads only the handful of fdinfo files of GPU
//! clients.

use super::nvml;
use crate::linux_gpu_clients::{self as clients, DrmClient, EngineAccountant};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};
use tm_core::model::{GpuEngine, GpuEngineShare, GpuInfo, ProcessEntry};

/// How long one process' list of DRM descriptors is trusted.
const FD_RESCAN: Duration = Duration::from_secs(5);
/// How long it is trusted while the process' descriptor count stays the same.
/// Opening a GPU device adds descriptors (the node, and the buffers and sync
/// files that follow), so a client that starts rendering is found after
/// [`FD_RESCAN`]; only a swap that keeps the count exact waits for this.
const FD_RESCAN_STEADY: Duration = Duration::from_secs(60);

struct FdCache {
    start_ticks: Option<i64>,
    scanned_at: Instant,
    /// The process' descriptor count (the Handles column) at that scan.
    fd_count: Option<u32>,
    /// Descriptor numbers that pointed at `/dev/dri/*` at the last scan.
    drm_fds: Vec<u32>,
}

#[derive(Default)]
pub(super) struct ProcessGpu {
    fds: HashMap<u32, FdCache>,
    accountant: EngineAccountant,
    /// Whether any DRM device reports per-client stats, keyed by the render
    /// nodes it was probed for.
    probe: Option<(Vec<std::ffi::OsString>, bool)>,
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
        // Walking every process' descriptors finds nothing when no device
        // reports per-client stats (the proprietary NVIDIA driver: NVML covers
        // it below), and a readable process is then not covered by the DRM
        // path either -- its GPU use is unknown, not a measured zero.
        if self.drm_reports_clients() {
            for process in processes.iter() {
                if let Some(clients) = self.read_clients(process, now) {
                    readable.insert(process.pid);
                    drm_clients.extend(clients.into_iter().map(|c| (process.pid, c)));
                }
            }
        } else {
            self.fds.clear();
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

    /// Whether some DRM device reports per-client stats in fdinfo, probed
    /// once per set of render nodes (a hot-plugged GPU is probed when it
    /// appears).
    fn drm_reports_clients(&mut self) -> bool {
        let mut nodes: Vec<std::ffi::OsString> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with("renderD"))
            .collect();
        nodes.sort();
        if let Some((probed, reports)) = &self.probe
            && *probed == nodes
        {
            return *reports;
        }
        let probes: Vec<Option<bool>> = nodes
            .iter()
            .map(|node| probe_render_node(&std::path::Path::new("/dev/dri").join(node)))
            .collect();
        let reports = any_reports_clients(&probes);
        tracing::debug!(?nodes, ?probes, reports, "DRM per-client stats probe");
        self.probe = Some((nodes, reports));
        reports
    }

    /// The DRM clients `process` holds, or `None` when its descriptors cannot
    /// be read (another user's process without root).
    fn read_clients(&mut self, process: &ProcessEntry, now: Instant) -> Option<Vec<DrmClient>> {
        let pid = process.pid;
        let stale = self.fds.get(&pid).is_none_or(|cache| {
            let age = now.saturating_duration_since(cache.scanned_at);
            cache.start_ticks != process.boot_start_ticks
                || (age >= FD_RESCAN
                    && (cache.fd_count != process.handles
                        || process.handles.is_none()
                        || age >= FD_RESCAN_STEADY))
        });
        if stale {
            let drm_fds = scan_drm_fds(pid)?;
            self.fds.insert(
                pid,
                FdCache {
                    start_ticks: process.boot_start_ticks,
                    scanned_at: now,
                    fd_count: process.handles,
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
            cache.scanned_at = now
                .checked_sub(FD_RESCAN_STEADY)
                .unwrap_or(cache.scanned_at);
        }
        Some(out)
    }
}

/// Whether the driver behind a render node reports per-client stats:
/// `Some(true)` when our own descriptor's fdinfo names a `drm-driver`,
/// `Some(false)` when it does not, `None` when the node cannot be opened.
fn probe_render_node(node: &std::path::Path) -> Option<bool> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(node).ok()?;
    let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).ok()?;
    Some(clients::parse_fdinfo(&text).is_some())
}

/// Whether to walk process descriptors for DRM clients. A node this user
/// cannot open could still report stats, so it keeps the walk on; no render
/// node at all, or only nodes whose driver reports nothing, turn it off.
fn any_reports_clients(probes: &[Option<bool>]) -> bool {
    probes.iter().any(|probe| *probe != Some(false))
}

/// Descriptor numbers of `pid` that refer to a DRM device node.
///
/// Runs over every descriptor of every readable process each [`FD_RESCAN`],
/// so it allocates nothing per descriptor: the directory is walked with
/// `readdir` and each link read with `readlinkat` into a stack buffer just
/// long enough to hold the `/dev/dri/` prefix (a longer target is truncated,
/// which is all the comparison needs).
fn scan_drm_fds(pid: u32) -> Option<Vec<u32>> {
    const PREFIX: &[u8] = b"/dev/dri/";
    let path = std::ffi::CString::new(format!("/proc/{pid}/fd")).ok()?;
    // SAFETY: `path` is a valid NUL-terminated string.
    let dir = unsafe { libc::opendir(path.as_ptr()) };
    if dir.is_null() {
        return None;
    }
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            // SAFETY: the stream came from `opendir` and is closed once.
            unsafe { libc::closedir(self.0) };
        }
    }
    let dir = Dir(dir);
    // SAFETY: `dir.0` is an open directory stream.
    let dir_fd = unsafe { libc::dirfd(dir.0) };
    let mut fds = Vec::new();
    let mut target = [0u8; PREFIX.len()];
    loop {
        // SAFETY: `dir.0` is open; the entry stays valid until the next call.
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is NUL-terminated within the entry.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let Some(fd) = std::str::from_utf8(name.to_bytes())
            .ok()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue; // `.` and `..`
        };
        // SAFETY: `name` is NUL-terminated and `target` is writable for its length.
        let len = unsafe {
            libc::readlinkat(
                dir_fd,
                name.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if usize::try_from(len).is_ok_and(|len| len == PREFIX.len()) && target == *PREFIX {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// The scan reports exactly the descriptors that point at a DRM node:
    /// an ordinary file is not one, and a render node (where this machine has
    /// one this user may open) is.
    #[test]
    fn the_descriptor_scan_finds_drm_nodes_only() {
        let pid = std::process::id();
        let plain = std::fs::File::open("/proc/self/stat").expect("open");
        let fds = scan_drm_fds(pid).expect("own descriptors are readable");
        assert!(!fds.contains(&(plain.as_raw_fd() as u32)));
        let render = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
            .find_map(|entry| std::fs::File::open(entry.path()).ok());
        if let Some(render) = render {
            let fds = scan_drm_fds(pid).expect("own descriptors are readable");
            assert!(fds.contains(&(render.as_raw_fd() as u32)), "{fds:?}");
            assert!(!fds.contains(&(plain.as_raw_fd() as u32)));
        }
        assert_eq!(scan_drm_fds(u32::MAX), None);
    }

    #[test]
    fn the_descriptor_walk_runs_only_where_a_driver_reports_clients() {
        assert!(!any_reports_clients(&[]));
        assert!(!any_reports_clients(&[Some(false)]));
        assert!(any_reports_clients(&[Some(false), Some(true)]));
        // Unknown is not "reports nothing".
        assert!(any_reports_clients(&[None]));
        assert!(any_reports_clients(&[Some(false), None]));
    }

    /// A driver without per-client fdinfo (the proprietary NVIDIA driver
    /// prints only the generic `pos`/`flags` lines) reports nothing.
    #[test]
    fn fdinfo_without_a_drm_driver_reports_no_client() {
        assert!(
            clients::parse_fdinfo("pos:\t0\nflags:\t02100002\nmnt_id:\t30\nino:\t252\n").is_none()
        );
        assert!(
            clients::parse_fdinfo("pos:\t0\ndrm-driver:\tamdgpu\ndrm-client-id:\t7\n").is_some()
        );
    }

    /// A process' descriptors are walked again after [`FD_RESCAN`] only when
    /// its descriptor count moved, and after [`FD_RESCAN_STEADY`] regardless.
    #[test]
    fn descriptors_are_rescanned_when_their_count_moves() {
        // A child of our own, not this process: other tests here open and
        // close a render node, which would turn a cached descriptor stale.
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn cat");
        let pid = child.id();
        let mut gpu = ProcessGpu::default();
        let mut process = ProcessEntry::new(pid, "cat");
        process.boot_start_ticks = Some(1);
        process.handles = Some(10);
        let t0 = Instant::now();
        let scanned_at = |gpu: &ProcessGpu| gpu.fds[&pid].scanned_at;

        gpu.read_clients(&process, t0).expect("own descriptors");
        assert_eq!(scanned_at(&gpu), t0);
        // Same count after the short window: the cached list stands.
        let t1 = t0 + FD_RESCAN + Duration::from_secs(1);
        gpu.read_clients(&process, t1).expect("own descriptors");
        assert_eq!(scanned_at(&gpu), t0);
        // A moved count is walked again, but not inside the short window.
        process.handles = Some(11);
        let t2 = t1 + Duration::from_secs(1);
        gpu.read_clients(&process, t2).expect("own descriptors");
        assert_eq!(scanned_at(&gpu), t2);
        process.handles = Some(12);
        gpu.read_clients(&process, t2 + Duration::from_secs(1))
            .expect("own descriptors");
        assert_eq!(scanned_at(&gpu), t2);
        // An unchanged count is still walked after the steady window.
        let t3 = t2 + FD_RESCAN_STEADY;
        gpu.read_clients(&process, t3).expect("own descriptors");
        assert_eq!(scanned_at(&gpu), t3);
        // A new generation of the PID is walked at once.
        process.boot_start_ticks = Some(2);
        let t4 = t3 + Duration::from_secs(1);
        gpu.read_clients(&process, t4).expect("own descriptors");
        assert_eq!(scanned_at(&gpu), t4);
        drop(child.stdin.take());
        child.wait().expect("cat exits");
    }
}
