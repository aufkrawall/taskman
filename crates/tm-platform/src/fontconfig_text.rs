//! Linux fontconfig text settings, parsed from `fc-match -v`.
//!
//! This is intentionally a process-wide policy: fontconfig exposes no per-monitor order.
//! The startup query is cached, and spawning `fc-match` per frame would be both slow and
//! racy. The command is resolved from a small set of root-owned absolute system paths
//! rather than `PATH`, but it inherits the user's environment so `FONTCONFIG_FILE`,
//! `XDG_CONFIG_HOME`, `HOME` and desktop-specific overrides continue to select the user's
//! real configuration. Every invocation has one absolute deadline, capped stdout and
//! kill/reap cleanup on timeout.

use super::text_rendering::{LcdFilterChoice, Params};

const TRUSTED_FC_MATCH_PATHS: &[&str] = &[
    "/usr/bin/fc-match",
    "/bin/fc-match",
    "/usr/local/bin/fc-match",
    "/run/current-system/sw/bin/fc-match",
    "/nix/var/nix/profiles/default/bin/fc-match",
];
const FC_MATCH_DEADLINE: std::time::Duration = std::time::Duration::from_millis(750);
const FC_MATCH_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
struct ProgramMetadata {
    owner_uid: u32,
    mode: u32,
    is_file: bool,
    is_dir: bool,
}

fn is_trusted_program(metadata: ProgramMetadata) -> bool {
    metadata.is_file
        && metadata.owner_uid == 0
        && metadata.mode & 0o022 == 0
        && metadata.mode & 0o111 != 0
}

fn is_trusted_parent_directory(metadata: ProgramMetadata) -> bool {
    metadata.is_dir && metadata.owner_uid == 0 && metadata.mode & 0o022 == 0
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FontconfigProperties {
    antialias: Option<bool>,
    rgba: Option<u8>,
    lcdfilter: Option<u8>,
}

fn token(value: &str) -> Option<&str> {
    value
        .trim()
        .split(['(', ' ', '\t'])
        .find(|part| !part.is_empty())
}

fn parse_bool(value: &str) -> Option<bool> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn parse_rgba(value: &str) -> Option<u8> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "unknown" | "0" => Some(0),
        "rgb" | "1" => Some(1),
        "bgr" | "2" => Some(2),
        "vrgb" | "3" => Some(3),
        "vbgr" | "4" => Some(4),
        "none" | "5" => Some(5),
        _ => None,
    }
}

fn parse_lcdfilter(value: &str) -> Option<u8> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "none" | "0" => Some(0),
        "default" | "1" => Some(1),
        "light" | "2" => Some(2),
        "legacy" | "3" => Some(3),
        _ => None,
    }
}

fn parse_properties(text: &str) -> FontconfigProperties {
    let mut properties = FontconfigProperties::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "antialias" => properties.antialias = parse_bool(value),
            "rgba" => properties.rgba = parse_rgba(value),
            "lcdfilter" => properties.lcdfilter = parse_lcdfilter(value),
            _ => {}
        }
    }
    properties
}

fn params_from_output(text: &str) -> Params {
    let properties = parse_properties(text);
    let mut params = Params::default();

    // `antialias: False` is an explicit user choice. Missing or malformed values are not
    // evidence that sub-pixel text is safe, so they fail closed too.
    if properties.antialias != Some(true) {
        return params;
    }

    match properties.rgba {
        Some(1) => params.enabled = true,
        Some(2) => {
            params.enabled = true;
            params.bgr = true;
        }
        _ => return params,
    }

    params.lcd_filter = match properties.lcdfilter {
        Some(0) => LcdFilterChoice::None,
        Some(2) => LcdFilterChoice::Light,
        Some(3) => LcdFilterChoice::Legacy,
        // Fontconfig's default and unknown future values use FreeType's default filter.
        _ => LcdFilterChoice::Default,
    };
    params
}

