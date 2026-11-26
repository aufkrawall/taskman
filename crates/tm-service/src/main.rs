//! TaskMan's privileged core service.
//!
//! The service owns only allowlisted privileged control operations. System
//! sampling and all GUI state intentionally remain in the interactive user's
//! process so a privileged snapshot cannot become a cross-user data channel.
//!
//! Windows runs it under SCM as LocalSystem. Linux runs it as the
//! `taskman-core.service` system unit, whose `ExecStart` passes `--run`.

#[cfg(target_os = "linux")]
fn main() {
    linux_service::main();
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn main() {
    eprintln!("taskman-service is only supported on Windows and Linux");
}

#[cfg(target_os = "windows")]
fn main() -> windows_service::Result<()> {
    if std::env::args().any(|arg| arg == "--selfcheck") {
        return match tm_platform::win::core_service::selfcheck() {
            Ok(summary) => {
                println!("{summary}");
                Ok(())
            }
            Err(error) => {
                eprintln!("taskman-service self-check failed: {error}");
                std::process::exit(1);
            }
        };
    }

    tm_platform::win::prioritize_control_plane();
    tm_platform::win::enable_debug_privilege();
    service::run_dispatcher()
}

#[cfg(target_os = "windows")]
mod service {
    use std::ffi::OsString;
    use std::sync::mpsc;
    use std::time::Duration;

    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
    };

    define_windows_service!(service_main, service_entry);

    pub fn run_dispatcher() -> windows_service::Result<()> {
        windows_service::service_dispatcher::start(
            tm_platform::win::core_service::SERVICE_NAME,
            service_main,
        )
    }

    fn service_entry(_arguments: Vec<OsString>) {
        if let Err(error) = run() {
            tracing::error!(%error, "core service entry failed");
            std::process::exit(1);
        }
    }

    fn run() -> windows_service::Result<()> {
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let handler = move |event| match event {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.try_send(());
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = service_control_handler::register(
            tm_platform::win::core_service::SERVICE_NAME,
            handler,
        )?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StartPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 1,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })?;

        // File logging is a best-effort diagnostic. `prepare_service_log_dir`
        // returns `None` (never an error) when the directory cannot be proven
        // safe, because an unprivileged user can plant an entry under
        // `%ProgramData%\TaskMan\logs` before the first install. That must
        // disable the file appender only, never the privileged broker.
        let protected_log_directory = tm_platform::win::core_service::prepare_service_log_dir()
            .ok()
            .flatten();
        // The guard lives in `tm_core::logging` for the process lifetime.
        if let Some(log_directory) = protected_log_directory.as_ref() {
            tm_core::logging::init_in_dir(
                tm_core::logging::LogConfig::default(),
                log_directory,
                tm_platform::win::core_service::SERVICE_LOG_FILE_PREFIX,
            );
        }
        // A worker-thread panic must not leave SCM reporting a live broker
        // with half its bounded capacity gone. Crash the service process so
        // the configured 5/15/60-second recovery policy starts a clean image.
        // The `tracing::error!` alone never survived: this hook aborts, so the
        // appender's worker thread is never scheduled to drain it. Write the
        // record synchronously first, into the protected log directory.
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(panic = %info, "core service panic; aborting for SCM recovery");
            tm_core::logging::write_crash_record(info);
            std::process::abort();
        }));

        let result = tm_platform::win::core_service::run_broker(stop_rx, || {
            status_handle
                .set_service_status(ServiceStatus {
                    service_type: ServiceType::OWN_PROCESS,
                    current_state: ServiceState::Running,
                    controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                    exit_code: ServiceExitCode::Win32(0),
                    checkpoint: 0,
                    wait_hint: Duration::ZERO,
                    process_id: None,
                })
                .map_err(|error| {
                    tm_core::TmError::platform("report core service readiness", error.to_string())
                })
        });
        let exit_code = match &result {
            Ok(()) => ServiceExitCode::Win32(0),
            // Distinct codes per failure class: the GUI reads this back
            // through SCM to tell "broken install, run Repair" from an
            // ordinary broker fault. Event 7024 renders both as meaningless
            // Win32 error text, so the code is a machine-readable channel.
            Err(error) => ServiceExitCode::ServiceSpecific(
                tm_platform::win::core_service::service_exit_code(error),
            ),
        };
        if let Err(error) = result {
            tracing::error!(%error, "core service stopped with an error");
        }

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------- Linux arm

