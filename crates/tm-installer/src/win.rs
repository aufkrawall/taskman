//! Direct Win32 plumbing used by the setup wizard.
//!
//! This module is the only place in the installer that talks to the OS
//! directly. Three rules shape it:
//!
//! * No policy lives here: [`crate::install`] decides WHAT happens; this
//!   module only knows HOW to poke Windows for one small thing at a time.
//! * The hardened service install/uninstall lifecycle is NOT reimplemented
//!   here. It runs through the app's own elevated `--core-service=install` /
//!   `--core-service=uninstall` helper (see `tm-platform::win::core_service`),
//!   so the pinned-copy, ACL and SCM logic keeps a single owner.
//! * Every wait is an event-driven handle wait with a deadline - never a
//!   sleep-and-poll bandaid.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tm_core::error::{Result, TmError};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_Programs,
};
use windows::core::{PCWSTR, PWSTR};

const GUI_EXE_NAME: &str = "taskman.exe";

fn err(context: &'static str, detail: impl Into<String>) -> TmError {
    TmError::platform(context, detail)
}

/// NUL-terminated UTF-16 for Win32 `PCWSTR` parameters.
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_wide(raw: &[u16]) -> String {
    let end = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

/// Case-folded absolute path text, the key used to compare image paths.
fn normalize(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

// ---------------------------------------------------------------------------
// Known folders
// ---------------------------------------------------------------------------

fn known_folder(folder: &windows::core::GUID) -> Result<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
    unsafe {
        let raw = SHGetKnownFolderPath(folder, KNOWN_FOLDER_FLAG(0), None)
            .map_err(|error| err("known folder", error.to_string()))?;
        let text = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0.cast()));
        if text.is_empty() {
            return Err(err("known folder", "Windows returned an empty path"));
        }
        Ok(PathBuf::from(text))
    }
}

/// The protected install location. MUST stay identical to
/// `tm_platform::win::core_service`'s `install_dir` - the service install
/// helper copies exactly here, and the broker pins the installed GUI path.
pub fn install_dir() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("TaskMan"))
}

/// `%ProgramData%\TaskMan`, owned by the service install lifecycle.
pub fn service_data_dir() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramData)?.join("TaskMan"))
}

/// Per-user Start menu `\Programs` folder for the shortcut.
pub fn start_menu_dir() -> Result<PathBuf> {
    known_folder(&FOLDERID_Programs)
}

/// Per-user Desktop folder for the optional shortcut.
pub fn desktop_dir() -> Result<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

// ---------------------------------------------------------------------------
// Elevation / identity
// ---------------------------------------------------------------------------

pub fn is_elevated() -> bool {
    tm_platform::win::is_elevated()
}

/// The SID of the account running setup. This is the account the broker will
/// authorize (`--core-service-user=`): UAC elevation keeps the same user, so
/// the elevated setup token's user IS the administrator who double-clicked.
pub fn current_user_sid() -> Result<String> {
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|error| err("open process token", error.to_string()))?;
        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        if size == 0 {
            let _ = CloseHandle(token);
            return Err(err("token user", "cannot size the token user buffer"));
        }
        let mut buffer = vec![0u8; size as usize];
        let filled = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        );
        let _ = CloseHandle(token);
        filled.map_err(|error| err("token user", error.to_string()))?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)
            .map_err(|error| err("token user SID", error.to_string()))?;
        let sid = text.to_string().unwrap_or_default();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        if sid.is_empty() {
            return Err(err("token user SID", "Windows returned an empty SID"));
        }
        Ok(sid)
    }
}

// ---------------------------------------------------------------------------
// Stopping a running Task Manager
// ---------------------------------------------------------------------------

fn process_image_path(pid: u32) -> Option<PathBuf> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 32_768];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some(PathBuf::from(from_wide(&buffer[..size as usize])))
    }
}

