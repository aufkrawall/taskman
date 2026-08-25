//! OS locale awareness: number separators and date layout follow the user's
//! regional settings (decimal comma vs point, group separator, date order).
//!
//! Detection runs once at startup:
//! * Windows — `GetUserDefaultLocaleName` + `GetLocaleInfoEx` for
//!   `LOCALE_SDECIMAL` / `LOCALE_SGROUPING` / `LOCALE_SSHORTDATE`.
//! * Unix — `LC_ALL` / `LC_NUMERIC` / `LANG` env vars with a small table of
//!   known locales; unknown locales default to the en-US style.
//!
//! The detected values live in a global set once by the app entrypoint; the
//! formatting helpers in [`crate::format`] read from here.

use std::sync::OnceLock;

/// Order of day/month/year implied by the locale's short date pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOrder {
    Dmy,
    Mdy,
    Ymd,
}

/// Locale-derived formatting rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocaleFmt {
    /// Decimal separator ("de-DE" → ',').
    pub decimal: char,
    /// Digit group separator ("de-DE" → '.', "en-US" → ','), None = no grouping.
    pub grouping: Option<char>,
    pub date_order: DateOrder,
    /// BCP-47-ish language tag the data came from ("de-DE", "en-US", …).
    pub lang_tag: &'static str,
}

impl Default for LocaleFmt {
    fn default() -> Self {
        EN_US
    }
}

pub const EN_US: LocaleFmt = LocaleFmt {
    decimal: '.',
    grouping: Some(','),
    date_order: DateOrder::Mdy,
    lang_tag: "en-US",
};

pub const DE_DE: LocaleFmt = LocaleFmt {
    decimal: ',',
    grouping: Some('.'),
    date_order: DateOrder::Dmy,
    lang_tag: "de-DE",
};

static LOCALE: OnceLock<LocaleFmt> = OnceLock::new();

/// Install the process-wide locale (called once at startup).
pub fn init(fmt: LocaleFmt) {
    let _ = LOCALE.set(fmt);
}

/// The active locale; falls back to en-US style before `init`.
pub fn get() -> LocaleFmt {
    *LOCALE.get_or_init(Default::default)
}

/// True when the active locale is German-like (used as the default UI language).
pub fn is_german() -> bool {
    get().lang_tag.starts_with("de")
}

// ---------------------------------------------------------------- detection

/// OS-level detection is provided by tm-platform on Windows
/// (`tm_platform::detect_locale`); here we only offer the env fallback.
/// Env-var based fallback (`LC_ALL` > `LC_NUMERIC` > `LANG`).
pub fn detect_env() -> Option<LocaleFmt> {
    let raw = ["LC_ALL", "LC_NUMERIC", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty() && v != "C"))?;
    Some(from_lang_tag(&raw))
}

/// Map a language tag / POSIX locale string to formatting rules using a table
/// of comma-decimal regions; everything else gets the international default.
pub fn from_lang_tag(tag: &str) -> LocaleFmt {
    let base = tag
        .split(['.', '@', '+'])
        .next()
        .unwrap_or(tag)
        .replace('_', "-");
    let lower = base.to_ascii_lowercase();
    // Comma-decimal languages (primary subtag is enough).
    const COMMA_DECIMAL: [&str; 22] = [
        "de", "es", "fr", "it", "pt", "nl", "ru", "pl", "tr", "cs", "sk", "hu", "ro", "el", "da",
        "sv", "no", "nb", "nn", "fi", "hr", "sr",
    ];
    if COMMA_DECIMAL.iter().any(|l| lower.starts_with(l)) {
        return LocaleFmt {
            decimal: ',',
            grouping: Some('.'),
            date_order: DateOrder::Dmy,
            lang_tag: leak(base),
        };
    }
    // English variants differ in dates: GB uses DMY, US MDY.
    if lower.starts_with("en") {
        let order = if lower.contains("-gb")
            || lower.contains("-ie")
            || lower.contains("-au")
            || lower.contains("-nz")
        {
            DateOrder::Dmy
        } else {
            DateOrder::Mdy
        };
        return LocaleFmt {
            decimal: '.',
            grouping: Some(','),
            date_order: order,
            lang_tag: leak(base),
        };
    }
    LocaleFmt {
        decimal: '.',
        grouping: Some(','),
        date_order: DateOrder::Ymd,
        lang_tag: leak(base),
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

// ---------------------------------------------------------------- windows
// Windows detection lives in tm-platform (`win::detect_locale`) — all OS
// specifics stay out of tm-core.
