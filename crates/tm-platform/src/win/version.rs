//! PE version metadata (FileDescription / CompanyName) used for the
//! Task-Manager-style friendly process names and the startup publisher
//! column. Cached per path — version info never changes at runtime.
//!
//! WHICH string table is read is decided by the file's own
//! `\VarFileInfo\Translation` table. Hardcoding the English blocks
//! (040904b0/040904e4) returned empty metadata for every binary that ships
//! only a localized table — on a German Windows that is csrss, smss, wininit,
//! winlogon, services, dwm, audiodg and friends. Those are exactly the Windows
//! core images whose version metadata is the evidence behind the sampler's
//! system-role fallbacks, so the missing CompanyName left the User name and
//! Elevated columns of every protected process blank.

use std::collections::HashMap;
use std::sync::Mutex;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::core::PCWSTR;

static CACHE: Mutex<Option<HashMap<String, [String; 2]>>> = Mutex::new(None);

/// (file description, company name) for an executable; empty strings when
/// unavailable. Results are cached process-wide.
pub fn query(path: &str) -> [String; 2] {
    {
        let guard = tm_core::sync::lock(&CACHE);
        if let Some(map) = guard.as_ref()
            && let Some(v) = map.get(path)
        {
            return v.clone();
        }
    }
    let result = query_uncached(path);
    let mut guard = tm_core::sync::lock(&CACHE);
    guard
        .get_or_insert_with(HashMap::new)
        .insert(path.to_string(), result.clone());
    result
}

/// Candidate `StringFileInfo` blocks for one version resource, best first.
///
/// The file's own translation table leads, with the user's UI language
/// preferred inside it (that is what Explorer and Task Manager show) and the
/// table's order deciding the rest. The English and language-neutral blocks
/// stay as fallbacks for files with no usable translation table.
fn candidate_codepages(translations: &[(u16, u16)], ui_language: u16) -> Vec<String> {
    let mut ordered = translations.to_vec();
    ordered.sort_by_key(|(lang, _)| *lang != ui_language);
    let mut out: Vec<String> = ordered
        .iter()
        .map(|(lang, codepage)| format!("{lang:04x}{codepage:04x}"))
        .collect();
    for legacy in ["040904b0", "040904e4", "000004b0"] {
        if !out.iter().any(|block| block == legacy) {
            out.push(legacy.to_string());
        }
    }
    out
}

fn query_uncached(path: &str) -> [String; 2] {
    let mut out = [String::new(), String::new()];
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), Some(&mut handle));
        if size == 0 {
            return out;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            Some(0),
            size,
            data.as_mut_ptr().cast(),
        )
        .is_err()
        {
            return out;
        }
        let codepages = candidate_codepages(&translation_table(&data), GetUserDefaultUILanguage());
        for (idx, key) in ["FileDescription", "CompanyName"].iter().enumerate() {
            out[idx] = query_string(&data, key, &codepages).unwrap_or_default();
        }
    }
    out
}

/// `\VarFileInfo\Translation`: the (language id, codepage) pairs this file's
/// string tables are written in. Empty when the file has none.
///
/// The value is an array of 32-bit pairs and `VerQueryValueW` reports its
/// length in BYTES (unlike string queries, which count characters).
unsafe fn translation_table(data: &[u8]) -> Vec<(u16, u16)> {
    let query: Vec<u16> = "\\VarFileInfo\\Translation"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(query.as_ptr()),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || ptr.is_null()
        {
            return Vec::new();
        }
        let words = std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize / 2);
        words
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0], pair[1]))
            .collect()
    }
}

/// First block in `codepages` that carries `key`, or `None`.
unsafe fn query_string(data: &[u8], key: &str, codepages: &[String]) -> Option<String> {
    for codepage in codepages {
        let query: Vec<u16> = format!("\\StringFileInfo\\{codepage}\\{key}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            if !VerQueryValueW(
                data.as_ptr().cast(),
                PCWSTR(query.as_ptr()),
                &mut ptr,
                &mut len,
            )
            .as_bool()
                || ptr.is_null()
                || len == 0
            {
                continue;
            }
            let words = std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize);
            let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
            let text = String::from_utf16_lossy(&words[..end]);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this module was fixed for: a file that carries ONLY a
    /// localized translation (csrss.exe and friends on a German Windows) must
    /// still lead with its own block instead of being asked for English only.
    #[test]
    fn a_localized_only_translation_leads_the_candidates() {
        let candidates = candidate_codepages(&[(0x0407, 0x04b0)], 0x0407);
        assert_eq!(candidates[0], "040704b0");
        // ...and the legacy blocks still answer for files without a table.
        assert!(candidates.contains(&"040904b0".to_string()));
        assert!(candidates.contains(&"000004b0".to_string()));
    }

    /// Explorer and Task Manager show the user's UI language when the file
    /// has it; the translation table's order is only the tie-breaker.
    #[test]
    fn the_ui_language_beats_the_table_order() {
        let table = [(0x0409, 0x04b0), (0x0407, 0x04b0)];
        assert_eq!(
            candidate_codepages(&table, 0x0407)[..2],
            ["040704b0".to_string(), "040904b0".to_string()]
        );
        assert_eq!(
            candidate_codepages(&table, 0x0409)[..2],
            ["040904b0".to_string(), "040704b0".to_string()]
        );
    }

    #[test]
    fn legacy_blocks_are_appended_once() {
        let candidates = candidate_codepages(&[(0x0409, 0x04b0)], 0x0409);
        assert_eq!(
            candidates
                .iter()
                .filter(|block| *block == "040904b0")
                .count(),
            1
        );
        assert_eq!(
            candidates,
            [
                "040904b0".to_string(),
                "040904e4".to_string(),
                "000004b0".to_string(),
            ]
        );
    }

    /// Live pin: Windows core binaries must resolve their metadata no matter
    /// which translation they ship. `csrss.exe` is present on every Windows,
    /// and on localized systems its table is not the English one.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_windows_core_binary_resolves_its_version_metadata() {
        let system_root = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let path = system_root.join("System32").join("csrss.exe");
        let [description, company] = query(&path.to_string_lossy());
        assert_eq!(company, "Microsoft Corporation", "csrss.exe company");
        assert!(!description.is_empty(), "csrss.exe FileDescription");
    }
}
