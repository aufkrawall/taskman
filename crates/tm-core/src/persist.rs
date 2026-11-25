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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);

fn create_unique_temp(path: &Path, tmp_ext: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("file"));
    let pid = std::process::id();
    let mut attempt = 0u32;
    loop {
        let seq = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{file_name}.{pid}-{seq}.{tmp_ext}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 1024 => {
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// A temp file older than this is a leftover from a crash: live writers keep
/// their temp for milliseconds. Sweeping them prevents stale copies of user
/// data from accumulating in the config/data directory.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Remove temp files for `path` that no live save can own any more. Only
/// names matching this destination's `.<pid>-<seq>.<tmp_ext>` shape are
/// touched, and only when they are old enough that a concurrent writer (in
/// this process or another instance) cannot still be filling one.
fn sweep_stale_temps(path: &Path, tmp_ext: &str) {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("file"));
    let prefix = format!("{file_name}.");
    let suffix = format!(".{tmp_ext}");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(&suffix) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified.elapsed().is_ok_and(|age| age > STALE_TEMP_AGE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Replace `path` with whatever `write` produces via an exclusive temp file.
pub(crate) fn replace_file(
    path: &Path,
    tmp_ext: &str,
    write: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    sweep_stale_temps(path, tmp_ext);
    let (tmp, file) = create_unique_temp(path, tmp_ext)?;
    let write_result = (|| {
        let mut out = std::io::BufWriter::new(file);
        write(&mut out)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        drop(out);
        std::fs::rename(&tmp, path)
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

/// A character that carries nothing a loader could use: whitespace, a NUL left
/// by a write that never reached the disk, or a byte-order mark.
fn is_contentless(c: char) -> bool {
    c.is_whitespace() || c == '\0' || c == '\u{feff}'
}

/// Whether the text of a file says nothing: every character of it is
/// contentless, so the file holds no settings, history or entries to recover
/// even though it has bytes. Useful in the `Err` arm of a parser, the first
/// point where the caller knows both that the file failed and that nothing in
/// it is worth keeping.
pub(crate) fn says_nothing(text: &str) -> bool {
    text.chars().all(is_contentless)
}

/// Remove a file the caller has read and found empty of anything usable: no
/// settings, no history, no entries.
///
/// Zero bytes is only the crudest form of that — a write that never reached
/// the disk. A NUL-filled file, a bare byte-order mark, whitespace, or pairs
/// that sit before the first section all say just as little, and only the
/// caller can tell because only the caller reads the contents; a caller that
/// decides on the text alone asks [`says_nothing`] first. Such a file is
/// removed rather than moved aside, so it cannot take the `.bad` name from
/// the copy that an earlier, real failure kept: that copy is the last file
/// the user can still restore from. Best effort: a failed removal is logged,
/// never fatal, and the caller still treats the load as unusable.
pub(crate) fn discard_contentless(path: &Path, what: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            "{what} held nothing; removed it instead of keeping it aside"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "{what} held nothing and could not be removed; the next save replaces it"
        ),
    }
}

/// Move an existing file that could not be used out of the way (to
/// `<name>.bad`) before the caller starts over.
///
/// Starting over means the next save replaces that file name; the caller may
/// also build its state from another source instead (the settings loader falls
/// back to the legacy `settings.json`). A file that is oversized, not UTF-8 or
/// merely unreadable right now still holds the user's settings or history, so
/// it is kept for recovery instead of being overwritten, and it takes the
/// `.bad` name from any older copy: the newest bytes win, because nothing here
/// reads them. Best effort: failing to move it is logged, never fatal.
///
/// A zero-byte source goes to [`discard_contentless`] instead: it holds
/// nothing to recover, so it must not take the `.bad` name. That size check is
/// the only test available to a caller that cannot look inside the file — a
/// failed read, an oversized file, one that is not UTF-8 — so those callers use
/// this function and accept that bytes which say nothing still take the name. A
/// caller that parses the text knows better: it asks [`says_nothing`] and sends
/// the file to [`discard_contentless`] itself.
///
/// A source that was moved or removed no longer sits where the next load
/// would re-read it. When either step fails the warning is logged and the file
/// stays, so the next load reads the same unusable bytes again.
pub(crate) fn set_aside(path: &Path, what: &str) {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0) {
        discard_contentless(path, what);
        return;
    }
    let mut aside = path.as_os_str().to_owned();
    aside.push(".bad");
    match std::fs::rename(path, &aside) {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            kept = %Path::new(&aside).display(),
            "{what} could not be used; kept it aside and starting over"
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
        let dir = std::env::temp_dir().join(format!(
            "tm-persist-{}-{}",
            std::process::id(),
            NEXT_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let path = dir.join("store.json");
        // Start from an empty directory: a leftover temp file from a crashed
        // earlier run must not be able to fail this test's exact-set check.
        let _ = std::fs::remove_dir_all(&dir);
        replace_file(&path, "json.tmp", |out| out.write_all(b"first")).unwrap();
        replace_file(&path, "json.tmp", |out| out.write_all(b"second")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        // The temp file is `<name>.<pid>-<seq>.<ext>`, not `<name>.<ext>`, so
        // `path.with_extension("json.tmp")` never names what a save creates.
        // Only these two saves may have run in this directory, so an exact
        // name set proves none was left behind; a suffix scan would also blame
        // a crashed earlier run whose PID Windows reused.
        let entries: std::collections::HashSet<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            std::collections::HashSet::from(["store.json".to_string()]),
            "a completed save left a temp file behind"
        );

        set_aside(&path, "test store");
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("store.json.bad")).unwrap(),
            "second"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An existing aside copy is the user's last recoverable file. A zero-byte
    /// source holds nothing, so it is discarded instead of being moved over
    /// that copy — and it still leaves the load path for the next start.
    #[test]
    fn an_empty_source_never_replaces_an_existing_aside_copy() {
        let dir = std::env::temp_dir().join(format!(
            "tm-persist-aside-{}-{}",
            std::process::id(),
            NEXT_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("store.json");
        let aside = dir.join("store.json.bad");
        std::fs::write(&aside, b"the last recoverable copy").unwrap();
        std::fs::write(&dest, b"").unwrap();

        set_aside(&dest, "test store");

        assert!(!dest.exists(), "the unusable file must not be left for the next load");
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "the last recoverable copy",
            "an empty file must not replace the copy kept aside earlier"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The caller parses the file, so it knows a source that has bytes still
    /// says nothing. That decision must reach the same outcome: the source is
    /// removed, and the copy an earlier failure kept stays byte-identical.
    #[test]
    fn a_contentless_source_never_replaces_an_existing_aside_copy() {
        let dir = std::env::temp_dir().join(format!(
            "tm-persist-contentless-{}-{}",
            std::process::id(),
            NEXT_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("store.json");
        let aside = dir.join("store.json.bad");
        std::fs::write(&aside, b"the last recoverable copy").unwrap();
        std::fs::write(&dest, vec![0u8; 64]).unwrap();

        discard_contentless(&dest, "test store");

        assert!(!dest.exists(), "the unusable file must not be left for the next load");
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "the last recoverable copy",
            "a file that says nothing must not replace the copy kept aside earlier"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The forms a write that never landed takes, and the bound that keeps a
    /// real payload out: every character must be contentless, so one byte-order
    /// mark in front of actual content makes the whole text content.
    #[test]
    fn says_nothing_covers_whitespace_nuls_and_a_bare_byte_order_mark() {
        assert!(says_nothing(""), "empty text says nothing");
        assert!(says_nothing(" \t\r\n"), "whitespace says nothing");
        assert!(says_nothing("\0\0\0"), "a NUL hole says nothing");
        assert!(says_nothing("\u{feff}"), "a bare BOM says nothing");
        assert!(!says_nothing("\u{feff}{}"), "a BOM in front of content is content");
        assert!(!says_nothing("{}"), "an empty JSON object is content");
    }

    #[test]
    fn an_old_temp_file_is_swept_but_a_recent_one_is_left_alone() {
        let dir = std::env::temp_dir().join(format!(
            "tm-persist-sweep-{}-{}",
            std::process::id(),
            NEXT_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("store.json");
        let stale = dir.join("store.json.99999-0.json.tmp");
        let recent = dir.join("store.json.99999-1.json.tmp");
        std::fs::write(&stale, b"old").unwrap();
        std::fs::write(&recent, b"live").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        replace_file(&dest, "json.tmp", |out| out.write_all(b"fresh")).unwrap();

        assert!(!stale.exists(), "a crash leftover must be swept");
        assert!(
            recent.exists(),
            "a recent temp may still belong to a live save"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "fresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_never_share_temp_file_or_corrupt_destination() {
        let unique = format!(
            "tm-persist-concurrent-{}-{}",
            std::process::id(),
            NEXT_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(unique);
        let dest = dir.join("settings.ini");

        let threads = 4usize;
        let iters = 20usize;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let observed_errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let mut handles = Vec::with_capacity(threads);
        for tid in 0..threads {
            let barrier = std::sync::Arc::clone(&barrier);
            let dest = dest.clone();
            let observed_errors = std::sync::Arc::clone(&observed_errors);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for iter in 0..iters {
                    let expected =
                        format!("worker={tid};iter={iter};payload={}\n", "x".repeat(256));
                    // Verify temp creation succeeds without colliding on a fixed name;
                    // atomic rename may hit transient Windows PermissionDenied under
                    // heavy simultaneous rename contention on NTFS, which is fine as
                    // long as the destination stays uncorrupted and no temp files leak.
                    if let Err(err) =
                        replace_file(&dest, "ini.tmp", |out| out.write_all(expected.as_bytes()))
                        && err.kind() != std::io::ErrorKind::PermissionDenied
                    {
                        observed_errors
                            .lock()
                            .expect("lock")
                            .push(format!("worker {tid} iter {iter}: {err}"));
                    }
                    if let Ok(content) = std::fs::read_to_string(&dest) {
                        let ok = content.starts_with("worker=")
                            && content.contains(";payload=")
                            && content.ends_with('\n')
                            && content.lines().count() == 1;
                        if !ok {
                            observed_errors
                                .lock()
                                .expect("lock")
                                .push(format!("corrupt content: {content:?}"));
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker panicked");
        }

        let errs = observed_errors.lock().expect("lock");
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
        let final_content = std::fs::read_to_string(&dest).expect("final destination readable");
        assert!(final_content.ends_with('\n'));

        let entries: std::collections::HashSet<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            std::collections::HashSet::from(["settings.ini".to_string()])
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
