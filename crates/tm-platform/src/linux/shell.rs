//! Linux shell integration through `xdg-open`, without command interpolation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use tm_core::error::{Result, TmError};

use super::launch;

pub(super) fn open_file_location(path: &str) -> Result<()> {
    let path = existing_absolute_path(path, "open file location")?;
    let target = containing_directory(&path);
    spawn_xdg_open(target.as_os_str())
}

/// `xdg-open` has no standard "show properties" request. Open the containing
/// directory instead of opening the file itself with an unrelated default
/// application; the desktop file manager can then show the item's details.
pub(super) fn open_properties(path: &str) -> Result<()> {
    let path = existing_absolute_path(path, "open properties")?;
    let target = containing_directory(&path);
    spawn_xdg_open(target.as_os_str())
}

pub(super) fn open_url(url: &str) -> Result<()> {
    validate_http_url(url)
        .map_err(|error| TmError::platform("open URL", error))?;
    spawn_xdg_open(OsStr::new(url))
}

fn validate_http_url(url: &str) -> std::result::Result<(), &'static str> {
    if url.contains('\0') {
        return Err("URL contains NUL");
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err("only http:// and https:// URLs are supported");
    }
    Ok(())
}

fn spawn_xdg_open(target: &OsStr) -> Result<()> {
    launch::spawn_detached_reaped(
        OsStr::new("xdg-open"),
        &[target.to_os_string()],
        &[],
    )
    .map_err(|error| TmError::platform("xdg-open", error.to_string()))
}

fn existing_absolute_path(path: &str, context: &'static str) -> Result<PathBuf> {
    if path.contains('\0') {
        return Err(TmError::platform(context, "path contains NUL"));
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(TmError::platform(context, "path is not absolute"));
    }
    std::fs::metadata(&path).map_err(|error| TmError::platform(context, error.to_string()))?;
    Ok(path)
}

fn containing_directory(path: &Path) -> &Path {
    if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_paths_must_be_absolute_and_cannot_contain_nul() {
        assert!(existing_absolute_path("relative/path", "test").is_err());
        assert!(existing_absolute_path("/tmp/bad\0name", "test").is_err());
    }

    #[test]
    fn file_locations_open_the_containing_directory() {
        let path = Path::new("/tmp/example.txt");
        assert_eq!(containing_directory(path), Path::new("/tmp"));
    }

    #[test]
    fn only_http_urls_are_forwarded_to_xdg_open() {
        assert!(validate_http_url("https://example.invalid/path").is_ok());
        assert!(validate_http_url("HTTP://example.invalid").is_ok());
        assert!(validate_http_url("file:///tmp/secret").is_err());
        assert!(validate_http_url("custom-scheme:payload").is_err());
        assert!(validate_http_url("https://bad\0url").is_err());
    }
}
