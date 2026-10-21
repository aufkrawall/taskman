//! GPU adapter discovery via DXGI and LUID-accurate merge with PDH records.
//!
//! Correctness rules (implement.md §13):
//! * DXGI `AdapterLuid` is the join key between static adapter info and PDH
//!   engine/memory records — one adapter's utilization is never copied to
//!   another.
//! * Adapter utilization follows Task Manager semantics: the busiest engine
//!   on that adapter (max), not a naive sum across engines which would exceed
//!   100 % and misattribute multi-engine load. One engine's load is the SUM
//!   of every process's share of it. Per-process values use the same rule;
//!   the dominant engine is preserved for the "GPU engine" column.
//! * Processes and adapters cover the same adapter set: the per-process view
//!   only counts records of adapters that `merge` produced, so no row can
//!   exceed the machine total it is shown under.
//! * Adapter dedicated and shared memory follow Task Manager semantics: read
//!   directly from `\GPU Adapter Memory(*)` per LUID, NEVER summed across
//!   per-process `\GPU Process Memory(*)` instances. Summing per-process
//!   memory severely overstates actual VRAM usage because Desktop Window
//!   Manager (DWM) and applications share surfaces and duplicate committed
//!   allocations.

use tm_core::model::{AdapterLuid, GpuEngine, GpuEngineShare, GpuInfo};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1,
};
use windows::core::Interface;

/// Static adapter info (name, VRAM, LUID).
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub name: String,
    pub dedicated_vram: u64,
    pub luid: AdapterLuid,
    pub driver_version: String,
    pub is_software: bool,
}

/// Every DXGI adapter, software renderers included.
///
/// The full list is what the sampler checks PDH's LUIDs against: the Basic
/// Render Driver publishes engine instances like any adapter, and treating
/// its LUID as unknown re-enumerated DXGI every 30 s for as long as the app
/// ran. [`filter_adapters`] picks the set that is displayed.
pub fn adapters() -> Vec<AdapterInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            tracing::debug!("CreateDXGIFactory1 failed; no adapter info");
            return out;
        };
        let mut idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(idx) {
            if let Ok(desc) = adapter.GetDesc1() {
                let name = utf16_to_string(&desc.Description);
                let is_software = (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0)
                    || (desc.VendorId == 0x1414 && desc.DeviceId == 0x8c)
                    || name.contains("Microsoft Basic Render Driver")
                    || name.contains("Microsoft Basic Display Adapter");
                // An adapter with no user-mode driver (software renderers,
                // and anything that answers 0) has no version to report. An
                // empty string is what the UI renders as unavailable; a
                // formatted "0.0.0.0" would be a fabricated one.
                let driver_version = match adapter.CheckInterfaceSupport(&IDXGIDevice::IID) {
                    Ok(uversion) if uversion != 0 => {
                        let hi = (uversion >> 32) as u32;
                        let lo = (uversion & 0xffff_ffff) as u32;
                        format!("{}.{}.{}.{}", hi >> 16, hi & 0xffff, lo >> 16, lo & 0xffff)
                    }
                    _ => String::new(),
                };
                out.push(AdapterInfo {
                    name,
                    dedicated_vram: desc.DedicatedVideoMemory as u64,
                    luid: AdapterLuid {
                        high: desc.AdapterLuid.HighPart,
                        low: desc.AdapterLuid.LowPart,
                    },
                    driver_version,
                    is_software,
                });
            }
            idx += 1;
        }
    }
    out
}

