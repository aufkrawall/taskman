//! Static CPU facts: sockets, physical cores, caches, virtualization, base clock.
//! Sources: GetLogicalProcessorInformationEx (topology/caches) + raw-cpuid.

use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct CpuStatic {
    pub sockets: usize,
    pub physical_cores: usize,
    /// Total distinct L1 cache across the package, KB.
    pub l1_kb_total: u64,
    pub l2_kb_total: u64,
    pub l3_kb_total: u64,
    pub base_mhz: f32,
    pub virtualization: String,
}

impl CpuStatic {
    pub fn probe() -> CpuStatic {
        let mut out = CpuStatic::default();

        // ---- topology + caches via GLPI ------------------------------------
        collect_topology(&mut out);

        // ---- cpuid extras ---------------------------------------------------
        #[cfg(target_arch = "x86_64")]
        {
            let cpuid = raw_cpuid::CpuId::new();
            if let Some(fi) = cpuid.get_feature_info() {
                let hypervisor = fi.has_hypervisor();
                let has_vmx = fi.has_vmx();
                let has_svm = cpuid
                    .get_extended_processor_and_feature_identifiers()
                    .is_some_and(|x| x.has_svm());
                out.virtualization = if has_vmx || has_svm || hypervisor {
                    "Enabled".into()
                } else {
                    "Disabled".into()
                };
            }
            if let Some(freq) = cpuid.get_processor_frequency_info() {
                out.base_mhz = freq.processor_base_frequency() as f32;
            }
            if out.base_mhz == 0.0
                && let Some(brand) = cpuid.get_processor_brand_string()
            {
                out.base_mhz = parse_base_from_brand(brand.as_str());
            }
        }

        if out.sockets == 0 {
            out.sockets = 1;
        }
        out
    }
}

/// Parse "@ 3.40GHz" from a brand string as a base-clock fallback.
fn parse_base_from_brand(brand: &str) -> f32 {
    let Some(idx) = brand.to_ascii_lowercase().find("ghz") else {
        return 0.0;
    };
    let lower = brand.to_ascii_lowercase();
    let start = lower[..idx]
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == ' '))
        .map_or(0, |(i, _)| i + 1);
    lower[start..idx]
        .trim()
        .parse()
        .ok()
        .map_or(0.0, |g: f32| g * 1000.0)
}

fn collect_topology(out: &mut CpuStatic) {
    unsafe {
        use windows::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, RelationAll, RelationCache, RelationProcessorCore,
            RelationProcessorPackage, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
        };

        let mut len: u32 = 0;
        let _ = GetLogicalProcessorInformationEx(RelationAll, None, &mut len);
        if len == 0 {
            return;
        }
        let mut buf = vec![0u8; len as usize];
        if GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr() as *mut _), &mut len)
            .is_err()
        {
            return;
        }

        // Walk the variable-length entries.
        let mut offset = 0usize;
        let mut seen_l1: HashSet<u64> = HashSet::new(); // dedupe by mask+size
        while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= buf.len() {
            let entry =
                &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            let relationship = entry.Relationship;
            let rel_pkg = RelationProcessorPackage.0;
            let rel_core = RelationProcessorCore.0;
            let rel_cache = RelationCache.0;
            match relationship.0 {
                r if r == rel_pkg => out.sockets += 1,
                r if r == rel_core => out.physical_cores += 1,
                r if r == rel_cache => {
                    let cache = &entry.Anonymous.Cache;
                    // Sharers across processor groups (usually one group).
                    let sharers = cache.Anonymous.GroupMask.Mask.count_ones().max(1) as u64;
                    let size_bytes = cache.CacheSize as u64;
                    let level = cache.Level;
                    match level {
                        1 => out.l1_kb_total += size_bytes / sharers / 1024,
                        2 => out.l2_kb_total += size_bytes / sharers / 1024,
                        3 => out.l3_kb_total += size_bytes / sharers / 1024,
                        _ => {}
                    }
                    let _ = &mut seen_l1;
                }
                _ => {}
            }
            offset += entry.Size as usize;
            if entry.Size == 0 {
                break; // safety against malformed data
            }
        }
    }
}
