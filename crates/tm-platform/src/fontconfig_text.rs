//! Linux fontconfig text settings, parsed from `fc-match -v`.
//!
//! This is intentionally a process-wide policy: fontconfig exposes no per-monitor order.
//! The startup query is cached, and spawning `fc-match` per frame would be both slow and
//! racy. The command is resolved from a small set of root-owned absolute system paths
//! rather than `PATH`, but it inherits the user's environment so `FONTCONFIG_FILE`,
//! `XDG_CONFIG_HOME`, `HOME` and desktop-specific overrides continue to select the user's
//! real configuration. Successful resolution is memoized for the process lifetime because it
//! only reads root-owned system state; a miss is retried, since its causes (a package manager
//! installing fontconfig, a stalled autofs mount, a transient descriptor shortage) can clear.
//! Every invocation has one absolute deadline, capped
//! stdout and kill/reap cleanup on timeout, and waits for the child by parking on it
//! (pidfd on Linux, a capped backoff elsewhere) instead of spinning.

use super::text_rendering::{HintStyleChoice, LcdFilterChoice, Params};

const TRUSTED_FC_MATCH_PATHS: &[&str] = &[
    "/usr/bin/fc-match",
    "/bin/fc-match",
    "/usr/local/bin/fc-match",
    "/run/current-system/sw/bin/fc-match",
    "/nix/var/nix/profiles/default/bin/fc-match",
];
#[cfg(target_os = "linux")]
const FC_MATCH_DEADLINE: std::time::Duration = std::time::Duration::from_millis(750);
#[cfg(target_os = "linux")]
const FC_MATCH_OUTPUT_LIMIT: usize = 64 * 1024;
/// First pause between child-status polls, cheap enough for a child that exits at once.
const POLL_BACKOFF_START: std::time::Duration = std::time::Duration::from_micros(100);
/// Longest pause between child-status polls; caps the wakeup rate of a slow child.
const POLL_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_millis(5);

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
    hinting: Option<bool>,
    hintstyle: Option<u8>,
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

fn parse_hintstyle(value: &str) -> Option<u8> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "hintnone" | "none" | "0" => Some(0),
        "hintslight" | "slight" | "1" => Some(1),
        "hintmedium" | "medium" | "2" => Some(2),
        "hintfull" | "full" | "3" => Some(3),
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
            "hinting" => properties.hinting = parse_bool(value),
            "hintstyle" => properties.hintstyle = parse_hintstyle(value),
            _ => {}
        }
    }
    properties
}