fn pids_for_image(wanted: &str) -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut pids = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name = from_wide(&entry.szExeFile);
                if name.eq_ignore_ascii_case(GUI_EXE_NAME)
                    && let Some(path) = process_image_path(entry.th32ProcessID)
                    && normalize(&path) == wanted
                {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        pids
    }
}

/// Close windows belonging to `pid` so a cooperative exit can happen first.
fn post_close_to_windows(pid: u32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindow, PostMessageW, WM_CLOSE,
    };
    unsafe extern "system" fn enum_proc(
        hwnd: windows::Win32::Foundation::HWND,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::core::BOOL {
        unsafe {
            let target = lparam.0 as u32;
            let mut owner = 0u32;
            let _ = GetWindowThreadProcessId(hwnd, Some(&mut owner));
            if owner == target && IsWindow(Some(hwnd)).as_bool() {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_CLOSE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
        windows::core::BOOL(1)
    }
    let _ = unsafe {
        EnumWindows(
            Some(enum_proc),
            windows::Win32::Foundation::LPARAM(pid as isize),
        )
    };
}

/// Stop every running `taskman.exe` whose image path is exactly `image`.
///
/// First the main windows are asked to close (event-driven wait on the
/// process handle); anything still alive after the deadline is terminated.
/// Returns how many processes were stopped.
pub fn stop_running_gui(image: &Path) -> Result<usize> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };
    let wanted = normalize(image);
    let mut stopped = 0usize;
    for pid in pids_for_image(&wanted) {
        let Ok(handle) =
            (unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) })
        else {
            continue;
        };
        post_close_to_windows(pid);
        // Give the window a chance to close on its own; a task manager has no
        // unsaved state, so the deadline below is a UX courtesy, not a race.
        let mut exited = unsafe { WaitForSingleObject(handle, 3000) } == WAIT_OBJECT_0;
        if !exited {
            let _ = unsafe { TerminateProcess(handle, 1) };
            exited = unsafe { WaitForSingleObject(handle, 5000) } == WAIT_OBJECT_0;
        }
        let _ = unsafe { CloseHandle(handle) };
        if exited {
            stopped += 1;
        } else {
            return Err(err(
                "stop Task Manager",
                format!("process {pid} survived both close and terminate"),
            ));
        }
    }
    Ok(stopped)
}

// ---------------------------------------------------------------------------
// Shortcuts (COM IShellLink)
// ---------------------------------------------------------------------------

