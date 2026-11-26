//! Per-process GPU usage for the Linux backend: DRM client `fdinfo` parsing,
//! engine-time deltas, and the engine naming shared with NVML readings.
//!
//! Every open DRM file descriptor of a modern open driver (amdgpu, i915, xe,
//! nouveau, panfrost, msm, v3d, …) publishes its client's lifetime engine time
//! and memory in `/proc/<pid>/fdinfo/<fd>` — the documented `drm-usage-stats`
//! format that `nvtop`, `intel_gpu_top` and `gputop` read. Differencing the
//! engine time over a window gives each process' busy share per engine, which
//! is exactly the per-engine model Windows' GPU counters use.
//!
//! The proprietary NVIDIA driver publishes none of this; its processes come
//! from NVML instead and are normalized to the same engine names here.
//!
//! Platform-neutral on purpose (text parsing and bookkeeping only) so its
//! tests run on every host.

use std::collections::{BTreeMap, HashMap};

/// One engine's lifetime counter as one fdinfo read reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCounter {
    /// `drm-engine-<name>: <N> ns` — busy nanoseconds, possibly summed over
    /// `drm-engine-capacity-<name>` parallel instances.
    Nanoseconds { busy_ns: u64, capacity: u32 },
    /// `drm-cycles-<name>` / `drm-total-cycles-<name>` (xe): busy GPU cycles
    /// against total GPU cycles elapsed on that engine class.
    Cycles { busy: u64, total: u64 },
}