/// Discards software/fallback renderers (like Microsoft Basic Render Driver)
/// whenever at least one physical/hardware GPU is present.
pub fn filter_adapters(mut adapters: Vec<AdapterInfo>) -> Vec<AdapterInfo> {
    if adapters.iter().any(|a| !a.is_software) {
        adapters.retain(|a| !a.is_software);
    }
    adapters
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// How many engine types one adapter may report.
///
/// Every engine type has to survive, not just the busy ones: the Performance
/// page lets the user chart a SPECIFIC engine, and an encoder that is idle
/// right now is exactly the one they are about to ask about. A modern GPU
/// exposes 3D, Compute, Copy, VideoDecode, VideoEncode, VideoProcessing,
/// Security and a couple of vendor engines, so the cap is a sanity bound
/// against a driver publishing unbounded instance names, not a display limit.
const MAX_ENGINE_TYPES: usize = 16;

/// One physical engine on one adapter: node, engine ordinal and type, as the
/// PDH instance names it. Every process using that engine has its own
/// instance with the same key.
type EngineKey<'a> = (Option<u32>, Option<u32>, &'a str);

/// Load of every physical engine on ONE adapter.
///
/// Processes time-slice an engine, so a process's instance is its share of
/// that engine's time and the shares add up to the engine's load. Taking the
/// busiest single process instead understated the adapter whenever two
/// processes shared an engine, and put group rows (an app family, a user)
/// above the machine total they are shown under.
fn engine_loads<'a>(
    records: impl IntoIterator<Item = &'a crate::win::perfcounters::GpuEngineRecord>,
) -> std::collections::HashMap<EngineKey<'a>, f32> {
    let mut loads = std::collections::HashMap::new();
    for r in records {
        *loads
            .entry((r.phys_index, r.engine_index, r.engine_type.as_str()))
            .or_insert(0.0) += r.utilization_pct;
    }
    for load in loads.values_mut() {
        *load = f32::clamp(*load, 0.0, 100.0);
    }
    loads
}

