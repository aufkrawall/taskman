//! Human-readable formatting helpers. Pure where possible, heavily
//! unit-tested. Number/date presentation follows the OS locale detected at
//! startup ([`crate::locale`]) — e.g. decimal comma on German systems,
//! decimal point elsewhere. Unit words follow the UI language
//! ([`crate::i18n`]).

use crate::locale::{self, DateOrder, LocaleFmt};
use crate::i18n::{self, Lang};

/// Format a float with the active locale's separators and grouping.
pub fn num_fixed(v: f64, decimals: usize) -> String {
    fixed_with(v, decimals, locale::get())
}

/// Locale-parameterized core of [`num_fixed`] (also unit-testable).
pub fn fixed_with(v: f64, decimals: usize, loc: LocaleFmt) -> String {
    let v = if v.is_finite() { v } else { 0.0 };
    let s = format!("{v:.decimals$}");
    let (int_part, dec_part) = match s.split_once('.') {
        Some((i, d)) => (i, Some(d)),
        None => (s.as_str(), None),
    };
    let neg = int_part.starts_with('-');
    let digits = if neg { &int_part[1..] } else { int_part };
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 4);
    if neg {
        out.push('-');
    }
    if let Some(g) = loc.grouping {
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(g);
            }
            out.push(c);
        }
    } else {
        out.push_str(digits);
    }
    if let Some(d) = dec_part {
        out.push(loc.decimal);
        out.push_str(d);
    }
    out
}

/// Integer with group separators: 238044 → "238.044" (de) / "238,044" (en).
pub fn format_thousands(n: u64) -> String {
    num_fixed(n as f64, 0)
}

// ---------------------------------------------------------------- bytes

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
        format!("{} MB/s", num_fixed(mb, 1))
    } else if kb >= 1.0 {
        format!("{} KB/s", num_fixed(kb.round(), 0))
    } else {
        format!("{bps:.0} B/s")
    }
}

