//! Crash-safe replacement of the small files this app persists (settings,
//! app history, startup impact).
//!
//! Write a same-directory temp file, force it to stable storage, then rename
//! it over the destination. Flushing a `File` only empties Rust's side of the
//! pipe (on Windows it is a no-op); without `sync_all` a power loss after the
//! rename reached the disk but before the data did leaves an empty or
//! zero-filled file, which the loaders treat as missing — and the next save
//! then overwrites the user's real settings or history with defaults.

use std::io::Write;
use std::path::Path;

/// Replace `path` with whatever `write` produces, via `<path>.<tmp_ext>`.
pub(crate) fn replace_file(
    path: &Path,
    tmp_ext: &str,
    write: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(tmp_ext);
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    write(&mut out)?;
    out.flush()?;
    out.get_ref().sync_all()?;
    drop(out);
    std::fs::rename(&tmp, path)
}

/// Move an existing file that could not be used out of the way (to
/// `<name>.bad`) before the caller starts over with defaults.
///
/// Starting fresh means the next save replaces the file. A file that is
/// oversized, corrupt, not UTF-8 or merely unreadable right now still holds
/// the user's settings or history, so it is kept for recovery instead of
/// being overwritten. Best effort: failing to move it is logged, never fatal.
pub(crate) fn set_aside(path: &Path, what: &str) {
    let mut aside = path.as_os_str().to_owned();
    aside.push(".bad");
    match std::fs::rename(path, &aside) {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            kept = %Path::new(&aside).display(),
            "{what} could not be used; kept it aside and starting fresh"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "{what} could not be used and could not be kept aside; the next save replaces it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_destination_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("tm-persist-{}", std::process::id()));
        let path = dir.join("store.json");
        replace_file(&path, "json.tmp", |out| out.write_all(b"first")).unwrap();
        replace_file(&path, "json.tmp", |out| out.write_all(b"second")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!path.with_extension("json.tmp").exists());

        set_aside(&path, "test store");
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("store.json.bad")).unwrap(),
            "second"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
