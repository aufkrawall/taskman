//! GPU adapter discovery via DXGI.

use tm_core::model::{GpuEngine, GpuInfo};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

/// Static adapter info (name, VRAM, LUID).
#[derive(Debug, Clone)]
#[allow(dead_code)] // luid fields reserved for per-GPU counter mapping
pub struct AdapterInfo {
    pub name: String,
    pub dedicated_vram: u64,
    pub luid_high: i32,
    pub luid_low: u32,
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
        loop {
            match factory.EnumAdapters1(idx) {
                Ok(adapter) => {
                    if let Ok(desc) = adapter.GetDesc1() {
                        out.push(AdapterInfo {
                            name: utf16_to_string(&desc.Description),
                            dedicated_vram: desc.DedicatedVideoMemory as u64,
                            luid_high: desc.AdapterLuid.HighPart,
                            luid_low: desc.AdapterLuid.LowPart,
                            driver_version: String::new(),
                        });
                    }
                    idx += 1;
                }
                Err(_) => break,
            }
        }
    }
    out
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Merge DXGI adapters with PDH engine stats into `GpuInfo`s.
pub fn merge(adapters: Vec<AdapterInfo>, engines: Vec<GpuEngine>) -> Vec<GpuInfo> {
    let total_util = engines.first().map(|e| e.util_pct).unwrap_or(0.0);
    adapters
        .into_iter()
        .enumerate()
        .map(|(id, a)| GpuInfo {
            id,
            name: if a.name.is_empty() { format!("GPU {id}") } else { a.name },
            driver_version: a.driver_version,
            util_pct: total_util,
            mem_used_bytes: 0,
            mem_total_bytes: a.dedicated_vram,
            dedicated_used_bytes: 0,
            temperature_c: None,
            engines: engines.clone(),
        })
        .collect()
}