fn fmt1(v: f64, unit: &str) -> String {
    if v >= 100.0 {
        format!("{} {unit}", num_fixed(v.round(), 0))
    } else {
        format!("{} {unit}", num_fixed(v, 1))
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

// ---------------------------------------------------------------- localized cell formats

/// Percent for table cells: integer above ~10 %, one decimal below
/// ("0,3 %" / "0.3 %"), plain "0 %" for zero.
pub fn format_pct_cell(pct: f32) -> String {
    if pct.abs() < 0.05 {
        return "0 %".into();
    }
    if pct.abs() >= 9.95 {
        format!("{} %", num_fixed((pct.round()) as f64, 0))
    } else {
        format!("{} %", num_fixed(pct as f64, 1))
    }
}

/// Header-aggregate percent, always integer like TM ("37 %").
pub fn format_pct_hdr(pct: f32) -> String {
    format!("{} %", num_fixed(pct.round() as f64, 0))
}

/// Bytes as locale-formatted MB (process memory column): "2.918,9 MB".
pub fn format_mb(bytes: u64) -> String {
    if bytes == 0 {
        return "0 MB".into();
    }
    format!("{} MB", num_fixed(bytes as f64 / (1024.0 * 1024.0), 1))
}

/// Bytes with adaptive units, locale-formatted: "11,9 GB" / "11.9 GB".
pub fn format_bytes_loc(bytes: u64) -> String {
    let b = bytes as f64;
    const K: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if b < K * K {
        format!("{} KB", num_fixed(b / K, 1))
    } else if b < K * K * K {
        format!("{} MB", num_fixed(b / (K * K), 1))
    } else if b < K * K * K * K {
        format!("{} GB", num_fixed(b / (K * K * K), 1))
    } else {
        format!("{} TB", num_fixed(b / (K * K * K * K), 1))
    }
}

/// Disk rate, always MB/s with one decimal like TM: "0,1 MB/s".
pub fn format_rate_mb(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "0 MB/s".into();
    }
    format!("{} MB/s", num_fixed(bps / (1024.0 * 1024.0), 1))
}

/// Network rate per UI language: "0 MBit/s" (de) / "0 Mbps" (en).
pub fn format_mbit(bps: f64) -> String {
    let unit = i18n::unit_mbit_per_s();
    if !bps.is_finite() || bps <= 0.0 {
        return format!("0 {unit}");
    }
    let mbit = bps * 8.0 / (1000.0 * 1000.0);
    if mbit >= 100.0 {
        format!("{} {unit}", num_fixed(mbit, 0))
    } else {
        format!("{} {unit}", num_fixed(mbit, 1))
    }
}

/// Network volume for the Performance sidebar: "48,0 KBit" / "48.0 kbps".
pub fn format_kbit(bps: f64) -> String {
    let unit = i18n::unit_kbit();
    if !bps.is_finite() || bps <= 0.0 {
        return format!("0 {unit}");
    }
    let kbit = match i18n::lang() {
        Lang::De => bps * 8.0 / 1024.0,
        Lang::En => bps * 8.0 / 1000.0,
    };
    if kbit < 0.05 {
        return format!("0 {unit}");
    }
    let div = if matches!(i18n::lang(), Lang::De) { 1024.0 } else { 1000.0 };
    if kbit >= div {
        format!("{} {}", num_fixed(kbit / div, 1), if matches!(i18n::lang(), Lang::De) { "MBit" } else { "Mbps" })
    } else {
        format!("{} {unit}", num_fixed(kbit, 1))
    }
}

/// Frequency: "4,24 GHz" / "4.24 GHz"; em dash when unknown.
pub fn format_freq_ghz(mhz: f32) -> String {
    if mhz <= 0.0 {
        return "—".into();
    }
    format!("{} GHz", num_fixed(mhz as f64 / 1000.0, 2))
}

/// Details-tab memory in KiB with group separator: "238.044 K".
pub fn format_k(bytes: u64) -> String {
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

/// Epoch seconds as a local date in the locale's layout
/// ("25.07.2026", "7/25/2026", "2026-07-25"). No chrono dependency —
/// UTC-based civil-date math.
pub fn format_date(epoch_s: i64) -> String {
    format_date_in(epoch_s, locale::get())
}

pub fn format_date_in(epoch_s: i64, loc: LocaleFmt) -> String {
    let days = epoch_s.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400;
    let sep = match loc.date_order {
        DateOrder::Dmy if loc.lang_tag.starts_with("de") => '.',
        DateOrder::Ymd => '-',
        _ => '/',
    };
    match loc.date_order {
        DateOrder::Dmy => format!("{d:02}{sep}{mo:02}{sep}{y}"),
        DateOrder::Mdy => format!("{mo}/{d}/{y}"),
        DateOrder::Ymd => format!("{y}{sep}{mo:02}{sep}{d:02}"),
    }
}

/// Seconds with one decimal in the active locale: 17.0 → "17,0" / "17.0".
pub fn format_seconds(secs: f64) -> String {
    num_fixed(secs, 1)
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
    format!("{} GHz", num_fixed(mhz as f64 / 1000.0, 2))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        // Tests run before locale::init, so the en-US style default applies.
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(300 * 1024), "300 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(13 * 1024 * 1024 * 1024 + 600), "13.0 GB");
        assert_eq!(format_bytes(5 * 1024u64.pow(4)), "5.0 TB");
    }

    #[test]
    fn rates() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(-5.0), "0 B/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s");
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
    fn german_number_layout() {
        let de = locale::DE_DE;
        assert_eq!(fixed_with(238_044.0, 0, de), "238.044");
        assert_eq!(fixed_with(1234.0, 0, de), "1.234");
        assert_eq!(fixed_with(999.0, 0, de), "999");
        assert_eq!(fixed_with(2918.9375, 1, de), "2.918,9");
        // format_mb follows the process locale (en-US default in tests).
        assert_eq!(format_mb(3_060_688_486), "2,918.9 MB");
    }

    #[test]
    fn english_number_layout() {
        let en = locale::EN_US;
        assert_eq!(fixed_with(238_044.0, 0, en), "238,044");
        assert_eq!(fixed_with(2918.9375, 1, en), "2,918.9");
        assert_eq!(fixed_with(0.3123, 1, en), "0.3");
        assert_eq!(fixed_with(17.04, 1, en), "17.0");
    }

    #[test]
    fn date_orders() {
        let epoch = 1_784_966_400; // 2026-07-25
        assert_eq!(format_date_in(epoch, locale::DE_DE), "25.07.2026");
        let us = locale::EN_US;
        assert_eq!(format_date_in(epoch, us), "7/25/2026");
        let iso = locale::LocaleFmt { date_order: locale::DateOrder::Ymd, ..us };
        assert_eq!(format_date_in(epoch, iso), "2026-07-25");
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
        assert_eq!(format_freq_mhz(4560.0), format!("{} GHz", num_fixed(4.56, 2)));
        assert_eq!(format_freq_mhz(3400.0), format!("{} GHz", num_fixed(3.40, 2)));
        assert_eq!(format_freq_mhz(0.0), "");
    }

    #[test]
    fn nice_scale_basics() {
        let (max, step) = nice_scale(34.0);
        assert!(max >= 34.0);
        assert!((max / step).fract().abs() < 1e-9);
        assert!((3.0..=8.0).contains(&(max / step)));

        let (m2, s2) = nice_scale(0.0);
        assert_eq!((m2, s2), (100.0, 25.0));

        let (m3, _) = nice_scale(96.0);
        assert!(m3 >= 96.0);
        let (m4, s4) = nice_scale(13.6 * 1024.0 * 1024.0 * 1024.0);
        assert!(m4 >= 13.6 * 1024.0 * 1024.0 * 1024.0);
        assert!(s4 > 0.0);
    }
}
