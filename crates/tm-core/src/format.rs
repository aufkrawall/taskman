//! Human-readable formatting helpers. Pure functions, heavily unit-tested.

/// Format bytes with binary-ish mixed units like Task Manager ("13.6 GB", "512 KB").
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const K: f64 = 1024.0;
    if b < K * K {
        fmt1(b / K, "KB")
    } else if b < K * K * K {
        fmt1(b / (K * K), "MB")
    } else if b < K * K * K * K {
        fmt1(b / (K * K * K), "GB")
    } else {
        fmt1(b / (K * K * K * K), "TB")
    }
}

/// Rate formatting: bytes/sec -> "1.2 MB/s"; zero prints "0 B/s".
pub fn format_rate(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 B/s".into();
    }
    format!("{}/s", format_bytes(bps as u64))
}

/// MB/s style rates for disk/network columns when values are large.
pub fn format_rate_short(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0".into();
    }
    let mb = bps / (1024.0 * 1024.0);
    let kb = bps / 1024.0;
    if mb >= 1.0 {
        format!("{mb:.1} MB/s")
    } else if kb >= 1.0 {
        format!("{kb:.0} KB/s")
    } else {
        format!("{bps:.0} B/s")
    }
}

fn fmt1(v: f64, unit: &str) -> String {
    if v >= 100.0 {
        format!("{v:.0} {unit}")
    } else {
        format!("{v:.1} {unit}")
    }
}

/// Uptime in the Windows TM style `d:hh:mm:ss` (days are always shown,
/// matching "0:05:43:22" in the reference).
pub fn format_uptime(total_secs: u64) -> String {
    let d = total_secs / 86_400;
    let h = (total_secs % 86_400) / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{d}:{h:02}:{m:02}:{s:02}")
}

// ---------------------------------------------------------------- German locale
// The reference Task Manager runs on a de-DE system: decimal comma,
// dot as group separator, comma numbers like "2.918,9 MB" or "4,24 GHz".

/// Format a float German-style: dot group separator, comma decimals.
fn de_fixed(v: f64, decimals: usize) -> String {
    let v = if v.is_finite() { v } else { 0.0 };
    let s = format!("{v:.decimals$}");
    let (int_part, dec_part) = match s.split_once('.') {
        Some((i, d)) => (i, Some(d)),
        None => (s.as_str(), None),
    };
    // Group the integer part in threes.
    let neg = int_part.starts_with('-');
    let digits = if neg { &int_part[1..] } else { int_part };
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(c);
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&grouped);
    if let Some(d) = dec_part {
        out.push(',');
        out.push_str(d);
    }
    out
}

/// Integer with dot group separators: 238044 -> "238.044".
pub fn format_thousands(n: u64) -> String {
    de_fixed(n as f64, 0)
}

/// Percent German-style: "2 %" above 10, one decimal below ("0,3 %"),
/// plain "0 %" for zero.
pub fn format_pct_de(pct: f32) -> String {
    if pct.abs() < 0.05 {
        return "0 %".into();
    }
    if pct.abs() >= 9.95 {
        format!("{} %", de_fixed(pct.round() as f64, 0))
    } else {
        format!("{} %", de_fixed(pct as f64, 1))
    }
}

/// Header-aggregate percent, always integer like TM ("37 %").
pub fn format_pct_de_int(pct: f32) -> String {
    format!("{} %", de_fixed(pct.round() as f64, 0))
}

/// Bytes as German-formatted MB (Task Manager shows process memory in MB):
/// "2.918,9 MB", "82,5 MB".
pub fn format_mb_de(bytes: u64) -> String {
    if bytes == 0 {
        return "0 MB".into();
    }
    let mb = bytes as f64 / (1024.0 * 1024.0);
    format!("{} MB", de_fixed(mb, 1))
}

/// Bytes with adaptive units, German format: "11,9 GB", "512 KB".
pub fn format_bytes_de(bytes: u64) -> String {
    let b = bytes as f64;
    const K: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if b < K * K {
        format!("{} KB", de_fixed(b / K, 1))
    } else if b < K * K * K {
        format!("{} MB", de_fixed(b / (K * K), 1))
    } else if b < K * K * K * K {
        format!("{} GB", de_fixed(b / (K * K * K), 1))
    } else {
        format!("{} TB", de_fixed(b / (K * K * K * K), 1))
    }
}

