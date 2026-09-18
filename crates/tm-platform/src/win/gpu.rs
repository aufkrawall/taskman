//! GPU adapter discovery via DXGI and LUID-accurate merge with PDH records.
//!
//! Correctness rules (implement.md §13):
//! * DXGI `AdapterLuid` is the join key between static adapter info and PDH
//!   engine/memory records — one adapter's utilization is never copied to
//!   another.
//! * Adapter utilization follows Task Manager semantics: the busiest engine
//!   on that adapter (max), not a naive sum which would exceed 100 % and
//!   misattribute multi-engine load. Per-process values use the same rule;
//!   the dominant engine is preserved for the "GPU engine" column.

use tm_core::model::{AdapterLuid, GpuEngine, GpuInfo};
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
    filter_adapters(out)
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

/// Aggregated per-engine-type utilization of one adapter (max across
/// instances of that type — engines run concurrently, summing overstates).
///
/// Sorted by busiest first, which is what makes `engines[0]` the engine the
/// adapter's own utilization number came from.
fn adapter_engines<'a>(
    records: impl IntoIterator<Item = &'a crate::win::perfcounters::GpuEngineRecord>,
) -> Vec<GpuEngine> {
    let mut best: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    for r in records {
        let e = best.entry(r.engine_type.as_str()).or_insert(0.0);
        *e = e.max(r.utilization_pct);
    }
    let mut out: Vec<GpuEngine> = best
        .into_iter()
        .map(|(name, util)| GpuEngine {
            name: name.to_string(),
            util_pct: util.clamp(0.0, 100.0),
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
    mem_records: &[crate::win::perfcounters::GpuMemRecord],
) -> Vec<GpuInfo> {
    adapters
        .into_iter()
        .enumerate()
        .map(|(id, a)| {
            let own_engines: Vec<_> = engine_records.iter().filter(|r| r.luid == a.luid).collect();
            // Busiest relevant engine on THIS adapter == adapter utilization.
            let util = own_engines
                .iter()
                .map(|r| r.utilization_pct)
                .fold(0.0f32, f32::max);
            let ded_used: u64 = mem_records
                .iter()
                .filter(|m| m.luid == Some(a.luid))
                .map(|m| m.dedicated_bytes)
                .sum();
            let shared_used: u64 = mem_records
                .iter()
                .filter(|m| m.luid == Some(a.luid))
                .map(|m| m.shared_bytes)
                .sum();
            GpuInfo {
                id,
                name: if a.name.is_empty() {
                    format!("GPU {id}")
                } else {
                    a.name
                },
                driver_version: a.driver_version,
                util_pct: util.clamp(0.0, 100.0),
                mem_used_bytes: ded_used,
                mem_total_bytes: a.dedicated_vram,
                dedicated_used_bytes: ded_used,
                shared_used_bytes: shared_used,
                temperature_c: None,
                luid: Some(a.luid),
                engines: adapter_engines(own_engines.iter().copied()),
            }
        })
        .collect()
}

/// Per-process GPU view: dominant engine + aggregated memory.
/// Utilization follows the busiest-engine rule; the dominant engine label
/// uses the physical adapter index so it reads like Task Manager's
/// "GPU 0 - 3D".
pub struct ProcessGpuView {
    pub pid: u32,
    pub util_pct: f32,
    /// e.g. "GPU 0 - VideoDecode"; None when no engine sample exists.
    pub dominant_engine: Option<String>,
    pub dedicated_bytes: u64,
    pub shared_bytes: u64,
}

pub fn process_gpu_view(
    engine_records: &[crate::win::perfcounters::GpuEngineRecord],
    mem_records: &[crate::win::perfcounters::GpuMemRecord],
) -> Vec<ProcessGpuView> {
    let mut pids: Vec<u32> = engine_records
        .iter()
        .filter_map(|r| r.pid)
        .chain(mem_records.iter().filter_map(|m| m.pid))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids.into_iter()
        .map(|pid| {
            let own: Vec<_> = engine_records
                .iter()
                .filter(|r| r.pid == Some(pid))
                .collect();
            let best = own.iter().copied().max_by(|a, b| {
                a.utilization_pct
                    .partial_cmp(&b.utilization_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            ProcessGpuView {
                pid,
                util_pct: own
                    .iter()
                    .map(|r| r.utilization_pct)
                    .fold(0.0f32, f32::max)
                    .clamp(0.0, 100.0),
                dominant_engine: best
                    .map(|r| format!("GPU {} - {}", r.phys_index.unwrap_or(0), r.engine_type)),
                dedicated_bytes: mem_records
                    .iter()
                    .filter(|m| m.pid == Some(pid))
                    .map(|m| m.dedicated_bytes)
                    .sum(),
                shared_bytes: mem_records
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
    use crate::win::perfcounters::{GpuEngineRecord, GpuMemRecord};

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
        let merged = merge(adapters, &records, &mems);
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
        let procs = process_gpu_view(&records, &mems);
        let p2 = procs.iter().find(|p| p.pid == 2).expect("pid 2 present");
        assert!((p2.util_pct - 95.0).abs() < f32::EPSILON);
        assert_eq!(p2.dominant_engine.as_deref(), Some("GPU 0 - Copy"));
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
        let list = adapters();
        if list.iter().any(|a| !a.is_software) {
            assert!(
                list.iter()
                    .all(|a| !a.name.contains("Microsoft Basic Render Driver")),
                "Microsoft Basic Render Driver should not be present when real GPU exists: {list:?}"
            );
        }
    }
}
