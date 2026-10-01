//! Desktop identity kept separate from setup's elevated administrator token.

use std::path::{Path, PathBuf};

use tm_core::error::{Result, TmError};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_IMPERSONATE, TOKEN_QUERY, TokenElevation, TokenPrimary,
};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessWithTokenW, GetCurrentProcess, LOGON_WITH_PROFILE,
    OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::{PCWSTR, PWSTR};

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Interactive setup targets the shell's user, including over-the-shoulder
/// UAC. Without a desktop (deployment), the calling account owns the install.
pub struct InstallUser {
    token: Token,
}

fn error(context: &'static str, error: impl std::fmt::Display) -> TmError {
    TmError::platform(context, error.to_string())
}

impl InstallUser {
    pub fn resolve() -> Result<Self> {
        unsafe {
            let shell = GetShellWindow();
            let mut token = HANDLE::default();
            if shell.0.is_null() {
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                    .map_err(|e| error("setup user token", e))?;
            } else {
                // Credential-based UAC can put setup and the shell under
                // different accounts. This only adjusts setup's own token.
                tm_platform::win::enable_debug_privilege();
                let mut pid = 0;
                GetWindowThreadProcessId(shell, Some(&mut pid));
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                    .map_err(|e| error("desktop user process", e))?;
                let result = OpenProcessToken(
                    process,
                    TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
                    &mut token,
                );
                let _ = CloseHandle(process);
                result.map_err(|e| error("desktop user token", e))?;
            }
            Ok(Self {
                token: Token(token),
            })
        }
    }

    pub fn sid(&self) -> Result<String> {
        crate::win::sid_for_token(self.token.0)
    }

    pub fn start_menu_dir(&self) -> Result<PathBuf> {
        crate::win::known_folder_for_user(
            &windows::Win32::UI::Shell::FOLDERID_Programs,
            Some(self.token.0),
        )
    }

    pub fn desktop_dir(&self) -> Result<PathBuf> {
        crate::win::known_folder_for_user(
            &windows::Win32::UI::Shell::FOLDERID_Desktop,
            Some(self.token.0),
        )
    }

    pub fn ensure_launchable(&self) -> Result<()> {
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        unsafe {
            GetTokenInformation(
                self.token.0,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
            .map_err(|e| error("desktop token elevation", e))?;
        }
        require_unelevated(elevation.TokenIsElevated)
    }

    /// Launch with the desktop token and its environment, never with setup's
    /// administrator token or administrator profile environment.
    pub fn launch(&self, exe: &Path) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::System::Environment::{
            CreateEnvironmentBlock, DestroyEnvironmentBlock,
        };

        self.ensure_launchable()?;
        let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let work: Vec<u16> = exe
            .parent()
            .unwrap_or(exe)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut command: Vec<u16> = format!("\"{}\"", exe.display())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let mut primary = HANDLE::default();
            DuplicateTokenEx(
                self.token.0,
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
            .map_err(|e| error("duplicate desktop token", e))?;
            let primary = Token(primary);
            let mut environment = std::ptr::null_mut();
            CreateEnvironmentBlock(&mut environment, Some(primary.0), false)
                .map_err(|e| error("desktop environment", e))?;
            let startup = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR::null(),
                ..Default::default()
            };
            let mut process = PROCESS_INFORMATION::default();
            let result = CreateProcessWithTokenW(
                primary.0,
                LOGON_WITH_PROFILE,
                PCWSTR(path.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                CREATE_UNICODE_ENVIRONMENT,
                Some(environment.cast_const()),
                PCWSTR(work.as_ptr()),
                &startup,
                &mut process,
            );
            let _ = DestroyEnvironmentBlock(environment);
            result.map_err(|e| error("launch Task Manager as desktop user", e))?;
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
        Ok(())
    }
}

fn require_unelevated(elevated: u32) -> Result<()> {
    if elevated != 0 {
        Err(error(
            "launch Task Manager",
            "no unelevated desktop token is available; disable post-install launch",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_install_launch_refuses_an_elevated_or_desktopless_admin_token() {
        assert!(require_unelevated(0).is_ok());
        assert!(
            require_unelevated(1)
                .unwrap_err()
                .to_string()
                .contains("disable post-install launch")
        );
    }

    #[test]
    fn desktop_identity_resolves_user_folders_without_machine_writes() {
        let shell = unsafe { GetShellWindow() };
        if shell.0.is_null() {
            return; // headless CI has no desktop user
        }
        let user = InstallUser::resolve().unwrap();
        assert!(user.sid().unwrap().starts_with("S-1-"));
        assert!(user.start_menu_dir().unwrap().is_absolute());
        assert!(user.desktop_dir().unwrap().is_absolute());
        user.ensure_launchable().unwrap();
    }
}