#[cfg(target_os = "linux")]
mod linux_service {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tm_platform::linux::core_service::server;

    use crate::linux_args;

    /// Where the service writes its daily log. Root-owned on every mainstream
    /// distribution, so the directory below it can be created and verified.
    const SERVICE_LOG_DIRECTORY: &str = "/var/log/taskman";
    /// Daily-rolling file prefix, so a service log is recognisable next to the
    /// GUI's own logs.
    const SERVICE_LOG_FILE_PREFIX: &str = "taskman-service.log";
    /// Exit status for a malformed command line, distinct from the `1` a broker
    /// failure reports so a unit test or a script can tell them apart.
    const EXIT_USAGE: i32 = 2;

    const HELP: &str = "\
TaskMan privileged core service.

Usage: taskman-service <command>

Commands:
  --run            run the broker in the foreground (the unit's ExecStart)
  --selfcheck      validate the manifest, the install tree and the socket, then
                   print one JSON report; never starts, stops or installs a unit
  --version        print the service version and the wire protocol
  --help           print this help

Development:
  --run --dev      run the broker with the install-root and service-image
                   checks disabled. The packaged unit never passes it, and it
                   still needs root plus the root-owned manifest, because the
                   authorized uids cannot come from anywhere a user can write.

The environment is not read: every path comes from the build or the manifest.";

    pub fn main() {
        let arguments = std::env::args().skip(1).collect::<Vec<_>>();
        match linux_args::plan(&arguments) {
            linux_args::Plan::Run { dev } => run_broker(dev),
            linux_args::Plan::Selfcheck => selfcheck(),
            linux_args::Plan::Version => println!(
                "taskman-service {} (linux core service, protocol {})",
                env!("CARGO_PKG_VERSION"),
                tm_platform::linux_core_service::PROTOCOL_VERSION
            ),
            linux_args::Plan::Help => print!("{}", help()),
            linux_args::Plan::Usage(problem) => {
                eprintln!("taskman-service: {problem}");
                eprintln!();
                eprint!("{}", help());
                std::process::exit(EXIT_USAGE);
            }
        }
    }

    fn help() -> String {
        format!(
            "{HELP}\n\nSocket:   {}\nManifest: {}\n",
            tm_platform::linux_core_service::SOCKET_PATH,
            tm_platform::linux_core_service::MANIFEST_PATH,
        )
    }

    fn run_broker(dev: bool) {
        // File logging is best effort and never gates the broker: a log
        // directory that cannot be proven root-owned disables the appender
        // only, because an unusable directory must not take the privileged
        // control plane down with it.
        if let Some(directory) = prepare_log_directory() {
            tm_core::logging::init_in_dir(
                tm_core::logging::LogConfig::default(),
                &directory,
                SERVICE_LOG_FILE_PREFIX,
            );
        }
        // A worker-thread panic must not leave a privileged service alive with
        // part of its bounded capacity gone. Crash the process so systemd's
        // restart policy starts a clean image. Release builds abort, so the
        // appender is never scheduled again: write the record synchronously
        // first.
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(panic = %info, "core service panic; aborting for systemd recovery");
            tm_core::logging::write_crash_record(info);
            std::process::abort();
        }));

        let result = server::run_service(dev);
        if let Err(error) = &result {
            tracing::error!(%error, "linux core service stopped with an error");
            eprintln!("taskman-service: {error}");
        }
        // Flush the file appender before the process ends.
        tm_core::logging::shutdown();
        if result.is_err() {
            std::process::exit(1);
        }
    }

    /// Validate, then print the report. A failed installation still prints the
    /// whole JSON so the operator sees every check, and exits non-zero.
    fn selfcheck() {
        match tm_platform::linux::core_service::server::selfcheck() {
            Ok(report) => println!("{report}"),
            Err(report) => {
                println!("{report}");
                eprintln!("taskman-service self-check failed: see the \"failures\" entries above");
                std::process::exit(1);
            }
        }
    }

    /// The log directory, or `None` when it cannot be proven safe.
    ///
    /// The broker's own rule decides this: the directory AND every ancestor must
    /// be a real, root-owned directory that nobody but root can write. A
    /// writable parent would let an ordinary user swap whatever the appender
    /// opens later, which no per-file check could detect.
    fn prepare_log_directory() -> Option<PathBuf> {
        let directory = Path::new(SERVICE_LOG_DIRECTORY);
        match std::fs::symlink_metadata(directory) {
            Ok(_) => protected_log_directory(directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // `/var/log` is root-owned on every mainstream distribution, but
                // it is proven rather than assumed: nothing may be created inside
                // a tree the service cannot trust.
                if let Some(problem) = server::protected_directory_chain(directory.parent()?) {
                    tracing::warn!(
                        %problem,
                        "service log directory parent cannot be proven safe; file logging disabled"
                    );
                    return None;
                }
                if std::fs::create_dir(directory).is_err() {
                    return None;
                }
                let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755));
                protected_log_directory(directory)
            }
            Err(_) => None,
        }
    }

    fn protected_log_directory(directory: &Path) -> Option<PathBuf> {
        if let Some(problem) = server::protected_directory_chain(directory) {
            tracing::warn!(
                directory = %directory.display(),
                %problem,
                "service log directory cannot be proven safe; file logging disabled"
            );
            return None;
        }
        Some(directory.to_path_buf())
    }
}

