//! Self-owned XDG autostart entry for Linux.
//!
//! The entry is written under `$XDG_CONFIG_HOME/autostart` only when the user
//! turns it on. Disabling removes a regular, user-owned desktop file bearing
//! TaskMan's marker and refuses to touch any other file. Linux has no tray
//! implementation here, so the entry deliberately starts a visible window.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tm_core::error::{Result, TmError};

const ENTRY_FILE_NAME: &str = "io.github.aufkrawall.Taskman.desktop";
const OWNED_MARKER: &str = "X-Taskman-Autostart=1";
const MAX_ENTRY_BYTES: u64 = 64 * 1024;

pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        config_home().is_some()
            && std::env::current_exe()
                .ok()
                .is_some_and(|path| path.is_absolute() && path.to_str().is_some())
    })
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let home = config_home()
        .ok_or_else(|| TmError::platform("autostart", "no absolute XDG config home"))?;
    if enabled {
        let executable = std::env::current_exe()
            .map_err(|error| TmError::platform("autostart executable", error.to_string()))?;
        set_enabled_at(&home, &executable, true)
    } else {
        remove_owned_entry(&entry_path(&home))
    }
}

fn config_home() -> Option<PathBuf> {
    super::startup::config_home()
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".config"))
        })
}

fn set_enabled_at(config_home: &Path, executable: &Path, enabled: bool) -> Result<()> {
    let path = entry_path(config_home);
    if !enabled {
        return remove_owned_entry(&path);
    }

    let parent = path
        .parent()
        .ok_or_else(|| TmError::platform("XDG autostart", "entry has no parent directory"))?;
    fs::create_dir_all(parent)
        .map_err(|error| TmError::platform("create XDG autostart directory", error.to_string()))?;
    validate_autostart_directory(parent)?;
    let contents = desktop_entry(executable)?;
    write_owned_entry(&path, &contents)?;
    tracing::info!(?path, "enabled self-owned XDG autostart entry");
    Ok(())
}