fn params_from_output(text: &str) -> Params {
    let properties = parse_properties(text);
    // FreeType/Fontconfig on Linux has no DirectWrite enhanced-contrast boost, so keep
    // contrast at 0.0 instead of inheriting Windows' 0.5 default.
    let mut params = Params {
        contrast: 0.0,
        ..Params::default()
    };

    params.hint_style = if properties.hinting == Some(false) {
        HintStyleChoice::None
    } else {
        match properties.hintstyle {
            Some(0) => HintStyleChoice::None,
            Some(1) => HintStyleChoice::Slight,
            Some(2) => HintStyleChoice::Medium,
            Some(3) => HintStyleChoice::Full,
            _ => HintStyleChoice::Default,
        }
    };

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

/// The trusted `fc-match` path, resolved once per process.
///
/// A hit stays: the program is root-owned, every ancestor directory is root-owned and not
/// group/world-writable, and a non-root process cannot replace what it would execute later
/// in the same session, so the trust decision is stable.
///
/// A miss is deliberately NOT cached. Its causes have nothing to do with trust — fontconfig
/// installed after first use, a stalled autofs/NFS mount, a transient `EMFILE`/`ENOMEM` from
/// `canonicalize` — so failing closed for the rest of the session would outlast the problem.
#[cfg(target_os = "linux")]
fn resolve_fc_match() -> Option<std::path::PathBuf> {
    static RESOLVED: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    if let Some(path) = RESOLVED.get() {
        return Some(path.clone());
    }
    let path = resolve_fc_match_uncached()?;
    let _ = RESOLVED.set(path.clone());
    Some(path)
}

#[cfg(target_os = "linux")]
fn resolve_fc_match_uncached() -> Option<std::path::PathBuf> {
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
    match_face(pattern).map(|(path, _)| path)
}

/// Ask the user's fontconfig for both the concrete font file and its face index (`%{index}`).
#[cfg(target_os = "linux")]
pub(super) fn match_face(pattern: &str) -> Option<(String, u32)> {
    let output = run_fc_match(&["-f", "%{file}\n%{index}\n", pattern])?;
    font_face_from_output(&output)
}

fn font_face_from_output(output: &[u8]) -> Option<(String, u32)> {
    let text = String::from_utf8_lossy(output);
    let mut lines = text.lines();
    let path = lines.next()?.trim().to_owned();
    if path.is_empty() {
        return None;
    }
    let path = std::path::Path::new(&path);
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    let index = lines
        .next()
        .and_then(|line| line.trim().parse::<u32>().ok())
        .unwrap_or(0);
    Some((canonical.to_string_lossy().into_owned(), index))
}

#[cfg(test)]
fn font_path_from_output(output: &[u8]) -> Option<String> {
    font_face_from_output(output).map(|(path, _)| path)
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
    let (status, _) = wait_with_deadline(&mut child, deadline);
    let Some(status) = status else {
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

/// Poll `child` until it exits or `deadline` passes.
///
/// Returns the child's own exit status (or `None` when the deadline passed first) plus the
/// number of status polls it took, so a test can bound the poll rate. One absolute deadline
/// bounds the whole wait.
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: std::time::Instant,
) -> (Option<std::process::ExitStatus>, u32) {
    let waiter = ChildWaiter::new(child);
    let mut backoff = PollBackoff::new();
    let mut polls = 0u32;
    loop {
        polls = polls.saturating_add(1);
        match child.try_wait() {
            Ok(Some(status)) => return (Some(status), polls),
            Ok(None) => {}
            Err(_) => return (None, polls),
        }
        if std::time::Instant::now() >= deadline {
            return (None, polls);
        }
        waiter.park(&mut backoff, deadline);
    }
}

/// Pause between child-status polls: 100 us, doubling, capped at 5 ms.
#[derive(Clone, Copy, Debug)]
struct PollBackoff {
    next: std::time::Duration,
}

impl PollBackoff {
    fn new() -> Self {
        Self {
            next: POLL_BACKOFF_START,
        }
    }

    /// The pause to take now, then the next (doubled, capped) one.
    fn step(&mut self) -> std::time::Duration {
        let pause = self.next;
        self.next = (self.next * 2).min(POLL_BACKOFF_CAP);
        pause
    }
}

/// Parks the polling thread until the child may have exited.
///
/// Linux pins the child with a pidfd and polls that, so a slow child costs no CPU;
/// kernels or seccomp profiles without `pidfd_open` fall back to the caller's backoff.
struct ChildWaiter {
    #[cfg(target_os = "linux")]
    pidfd: Option<libc::c_int>,
}

impl ChildWaiter {
    #[cfg(target_os = "linux")]
    fn new(child: &std::process::Child) -> Self {
        // SAFETY: `pidfd_open` only reads the pid; a negative result means the kernel or a
        // seccomp profile refused it, which is not an error for this caller.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
        Self {
            pidfd: (fd >= 0).then_some(fd as libc::c_int),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn new(_child: &std::process::Child) -> Self {
        Self {}
    }

    /// Block until the child may have exited, never past `deadline`.
    fn park(&self, backoff: &mut PollBackoff, deadline: std::time::Instant) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if self.wait_on_child(remaining) {
            return;
        }
        std::thread::sleep(backoff.step().min(remaining));
    }

    /// Whether the pidfd park really waited. A kernel or seccomp profile that refuses
    /// `poll` must fall back to the sleep: re-polling a permanently failing call would
    /// turn the park back into the spin it replaced.
    #[cfg(target_os = "linux")]
    fn wait_on_child(&self, remaining: std::time::Duration) -> bool {
        self.pidfd.is_some_and(|fd| poll_child_exit(fd, remaining))
    }

    #[cfg(not(target_os = "linux"))]
    fn wait_on_child(&self, _remaining: std::time::Duration) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
impl Drop for ChildWaiter {
    fn drop(&mut self) {
        if let Some(fd) = self.pidfd {
            // SAFETY: `fd` is the descriptor `new` opened and nothing else owns it.
            let _ = unsafe { libc::close(fd) };
        }
    }
}

/// Block until the child behind `fd` exits or `remaining` passes.
///
/// Returns whether the wait actually happened. A timeout counts as a wait — the caller
/// re-checks its deadline either way — while an interrupted, failed or nonsensical call does
/// not, so the caller can fall back to its backoff instead of re-polling at full speed.
/// `poll` reports an unusable descriptor as `POLLNVAL` rather than an error, hence that check
/// as well.
#[cfg(target_os = "linux")]
fn poll_child_exit(fd: libc::c_int, remaining: std::time::Duration) -> bool {
    let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` describes one live descriptor for the duration of the call and the
    // timeout is bounded; a timeout or an interrupted call just re-enters the caller's loop.
    let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    ready >= 0 && (pollfd.revents & libc::POLLNVAL) == 0
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
    fn linux_params_zero_contrast_and_parse_hintstyle() {
        let slight = params_from_output(
            "Pattern has 34 elts (size 16)\n\
             \tantialias: True(w)\n\
             \thinting: True(w)\n\
             \thintstyle: 1(i)(w)\n\
             \trgba: 1(i)(w)\n\
             \tlcdfilter: 1(i)(w)\n",
        );
        assert_eq!(slight.contrast, 0.0);
        assert_eq!(slight.hint_style, HintStyleChoice::Slight);

        let disabled_hinting = params_from_output(
            "Pattern has 34 elts (size 16)\n\
             \tantialias: True(w)\n\
             \thinting: False(w)\n\
             \thintstyle: 3(i)(w)\n\
             \trgba: 5(i)(w)\n",
        );
        assert!(!disabled_hinting.enabled);
        assert_eq!(disabled_hinting.contrast, 0.0);
        assert_eq!(disabled_hinting.hint_style, HintStyleChoice::None);

        let full = params_from_output(
            "\tantialias: True(w)\n\
             \thintstyle: hintfull(s)\n\
             \trgba: 1(i)(w)\n",
        );
        assert_eq!(full.hint_style, HintStyleChoice::Full);
    }

    #[test]
    fn font_face_from_output_parses_collection_index() {
        let current = std::env::current_exe().expect("test executable");
        let current = std::fs::canonicalize(current).expect("canonical test executable");
        let expected = current.to_string_lossy().into_owned();

        let with_index = format!("{}\n2\n", current.display());
        assert_eq!(
            font_face_from_output(with_index.as_bytes()),
            Some((expected.clone(), 2))
        );

        let missing_index = format!("{}\n", current.display());
        assert_eq!(
            font_face_from_output(missing_index.as_bytes()),
            Some((expected, 0))
        );
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

    /// A child that exits with `code` immediately.
    fn exit_command(code: i32) -> std::process::Command {
        let script = format!("exit {code}");
        #[cfg(target_os = "windows")]
        let mut command = std::process::Command::new("cmd");
        #[cfg(not(target_os = "windows"))]
        let mut command = std::process::Command::new("/bin/sh");
        #[cfg(target_os = "windows")]
        command.args(["/C", script.as_str()]);
        #[cfg(not(target_os = "windows"))]
        command.args(["-c", script.as_str()]);
        command
    }

    /// A child that lives about a second.
    ///
    /// `ping` directly rather than through `cmd` on Windows: a shell wrapper would leave
    /// the sleeper alive when a caller kills only the child it spawned.
    fn slow_command() -> std::process::Command {
        #[cfg(target_os = "windows")]
        let mut command = std::process::Command::new("ping");
        #[cfg(not(target_os = "windows"))]
        let mut command = std::process::Command::new("/bin/sh");
        #[cfg(target_os = "windows")]
        command.args(["-n", "2", "127.0.0.1"]);
        #[cfg(not(target_os = "windows"))]
        command.args(["-c", "sleep 1"]);
        command
    }

    #[test]
    fn poll_backoff_grows_monotonically_to_a_hard_cap() {
        // The fc-match deadline is 750 ms; the schedule must cover it in few polls.
        let deadline = std::time::Duration::from_millis(750);
        let mut backoff = PollBackoff::new();
        let mut pause = backoff.step();
        assert_eq!(pause, POLL_BACKOFF_START);

        let mut covered = pause;
        let mut steps = 1u32;
        while covered < deadline {
            let next = backoff.step();
            assert!(next >= pause, "the backoff must never shrink");
            assert!(next <= POLL_BACKOFF_CAP, "the backoff must stay capped");
            pause = next;
            covered += pause;
            steps += 1;
            assert!(
                steps < 400,
                "the schedule must reach the deadline in few steps"
            );
        }
        assert_eq!(pause, POLL_BACKOFF_CAP, "growth must stop at the cap");
        assert!(steps <= 200, "750 ms must need at most 200 polls: {steps}");
        assert_eq!(backoff.step(), POLL_BACKOFF_CAP);
    }

    #[test]
    fn wait_with_deadline_reports_the_childs_real_exit_status() {
        for (code, success) in [(0, true), (3, false)] {
            let mut child = exit_command(code)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn a short-lived child");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let (status, polls) = wait_with_deadline(&mut child, deadline);
            let status = status.expect("an exiting child must report its own status");
            assert_eq!(status.success(), success, "exit {code}: {status:?}");
            if code != 0 {
                assert_eq!(
                    status.code(),
                    Some(code),
                    "the wait must pass the real exit code through"
                );
            }
            assert!(polls > 0);
        }
    }

    #[test]
    fn wait_with_deadline_parks_instead_of_spinning_on_a_living_child() {
        let mut child = slow_command()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a child that lives about a second");
        let started = std::time::Instant::now();
        let (status, polls) =
            wait_with_deadline(&mut child, started + std::time::Duration::from_secs(30));
        let elapsed = started.elapsed();
        assert!(status.is_some_and(|status| status.success()), "{status:?}");
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "the child must really live: {elapsed:?}"
        );
        // The poll RATE is the property under test, not the count: a count would have to
        // grow with the child's lifetime on a loaded machine, while a spin is ~10k/s here
        // whatever the load. One poll per capped backoff step (one park when pidfd is
        // available) is about 200/s.
        let rate = f64::from(polls) / elapsed.as_secs_f64().max(0.001);
        assert!(
            rate < 1_000.0,
            "waiting {elapsed:?} polled {polls} times ({rate:.0}/s)"
        );
    }

    #[test]
    fn wait_with_deadline_reports_timeout_and_leaves_the_child_reapable() {
        let mut child = slow_command()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a child that outlives the deadline");
        let started = std::time::Instant::now();
        let (status, _) =
            wait_with_deadline(&mut child, started + std::time::Duration::from_millis(50));
        assert!(
            status.is_none(),
            "a child outliving the deadline reports no status"
        );
        let _ = child.kill();
        let reaped = child
            .wait()
            .expect("the killed child must still be reapable");
        assert!(
            !reaped.success(),
            "a killed child must not report success: {reaped:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the deadline must bound the wait: {:?}",
            started.elapsed()
        );
    }
}