/// Disk rate German-style, always MB/s with one decimal like TM: "0,1 MB/s", "0 MB/s".
pub fn format_rate_de(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 MB/s".into();
    }
    let mb = bps / (1024.0 * 1024.0);
    format!("{} MB/s", de_fixed(mb, 1))
}

/// Network rate German-style as MBit/s: "0 MBit/s", "0,1 MBit/s".
pub fn format_mbit_de(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 MBit/s".into();
    }
    let mbit = bps * 8.0 / (1000.0 * 1000.0);
    if mbit >= 100.0 {
        format!("{} MBit/s", de_fixed(mbit, 0))
    } else {
        format!("{} MBit/s", de_fixed(mbit, 1))
    }
}

/// Network rate in KBit for the Performance sidebar: "48,0 KBit".
pub fn format_kbit_de(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 KBit".into();
    }
    let kbit = bps * 8.0 / 1024.0;
    if kbit < 0.05 {
        return "0 KBit".into();
    }
    if kbit >= 1024.0 {
        format!("{} MBit", de_fixed(kbit / 1024.0, 1))
    } else {
        format!("{} KBit", de_fixed(kbit, 1))
    }
}

/// Frequency German-style: "4,24 GHz", "3,40 GHz".
pub fn format_freq_de(mhz: f32) -> String {
    if mhz <= 0.0 {
        return "—".into();
    }
    format!("{} GHz", de_fixed(mhz as f64 / 1000.0, 2))
}

/// Details-tab memory in KiB with group separator: "238.044 K".
pub fn format_k_de(bytes: u64) -> String {
    format!("{} K", format_thousands(bytes / 1024))
}

/// Details-tab CPU: zero-padded two-digit integer like TM ("00", "07").
pub fn format_cpu_detail(pct: f32) -> String {
    let v = pct.round() as i64;
    if v < 100 {
        format!("{v:02}")
    } else {
        format!("{v}")
    }
}

/// Epoch seconds as "25.07.2026" (local date, no chrono dependency — UTC-based).
pub fn format_date_de(epoch_s: i64) -> String {
    let days = epoch_s.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{d:02}.{mo:02}.{}", yoe + era * 400)
}

/// Seconds with one German decimal: 17.0 -> "17,0".
pub fn format_seconds_de(secs: f64) -> String {
    de_fixed(secs, 1)
}

/// Short duration for process CPU time columns ("123:45:06" hours:min:sec).
pub fn format_cpu_time(seconds: f64) -> String {
    if !(seconds.is_finite()) || seconds < 0.0 {
        return "0:00:00".into();
    }
    let total = seconds.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h}:{m:02}:{s:02}")
}

/// MHz frequency like TM: 4567 MHz shown GHz-aware.
pub fn format_freq_mhz(mhz: f32) -> String {
    if mhz <= 0.0 {
        return "".into();
    }
    format!("{:.2} GHz", mhz / 1000.0)
}

/// Choose a "nice" y-axis max and step for charts whose data peaks at `peak`.
/// Returns (nice_max, step). Guarantees nice_max >= peak, step divides range
/// into 4..=8 steps, and both are round numbers (1/2/2.5/5 × 10^n).
pub fn nice_scale(peak: f64) -> (f64, f64) {
    if !(peak.is_finite()) || peak <= 0.0 {
        return (100.0, 25.0);
    }
    // Grow slightly so the line never touches the top border.
    let target = peak.max(1e-6) * 1.05;
    let mag = 10f64.powf(target.log10().floor());
    let candidates = [1.0, 2.0, 2.5, 5.0, 10.0];
    let mut nice_max = 10.0 * mag;
    let mut step = 2.5 * mag;
    for (i, c) in candidates.iter().enumerate() {
        let cand = c * mag;
        if cand >= target {
            nice_max = cand;
            step = match i {
                0 => 0.25 * mag,
                1 => 0.5 * mag,
                2 => 0.5 * mag,
                3 => 1.0 * mag,
                _ => 2.0 * mag,
            };
            break;
        }
    }
    // Keep at least 4 gridlines but not more than ~8.
    while nice_max / step > 8.0 {
        step *= 2.0;
    }
    while nice_max / step < 3.0 && step > 1e-9 {
        step /= 2.0;
    }
    (nice_max, step)
}