/// Argument contract of the service binary.
///
/// Platform-neutral on purpose: the Windows test host compiles and tests it even
/// though only the Linux binary dispatches on it, so the command line cannot
/// drift away from the unit file unobserved.
#[cfg(any(target_os = "linux", test))]
mod linux_args {
    pub const RUN_ARGUMENT: &str = "--run";
    pub const DEV_ARGUMENT: &str = "--dev";
    const SELFCHECK_ARGUMENT: &str = "--selfcheck";
    const VERSION_ARGUMENT: &str = "--version";
    const HELP_ARGUMENT: &str = "--help";

    /// What one command line asks the service to do.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Plan {
        /// Run the broker in the foreground; `dev` is the explicit `--dev`
        /// escape hatch, which the packaged unit never passes.
        Run {
            dev: bool,
        },
        /// Validate the installation and print one JSON report.
        Selfcheck,
        Version,
        Help,
        /// Unknown or malformed arguments; the message names the problem.
        Usage(String),
    }

    /// Parse the argv tail (without the program name).
    ///
    /// A bare invocation is a usage error: the unit always passes `--run`, so a
    /// silent start would hide a broken `ExecStart`.
    pub fn plan(arguments: &[String]) -> Plan {
        match arguments {
            [] => Plan::Usage("no arguments given".into()),
            [single] => match single.as_str() {
                RUN_ARGUMENT => Plan::Run { dev: false },
                SELFCHECK_ARGUMENT => Plan::Selfcheck,
                VERSION_ARGUMENT => Plan::Version,
                HELP_ARGUMENT => Plan::Help,
                other => Plan::Usage(format!("unknown argument {other:?}")),
            },
            [first, second]
                if first.as_str() == RUN_ARGUMENT && second.as_str() == DEV_ARGUMENT =>
            {
                Plan::Run { dev: true }
            }
            _ => Plan::Usage(
                "only --run, --run --dev, --selfcheck, --version and --help are accepted".into(),
            ),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn args(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| (*value).to_string()).collect()
        }

        #[test]
        fn the_unit_command_line_runs_the_broker() {
            assert_eq!(plan(&args(&["--run"])), Plan::Run { dev: false });
            assert_eq!(plan(&args(&["--run", "--dev"])), Plan::Run { dev: true });
        }

        #[test]
        fn the_diagnostic_switches_are_exact() {
            assert_eq!(plan(&args(&["--selfcheck"])), Plan::Selfcheck);
            assert_eq!(plan(&args(&["--version"])), Plan::Version);
            assert_eq!(plan(&args(&["--help"])), Plan::Help);
        }

        #[test]
        fn anything_else_is_a_usage_error() {
            for arguments in [
                args(&[]),
                args(&["--dev"]),
                args(&["--run", "--run"]),
                args(&["--run", "extra"]),
                args(&["--selfcheck", "--dev"]),
                args(&["--unknown"]),
                args(&["run"]),
            ] {
                match plan(&arguments) {
                    // The message is what a broken ExecStart prints, so it must name
                    // something rather than being empty.
                    Plan::Usage(problem) => {
                        assert!(!problem.is_empty(), "{arguments:?} had no message");
                    }
                    other => panic!("{arguments:?} was accepted: {other:?}"),
                }
            }
        }
    }
}
