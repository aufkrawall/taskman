//! Static RAM hardware facts via SMBIOS (Type 16 Memory Array + Type 17
//! Memory Device), read once through `GetSystemFirmwareTable(RSMB)`.
//!
//! This is what the original Task Manager shows on its Memory page: speed
//! (MT/s), slots used, form factor — plus manufacturer/part number and the
//! hardware-reserved amount (installed RAM minus what Windows can address).

use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

#[derive(Debug, Clone, Default)]
pub struct RamStatic {
    /// Physically installed bytes (populated modules).
    pub installed_bytes: u64,
    /// Configured speed of the fastest populated module, MT/s.
    pub speed_mts: u32,
    /// Maximum supported speed, MT/s.
    pub speed_max_mts: u32,
    pub slots_used: u32,
    pub slots_total: u32,
    /// "DIMM" / "SODIMM" / ... of the first populated module.
    pub form_factor: String,
    pub manufacturer: String,
    pub part_number: String,
}

/// Probe once at startup; never changes while the machine runs.
pub fn probe() -> RamStatic {
    let Some(table) = smbios_table() else {
        return RamStatic::default();
    };
    ram_static_from_table(&table)
}

/// Pure Type-16/17 walker so malformed/truncated tables are testable. The
/// table is firmware-supplied (hypervisor-controlled in VMs), so EVERY field
/// read is length-guarded — a spec-legal short record must degrade to fewer
/// facts, never panic (release builds use `panic = "abort"`, so one bad
/// index would kill the whole app in `Sampler::lazy_init`).
fn ram_static_from_table(table: &[u8]) -> RamStatic {
    let mut out = RamStatic::default();
    let mut modules: Vec<Module> = Vec::new();
    let mut slots_total = 0u32;

    for rec in records(table) {
        match rec.r#type {
            16 => {
                // Memory Array: Number of Possible Slots at 0Dh (word).
                if rec.data.len() >= 0x0F {
                    slots_total += u16_from(rec.data, 0x0D) as u32;
                }
            }
            17 => {
                // Memory Device. Field offsets grow with the SMBIOS
                // revision: 0x15-byte records (2.3–2.5) end right after the
                // form factor, so every later read needs its own guard.
                let d = rec.data;
                if d.len() < 0x0F {
                    continue;
                }
                let size_raw = u16_from(d, 0x0C);
                let populated = size_raw != 0 && size_raw != 0xFFFF;
                let form_factor = form_factor_label(d[0x0E]);
                // "Speed" (word) at 15h; absent on the shortest records.
                let speed_max = if d.len() >= 0x17 {
                    valid_speed(u16_from(d, 0x15))
                } else {
                    0
                };
                // SMBIOS 3.0+: configured clock speed at 20h (word).
                let speed_cfg = if d.len() >= 0x22 {
                    valid_speed(u16_from(d, 0x20))
                } else {
                    0
                };
                // Strings are 1-based indexes at 17h/1Ah (2.6+ records).
                let manufacturer = if d.len() >= 0x18 {
                    rec.string(d[0x17] as usize)
                } else {
                    String::new()
                };
                let part = if d.len() >= 0x1B {
                    rec.string(d[0x1A] as usize)
                } else {
                    String::new()
                };
                let size_bytes = if populated { module_size_bytes(d) } else { 0 };
                if populated {
                    modules.push(Module {
                        size_bytes,
                        // Older SMBIOS revisions only fill "Speed"; prefer
                        // the configured value, falling back to it.
                        speed_cfg: if speed_cfg > 0 { speed_cfg } else { speed_max },
                        speed_max,
                        form_factor,
                        manufacturer,
                        part_number: part,
                    });
                }
            }
            _ => {}
        }
    }

    out.slots_total = slots_total;
    out.slots_used = modules.len() as u32;
    out.installed_bytes = modules.iter().map(|m| m.size_bytes).sum();
    out.speed_mts = modules.iter().map(|m| m.speed_cfg).fold(0u32, u32::max);
    out.speed_max_mts = modules.iter().map(|m| m.speed_max).fold(0u32, u32::max);
    out.form_factor = modules
        .iter()
        .find(|m| !m.form_factor.is_empty())
        .map(|m| m.form_factor.clone())
        .or_else(|| modules.first().map(|_| "DIMM".to_string()))
        .unwrap_or_default();
    out.manufacturer = modules
        .iter()
        .map(|m| m.manufacturer.clone())
        .find(|m| !m.is_empty() && !m.eq_ignore_ascii_case("unknown"))
        .unwrap_or_default();
    out.part_number = modules
        .iter()
        .find(|m| !m.part_number.is_empty())
        .map(|m| m.part_number.clone())
        .unwrap_or_default();
    out
}

struct Module {
    size_bytes: u64,
    speed_cfg: u32,
    speed_max: u32,
    form_factor: String,
    manufacturer: String,
    part_number: String,
}

/// 0 / 0xFFFF are "unknown", not real speeds.
fn valid_speed(v: u16) -> u32 {
    if v == 0 || v == 0xFFFF { 0 } else { v as u32 }
}

