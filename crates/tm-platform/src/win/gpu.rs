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
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

/// Static adapter info (name, VRAM, LUID).
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub name: String,
    pub dedicated_vram: u64,
    pub luid: AdapterLuid,
    pub driver_version: String,
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
                out.push(AdapterInfo {
                    name: utf16_to_string(&desc.Description),
                    dedicated_vram: desc.DedicatedVideoMemory as u64,
                    luid: AdapterLuid {
                        high: desc.AdapterLuid.HighPart,
                        low: desc.AdapterLuid.LowPart,
                    },
                    driver_version: String::new(),
                });
            }
            idx += 1;
        }
    }
    out
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Aggregated per-engine-type utilization of one adapter (max across
/// instances of that type — engines run concurrently, summing overstates).
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
    out.sort_by(|a, b| {
        b.util_pct
            .partial_cmp(&a.util_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(6);
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
            },
            AdapterInfo {
                name: "dGPU".into(),
                dedicated_vram: 8192,
                luid: luid(0x2222),
                driver_version: String::new(),
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
        assert_eq!(g.engines[0].name, "Copy");
        assert_eq!(g.mem_used_bytes, 10);
        assert_eq!(g.shared_used_bytes, 4);

        // Per-process dominant engine helper shares the same semantics.
        let procs = process_gpu_view(&records, &mems);
        let p2 = procs.iter().find(|p| p.pid == 2).expect("pid 2 present");
        assert!((p2.util_pct - 95.0).abs() < f32::EPSILON);
        assert_eq!(p2.dominant_engine.as_deref(), Some("GPU 0 - Copy"));
    }
}
