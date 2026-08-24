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

/// Uptime in the Windows TM style `d:hh:mm:ss` or `hh:mm:ss` under a day.
pub fn format_uptime(total_secs: u64) -> String {
    let d = total_secs / 86_400;
    let h = (total_secs % 86_400) / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if d > 0 {
        format!("{d}:{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
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
        format!("{:.0}%", pct)
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
        assert_eq!(format_uptime(0), "00:00:00");
        assert_eq!(format_uptime(3661), "01:01:01");
        assert_eq!(
            format_uptime((2 * 86400) + (4 * 3600) + (24 * 60) + 24),
            "2:04:24:24"
        );
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
