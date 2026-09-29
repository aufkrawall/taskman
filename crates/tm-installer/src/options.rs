//! Setup command line.
//!
//! Flag spelling follows the established Windows setup conventions (and the
//! green-curve setup this installer was modeled on): `/S`, `--uninstall`,
//! `--no-start-menu`, `--desktop`, `--launch`. Everything else fails loudly -
//! a setup that silently ignores a mistyped flag installs the wrong thing.

use tm_core::error::{Result, TmError};

pub const USAGE: &str = "\
Task Manager setup

USAGE:
  taskman-setup.exe [OPTIONS]

OPTIONS:
  /S, --silent          Unattended install/uninstall; exit code 0 on success
  --uninstall           Remove an existing installation
  --no-start-menu       Do not create the Start menu shortcut
  --desktop             Create a desktop shortcut (off by default)
  --launch              Start Task Manager when the install completes
  --no-service          Do not register the background service now
                        (the app can register it later from Settings, with UAC)
  --dry-run             Print the planned steps without changing anything
  --help                Show this help

The installation directory is fixed at %ProgramFiles%\\TaskMan: the background
service is pinned to that protected location, so custom install directories
are not supported.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub mode: Mode,
    pub silent: bool,
    pub start_menu: bool,
    pub desktop: bool,
    pub launch: bool,
    pub service: bool,
    pub dry_run: bool,
    pub help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            mode: Mode::Install,
            silent: false,
            // Start menu on, desktop off: the install is machine-wide, the
            // shortcuts are for the administrator who ran the setup.
            start_menu: true,
            desktop: false,
            launch: false,
            service: true,
            dry_run: false,
            help: false,
        }
    }
}

fn error(detail: impl Into<String>) -> TmError {
    TmError::platform("setup arguments", detail)
}

/// Parse setup arguments (without the program name).
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut opts = Options::default();

    for raw in args {
        // Windows-style flags are case-insensitive; GNU-style long flags are
        // matched exactly.
        let lower = raw.to_ascii_lowercase();
        match lower.as_str() {
            "/s" | "/silent" | "--silent" => opts.silent = true,
            "/uninstall" | "--uninstall" | "-u" => opts.mode = Mode::Uninstall,
            "/?" | "--help" | "-h" => opts.help = true,
            "/dry-run" | "--dry-run" => opts.dry_run = true,
            _ => match raw.as_str() {
                "--no-start-menu" => opts.start_menu = false,
                "--desktop" => opts.desktop = true,
                "--launch" => opts.launch = true,
                "--no-service" => opts.service = false,
                _ => {
                    if lower.starts_with("/d=") || lower.starts_with("/d:") {
                        return Err(error(
                            "custom install directories are not supported: the background \
                             service is pinned to the protected %ProgramFiles%\\TaskMan \
                             location (remove the /D flag)",
                        ));
                    }
                    return Err(error(format!(
                        "unrecognized argument {raw:?} (run taskman-setup.exe --help)"
                    )));
                }
            },
        }
    }

    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(args: &[&str]) -> Result<Options> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_install_the_full_experience() {
        let opts = parse_all(&[]).unwrap();
        assert_eq!(opts.mode, Mode::Install);
        assert!(opts.service, "service install is on by default");
        assert!(opts.start_menu, "Start menu shortcut is on by default");
        assert!(!opts.desktop, "desktop shortcut is opt-in");
        assert!(!opts.launch);
        assert!(!opts.silent);
    }

    #[test]
    fn silent_and_uninstall_flags_parse_case_insensitively() {
        let opts = parse_all(&["/S", "/UNINSTALL"]).unwrap();
        assert!(opts.silent);
        assert_eq!(opts.mode, Mode::Uninstall);
        let opts = parse_all(&["--silent", "--uninstall"]).unwrap();
        assert!(opts.silent);
        assert_eq!(opts.mode, Mode::Uninstall);
    }

    #[test]
    fn shortcut_and_service_opt_outs_parse() {
        let opts =
            parse_all(&["--no-start-menu", "--desktop", "--launch", "--no-service"]).unwrap();
        assert!(!opts.start_menu);
        assert!(opts.desktop);
        assert!(opts.launch);
        assert!(!opts.service);
    }

    /// `/D` is the Windows convention for "install here"; accepting it and
    /// then ignoring it would install somewhere the user did not ask for, so
    /// it must fail with an explanation instead.
    #[test]
    fn custom_install_directory_is_refused_with_an_explanation() {
        for flag in ["/D=C:\\Tools\\TaskMan", "/d:C:\\Tools\\TaskMan"] {
            let error = parse_all(&[flag]).unwrap_err();
            assert!(
                error.to_string().contains("not supported"),
                "{flag}: {error}"
            );
        }
    }

    #[test]
    fn unknown_arguments_fail_loudly() {
        let error = parse_all(&["--instal"]).unwrap_err();
        assert!(error.to_string().contains("unrecognized"), "{error}");
    }
}