/// Type 17 size: prefer the dword Extended Size (1Ch) when present; else the
/// word Size (0Ch). Per SMBIOS 3.1+ bit 15 selects MB vs KB — but some
/// vendors report MB without the flag, so sub-128 MB "KB" values (no real
/// module that small has shipped in decades) are re-read as MB.
fn module_size_bytes(d: &[u8]) -> u64 {
    if d.len() >= 0x20 {
        let ext = u32_from(d, 0x1C);
        if ext != 0 {
            // Bit 30 = units (set → MB), bits 29:0 = value.
            return if ext & 0x4000_0000 != 0 {
                (ext & 0x3FFF_FFFF) as u64 * 1024 * 1024
            } else {
                ext as u64 * 1024
            };
        }
    }
    let raw = u16_from(d, 0x0C);
    if raw & 0x8000 != 0 {
        (raw & 0x7FFF) as u64 * 1024 * 1024
    } else {
        let as_kb = raw as u64 * 1024;
        if as_kb < 128 * 1024 * 1024 {
            raw as u64 * 1024 * 1024
        } else {
            as_kb
        }
    }
}

fn form_factor_label(v: u8) -> String {
    match v {
        0x03 => "SIMM".into(),
        0x09 => "DIMM".into(),
        0x0C | 0x12 => "SODIMM".into(),
        0x0D => "SRIMM".into(),
        0x0F => "FB-DIMM".into(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- SMBIOS walk

struct Record<'a> {
    r#type: u8,
    data: &'a [u8], // formatted (header-inclusive) part
    strings: &'a [u8],
}

impl Record<'_> {
    /// 1-based string index → owned string ("" when unset).
    fn string(&self, idx: usize) -> String {
        if idx == 0 {
            return String::new();
        }
        for (cur, s) in (1usize..).zip(self.strings.split(|&b| b == 0)) {
            if cur == idx {
                return String::from_utf8_lossy(s).trim_end().to_string();
            }
        }
        String::new()
    }
}

/// Iterate the formatted+strings records of a raw SMBIOS structure table.
fn records(table: &[u8]) -> Vec<Record<'_>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= table.len() {
        let r#type = table[i];
        let len = table[i + 1] as usize;
        if len < 4 || i + len > table.len() {
            break;
        }
        // Strings region: after the formatted area, terminated by "\0\0".
        let str_start = i + len;
        let mut end = str_start;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        let strings_end = (end + 2).min(table.len());
        out.push(Record {
            r#type,
            data: &table[i..i + len],
            strings: &table[str_start..strings_end],
        });
        i = strings_end;
    }
    out
}

fn u16_from(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

fn u32_from(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

/// Fetch the raw SMBIOS entry table ("RSMB" firmware table provider).
/// Layout: UsedCallingMethod(1) VersionMajor(1) VersionMinor(1)
/// DmiRevision(1) Length(4) then the SMBIOS structure table.
fn smbios_table() -> Option<Vec<u8>> {
    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let written = GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) as usize;
        if written < 8 {
            return None;
        }
        let table_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let end = (8 + table_len).min(written);
        Some(buf[8..end].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a spec-legal SMBIOS 2.x Type-17 record is only 0x15 bytes
    /// long. The old code guarded with `< 0x15` and then read offsets up to
    /// 0x1A — an out-of-bounds panic (release builds abort the whole app).
    /// Short records must now contribute what they contain, never panic.
    #[test]
    fn short_smbios_type17_record_does_not_panic() {
        // 0x15 formatted bytes (header incl. Size@0Ch, FormFactor@0Eh) +
        // strings region "KINGSTON\0" + terminator.
        let mut rec = vec![0u8; 0x15];
        rec[0] = 17;
        rec[1] = 0x15;
        rec[0x0C..0x0E].copy_from_slice(&8192u16.to_le_bytes()); // 8 GB, populated
        rec[0x0E] = 0x09; // DIMM
        rec.extend_from_slice(b"KINGSTON\0\0");
        let mut table = rec;
        // A well-formed Type-16 slot record so slots_total is observable.
        let mut t16 = vec![0u8; 0x0F];
        t16[0] = 16;
        t16[1] = 0x0F;
        t16[0x0D..0x0F].copy_from_slice(&2u16.to_le_bytes()); // 2 slots
        t16.extend_from_slice(&[0, 0]);
        table.extend_from_slice(&t16);

        let out = ram_static_from_table(&table);
        assert_eq!(out.slots_used, 1);
        assert_eq!(out.slots_total, 2);
        assert_eq!(out.installed_bytes, 8192 * 1024 * 1024);
        assert_eq!(out.form_factor, "DIMM");
        // "Speed" (0x15h) and part number (0x1Ah) do not fit a 0x15 record.
        assert_eq!(out.speed_mts, 0);
        assert_eq!(out.part_number, "");
    }

    /// Full 2.6+ record: all fields must still be parsed (guards must not
    /// hide data from LONG records).
    #[test]
    fn full_smbios_type17_record_parses_every_field() {
        let mut rec = vec![0u8; 0x22];
        rec[0] = 17;
        rec[1] = 0x22;
        rec[0x0C..0x0E].copy_from_slice(&16384u16.to_le_bytes());
        rec[0x0E] = 0x0C; // SODIMM
        rec[0x15..0x17].copy_from_slice(&5600u16.to_le_bytes()); // Speed
        rec[0x20..0x22].copy_from_slice(&6400u16.to_le_bytes()); // Configured
        rec[0x17] = 1; // manufacturer string #1
        rec[0x1A] = 2; // part string #2
        rec.extend_from_slice(b"ACME\0MX-1\0\0");

        let out = ram_static_from_table(&rec);
        assert_eq!(out.slots_used, 1);
        assert_eq!(out.installed_bytes, 16384 * 1024 * 1024);
        assert_eq!(out.form_factor, "SODIMM");
        assert_eq!(out.speed_mts, 6400);
        assert_eq!(out.manufacturer, "ACME");
        assert_eq!(out.part_number, "MX-1");
    }
}
