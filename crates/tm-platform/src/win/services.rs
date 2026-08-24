//! Windows services via the Service Control Manager.

use tm_core::error::{Result, TmError};
use tm_core::model::*;
use windows::core::PCWSTR;
use windows::Win32::System::Services as scm;

pub fn list_services() -> Result<Vec<ServiceInfo>> {
    let started = std::time::Instant::now();
    unsafe {
        let mgr = open_mgr()?;

        // Size probe.
        let mut needed: u32 = 0;
        let mut returned: u32 = 0;
        let _ = scm::EnumServicesStatusExW(
            mgr,
            scm::SC_ENUM_PROCESS_INFO,
            scm::SERVICE_WIN32,
            scm::SERVICE_STATE_ALL,
            None,
            &mut needed,
            &mut returned,
            None,
            PCWSTR::null(),
        );
        if needed == 0 {
            let _ = scm::CloseServiceHandle(mgr);
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; needed as usize];
        let result = scm::EnumServicesStatusExW(
            mgr,
            scm::SC_ENUM_PROCESS_INFO,
            scm::SERVICE_WIN32,
            scm::SERVICE_STATE_ALL,
            Some(&mut buf),
            &mut needed,
            &mut returned,
            None,
            PCWSTR::null(),
        );

        let mut out = Vec::new();
        if result.is_ok() && returned > 0 {
            let items = std::slice::from_raw_parts(
                buf.as_ptr() as *const scm::ENUM_SERVICE_STATUS_PROCESSW,
                returned as usize,
            );
            for it in items {
                let name = pwstr_copy(it.lpServiceName);
                let display = pwstr_copy(it.lpDisplayName);
                let status = map_status(it.ServiceStatusProcess.dwCurrentState.0);
                let pid_raw = it.ServiceStatusProcess.dwProcessId;
                out.push(ServiceInfo {
                    name,
                    display_name: display,
                    description: String::new(),
                    pid: (pid_raw != 0).then_some(pid_raw),
                    status,
                    group: String::new(),
                    startup_type: String::new(),
                    account: String::new(),
                });
            }
        }
        let _ = scm::CloseServiceHandle(mgr);

        tracing::debug!(
            count = out.len(),
            ms = started.elapsed().as_millis() as u64,
            "services enumerated"
        );
        enrich(&out)
    }
}

fn open_mgr() -> Result<scm::SC_HANDLE> {
    unsafe {
        scm::OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), scm::SC_MANAGER_ENUMERATE_SERVICE)
            .map_err(|e| TmError::platform("OpenSCManagerW", e.to_string()))
    }
}

/// Fill description/startup-type/group/account. Failures degrade to "".
fn enrich(services: &[ServiceInfo]) -> Result<Vec<ServiceInfo>> {
    let mut out = Vec::with_capacity(services.len());
    unsafe {
        let mgr = open_mgr()?;
        for s in services {
            let mut info = s.clone();
            let name_w: Vec<u16> = s.name.encode_utf16().chain([0]).collect();
            if let Ok(h) =
                scm::OpenServiceW(mgr, PCWSTR::from_raw(name_w.as_ptr()), scm::SERVICE_QUERY_CONFIG)
            {
                let mut needed: u32 = 0;
                let _ = scm::QueryServiceConfigW(h, None, 0, &mut needed);
                if needed > 0 {
                    let mut buf = vec![0u8; needed as usize];
                    if scm::QueryServiceConfigW(
                        h,
                        Some(buf.as_mut_ptr() as *mut scm::QUERY_SERVICE_CONFIGW),
                        buf.len() as u32,
                        &mut needed,
                    )
                    .is_ok()
                    {
                        let cfg = &*(buf.as_ptr() as *const scm::QUERY_SERVICE_CONFIGW);
                        info.startup_type = start_type_label(cfg.dwStartType.0).into();
                        info.group = pwstr_copy(cfg.lpLoadOrderGroup);
                        info.account = pwstr_copy(cfg.lpServiceStartName);
                    }
                }

                let mut needed: u32 = 0;
                let _ = scm::QueryServiceConfig2W(
                    h,
                    scm::SERVICE_CONFIG_DESCRIPTION,
                    None,
                    &mut needed,
                );
                if needed > 0 {
                    let mut buf = vec![0u8; needed as usize];
                    if scm::QueryServiceConfig2W(
                        h,
                        scm::SERVICE_CONFIG_DESCRIPTION,
                        Some(&mut buf),
                        &mut needed,
                    )
                    .is_ok()
                    {
                        let desc = &*(buf.as_ptr() as *const scm::SERVICE_DESCRIPTIONW);
                        info.description = pwstr_copy(desc.lpDescription);
                    }
                }
                let _ = scm::CloseServiceHandle(h);
            }
            out.push(info);
        }
        let _ = scm::CloseServiceHandle(mgr);
    }
    Ok(out)
}