/// Create a `.lnk` shortcut. COM is initialized per call because the wizard
/// runs install work off the UI thread.
pub fn create_shortcut(
    lnk: &Path,
    target: &Path,
    working_dir: &Path,
    icon: &Path,
    description: &str,
) -> Result<()> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::Interface;

    unsafe {
        // RPC_E_CHANGED_MODE means this thread already runs in another COM
        // model; the calls below do not care which one it is.
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let owned = init.is_ok();

        let result = (|| -> Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| err("create shortcut", error.to_string()))?;
            let target_w = wide(target);
            let work_w = wide(working_dir);
            let icon_w = wide(icon);
            let desc_w = wide(description);
            let lnk_w = wide(lnk);
            link.SetPath(PCWSTR(target_w.as_ptr()))
                .map_err(|error| err("shortcut target", error.to_string()))?;
            link.SetWorkingDirectory(PCWSTR(work_w.as_ptr()))
                .map_err(|error| err("shortcut workdir", error.to_string()))?;
            link.SetIconLocation(PCWSTR(icon_w.as_ptr()), 0)
                .map_err(|error| err("shortcut icon", error.to_string()))?;
            link.SetDescription(PCWSTR(desc_w.as_ptr()))
                .map_err(|error| err("shortcut description", error.to_string()))?;
            let persist: IPersistFile = link
                .cast()
                .map_err(|error| err("shortcut persistence", error.to_string()))?;
            persist
                .Save(PCWSTR(lnk_w.as_ptr()), true)
                .map_err(|error| err("write shortcut", error.to_string()))?;
            Ok(())
        })();

        if owned {
            CoUninitialize();
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Add/Remove Programs registration
// ---------------------------------------------------------------------------

const ARP_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\TaskMan";

/// Register the uninstall entry shown in Windows Settings > Apps.
pub fn write_arp(install_dir: &Path, version: &str, estimated_size_kb: u32) -> Result<()> {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW,
    };

    fn set_sz(key: windows::Win32::System::Registry::HKEY, name: &str, value: &str) -> Result<()> {
        use windows::Win32::System::Registry::RegSetValueExW;
        let name_w = wide(name);
        let mut data = Vec::new();
        for unit in value.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data.extend_from_slice(&[0, 0]);
        let status =
            unsafe { RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, REG_SZ, Some(&data)) };
        if status.is_ok() {
            Ok(())
        } else {
            Err(err(
                "write uninstall registry value",
                format!("{} failed: win32 error {}", name, status.0),
            ))
        }
    }

    fn set_dword(
        key: windows::Win32::System::Registry::HKEY,
        name: &str,
        value: u32,
    ) -> Result<()> {
        use windows::Win32::System::Registry::RegSetValueExW;
        let name_w = wide(name);
        let status = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(name_w.as_ptr()),
                None,
                REG_DWORD,
                Some(&value.to_le_bytes()),
            )
        };
        if status.is_ok() {
            Ok(())
        } else {
            Err(err(
                "write uninstall registry value",
                format!("{} failed: win32 error {}", name, status.0),
            ))
        }
    }

    let gui = install_dir.join(GUI_EXE_NAME);
    let setup = install_dir.join("taskman-setup.exe");
    let mut key = windows::Win32::System::Registry::HKEY::default();
    let subkey_w = wide(ARP_SUBKEY);
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if !status.is_ok() {
        return Err(err(
            "create uninstall registry key",
            format!("win32 error {}", status.0),
        ));
    }

    let result = (|| -> Result<()> {
        set_sz(key, "DisplayName", "Task Manager")?;
        set_sz(key, "DisplayVersion", version)?;
        set_sz(key, "Publisher", "aufkrawall")?;
        set_sz(key, "DisplayIcon", &gui.to_string_lossy())?;
        set_sz(key, "InstallLocation", &install_dir.to_string_lossy())?;
        set_sz(
            key,
            "UninstallString",
            &format!("\"{}\" --uninstall", setup.to_string_lossy()),
        )?;
        set_sz(
            key,
            "QuietUninstallString",
            &format!("\"{}\" --uninstall /S", setup.to_string_lossy()),
        )?;
        set_dword(key, "NoModify", 1)?;
        set_dword(key, "NoRepair", 1)?;
        set_dword(key, "EstimatedSize", estimated_size_kb)?;
        Ok(())
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// Remove the Add/Remove Programs entry. Missing key is success.
pub fn remove_arp() -> Result<()> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RegDeleteTreeW};
    let subkey_w = wide(ARP_SUBKEY);
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(subkey_w.as_ptr())) };
    // 2 = ERROR_FILE_NOT_FOUND: nothing to remove is not a failure.
    if status.is_ok() || status.0 == 2 {
        Ok(())
    } else {
        Err(err(
            "remove uninstall registry key",
            format!("win32 error {}", status.0),
        ))
    }
}

// ---------------------------------------------------------------------------
// Service helper + fallbacks
// ---------------------------------------------------------------------------

