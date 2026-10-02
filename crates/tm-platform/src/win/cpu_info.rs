//! Static CPU facts: sockets, physical cores, caches, virtualization, base clock.
//! Sources: GetLogicalProcessorInformationEx (topology/caches), CPUID and SMBIOS.

#[derive(Debug, Clone, Default)]
pub struct CpuStatic {
    pub sockets: usize,
    pub physical_cores: usize,
    /// Total L1 cache (data + instruction) summed over every distinct
    /// cache instance on the machine, KB — what Task Manager labels L1.
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

        out.virtualization = virtualization_state().into();

        // ---- cpuid extras ---------------------------------------------------
        #[cfg(target_arch = "x86_64")]
        {
            let cpuid = raw_cpuid::CpuId::new();
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

        // A failed topology walk leaves `sockets` at 0, the model's
        // "unknown"; it used to be patched to 1 next to an unknown core count.
        out
    }
}

/// Task Manager's "Virtualization" row: whether hardware virtualization is
/// usable, not merely whether the CPU implements it.
///
/// CPUID's VMX/SVM bits describe the silicon and stay set when the firmware
/// has the feature switched off, so they reported "Enabled" on exactly the
/// machines where it is not. `PF_VIRT_FIRMWARE_ENABLED` is the OS's answer to
/// the firmware question. A running hypervisor (Hyper-V, VBS) also counts:
/// the root partition then sees virtualized CPUID, but the feature is in use.
fn virtualization_state() -> &'static str {
    use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED};

    let firmware_enabled = unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }.as_bool();
    #[cfg(target_arch = "x86_64")]
    let hypervisor = raw_cpuid::CpuId::new()
        .get_feature_info()
        .is_some_and(|fi| fi.has_hypervisor());
    #[cfg(not(target_arch = "x86_64"))]
    let hypervisor = false;
    if firmware_enabled || hypervisor {
        "Enabled"
    } else {
        "Disabled"
    }
}

/// Parse "@ 3.40GHz" from a brand string as a last-resort base-clock fallback.
/// x86_64 only: ARM64 has no CPUID brand string.
#[cfg(target_arch = "x86_64")]
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
        let mut buf = super::aligned::AlignedBuf::zeroed(len as usize);
        if GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr() as *mut _), &mut len)
            .is_err()
        {
            return;
        }

        // Walk the variable-length entries.
        let mut offset = 0usize;
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
                    add_cache(out, cache.Level, u64::from(cache.CacheSize));
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

/// Fold one `RelationCache` record into the per-level totals.
///
/// Every record is one physical cache instance; its processor mask only says
/// which logical processors share it. The size is therefore counted once, in
/// full — dividing it by the sharers reported a 32 MB L3 shared by 16
/// threads as 2 MB, and halved every SMT core's L1/L2.
fn add_cache(out: &mut CpuStatic, level: u8, size_bytes: u64) {
    let kb = size_bytes / 1024;
    match level {
        1 => out.l1_kb_total += kb,
        2 => out.l2_kb_total += kb,
        3 => out.l3_kb_total += kb,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ryzen 7 5700X: 8 SMT cores, each with 32 KB L1d + 32 KB L1i + 512 KB
    /// L2 shared by its two threads, and one 32 MB L3 shared by all 16.
    /// Task Manager shows 512 KB / 4.0 MB / 32.0 MB.
    #[test]
    fn shared_caches_count_once_at_full_size() {
        let mut out = CpuStatic::default();
        for _core in 0..8 {
            add_cache(&mut out, 1, 32 * 1024);
            add_cache(&mut out, 1, 32 * 1024);
            add_cache(&mut out, 2, 512 * 1024);
        }
        add_cache(&mut out, 3, 32 * 1024 * 1024);
        assert_eq!(out.l1_kb_total, 512);
        assert_eq!(out.l2_kb_total, 4096);
        assert_eq!(out.l3_kb_total, 32 * 1024);
    }

    /// Live: the kernel topology walk yields caches, and the virtualization
    /// row is always one of the two states the UI translates.
    #[test]
    fn live_topology_reports_caches_and_a_virtualization_state() {
        let mut out = CpuStatic::default();
        collect_topology(&mut out);
        assert!(out.physical_cores > 0);
        assert!(out.l1_kb_total > 0 && out.l2_kb_total > 0);
        assert!(matches!(virtualization_state(), "Enabled" | "Disabled"));
    }

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