fn validate_autostart_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)
        .map_err(|error| TmError::platform("inspect XDG autostart directory", error.to_string()))?;
    if !metadata.file_type().is_dir() {
        return Err(TmError::platform(
            "XDG autostart",
            "autostart path is not a real directory",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(TmError::platform(
            "XDG autostart",
            "autostart directory is owned by another user",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(TmError::platform(
            "XDG autostart",
            "autostart directory is writable by another user",
        ));
    }
    Ok(())
}

fn entry_path(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join(ENTRY_FILE_NAME)
}

fn desktop_entry(executable: &Path) -> Result<String> {
    if !executable.is_absolute() {
        return Err(TmError::platform(
            "XDG autostart",
            "executable path is not absolute",
        ));
    }
    let executable = executable.to_str().ok_or_else(|| {
        TmError::platform(
            "XDG autostart",
            "executable path is not valid UTF-8 for a desktop entry",
        )
    })?;
    let exec =
        quote_exec_arg(executable).map_err(|error| TmError::platform("XDG autostart", error))?;
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Taskman\n\
         Comment=Cross-platform task manager and system monitor\n\
         Exec={exec}\n\
         Icon=utilities-system-monitor\n\
         Terminal=false\n\
         Categories=System;Monitor;\n\
         Keywords=task;manager;process;system;monitor;\n\
        StartupNotify=false\n\
         Hidden=false\n\
         X-GNOME-Autostart-enabled=true\n\
         {OWNED_MARKER}\n"
    ))
}

/// Quote one executable path for the desktop-entry `Exec` value.
///
/// Desktop-entry string escaping is applied after `Exec` argument escaping:
/// the argument is quoted/escaped for the Exec parser, then every resulting
/// backslash is doubled for the key-file parser. A literal `%` is doubled so
/// it cannot be interpreted as a field code.
fn quote_exec_arg(argument: &str) -> std::result::Result<String, String> {
    if argument.is_empty() {
        return Err("executable path is empty".to_string());
    }
    if argument.chars().any(char::is_control) {
        return Err("executable path contains a control character".to_string());
    }
    let with_field_escapes = argument.replace('%', "%%");

    let exec_layer = if with_field_escapes.chars().any(|character| {
        character.is_whitespace()
            || matches!(
                character,
                '"' | '\''
                    | '\\'
                    | '>'
                    | '<'
                    | '~'
                    | '|'
                    | '&'
                    | ';'
                    | '$'
                    | '*'
                    | '?'
                    | '#'
                    | '('
                    | ')'
                    | '`'
            )
    }) {
        let mut quoted = String::with_capacity(with_field_escapes.len() + 2);
        quoted.push('"');
        for character in with_field_escapes.chars() {
            if matches!(character, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(character);
        }
        quoted.push('"');
        quoted
    } else {
        with_field_escapes
    };

    let mut value = String::with_capacity(exec_layer.len() + 8);
    for character in exec_layer.chars() {
        if character == '\\' {
            value.push_str("\\\\");
        } else {
            value.push(character);
        }
    }
    Ok(value)
}

fn write_owned_entry(path: &Path, contents: &str) -> Result<()> {
    let mut existing = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(TmError::platform(
                "open XDG autostart entry",
                error.to_string(),
            ));
        }
    };
    if let Some(file) = existing.as_mut() {
        validate_owned_regular_file(file)?;
        if file.metadata().map_err(TmError::from)?.mode() & 0o7777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    TmError::platform("secure XDG autostart entry", error.to_string())
                })?;
        }
        let existing = read_entry(file)?;
        if !is_owned_entry(&existing) {
            return Err(TmError::platform(
                "XDG autostart",
                "refusing to replace an entry not owned by TaskMan",
            ));
        }
        if existing == contents {
            return Ok(());
        }
    }

    let parent = path
        .parent()
        .ok_or_else(|| TmError::platform("XDG autostart", "entry has no parent"))?;
    let temp_name = format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default(),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let temp_path = parent.join(temp_name);
    let write_result = (|| -> std::io::Result<()> {
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&temp_path)?;
        temp.write_all(contents.as_bytes())?;
        temp.sync_all()?;
        if existing.is_some() {
            fs::rename(&temp_path, path)?;
        } else {
            rename_noreplace(&temp_path, path)?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(TmError::platform(
            "write XDG autostart entry",
            error.to_string(),
        ));
    }
    let verified = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| TmError::platform("verify XDG autostart entry", error.to_string()))?;
    validate_owned_regular_file(&verified)?;
    let mut verified = verified;
    if read_entry(&mut verified)? != contents {
        return Err(TmError::platform(
            "verify XDG autostart entry",
            "the atomically written entry did not match",
        ));
    }
    Ok(())
}

fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_owned_entry(path: &Path) -> Result<()> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(TmError::platform(
                "open XDG autostart entry",
                error.to_string(),
            ));
        }
    };
    validate_owned_regular_file(&file)?;
    let opened = file.metadata().map_err(TmError::from)?;
    let contents = read_entry(&mut file)?;
    if !is_owned_entry(&contents) {
        return Err(TmError::platform(
            "XDG autostart",
            "refusing to remove an entry not owned by TaskMan",
        ));
    }
    let current = fs::symlink_metadata(path).map_err(TmError::from)?;
    if current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Err(TmError::platform(
            "XDG autostart",
            "autostart entry changed while it was being validated",
        ));
    }
    fs::remove_file(path)
        .map_err(|error| TmError::platform("remove XDG autostart entry", error.to_string()))?;
    tracing::info!(?path, "disabled self-owned XDG autostart entry");
    Ok(())
}

fn validate_owned_regular_file(file: &File) -> Result<()> {
    let metadata = file.metadata().map_err(TmError::from)?;
    if !metadata.is_file() {
        return Err(TmError::platform(
            "XDG autostart",
            "entry is not a regular file",
        ));
    }
    // SAFETY: geteuid has no pointer arguments and cannot fail.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(TmError::platform(
            "XDG autostart",
            "entry is owned by another user",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(TmError::platform(
            "XDG autostart",
            "entry has multiple hard links",
        ));
    }
    if metadata.len() > MAX_ENTRY_BYTES {
        return Err(TmError::platform(
            "XDG autostart",
            "entry exceeds the size limit",
        ));
    }
    Ok(())
}

