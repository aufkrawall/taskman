//! PE version metadata (FileDescription / CompanyName) used for the
//! Task-Manager-style friendly process names and the startup publisher
//! column. Cached per path — version info never changes at runtime.

use std::collections::HashMap;
use parking_lot::Mutex as PlMutex;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};

static CACHE: PlMutex<Option<HashMap<String, [String; 2]>>> = PlMutex::new(None);

/// (file description, company name) for an executable; empty strings when
/// unavailable. Results are cached process-wide.
pub fn query(path: &str) -> [String; 2] {
    {
        let guard = CACHE.lock();
        if let Some(map) = guard.as_ref()
            && let Some(v) = map.get(path)
        {
            return v.clone();
        }
    }
    let result = query_uncached(path);
    let mut guard = CACHE.lock();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(path.to_string(), result.clone());
    result
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
        for (idx, key) in ["FileDescription", "CompanyName"].iter().enumerate() {
            let mut found: Option<String> = None;
            for codepage in ["040904b0", "040904e4"] {
                let query: Vec<u16> = format!("\\StringFileInfo\\{codepage}\\{key}")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let mut ptr = std::ptr::null_mut();
                let mut len = 0u32;
                if VerQueryValueW(
                    data.as_ptr().cast(),
                    PCWSTR(query.as_ptr()),
                    &mut ptr,
                    &mut len,
                )
                .as_bool()
                    && !ptr.is_null()
                    && len > 0
                {
                    let words = std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize);
                    let end = words.iter().position(|&w| w == 0).unwrap_or(words.len());
                    found = Some(String::from_utf16_lossy(&words[..end]));
                    break;
                }
            }
            if let Some(v) = found {
                out[idx] = v;
            }
        }
    }
    out
}