#[cfg(target_os = "linux")]
fn resolve_fc_match() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    for candidate in TRUSTED_FC_MATCH_PATHS {
        let Ok(path) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if !path.is_absolute() || !trusted_path_chain(&path) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if is_trusted_program(ProgramMetadata {
            owner_uid: metadata.uid(),
            mode: metadata.permissions().mode(),
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
        }) {
            return Some(path);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn trusted_path_chain(path: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let Some(parent) = path.parent() else {
        return false;
    };
    for directory in parent.ancestors() {
        let Ok(metadata) = std::fs::metadata(directory) else {
            return false;
        };
        if !is_trusted_parent_directory(ProgramMetadata {
            owner_uid: metadata.uid(),
            mode: metadata.permissions().mode(),
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
        }) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
pub(super) fn query() -> Params {
    static CACHE: std::sync::OnceLock<Params> = std::sync::OnceLock::new();
    *CACHE.get_or_init(query_uncached)
}

#[cfg(target_os = "linux")]
fn query_uncached() -> Params {
    let Some(output) = run_fc_match(&["-v", "sans-serif"]) else {
        return Params::default();
    };
    params_from_output(&String::from_utf8_lossy(&output))
}

/// Ask the user's fontconfig for a concrete font file.
///
/// This shares the trusted executable resolver and bounded process capture with the
/// text-policy query above, so font selection no longer falls back to a PATH lookup.
#[cfg(target_os = "linux")]
pub(super) fn match_file(pattern: &str) -> Option<String> {
    let output = run_fc_match(&["-f", "%{file}\n", pattern])?;
    font_path_from_output(&output)
}

fn font_path_from_output(output: &[u8]) -> Option<String> {
    let path = String::from_utf8_lossy(output)
        .lines()
        .next()?
        .trim()
        .to_owned();
    if path.is_empty() {
        return None;
    }
    let path = std::path::Path::new(&path);
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    canonical
        .is_file()
        .then(|| canonical.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn run_fc_match(args: &[&str]) -> Option<Vec<u8>> {
    let Some(program) = resolve_fc_match() else {
        tracing::debug!("no trusted absolute fc-match found; fontconfig query disabled");
        return None;
    };
    let mut command = std::process::Command::new(program);
    command.args(args.iter().copied());
    run_command_bounded(&mut command, FC_MATCH_DEADLINE, FC_MATCH_OUTPUT_LIMIT)
}

#[cfg(target_os = "linux")]
fn run_command_bounded(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    output_limit: usize,
) -> Option<Vec<u8>> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child);
        return None;
    };
    let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
        terminate_and_reap(&mut child);
        return None;
    };

    let output = capture_to_deadline(&mut stdout, deadline, output_limit);
    let Some(output) = output else {
        terminate_and_reap(&mut child);
        return None;
    };
    let Some(status) = wait_for_exit(&mut child, deadline) else {
        terminate_and_reap(&mut child);
        return None;
    };
    status.success().then_some(output)
}

#[cfg(target_os = "linux")]
fn capture_to_deadline<R: std::os::fd::AsRawFd>(
    reader: &mut R,
    deadline: std::time::Instant,
    output_limit: usize,
) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd as _;

    let fd = reader.as_raw_fd();
    let mut output = Vec::new();
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        // SAFETY: `pollfd` points to one initialized descriptor for the duration of the
        // call, and the timeout is bounded.
        let ready = unsafe { libc::poll(&mut pollfd, 1, remaining_ms) };
        if ready < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if ready == 0 {
            return None;
        }

        let remaining_output = output_limit.saturating_add(1).saturating_sub(output.len());
        if remaining_output == 0 {
            return None;
        }
        let mut chunk = [0u8; 4096];
        let want = remaining_output.min(chunk.len());
        // SAFETY: `chunk` is writable for `want` bytes and the descriptor is owned by
        // `reader`; `poll` reported it ready.
        let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), want) };
        if read < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if read == 0 {
            return Some(output);
        }
        output.extend_from_slice(&chunk[..read as usize]);
        if output.len() > output_limit {
            return None;
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_for_exit(
    child: &mut std::process::Child,
    deadline: std::time::Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::yield_now();
    }
}