fn start_type_label(t: u32) -> &'static str {
    let auto = scm::SERVICE_AUTO_START.0;
    let demand = scm::SERVICE_DEMAND_START.0;
    let disabled = scm::SERVICE_DISABLED.0;
    let boot = scm::SERVICE_BOOT_START.0;
    let system = scm::SERVICE_SYSTEM_START.0;
    if t == auto {
        "Automatic"
    } else if t == demand {
        "Manual"
    } else if t == disabled {
        "Disabled"
    } else if t == boot {
        "Boot"
    } else if t == system {
        "System"
    } else {
        ""
    }
}

fn map_status(state: u32) -> ServiceStatus {
    let stopped = scm::SERVICE_STOPPED.0;
    let start_p = scm::SERVICE_START_PENDING.0;
    let stop_p = scm::SERVICE_STOP_PENDING.0;
    let running = scm::SERVICE_RUNNING.0;
    let cont_p = scm::SERVICE_CONTINUE_PENDING.0;
    let pause_p = scm::SERVICE_PAUSE_PENDING.0;
    let paused = scm::SERVICE_PAUSED.0;
    if state == stopped {
        ServiceStatus::Stopped
    } else if state == start_p {
        ServiceStatus::StartPending
    } else if state == stop_p {
        ServiceStatus::StopPending
    } else if state == running {
        ServiceStatus::Running
    } else if state == cont_p {
        ServiceStatus::ContinuePending
    } else if state == pause_p {
        ServiceStatus::PausePending
    } else if state == paused {
        ServiceStatus::Paused
    } else {
        ServiceStatus::Unknown
    }
}

pub fn control_service(name: &str, action: crate::actions::ServiceAction) -> Result<()> {
    use crate::actions::ServiceAction::*;

    let name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe {
        let mgr = open_mgr()?;
        let access = match action {
            Start => scm::SERVICE_START | scm::SERVICE_QUERY_STATUS,
            Stop => scm::SERVICE_STOP | scm::SERVICE_QUERY_STATUS,
            Restart => scm::SERVICE_START | scm::SERVICE_STOP | scm::SERVICE_QUERY_STATUS,
        };
        let svc = scm::OpenServiceW(mgr, PCWSTR::from_raw(name_w.as_ptr()), access)
            .map_err(|e| TmError::platform("OpenServiceW", e.to_string()));

        let svc = match svc {
            Ok(h) => h,
            Err(e) => {
                let _ = scm::CloseServiceHandle(mgr);
                return Err(e);
            }
        };

        let outcome = (|| -> Result<()> {
            match action {
                Start => {
                    scm::StartServiceW(svc, None).map_err(wrap("StartServiceW"))?;
                    wait_state(svc, &[scm::SERVICE_RUNNING.0], 10)?;
                }
                Stop => {
                    scm::ControlService(svc, scm::SERVICE_CONTROL_STOP, std::ptr::null_mut())
                        .map_err(wrap("ControlService(stop)"))?;
                    wait_state(svc, &[scm::SERVICE_STOPPED.0], 15)?;
                }
                Restart => {
                    let _ = scm::ControlService(svc, scm::SERVICE_CONTROL_STOP, std::ptr::null_mut());
                    wait_state(svc, &[scm::SERVICE_STOPPED.0], 15)?;
                    scm::StartServiceW(svc, None).map_err(wrap("StartServiceW(restart)"))?;
                    wait_state(svc, &[scm::SERVICE_RUNNING.0], 10)?;
                }
            }
            Ok(())
        })();

        let _ = scm::CloseServiceHandle(svc);
        let _ = scm::CloseServiceHandle(mgr);
        outcome
    }
}

fn wrap(ctx: &'static str) -> impl Fn(windows::core::Error) -> TmError {
    move |e| TmError::platform(ctx, format!("{e}"))
}

/// Poll service state until it reaches one of `wanted` or timeout.
unsafe fn wait_state(
    svc: scm::SC_HANDLE,
    wanted: &[u32],
    timeout_s: u64,
) -> Result<()> {
    use std::time::{Duration, Instant};

    unsafe {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        let mut buf = [0u8; std::mem::size_of::<scm::SERVICE_STATUS_PROCESS>()];
        let mut needed: u32 = 0;
        if scm::QueryServiceStatusEx(svc, scm::SC_STATUS_PROCESS_INFO, Some(&mut buf), &mut needed)
            .is_ok()
        {
            let status = &*(buf.as_ptr() as *const scm::SERVICE_STATUS_PROCESS);
            if wanted.contains(&(status.dwCurrentState.0)) {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        if Instant::now() > deadline {
            return Err(TmError::platform("service wait", "timeout"));
        }
    }
    }
}

fn pwstr_copy(p: windows::core::PWSTR) -> String {
    if p.0.is_null() {
        return String::new();
    }
    unsafe { p.to_string().unwrap_or_default() }
}