/// One DRM client (one open file description) as fdinfo describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrmClient {
    pub driver: String,
    /// PCI slot (`0000:03:00.0`), the join key to the adapter.
    pub pdev: Option<String>,
    /// Unique per open file description: duplicated descriptors (and those
    /// inherited across fork) share it and must be counted once.
    pub client_id: Option<u64>,
    pub engines: BTreeMap<String, EngineCounter>,
    /// Device-local (VRAM) bytes resident for this client.
    pub vram_bytes: Option<u64>,
    /// System memory mapped for the GPU (GTT / system region) — Windows'
    /// "shared GPU memory".
    pub shared_bytes: Option<u64>,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>` text. `None` unless it describes a
/// DRM client (`drm-driver:` present).
pub fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let mut client = DrmClient::default();
    let mut capacities: HashMap<String, u32> = HashMap::new();
    let mut cycles: HashMap<String, u64> = HashMap::new();
    let mut total_cycles: HashMap<String, u64> = HashMap::new();
    let mut found_driver = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "drm-driver" => {
                client.driver = value.to_string();
                found_driver = true;
            }
            "drm-pdev" => client.pdev = Some(value.to_ascii_lowercase()),
            "drm-client-id" => client.client_id = value.parse().ok(),
            key => {
                if let Some(engine) = key.strip_prefix("drm-engine-capacity-") {
                    if let Ok(capacity) = value.parse::<u32>() {
                        capacities.insert(engine.to_string(), capacity.max(1));
                    }
                } else if let Some(engine) = key.strip_prefix("drm-engine-") {
                    if let Some(ns) = value.strip_suffix("ns").and_then(|n| n.trim().parse().ok()) {
                        client.engines.insert(
                            engine.to_string(),
                            EngineCounter::Nanoseconds {
                                busy_ns: ns,
                                capacity: 1,
                            },
                        );
                    }
                } else if let Some(engine) = key.strip_prefix("drm-total-cycles-") {
                    if let Ok(total) = value.parse() {
                        total_cycles.insert(engine.to_string(), total);
                    }
                } else if let Some(engine) = key.strip_prefix("drm-cycles-") {
                    if let Ok(busy) = value.parse() {
                        cycles.insert(engine.to_string(), busy);
                    }
                } else if let Some(region) = key
                    .strip_prefix("drm-resident-")
                    .or_else(|| key.strip_prefix("drm-memory-"))
                {
                    let Some(bytes) = parse_memory(value) else {
                        continue;
                    };
                    match memory_class(region) {
                        MemoryClass::Local => add(&mut client.vram_bytes, bytes),
                        MemoryClass::Shared => add(&mut client.shared_bytes, bytes),
                        MemoryClass::Other => {}
                    }
                }
            }
        }
    }
    if !found_driver {
        return None;
    }
    for (engine, capacity) in capacities {
        if let Some(EngineCounter::Nanoseconds { capacity: slot, .. }) =
            client.engines.get_mut(&engine)
        {
            *slot = capacity;
        }
    }
    for (engine, busy) in cycles {
        if let Some(total) = total_cycles.get(&engine) {
            client.engines.insert(
                engine,
                EngineCounter::Cycles {
                    busy,
                    total: *total,
                },
            );
        }
    }
    Some(client)
}

fn add(slot: &mut Option<u64>, bytes: u64) {
    *slot = Some(slot.unwrap_or(0).saturating_add(bytes));
}

enum MemoryClass {
    Local,
    Shared,
    Other,
}

/// Which Windows memory pool a DRM memory region corresponds to.
fn memory_class(region: &str) -> MemoryClass {
    match region {
        "vram" | "local0" | "local" | "vram0" => MemoryClass::Local,
        "gtt" | "system0" | "system" | "gart" => MemoryClass::Shared,
        // `drm-memory-cpu` (amdgpu) is CPU-visible-only memory, and unknown
        // regions are not guessed into either pool.
        _ => MemoryClass::Other,
    }
}

/// `<N> [KiB|MiB|GiB]` → bytes.
fn parse_memory(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number: u64 = parts.next()?.parse().ok()?;
    let scale = match parts.next() {
        None => 1,
        Some("KiB") => 1024,
        Some("MiB") => 1024 * 1024,
        Some("GiB") => 1024 * 1024 * 1024,
        Some(_) => return None,
    };
    Some(number.saturating_mul(scale))
}

/// The Windows engine type a driver's engine class corresponds to, so Linux
/// rows read "GPU 0 - 3D" like Windows and the engine graphs pick them up.
pub fn engine_type(driver_engine: &str) -> String {
    match driver_engine {
        // amdgpu, i915/xe, nouveau, panfrost/v3d, msm
        "gfx" | "render" | "rcs" | "gr" | "graphics" | "fragment" | "vertex-tiler" | "bin"
        | "gpu" => "3D".into(),
        "compute" | "ccs" | "csb" => "Compute".into(),
        "dec" | "video" | "vcs" | "nvdec" | "msvld" | "msppp" => "VideoDecode".into(),
        "enc" | "nvenc" | "vcn_enc" => "VideoEncode".into(),
        "enc_1" => "VideoEncode".into(),
        "jpeg" | "nvjpg" => "VideoJpeg".into(),
        "dma" | "copy" | "bcs" | "ce" | "sdma" => "Copy".into(),
        "video-enhance" | "vecs" | "vic" => "VideoProcessing".into(),
        other => other.to_string(),
    }
}

/// Per-client engine-time deltas between consecutive reads.
///
/// Keyed by (pid, client id, engine), so a client that two processes share
/// through an inherited descriptor is still measured once per holder pid that
/// it was first seen under, and a new client contributes only from its
/// second read (its lifetime counter may predate the window).
#[derive(Debug, Default)]
pub struct EngineAccountant {
    previous: HashMap<(u32, u64, String), EngineCounter>,
}

impl EngineAccountant {
    /// Busy percent per (pid, pdev, engine type) for this window.
    ///
    /// `clients` is every client read this tick with its holder pid. A client
    /// without an id cannot be told apart from its duplicates and is skipped.
    pub fn window(
        &mut self,
        clients: &[(u32, DrmClient)],
        interval_s: f64,
    ) -> HashMap<(u32, String, String), f32> {
        let interval_ns = (interval_s.max(0.001) * 1e9) as u64;
        let mut next = HashMap::new();
        let mut out: HashMap<(u32, String, String), f32> = HashMap::new();
        for (pid, client) in clients {
            let Some(id) = client.client_id else {
                continue;
            };
            let pdev = client.pdev.clone().unwrap_or_default();
            for (engine, counter) in &client.engines {
                let key = (*pid, id, engine.clone());
                if next.contains_key(&key) {
                    continue;
                }
                let pct = self
                    .previous
                    .get(&key)
                    .and_then(|before| busy_pct(*before, *counter, interval_ns));
                next.insert(key, *counter);
                if let Some(pct) = pct {
                    let slot = out
                        .entry((*pid, pdev.clone(), engine_type(engine)))
                        .or_insert(0.0);
                    *slot = (*slot + pct).min(100.0);
                }
            }
        }
        self.previous = next;
        out
    }

    pub fn forget(&mut self) {
        self.previous.clear();
    }
}

fn busy_pct(before: EngineCounter, now: EngineCounter, interval_ns: u64) -> Option<f32> {
    match (before, now) {
        (
            EngineCounter::Nanoseconds { busy_ns: a, .. },
            EngineCounter::Nanoseconds {
                busy_ns: b,
                capacity,
            },
        ) => {
            let busy = b.checked_sub(a)?;
            let available = interval_ns.saturating_mul(u64::from(capacity.max(1)));
            Some((busy as f64 / available as f64 * 100.0).clamp(0.0, 100.0) as f32)
        }
        (
            EngineCounter::Cycles { busy: a, total: ta },
            EngineCounter::Cycles { busy: b, total: tb },
        ) => {
            let busy = b.checked_sub(a)?;
            let total = tb.checked_sub(ta)?;
            if total == 0 {
                return Some(0.0);
            }
            Some((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMDGPU: &str = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t1137\n\
drm-driver:\tamdgpu\ndrm-client-id:\t17\ndrm-pdev:\t0000:03:00.0\n\
drm-memory-vram:\t102400 KiB\ndrm-memory-gtt:\t2048 KiB\ndrm-memory-cpu:\t0 KiB\n\
drm-engine-gfx:\t1000000000 ns\ndrm-engine-dec:\t5000 ns\n";

    #[test]
    fn amdgpu_fdinfo_yields_engines_and_both_memory_pools() {
        let client = parse_fdinfo(AMDGPU).unwrap();
        assert_eq!(client.driver, "amdgpu");
        assert_eq!(client.pdev.as_deref(), Some("0000:03:00.0"));
        assert_eq!(client.client_id, Some(17));
        assert_eq!(client.vram_bytes, Some(102_400 * 1024));
        assert_eq!(client.shared_bytes, Some(2048 * 1024));
        assert_eq!(
            client.engines["gfx"],
            EngineCounter::Nanoseconds {
                busy_ns: 1_000_000_000,
                capacity: 1
            }
        );
    }

    #[test]
    fn i915_capacity_and_xe_cycles_are_understood() {
        let i915 = parse_fdinfo(
            "drm-driver: i915\ndrm-client-id: 3\ndrm-engine-video: 400 ns\n\
             drm-engine-capacity-video: 2\ndrm-resident-system0: 4 MiB\n",
        )
        .unwrap();
        assert_eq!(
            i915.engines["video"],
            EngineCounter::Nanoseconds {
                busy_ns: 400,
                capacity: 2
            }
        );
        assert_eq!(i915.shared_bytes, Some(4 * 1024 * 1024));

        let xe = parse_fdinfo(
            "drm-driver: xe\ndrm-client-id: 9\ndrm-cycles-rcs: 50\ndrm-total-cycles-rcs: 200\n\
             drm-resident-vram0: 1 GiB\n",
        )
        .unwrap();
        assert_eq!(
            xe.engines["rcs"],
            EngineCounter::Cycles {
                busy: 50,
                total: 200
            }
        );
        assert_eq!(xe.vram_bytes, Some(1024 * 1024 * 1024));
    }

    #[test]
    fn non_drm_descriptors_are_not_clients() {
        assert_eq!(parse_fdinfo("pos:\t0\nflags:\t02\nmnt_id:\t5\n"), None);
    }

    #[test]
    fn engine_time_becomes_a_busy_share_per_window() {
        let client = |gfx_ns: u64, video_ns: u64| DrmClient {
            driver: "i915".into(),
            pdev: Some("0000:00:02.0".into()),
            client_id: Some(5),
            engines: [
                (
                    "render".to_string(),
                    EngineCounter::Nanoseconds {
                        busy_ns: gfx_ns,
                        capacity: 1,
                    },
                ),
                (
                    "video".to_string(),
                    EngineCounter::Nanoseconds {
                        busy_ns: video_ns,
                        capacity: 2,
                    },
                ),
            ]
            .into(),
            ..DrmClient::default()
        };
        let mut accountant = EngineAccountant::default();
        assert!(
            accountant.window(&[(7, client(0, 0))], 1.0).is_empty(),
            "a first read has no window"
        );
        // A duplicate descriptor of the same client is counted once.
        let shares = accountant.window(
            &[
                (7, client(250_000_000, 1_000_000_000)),
                (7, client(250_000_000, 1_000_000_000)),
            ],
            1.0,
        );
        let key = |engine: &str| (7, "0000:00:02.0".to_string(), engine.to_string());
        assert_eq!(shares[&key("3D")], 25.0);
        // 1 s of busy time over two video instances is half their capacity.
        assert_eq!(shares[&key("VideoDecode")], 50.0);
    }
}
