//! OS locale detection: number separators + short-date layout from the
//! user's regional settings (NLS APIs). Used by tm-core's formatters.

use tm_core::locale::{DateOrder, LocaleFmt};

/// Query the default user locale via `GetUserDefaultLocaleName` and read the
/// decimal separator, grouping flag and short-date pattern with
/// `GetLocaleInfoEx`. Returns None when unavailable (falls back to env vars).
pub fn detect_locale() -> Option<LocaleFmt> {
    use windows::core::PCWSTR;
    use windows::Win32::Globalization::{
        GetLocaleInfoEx, GetUserDefaultLocaleName, LOCALE_SDECIMAL, LOCALE_SGROUPING,
        LOCALE_SSHORTDATE,
    };

    let mut buf = [0u16; 85]; // LOCALE_NAME_MAX_LENGTH
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    if len <= 1 {
        return None;
    }
    let name = &buf[..len as usize - 1]; // drop trailing NUL
    let tag = String::from_utf16_lossy(name);
    let leaked: &'static str = Box::leak(tag.clone().into_boxed_str());

    let info = |lcid: u32| -> Option<String> {
        let mut out = [0u16; 80];
        let n = unsafe {
            GetLocaleInfoEx(
                PCWSTR::from_raw(name.as_ptr()),
                lcid,
                Some(&mut out),
            )
        };
        (n > 0).then(|| String::from_utf16_lossy(&out[..n as usize - 1]))
    };

    let decimal = info(LOCALE_SDECIMAL)
        .and_then(|s| s.chars().next())
        .unwrap_or('.');
    // SGROUPING "3;0" = group in threes until the end; "0" = no grouping.
    let grouping_raw = info(LOCALE_SGROUPING).unwrap_or_default();
    let grouping = if grouping_raw.starts_with('0') {
        None
    } else {
        Some(if decimal == ',' { '.' } else { ',' })
    };
    // Short-date pattern like "dd.MM.yyyy" / "M/d/yyyy" / "yyyy-MM-dd".
    let date_order = match info(LOCALE_SSHORTDATE).as_deref().map(str::trim) {
        Some(p) if p.starts_with('y') || p.starts_with("yyyy") => DateOrder::Ymd,
        Some(p) if p.starts_with('M') => DateOrder::Mdy,
        Some(p) if p.starts_with('d') => DateOrder::Dmy,
        _ => DateOrder::Dmy,
    };

    Some(LocaleFmt {
        decimal,
        grouping,
        date_order,
        lang_tag: leaked,
    })
}
