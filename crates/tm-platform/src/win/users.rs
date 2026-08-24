//! User sessions via WTS (Windows Terminal Services APIs).

use tm_core::error::{Result, TmError};
use tm_core::model::{UserSession, UserSessionState};
use windows::Win32::System::RemoteDesktop::{
    WTSDisconnectSession, WTSEnumerateSessionsW, WTSFreeMemory, WTSLogoffSession,
    WTSQuerySessionInformationW, WTS_SESSION_INFOW, WTS_INFO_CLASS, WTSUserName, WTSDomainName,
};

pub fn list_sessions() -> Result<Vec<UserSession>> {
    let mut out = Vec::new();
    unsafe {
        let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count: u32 = 0;
        WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count)
            .map_err(|e| TmError::platform("WTSEnumerateSessionsW", e.to_string()))?;

        let slice = std::slice::from_raw_parts(sessions, count as usize);
        for s in slice {
            let user = query_string(s.SessionId, WTSUserName);
            let domain = query_string(s.SessionId, WTSDomainName);
            out.push(UserSession {
                id: s.SessionId,
                user: if user.is_empty() { format!("(session {})", s.SessionId) } else { user },
                domain: (!domain.is_empty()).then_some(domain),
                state: map_state(s.State.0),
                logon_epoch_s: None,
                cpu_pct: 0.0,
                mem_bytes: 0,
                process_count: 0,
            });
        }
        WTSFreeMemory(sessions.cast());
    }
    Ok(out)
}

unsafe fn query_string(session_id: u32, class: WTS_INFO_CLASS) -> String {
    unsafe {
    let mut buf = windows::core::PWSTR::null();
    let mut len: u32 = 0;
    if WTSQuerySessionInformationW(None, session_id, class, &mut buf, &mut len).is_ok() {
        let s = if buf.0.is_null() {
            String::new()
        } else {
            buf.to_string().unwrap_or_default()
        };
        WTSFreeMemory(buf.0.cast());
        s
    } else {
        String::new()
    }
    }
}

pub fn control_session(session_id: u32, action: crate::actions::UserSessionAction) -> Result<()> {
    use crate::actions::UserSessionAction::*;
    unsafe {
        match action {
            Disconnect => WTSDisconnectSession(None, session_id, false)
                .map_err(|e| TmError::platform("WTSDisconnectSession", e.to_string())),
            Logoff => WTSLogoffSession(None, session_id, false)
                .map_err(|e| TmError::platform("WTSLogoffSession", e.to_string())),
        }
    }
}

fn map_state(v: i32) -> UserSessionState {
    use windows::Win32::System::RemoteDesktop::*;
    match v {
        v if v == WTSActive.0 => UserSessionState::Active,
        v if v == WTSConnected.0 => UserSessionState::Connected,
        v if v == WTSDisconnected.0 => UserSessionState::Disconnected,
        v if v == WTSIdle.0 => UserSessionState::Idle,
        v if v == WTSReset.0 => UserSessionState::Reset,
        v if v == WTSDown.0 => UserSessionState::Down,
        v if v == WTSInit.0 => UserSessionState::Init,
        _ => UserSessionState::Unknown,
    }
}