fn read_entry(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0)).map_err(TmError::from)?;
    let mut text = String::new();
    Read::by_ref(file)
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| TmError::platform("read XDG autostart entry", error.to_string()))?;
    if text.len() as u64 > MAX_ENTRY_BYTES {
        return Err(TmError::platform(
            "XDG autostart",
            "entry exceeds the size limit",
        ));
    }
    Ok(text)
}

fn is_owned_entry(text: &str) -> bool {
    let mut in_entry = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if in_entry && line == OWNED_MARKER {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "taskman-linux-autostart-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn exec_quoting_handles_reserved_characters_and_percent_fields() {
        assert_eq!(
            quote_exec_arg("/opt/TaskMan/taskman").unwrap(),
            "/opt/TaskMan/taskman"
        );
        assert_eq!(
            quote_exec_arg("/opt/Task Man/taskman").unwrap(),
            "\"/opt/Task Man/taskman\""
        );
        assert_eq!(
            quote_exec_arg("/opt/100%.desktop").unwrap(),
            "/opt/100%%.desktop"
        );
        assert_eq!(quote_exec_arg("/opt/$HOME").unwrap(), r#""/opt/\\$HOME""#);
        assert_eq!(quote_exec_arg(r"/opt/a\b").unwrap(), r#""/opt/a\\\\b""#);
        assert_eq!(quote_exec_arg(r#"/opt/a"b"#).unwrap(), r#""/opt/a\\"b""#);
        assert!(quote_exec_arg("").is_err());
        assert!(quote_exec_arg("/opt/line\nbreak").is_err());
    }

    #[test]
    fn autostart_entry_is_idempotent_and_never_requests_a_hidden_start() {
        let config = test_config("idempotent");
        let executable = Path::new("/opt/Task Man/taskman");
        set_enabled_at(&config, executable, true).unwrap();
        let path = config.join("autostart").join(ENTRY_FILE_NAME);
        let first = fs::read_to_string(&path).unwrap();
        assert!(is_owned_entry(&first));
        assert!(first.contains("Exec=\"/opt/Task Man/taskman\"\n"));
        assert!(!first.contains("--minimized-to-tray"));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

        set_enabled_at(&config, executable, true).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);

        set_enabled_at(&config, executable, false).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn disabling_and_enabling_leave_unowned_entries_untouched() {
        let config = test_config("unowned");
        let dir = config.join("autostart");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ENTRY_FILE_NAME);
        let original = "[Desktop Entry]\nType=Application\nName=Someone else\nExec=other\n";
        fs::write(&path, original).unwrap();

        assert!(set_enabled_at(&config, Path::new("/opt/taskman"), false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(set_enabled_at(&config, Path::new("/opt/taskman"), true).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn autostart_refuses_symlinked_entries() {
        let config = test_config("symlink");
        let dir = config.join("autostart");
        fs::create_dir_all(&dir).unwrap();
        let target = config.join("target.desktop");
        fs::write(&target, "[Desktop Entry]\nName=Elsewhere\n").unwrap();
        let path = dir.join(ENTRY_FILE_NAME);
        std::os::unix::fs::symlink(&target, &path).unwrap();

        assert!(set_enabled_at(&config, Path::new("/opt/taskman"), true).is_err());
        assert!(set_enabled_at(&config, Path::new("/opt/taskman"), false).is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "[Desktop Entry]\nName=Elsewhere\n"
        );
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn failed_first_write_does_not_leave_a_placeholder_entry() {
        let config = test_config("failed-first-write");
        let dir = config.join("autostart");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let path = dir.join(ENTRY_FILE_NAME);

        assert!(set_enabled_at(&config, Path::new("/opt/taskman"), true).is_err());
        assert!(!path.exists());

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(config);
    }
}