/// Percent label with no decimals above 10%, one decimal below (TM behavior).
pub fn format_pct(pct: f32) -> String {
    if pct.abs() >= 9.95 {
        format!("{pct:.0}%")
    } else {
        format!("{pct:.1}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(13 * 1024 * 1024 * 1024 + 600), "13.0 GB");
        assert_eq!(format_bytes(5 * 1024u64.pow(4)), "5.0 TB");
    }

    #[test]
    fn bytes_over_hundred_drop_decimal() {
        assert_eq!(format_bytes(300 * 1024), "300 KB");
    }

    #[test]
    fn rates() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(-5.0), "0 B/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s");
        assert_eq!(format_rate(2048.0), "2.0 KB/s");
        assert_eq!(format_rate_short(0.0), "0");
        assert_eq!(format_rate_short(1500.0), "1 KB/s");
        assert_eq!(format_rate_short(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
    }

    #[test]
    fn uptime_formats() {
        assert_eq!(format_uptime(0), "0:00:00:00");
        assert_eq!(format_uptime(3661), "0:01:01:01");
        assert_eq!(
            format_uptime((2 * 86400) + (4 * 3600) + (24 * 60) + 24),
            "2:04:24:24"
        );
    }

    #[test]
    fn german_formats() {
        assert_eq!(format_thousands(238_044), "238.044");
        assert_eq!(format_thousands(1234), "1.234");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_mb_de(3_060_688_486), "2.918,9 MB");
        assert_eq!(format_mb_de(8_808_038), "8,4 MB");
        assert_eq!(format_pct_de(0.3), "0,3 %");
        assert_eq!(format_pct_de(2.0), "2,0 %");
        assert_eq!(format_pct_de(12.4), "12 %");
        assert_eq!(format_pct_de_int(37.4), "37 %");
        assert_eq!(format_rate_de(0.0), "0 MB/s");
        assert_eq!(format_rate_de(150_000.0), "0,1 MB/s");
        assert_eq!(format_mbit_de(0.0), "0 MBit/s");
        assert_eq!(format_freq_de(4240.0), "4,24 GHz");
        assert_eq!(format_freq_de(3400.0), "3,40 GHz");
        assert_eq!(format_k_de(243_765_248), "238.052 K");
        assert_eq!(format_cpu_detail(0.2), "00");
        assert_eq!(format_cpu_detail(7.4), "07");
        assert_eq!(format_cpu_detail(101.0), "101");
        assert_eq!(format_seconds_de(17.04), "17,0");
        assert_eq!(format_bytes_de(34_253_000_000), "31,9 GB");
    }

    #[test]
    fn cpu_time_formats() {
        assert_eq!(format_cpu_time(0.0), "0:00:00");
        assert_eq!(format_cpu_time(59.4), "0:00:59");
        assert_eq!(format_cpu_time(61.0), "0:01:01");
        assert_eq!(format_cpu_time(3600.0 * 123.0 + 2706.0), "123:45:06");
        assert_eq!(format_cpu_time(-1.0), "0:00:00");
    }

    #[test]
    fn freq() {
        assert_eq!(format_freq_mhz(4560.0), "4.56 GHz");
        assert_eq!(format_freq_mhz(3400.0), "3.40 GHz");
        assert_eq!(format_freq_mhz(0.0), "");
    }

    #[test]
    fn nice_scale_basics() {
        let (max, step) = nice_scale(34.0);
        assert!(max >= 34.0);
        assert!((max / step).fract().abs() < 1e-9);
        assert!((3.0..=8.0).contains(&(max / step)));
        // Round numbers only.
        let scaled = max / step;
        assert!((scaled - scaled.round()).abs() < 1e-9);

        let (m2, s2) = nice_scale(0.0);
        assert_eq!((m2, s2), (100.0, 25.0));

        let (m3, _) = nice_scale(96.0);
        assert!(m3 >= 96.0);
        let (m4, s4) = nice_scale(13.6 * 1024.0 * 1024.0 * 1024.0);
        assert!(m4 >= 13.6 * 1024.0 * 1024.0 * 1024.0);
        assert!(s4 > 0.0);
    }

    #[test]
    fn percent_formatting() {
        assert_eq!(format_pct(34.2), "34%");
        assert_eq!(format_pct(9.94), "9.9%");
        assert_eq!(format_pct(0.04), "0.0%");
        assert_eq!(format_pct(100.0), "100%");
    }
}
