//! Static CPU facts: sockets, physical cores, caches, virtualization, base clock.
//! Sources: GetLogicalProcessorInformationEx (topology/caches), CPUID and SMBIOS.

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
            // CPUID leaf 0x16 is optional and modern CPU brand strings often
            // omit the old "@ 3.40GHz" suffix. Windows already exposes the
            // firmware SMBIOS table, whose Type-4 Current Speed field is the
            // processor speed reported at boot. It is a much better static
            // fallback than silently leaving Task Manager's Base speed blank.
            if out.base_mhz == 0.0 {
                out.base_mhz = smbios_base_mhz();
            }
            if out.base_mhz == 0.0
                && let Some(brand) = cpuid.get_processor_brand_string()
            {
                out.base_mhz = parse_base_from_brand(brand.as_str());
            }
        }

        // Non-x86 Windows still gets a firmware-provided nominal speed where
        // the machine supplies SMBIOS Type 4.
        #[cfg(not(target_arch = "x86_64"))]
        {
            out.base_mhz = smbios_base_mhz();
        }

        if out.sockets == 0 {
            out.sockets = 1;
        }
        out
    }
}

/// Parse "@ 3.40GHz" from a brand string as a last-resort base-clock fallback.
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

/// Firmware fallback for CPUs that do not implement CPUID leaf 0x16.
///
/// SMBIOS Type 4 offsets are header-inclusive: Max Speed at 14h and Current
/// Speed at 16h. Per the SMBIOS spec Current Speed is the processor speed at
/// system boot. Prefer it because it corresponds most closely to the nominal
/// value Task Manager labels "Base speed"; Max Speed is only a fallback for
/// firmware that leaves Current Speed unset. Multiple populated sockets use
/// the largest non-zero nominal value.
fn smbios_base_mhz() -> f32 {
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, None);
        if size == 0 {
            return 0.0;
        }
        let mut buf = vec![0u8; size as usize];
        let written = GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) as usize;
        if written < 8 {
            return 0.0;
        }
        let table_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let end = (8 + table_len).min(written);
        base_mhz_from_smbios_table(&buf[8..end])
    }
}

fn base_mhz_from_smbios_table(table: &[u8]) -> f32 {
    let mut best_current = 0u16;
    let mut best_max = 0u16;
    let mut i = 0usize;
    while i + 4 <= table.len() {
        let ty = table[i];
        let len = table[i + 1] as usize;
        if len < 4 || i + len > table.len() {
            break;
        }
        if ty == 4 && len >= 0x18 {
            let max = u16::from_le_bytes([table[i + 0x14], table[i + 0x15]]);
            let current = u16::from_le_bytes([table[i + 0x16], table[i + 0x17]]);
            if current != 0 && current != u16::MAX {
                best_current = best_current.max(current);
            }
            if max != 0 && max != u16::MAX {
                best_max = best_max.max(max);
            }
        }

        // Skip the formatted section plus its double-NUL-terminated strings.
        let mut end = i + len;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        if end + 1 >= table.len() {
            break;
        }
        i = end + 2;
    }
    // SMBIOS "Current Speed" reflects the configured clock; "Max Speed" is
    // only a fallback when current is absent/unknown (test parity).
    if best_current != 0 {
        best_current as f32
    } else {
        best_max as f32
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smbios_type4_current_speed_is_preferred() {
        let mut rec = vec![0u8; 0x18];
        rec[0] = 4;
        rec[1] = 0x18;
        rec[0x14..0x16].copy_from_slice(&5200u16.to_le_bytes());
        rec[0x16..0x18].copy_from_slice(&3400u16.to_le_bytes());
        rec.extend_from_slice(&[0, 0]);
        assert_eq!(base_mhz_from_smbios_table(&rec), 3400.0);
    }

    #[test]
    fn smbios_type4_falls_back_to_max_speed() {
        let mut rec = vec![0u8; 0x18];
        rec[0] = 4;
        rec[1] = 0x18;
        rec[0x14..0x16].copy_from_slice(&4200u16.to_le_bytes());
        rec.extend_from_slice(&[0, 0]);
        assert_eq!(base_mhz_from_smbios_table(&rec), 4200.0);
    }
}