#[cfg(target_os = "linux")]
fn terminate_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(antialias: &str, rgba: &str, lcdfilter: &str) -> String {
        format!(
            "Pattern has 34 elts (size 16)\n\
             \tantialias: {antialias}(w)\n\
             \trgba: {rgba}(i)(w)\n\
             \tlcdfilter: {lcdfilter}(i)(w)\n"
        )
    }

    #[test]
    fn horizontal_orders_enable_the_matching_stripe_order() {
        let rgb = params_from_output(&output("True", "1", "1"));
        assert!(rgb.enabled);
        assert!(!rgb.bgr);

        let bgr = params_from_output(&output("True", "2", "1"));
        assert!(bgr.enabled);
        assert!(bgr.bgr);
    }

    #[test]
    fn unknown_vertical_none_and_malformed_orders_fail_closed() {
        for rgba in ["0", "3", "4", "5", "bogus", ""] {
            let params = params_from_output(&output("True", rgba, "1"));
            assert!(!params.enabled, "rgba={rgba:?} must stay grayscale");
        }
        assert!(!params_from_output("rgba: 1(i)(w)\n").enabled);
        assert!(!params_from_output("garbage\n").enabled);
    }

    #[test]
    fn antialias_false_disables_subpixel_text() {
        let params = params_from_output(&output("False", "1", "1"));
        assert!(!params.enabled);
        assert!(!params_from_output(&output("not-a-bool", "1", "1")).enabled);
    }

    #[test]
    fn lcdfilter_maps_to_the_fork_filter_choices() {
        let filter = |value| params_from_output(&output("True", "1", value)).lcd_filter;
        assert_eq!(filter("0"), LcdFilterChoice::None);
        assert_eq!(filter("1"), LcdFilterChoice::Default);
        assert_eq!(filter("2"), LcdFilterChoice::Light);
        assert_eq!(filter("3"), LcdFilterChoice::Legacy);
        assert_eq!(filter("future"), LcdFilterChoice::Default);
    }

    #[test]
    fn trusted_path_policy_rejects_user_or_writable_programs() {
        assert!(is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o755,
            is_file: true,
            is_dir: false,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 1000,
            mode: 0o755,
            is_file: true,
            is_dir: false,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o775,
            is_file: true,
            is_dir: false,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o644,
            is_file: true,
            is_dir: false,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o755,
            is_file: false,
            is_dir: false,
        }));
        assert!(is_trusted_parent_directory(ProgramMetadata {
            owner_uid: 0,
            mode: 0o755,
            is_file: false,
            is_dir: true,
        }));
        assert!(!is_trusted_parent_directory(ProgramMetadata {
            owner_uid: 1000,
            mode: 0o755,
            is_file: false,
            is_dir: true,
        }));
        assert!(!is_trusted_parent_directory(ProgramMetadata {
            owner_uid: 0,
            mode: 0o775,
            is_file: false,
            is_dir: true,
        }));
        assert!(!is_trusted_parent_directory(ProgramMetadata {
            owner_uid: 0,
            mode: 0o755,
            is_file: true,
            is_dir: false,
        }));
        assert!(
            TRUSTED_FC_MATCH_PATHS
                .iter()
                .all(|path| path.starts_with('/'))
        );
    }

    #[test]
    fn font_file_policy_accepts_only_an_absolute_existing_file() {
        let current = std::env::current_exe().expect("test executable");
        let current = std::fs::canonicalize(current).expect("canonical test executable");
        let output = format!("{}\n", current.display());
        let expected = current.to_string_lossy().into_owned();
        assert_eq!(
            font_path_from_output(output.as_bytes()).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(font_path_from_output(b"./relative-font.ttf\n"), None);
        assert_eq!(font_path_from_output(b"\n"), None);
        assert_eq!(font_path_from_output(b"   \n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_enforces_output_limit_without_live_fontconfig() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf 12345678901234567890"]);
        assert_eq!(
            run_command_bounded(&mut command, std::time::Duration::from_secs(1), 16),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_times_out_kills_and_reaps_without_live_fontconfig() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "while :; do :; done"]);
        let started = std::time::Instant::now();
        assert_eq!(
            run_command_bounded(&mut command, std::time::Duration::from_millis(30), 1024),
            None
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a hung fc-match helper must be killed at its deadline"
        );
    }
}