/// Per-engine-type utilization of one adapter: the busiest engine of each
/// type (engines of one type run concurrently, summing them overstates).
///
/// Sorted by busiest first, which is what makes `engines[0]` the engine the
/// adapter's own utilization number came from.
fn adapter_engines(loads: &std::collections::HashMap<EngineKey<'_>, f32>) -> Vec<GpuEngine> {
    let mut best: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    for (&(_, _, engine_type), &load) in loads {
        let e = best.entry(engine_type).or_insert(0.0);
        *e = e.max(load);
    }
    let mut out: Vec<GpuEngine> = best
        .into_iter()
        .map(|(name, util)| GpuEngine {
            name: name.to_string(),
            util_pct: util,
        })
        .collect();
    // Busiest first, then by name so an all-idle adapter still reports a
    // stable order instead of the hash map's.
    out.sort_by(|a, b| {
        b.util_pct
            .partial_cmp(&a.util_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    out.truncate(MAX_ENGINE_TYPES);
    out
}

/// Merge DXGI adapters with LUID-keyed PDH records into per-adapter
/// `GpuInfo`s. Records for unknown LUIDs are ignored rather than spread over
/// every adapter; adapters without records report honest zeros/None.
pub fn merge(
    adapters: Vec<AdapterInfo>,
    engine_records: &[crate::win::perfcounters::GpuEngineRecord],
    adapter_mem_records: &[crate::win::perfcounters::GpuAdapterMemRecord],
) -> Vec<GpuInfo> {
    adapters
        .into_iter()
        .enumerate()
        .map(|(id, a)| {
            let loads = engine_loads(engine_records.iter().filter(|r| r.luid == a.luid));
            // Busiest engine on THIS adapter == adapter utilization.
            let util = loads.values().copied().fold(0.0f32, f32::max);
            let mem = adapter_mem_records.iter().find(|m| m.luid == a.luid);
            let ded_used = mem.map_or(0, |m| m.dedicated_bytes);
            let shared_used = mem.map_or(0, |m| m.shared_bytes);
            GpuInfo {
                id,
                name: if a.name.is_empty() {
                    format!("GPU {id}")
                } else {
                    a.name
                },
                driver_version: a.driver_version,
                util_pct: util,
                mem_used_bytes: ded_used,
                mem_total_bytes: a.dedicated_vram,
                dedicated_used_bytes: ded_used,
                shared_used_bytes: shared_used,
                temperature_c: None,
                luid: Some(a.luid),
                engines: adapter_engines(&loads),
            }
        })
        .collect()
}

/// Per-process GPU view: dominant engine + aggregated memory.
/// Utilization follows the busiest-engine rule; the dominant engine label
/// uses the adapter's index on the Performance page so it reads like Task
/// Manager's "GPU 0 - 3D".
pub struct ProcessGpuView {
    pub pid: u32,
    pub util_pct: f32,
    /// e.g. "GPU 0 - VideoDecode"; None when no engine sample exists.
    pub dominant_engine: Option<String>,
    /// Non-zero shares of each engine, for group rows.
    pub engines: Vec<GpuEngineShare>,
    pub dedicated_bytes: u64,
    pub shared_bytes: u64,
}

/// Per-process view over the adapters in `gpus` — the merged set the
/// Performance page and the machine totals are built from — and ONLY those.
///
/// PDH also publishes instances for adapters that set leaves out, the
/// software "Microsoft Basic Render Driver" above all: a WARP or headless
/// renderer pins its engines at 100 % while it is really CPU work. Counting
/// those records here showed a process at 100 % GPU under a 1 % total.
pub fn process_gpu_view(
    gpus: &[GpuInfo],
    engine_records: &[crate::win::perfcounters::GpuEngineRecord],
    mem_records: &[crate::win::perfcounters::GpuMemRecord],
) -> Vec<ProcessGpuView> {
    let adapter_of = |luid: Option<AdapterLuid>| {
        luid.and_then(|luid| gpus.iter().find(|g| g.luid == Some(luid)).map(|g| g.id))
    };
    let engines: Vec<_> = engine_records
        .iter()
        .filter_map(|r| Some((adapter_of(Some(r.luid))?, r)))
        .collect();
    let mems: Vec<_> = mem_records
        .iter()
        .filter(|m| adapter_of(m.luid).is_some())
        .collect();
    // One ordinal per physical engine, shared by every process on it.
    let mut ordinals: std::collections::HashMap<(usize, EngineKey<'_>), u32> =
        std::collections::HashMap::new();
    for &(adapter, r) in &engines {
        let next = ordinals.len() as u32;
        ordinals
            .entry((
                adapter,
                (r.phys_index, r.engine_index, r.engine_type.as_str()),
            ))
            .or_insert(next);
    }
    let mut pids: Vec<u32> = engines
        .iter()
        .filter_map(|(_, r)| r.pid)
        .chain(mems.iter().filter_map(|m| m.pid))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids.into_iter()
        .map(|pid| {
            let own: Vec<_> = engines
                .iter()
                .filter(|(_, r)| r.pid == Some(pid))
                .copied()
                .collect();
            let best = own.iter().copied().max_by(|a, b| {
                a.1.utilization_pct
                    .partial_cmp(&b.1.utilization_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            ProcessGpuView {
                pid,
                util_pct: own
                    .iter()
                    .map(|(_, r)| r.utilization_pct)
                    .fold(0.0f32, f32::max)
                    .clamp(0.0, 100.0),
                dominant_engine: best
                    .map(|(adapter, r)| format!("GPU {adapter} - {}", r.engine_type)),
                engines: own
                    .iter()
                    .filter(|(_, r)| r.utilization_pct > 0.0)
                    .map(|&(adapter, r)| GpuEngineShare {
                        adapter,
                        engine: ordinals[&(
                            adapter,
                            (r.phys_index, r.engine_index, r.engine_type.as_str()),
                        )],
                        util_pct: r.utilization_pct,
                    })
                    .collect(),
                dedicated_bytes: mems
                    .iter()
                    .filter(|m| m.pid == Some(pid))
                    .map(|m| m.dedicated_bytes)
                    .sum(),
                shared_bytes: mems
                    .iter()
                    .filter(|m| m.pid == Some(pid))
                    .map(|m| m.shared_bytes)
                    .sum(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::perfcounters::{GpuAdapterMemRecord, GpuEngineRecord, GpuMemRecord};

    fn luid(low: u32) -> AdapterLuid {
        AdapterLuid { high: 0, low }
    }

    fn eng(luid_low: u32, pid: u32, typ: &str, pct: f32) -> GpuEngineRecord {
        GpuEngineRecord {
            luid: luid(luid_low),
            pid: Some(pid),
            phys_index: Some(0),
            engine_index: Some(0),
            engine_type: typ.into(),
            utilization_pct: pct,
        }
    }

    #[test]
    fn gpu_multi_luid_records_do_not_cross_assign_utilization() {
        let adapters = vec![
            AdapterInfo {
                name: "iGPU".into(),
                dedicated_vram: 1024,
                luid: luid(0x1111),
                driver_version: String::new(),
                is_software: false,
            },
            AdapterInfo {
                name: "dGPU".into(),
                dedicated_vram: 8192,
                luid: luid(0x2222),
                driver_version: String::new(),
                is_software: false,
            },
        ];
        // Only the iGPU has engine load; dGPU must stay at zero even though a
        // global "take first value" implementation would copy it everywhere.
        let records = vec![eng(0x1111, 500, "3D", 87.0)];
        let merged = merge(adapters, &records, &[]);
        assert_eq!(merged.len(), 2);
        assert!((merged[0].util_pct - 87.0).abs() < f32::EPSILON);
        assert_eq!(merged[1].util_pct, 0.0);
        assert_eq!(merged[1].engines.len(), 0);
        assert_eq!(merged[0].luid, Some(luid(0x1111)));
        assert_eq!(merged[1].luid, Some(luid(0x2222)));
    }

    #[test]
    fn busiest_engine_wins_and_memory_is_per_adapter() {
        let adapters = vec![AdapterInfo {
            name: "gpu".into(),
            dedicated_vram: 4096,
            luid: luid(7),
            driver_version: String::new(),
            is_software: false,
        }];
        let records = vec![
            eng(7, 1, "3D", 20.0),
            eng(7, 2, "Copy", 95.0),
            eng(9, 3, "3D", 99.0), // other adapter — ignored
        ];
        let mems = vec![
            GpuMemRecord {
                luid: Some(luid(7)),
                pid: Some(1),
                dedicated_bytes: 10,
                shared_bytes: 4,
            },
            GpuMemRecord {
                luid: Some(luid(9)),
                pid: Some(3),
                dedicated_bytes: 999,
                shared_bytes: 0,
            },
        ];
        let adapter_mems = vec![
            GpuAdapterMemRecord {
                luid: luid(7),
                dedicated_bytes: 10,
                shared_bytes: 4,
            },
            GpuAdapterMemRecord {
                luid: luid(9),
                dedicated_bytes: 999,
                shared_bytes: 0,
            },
        ];
        let merged = merge(adapters, &records, &adapter_mems);
        assert_eq!(merged.len(), 1);
        let g = &merged[0];
        assert!((g.util_pct - 95.0).abs() < f32::EPSILON, "busiest engine");
        assert_eq!(
            g.engines[0].name, "Copy",
            "busiest engine is reported first"
        );
        // An idle engine must survive: the Performance page charts engines by
        // name, and an encoder at 0 % is exactly what a user asks to see.
        assert!(
            g.engines.iter().any(|e| e.name == "3D"),
            "idle engines are kept: {:?}",
            g.engines
        );
        assert_eq!(g.mem_used_bytes, 10);
        assert_eq!(g.shared_used_bytes, 4);

        // Per-process dominant engine helper shares the same semantics.
        let procs = process_gpu_view(&merged, &records, &mems);
        let p2 = procs.iter().find(|p| p.pid == 2).expect("pid 2 present");
        assert!((p2.util_pct - 95.0).abs() < f32::EPSILON);
        assert_eq!(p2.dominant_engine.as_deref(), Some("GPU 0 - Copy"));
    }

    fn hw(name: &str, luid_low: u32) -> AdapterInfo {
        AdapterInfo {
            name: name.into(),
            dedicated_vram: 8192,
            luid: luid(luid_low),
            driver_version: String::new(),
            is_software: false,
        }
    }

    /// Processes time-slice one engine, so their shares ADD UP to its load.
    /// The busiest single process understated the adapter (60 % for an
    /// engine two processes kept 90 % busy) and put the sum of two rows
    /// above the machine total.
    #[test]
    fn engine_load_is_the_sum_of_every_process_share() {
        let records = vec![
            eng(7, 1, "3D", 60.0),
            eng(7, 2, "3D", 30.0),
            eng(7, 3, "VideoDecode", 50.0),
            eng(7, 4, "VideoDecode", 20.0),
        ];
        let merged = merge(vec![hw("gpu", 7)], &records, &[]);
        let g = &merged[0];
        assert!((g.util_pct - 90.0).abs() < 1e-4, "{}", g.util_pct);
        assert_eq!(g.engines[0].name, "3D");
        assert!((g.engines[0].util_pct - 90.0).abs() < 1e-4);
        assert!((g.engines[1].util_pct - 70.0).abs() < 1e-4);

        // Counter rounding can push the shares of one engine past 100 %.
        let over = merge(
            vec![hw("gpu", 7)],
            &[eng(7, 1, "3D", 70.0), eng(7, 2, "3D", 40.0)],
            &[],
        );
        assert_eq!(over[0].util_pct, 100.0);

        // Two instances of one TYPE are two engines: no summing across them.
        let mut second_copy = eng(7, 2, "Copy", 40.0);
        second_copy.engine_index = Some(1);
        let copies = merge(
            vec![hw("gpu", 7)],
            &[eng(7, 1, "Copy", 40.0), second_copy],
            &[],
        );
        assert!((copies[0].util_pct - 40.0).abs() < 1e-4);
    }

    /// Regression: a process rendering on the hidden Basic Render Driver
    /// showed 100 % GPU under a 1 % machine total, because the per-process
    /// view counted every LUID while the adapters left the software one
    /// out. Its records, memory and engine label must all stay out.
    #[test]
    fn process_view_covers_only_the_merged_adapters() {
        let software = AdapterInfo {
            name: "Microsoft Basic Render Driver".into(),
            dedicated_vram: 0,
            luid: luid(0xd52a),
            driver_version: String::new(),
            is_software: true,
        };
        let shown = filter_adapters(vec![hw("RTX", 0xbcdb), software]);
        let records = vec![
            eng(0xd52a, 10, "3D", 100.0), // WARP test pinning the software adapter
            eng(0xbcdb, 10, "Copy", 1.0),
            eng(0xd52a, 11, "3D", 100.0), // only ever touched the software adapter
            eng(0xbcdb, 12, "3D", 1.0),
        ];
        let mems = vec![
            GpuMemRecord {
                luid: Some(luid(0xd52a)),
                pid: Some(10),
                dedicated_bytes: 0,
                shared_bytes: 500,
            },
            GpuMemRecord {
                luid: Some(luid(0xbcdb)),
                pid: Some(10),
                dedicated_bytes: 7,
                shared_bytes: 3,
            },
        ];
        let gpus = merge(shown, &records, &[]);
        let procs = process_gpu_view(&gpus, &records, &mems);
        let total = gpus.iter().map(|g| g.util_pct).fold(0.0, f32::max);
        for p in &procs {
            assert!(p.util_pct <= total, "pid {} above the total", p.pid);
        }
        let p10 = procs.iter().find(|p| p.pid == 10).expect("pid 10");
        assert!((p10.util_pct - 1.0).abs() < 1e-4);
        assert_eq!(p10.dominant_engine.as_deref(), Some("GPU 0 - Copy"));
        assert_eq!((p10.dedicated_bytes, p10.shared_bytes), (7, 3));
        assert!(
            procs.iter().all(|p| p.pid != 11),
            "a process measured only on a hidden adapter is not a GPU row"
        );
    }

    /// The label names the adapter as the Performance page does ("GPU 1"),
    /// not PDH's per-adapter node index, which is 0 on every single-node
    /// GPU; and every process on one engine gets the same engine id.
    #[test]
    fn process_view_labels_by_adapter_and_shares_engine_ids() {
        let records = vec![
            eng(1, 5, "3D", 10.0),
            eng(2, 5, "3D", 40.0),
            eng(2, 6, "3D", 20.0),
            eng(2, 6, "Copy", 0.0),
        ];
        let gpus = merge(vec![hw("iGPU", 1), hw("dGPU", 2)], &records, &[]);
        let procs = process_gpu_view(&gpus, &records, &[]);
        let p5 = procs.iter().find(|p| p.pid == 5).expect("pid 5");
        let p6 = procs.iter().find(|p| p.pid == 6).expect("pid 6");
        assert_eq!(p5.dominant_engine.as_deref(), Some("GPU 1 - 3D"));
        assert_eq!(p5.engines.len(), 2);
        let d3d = |p: &ProcessGpuView| {
            p.engines
                .iter()
                .find(|e| e.adapter == 1)
                .map(|e| e.engine)
                .expect("dGPU 3D share")
        };
        assert_eq!(d3d(p5), d3d(p6), "same physical engine, same id");
        assert_ne!(
            p5.engines[0].engine, p5.engines[1].engine,
            "same engine type on two adapters is two engines"
        );
        // Idle shares are left out; the process is still measured.
        assert_eq!(p6.engines.len(), 1);
    }

    #[test]
    fn adapter_memory_comes_from_adapter_records_not_process_sum() {
        let adapters = vec![AdapterInfo {
            name: "NVIDIA GeForce RTX 5070".into(),
            dedicated_vram: 12 * 1024 * 1024 * 1024,
            luid: luid(42),
            driver_version: "32.0.16.1692".into(),
            is_software: false,
        }];
        // Real adapter usage: 1.5 GB dedicated, 150 MB shared.
        let adapter_mems = vec![GpuAdapterMemRecord {
            luid: luid(42),
            dedicated_bytes: 1_500_000_000,
            shared_bytes: 150_000_000,
        }];
        let merged = merge(adapters, &[], &adapter_mems);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].dedicated_used_bytes, 1_500_000_000);
        assert_eq!(merged[0].mem_used_bytes, 1_500_000_000);
        assert_eq!(merged[0].shared_used_bytes, 150_000_000);
    }

    #[test]
    fn filter_adapters_drops_software_when_hardware_present() {
        let mixed = vec![
            AdapterInfo {
                name: "NVIDIA GeForce RTX 5070".into(),
                dedicated_vram: 12 * 1024 * 1024 * 1024,
                luid: luid(1),
                driver_version: "32.0.16.1692".into(),
                is_software: false,
            },
            AdapterInfo {
                name: "Microsoft Basic Render Driver".into(),
                dedicated_vram: 0,
                luid: luid(2),
                driver_version: String::new(),
                is_software: true,
            },
        ];
        let filtered = filter_adapters(mixed);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "NVIDIA GeForce RTX 5070");

        // When only software adapter is present (e.g. headless/VM), it is preserved.
        let software_only = vec![AdapterInfo {
            name: "Microsoft Basic Render Driver".into(),
            dedicated_vram: 0,
            luid: luid(2),
            driver_version: String::new(),
            is_software: true,
        }];
        let preserved = filter_adapters(software_only);
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0].name, "Microsoft Basic Render Driver");
    }

    #[test]
    fn live_adapters_contains_only_hardware_gpu_when_present() {
        let list = filter_adapters(adapters());
        if list.iter().any(|a| !a.is_software) {
            assert!(
                list.iter()
                    .all(|a| !a.name.contains("Microsoft Basic Render Driver")),
                "Microsoft Basic Render Driver should not be present when real GPU exists: {list:?}"
            );
        }
    }
}