/// Run the app's elevated core-service helper (`taskman.exe
/// --core-service=<install|uninstall> --core-service-user=<sid>`).
///
/// This is the ONLY supported way to create or remove the LocalSystem
/// service: the helper owns the pinned-copy, ACL, manifest and SCM logic.
pub fn run_core_service_helper(taskman_exe: &Path, operation: &str, sid: &str) -> Result<()> {
    let output = std::process::Command::new(taskman_exe)
        .arg(format!("--core-service={operation}"))
        .arg(format!("--core-service-user={sid}"))
        .output()
        .map_err(|error| err("run core service helper", error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        return Err(err(
            "core service helper",
            format!("taskman --core-service={operation} failed: {detail}"),
        ));
    }
    Ok(())
}

/// Last-resort service removal for a broken install whose `taskman.exe` is
/// already gone. The helper path above is preferred and used whenever the
/// installed GUI still exists.
pub fn delete_service_fallback() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let name = tm_platform::win::core_service::SERVICE_NAME;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| err("open service manager", error.to_string()))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(name, access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(()); // not installed
        }
        Err(error) => return Err(err("open core service", error.to_string())),
    };
    if let Ok(status) = service.query_status()
        && !matches!(
            status.current_state,
            ServiceState::Stopped | ServiceState::StopPending
        )
    {
        let _ = service.stop();
    }
    service
        .delete()
        .map_err(|error| err("delete core service", error.to_string()))
}

// ---------------------------------------------------------------------------
// Filesystem endgame for uninstall
// ---------------------------------------------------------------------------

fn is_within(path: &Path, tree: &Path) -> bool {
    normalize(path).starts_with(&normalize(tree))
}

/// Move a file that would otherwise block deletion of `tree`.
///
/// A running executable can be renamed but not deleted, so the uninstaller
/// moves itself out of the install tree first and schedules its own removal
/// at the next reboot afterwards.
pub fn relocate_self_out_of(tree: &Path) -> Result<Option<PathBuf>> {
    let current = std::env::current_exe()
        .map_err(|error| err("locate setup executable", error.to_string()))?;
    if !is_within(&current, tree) {
        return Ok(None);
    }
    let name = format!("taskman-setup-orphan-{}.exe", std::process::id());
    // Prefer %TEMP%; if that is another volume the rename fails and the
    // same-volume sibling is used instead.
    let mut candidates = vec![std::env::temp_dir().join(&name)];
    if let Some(parent) = tree.parent() {
        candidates.push(parent.join(&name));
    }
    for dest in candidates {
        if std::fs::rename(&current, &dest).is_ok() {
            return Ok(Some(dest));
        }
    }
    Err(err(
        "relocate setup executable",
        "cannot move the running setup out of the install directory",
    ))
}

/// Schedule `path` for deletion at reboot (best-effort cleanup of the
/// relocated uninstaller). Requires elevation, which setup has.
pub fn schedule_delete_at_reboot(path: &Path) -> Result<()> {
    use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
    let from = wide(path);
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR::null(),
            MOVEFILE_DELAY_UNTIL_REBOOT,
        )
        .map_err(|error| err("schedule setup cleanup", error.to_string()))
    }
}

/// Byte size of a directory tree (for the ARP `EstimatedSize` value).
pub fn dir_size_kb(dir: &Path) -> u32 {
    fn walk(path: &Path) -> u64 {
        let Ok(meta) = std::fs::metadata(path) else {
            return 0;
        };
        if meta.is_file() {
            return meta.len();
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| walk(&e.path()))
            .sum()
    }
    (walk(dir) / 1024).min(u32::MAX as u64) as u32
}

/// Start the installed Task Manager after a successful install.
pub fn launch(detached_exe: &Path) -> Result<()> {
    std::process::Command::new(detached_exe)
        .spawn()
        .map_err(|error| err("launch Task Manager", error.to_string()))?;
    Ok(())
}

/// Attach to the parent console so `--help`/`--dry-run` output is visible even
/// though release setup builds hide their console window. Copied from the app.
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Append one line to the setup log. Diagnostics only - like the service's
/// file logging, this must never gate the install.
///
/// Prefers `%ProgramData%\TaskMan\logs\setup.log` (the service's log
/// directory); falls back to `%TEMP%\taskman-setup.log`.
pub fn append_setup_log(line: &str) {
    use std::io::Write;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let candidates = [
        service_data_dir()
            .ok()
            .map(|dir| dir.join("logs").join("setup.log")),
        Some(std::env::temp_dir().join("taskman-setup.log")),
    ];
    for path in candidates.into_iter().flatten() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "[{stamp}] {line}");
            return;
        }
    }
}
