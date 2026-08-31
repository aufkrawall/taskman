//! TaskMan's Windows core service.
//!
//! The service owns only allowlisted privileged control operations. System
//! sampling and all GUI state intentionally remain in the interactive user's
//! process so a LocalSystem snapshot cannot become a cross-user data channel.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("taskman-service is only supported on Windows");
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

        let protected_log_directory = tm_platform::win::core_service::prepare_service_log_dir();
        let _log_guard = protected_log_directory
            .as_ref()
            .ok()
            .and_then(|log_directory| {
                tm_core::logging::init_in_dir(
                    tm_core::logging::LogConfig::default(),
                    log_directory,
                    tm_platform::win::core_service::SERVICE_LOG_FILE_PREFIX,
                )
            });
        // A worker-thread panic must not leave SCM reporting a live broker
        // with half its bounded capacity gone. Crash the service process so
        // the configured 5/15/60-second recovery policy starts a clean image.
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(panic = %info, "core service panic; aborting for SCM recovery");
            std::process::abort();
        }));

        let result = protected_log_directory.and_then(|_| {
            tm_platform::win::core_service::run_broker(stop_rx, || {
                status_handle
                    .set_service_status(ServiceStatus {
                        service_type: ServiceType::OWN_PROCESS,
                        current_state: ServiceState::Running,
                        controls_accepted: ServiceControlAccept::STOP
                            | ServiceControlAccept::SHUTDOWN,
                        exit_code: ServiceExitCode::Win32(0),
                        checkpoint: 0,
                        wait_hint: Duration::ZERO,
                        process_id: None,
                    })
                    .map_err(|error| {
                        tm_core::TmError::platform(
                            "report core service readiness",
                            error.to_string(),
                        )
                    })
            })
        });
        let exit_code = if result.is_ok() {
            ServiceExitCode::Win32(0)
        } else {
            ServiceExitCode::ServiceSpecific(1)
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
