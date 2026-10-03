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
//! * Setup runs elevated but usually starts from a user-writable folder, so
//!   nothing here trusts a path an unelevated user could reshape: DLL loads
//!   are confined to System32, the running image is read and moved through
//!   one pinned handle ([`SetupImage`]), and the log lives in an
//!   administrator-only folder.

use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tm_core::error::{Result, TmError};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_NAME_OPENED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GETFINALPATHNAMEBYHANDLE_FLAGS, SYNCHRONIZE, VOLUME_NAME_DOS, VOLUME_NAME_NT,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_Programs,
    FOLDERID_Windows,
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
    known_folder_for_user(folder, None)
}

pub(crate) fn known_folder_for_user(
    folder: &windows::core::GUID,
    token: Option<HANDLE>,
) -> Result<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};
    unsafe {
        let raw = SHGetKnownFolderPath(folder, KNOWN_FOLDER_FLAG(0), token)
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
// DLL search order
// ---------------------------------------------------------------------------

/// Restrict every later DLL load of this process to System32.
///
/// Setup runs elevated from a user-writable folder (Downloads), where a
/// planted `uxtheme.dll` or `dwmapi.dll` would otherwise be found before the
/// system copy. `build.rs` limits static imports to KnownDLLs and
/// delay-loads the rest; those resolve only after this call, which is why it
/// must be the first thing `main` does. It also confines DLLs that system
/// components load dynamically on setup's behalf.
pub fn restrict_dll_search_to_system32() -> Result<()> {
    use windows::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
    };
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|error| err("restrict DLL search path", error.to_string()))
}

// ---------------------------------------------------------------------------
// The running setup image
// ---------------------------------------------------------------------------

/// The running `taskman-setup.exe`, pinned once at startup.
///
/// A running image can be renamed, and setup typically starts from a folder
/// its unelevated user controls. Reading the payload or the uninstaller copy
/// by path would let that user rename the image after the UAC prompt and put
/// a different file at its path. Everything is read through this handle
/// instead: it denies write and delete sharing, so while setup runs the file
/// can be neither modified, renamed nor deleted, and it carries `DELETE`
/// access so the uninstaller can move itself out of the install tree by
/// handle.
pub struct SetupImage {
    file: File,
}

static SETUP_IMAGE: OnceLock<std::result::Result<SetupImage, String>> = OnceLock::new();

/// Pin the running image as early as possible (called from `main`). A
/// failure is kept and reported by [`setup_image`] when the image is
/// actually needed, so `--help` does not depend on it.
pub fn pin_setup_image() {
    setup_image_slot();
}

/// The pinned running image, or why it could not be pinned.
pub fn setup_image() -> Result<&'static SetupImage> {
    setup_image_slot()
        .as_ref()
        .map_err(|detail| err("setup image", detail.clone()))
}

fn setup_image_slot() -> &'static std::result::Result<SetupImage, String> {
    SETUP_IMAGE.get_or_init(|| {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let module = unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map_err(|error| format!("cannot locate the setup image: {error}"))?;
        SetupImage::pin_mapped(module.0.cast_const()).map_err(|error| error.to_string())
    })
}

impl SetupImage {
    /// Open the file backing the image mapped at `base` and prove that the
    /// handle refers to exactly that file.
    ///
    /// The file is opened by the name the mapping reports, which follows
    /// renames, and the name is compared again once the handle is pinned: a
    /// rename or substitution racing the open shows up as a mismatch, and
    /// after the pin neither is possible.
    fn pin_mapped(base: *const core::ffi::c_void) -> Result<SetupImage> {
        use std::os::windows::ffi::OsStringExt;
        let mapped = mapped_image_name(base)?;
        let mut path: Vec<u16> = r"\\?\GLOBALROOT".encode_utf16().collect();
        path.extend_from_slice(&mapped);
        let file = std::fs::OpenOptions::new()
            .access_mode((FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE | DELETE).0)
            .share_mode(FILE_SHARE_READ.0)
            .open(std::ffi::OsString::from_wide(&path))
            .map_err(|error| {
                let mapped = String::from_utf16_lossy(&mapped);
                err("pin setup image", format!("{mapped}: {error}"))
            })?;
        verify_mapped_image(&file, base)?;
        Ok(SetupImage { file })
    }

    /// Every byte of the image (setup executable plus appended payload),
    /// read through the pinned handle.
    pub fn read_all(&self) -> Result<Vec<u8>> {
        use std::os::windows::fs::FileExt;
        let len = usize::try_from(self.file.metadata()?.len())
            .map_err(|_| err("read setup image", "image is too large"))?;
        let mut data = vec![0u8; len];
        let mut at = 0usize;
        while at < len {
            // Positional reads: the UI preflight and the install worker may
            // share this handle, so no shared file pointer is involved.
            let read = self.file.seek_read(&mut data[at..], at as u64)?;
            if read == 0 {
                return Err(err("read setup image", "unexpected end of file"));
            }
            at += read;
        }
        Ok(data)
    }

    /// Write the image to `dest`, unless `dest` already IS this file (setup
    /// started from the install directory: copying a file onto itself fails).
    /// Returns whether anything was written.
    pub fn copy_to(&self, dest: &Path) -> Result<bool> {
        if self.is_same_file(dest)? {
            return Ok(false);
        }
        std::fs::write(dest, self.read_all()?)?;
        Ok(true)
    }

    fn is_same_file(&self, other: &Path) -> Result<bool> {
        let other = match std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .open(other)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(file_id(&self.file)? == file_id(&other)?)
    }

    /// Where the image lives now (normalized, drive-letter form).
    pub fn current_path(&self) -> Result<PathBuf> {
        normalized_path(&self.file)
    }

    /// Rename the image by handle. Only this exact file can move, whatever
    /// else appears at either path in the meantime; an existing `dest` is
    /// never replaced.
    pub fn rename_to(&self, dest: &Path) -> Result<()> {
        use windows::Win32::Storage::FileSystem::{
            FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
        };
        let name: Vec<u16> = dest.as_os_str().encode_wide().collect();
        let name_bytes = name.len() * std::mem::size_of::<u16>();
        // The struct already holds one UTF-16 unit, which stays zero as the
        // terminator. A u64 buffer keeps the HANDLE field aligned.
        let size = std::mem::size_of::<FILE_RENAME_INFO>() + name_bytes;
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `buffer` is zeroed (ReplaceIfExists = false, no root
        // directory), 8-byte aligned and large enough for the header plus
        // `name` and its terminator; the file name is written inside it.
        unsafe {
            (*info).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                (&raw mut (*info).FileName).cast::<u16>(),
                name.len(),
            );
            SetFileInformationByHandle(
                HANDLE(self.file.as_raw_handle()),
                FileRenameInfo,
                info.cast_const().cast(),
                size as u32,
            )
        }
        .map_err(|error| {
            err(
                "move setup executable",
                format!("{}: {error}", dest.display()),
            )
        })
    }
}

/// Fail unless `file` is the file backing the image mapped at `base`.
///
/// Both names are the NT names of open file objects, so for the same file
/// they are the same string; the mapping's name follows renames, so a file
/// that merely sits at the image's launch path does not match.
fn verify_mapped_image(file: &File, base: *const core::ffi::c_void) -> Result<()> {
    let opened = final_path(file, OPENED_NT_PATH)?;
    let mapped = mapped_image_name(base)?;
    if opened == mapped {
        Ok(())
    } else {
        Err(err(
            "verify setup image",
            format!(
                "{} is not the running setup image ({}); it was renamed or replaced after launch",
                String::from_utf16_lossy(&opened),
                String::from_utf16_lossy(&mapped)
            ),
        ))
    }
}

/// NT name (`\Device\HarddiskVolumeN\...`) of the file mapped at `base`.
fn mapped_image_name(base: *const core::ffi::c_void) -> Result<Vec<u16>> {
    use windows::Win32::System::ProcessStatus::K32GetMappedFileNameW;
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut buffer = vec![0u16; 32_768];
    let len = unsafe { K32GetMappedFileNameW(GetCurrentProcess(), base, &mut buffer) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(err(
            "setup image name",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    buffer.truncate(len);
    Ok(buffer)
}

/// The NT name a handle was opened with (`\Device\...`), same form as
/// [`mapped_image_name`].
const OPENED_NT_PATH: GETFINALPATHNAMEBYHANDLE_FLAGS =
    GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_OPENED.0 | VOLUME_NAME_NT.0);

/// Where an open file actually is: links and junctions resolved, long names,
/// drive-letter form.
fn normalized_path(file: &File) -> Result<PathBuf> {
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let raw = final_path(file, flags)?;
    Ok(PathBuf::from(strip_verbatim(&String::from_utf16_lossy(
        &raw,
    ))))
}

fn final_path(file: &File, flags: GETFINALPATHNAMEBYHANDLE_FLAGS) -> Result<Vec<u16>> {
    use windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 32_768];
    let len = unsafe { GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut buffer, flags) }
        as usize;
    if len == 0 || len >= buffer.len() {
        return Err(err(
            "file path",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    buffer.truncate(len);
    Ok(buffer)
}

/// Volume serial number + 128-bit file ID: identity independent of names.
fn file_id(file: &File) -> Result<(u64, [u8; 16])> {
    use windows::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut info = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|error| err("file identity", error.to_string()))?;
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` -> `\\srv\share`.
fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

// ---------------------------------------------------------------------------
// Elevation / identity
// ---------------------------------------------------------------------------

pub fn is_elevated() -> bool {
    tm_platform::win::is_elevated()
}

pub(crate) fn sid_for_token(token: HANDLE) -> Result<String> {
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_USER, TokenUser};
    unsafe {
        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        if size == 0 {
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

/// Whether `path` is `tree` or lies below it, compared by whole path
/// components and case-insensitively: a plain string prefix would put
/// `C:\Program Files\TaskManX` inside `C:\Program Files\TaskMan`.
fn is_within(path: &Path, tree: &Path) -> bool {
    let mut path = path.components();
    tree.components().all(|part| {
        path.next()
            .is_some_and(|candidate| normalize(candidate.as_ref()) == normalize(part.as_ref()))
    })
}

/// Move the running setup out of `tree` when it lives inside it.
///
/// A running executable can be renamed but not deleted, so the uninstaller
/// moves itself out of the install tree first and schedules its own removal
/// at the next reboot afterwards. The destination is a GUID-named file next
/// to the tree (Program Files: administrator-only and the same volume), and
/// the move is done by handle, so [`restore_setup_image`] can only ever move
/// this same file back.
pub fn relocate_self_out_of(tree: &Path) -> Result<Option<PathBuf>> {
    use windows::Win32::System::Com::CoCreateGuid;
    let image = setup_image()?;
    if !is_within(&image.current_path()?, tree) {
        return Ok(None);
    }
    let parent = tree.parent().ok_or_else(|| {
        err(
            "relocate setup executable",
            "install directory has no parent",
        )
    })?;
    let guid = unsafe { CoCreateGuid() }
        .map_err(|error| err("relocate setup executable", error.to_string()))?;
    let dest = parent.join(format!(".TaskMan-setup-orphan-{guid:?}.exe"));
    image.rename_to(&dest)?;
    Ok(Some(dest))
}

/// Move the relocated uninstaller back to `to` (uninstall rollback). The move
/// goes through the pinned handle: whatever may have appeared at the
/// relocated path is never touched.
pub fn restore_setup_image(to: &Path) -> Result<()> {
    setup_image()?.rename_to(to)
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

/// Attach to the parent console so `--help`/`--dry-run` output is visible even
/// though release setup builds hide their console window. Copied from the app.
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

// ---------------------------------------------------------------------------
// Protected directories and the setup log
// ---------------------------------------------------------------------------

/// Create `path` as a new directory whose protected DACL grants only SYSTEM
/// and Administrators. Atomic: the directory never exists with a weaker
/// DACL. Fails if anything already exists at `path`.
pub(crate) fn create_protected_dir(path: &Path) -> Result<()> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;

    let path_w = wide(path);
    let acl = wide("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(acl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|e| err("protected directory ACL", e.to_string()))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let result = CreateDirectoryW(PCWSTR(path_w.as_ptr()), Some(&attributes));
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result.map_err(|e| {
            err(
                "create protected directory",
                format!("{}: {e}", path.display()),
            )
        })
    }
}

const SETUP_LOG_NAME: &str = "setup.log";

/// `%SystemRoot%\Logs\TaskMan`, the setup log's folder.
///
/// Setup is elevated, so its log must live where an unelevated user cannot
/// plant a junction or link that turns an elevated append into a write
/// primitive. That rules out `%TEMP%` (the user's own for consent
/// elevation) and `%ProgramData%` (users may create folders there before the
/// first install). The service's `%ProgramData%\TaskMan\logs` is also off
/// limits for a second reason: it may hold only the service's own daily
/// logs, and a foreign `setup.log` there makes the service disable its file
/// logging. `%SystemRoot%\Logs` is writable only by administrators and
/// SYSTEM, and uninstall never removes it, so the uninstall's final log line
/// recreates nothing that uninstall just deleted.
fn setup_log_dir() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_Windows)?
        .join("Logs")
        .join("TaskMan"))
}

/// Append one line to the setup log. Diagnostics only - like the service's
/// file logging, this must never gate the install.
pub fn append_setup_log(line: &str) {
    if let Ok(dir) = setup_log_dir() {
        let _ = append_log_line(&dir, line);
    }
}

fn append_log_line(dir: &Path, line: &str) -> Result<()> {
    use std::io::Write;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut file = open_log_file(dir)?;
    writeln!(file, "[{stamp}] {line}")?;
    Ok(())
}

/// Open `dir\setup.log` for appending. The folder is created protected when
/// missing; an existing folder or log file that is a reparse point is
/// refused rather than followed.
fn open_log_file(dir: &Path) -> Result<File> {
    if create_protected_dir(dir).is_err() {
        // Usually an earlier run's folder. Whatever it is, it must be a
        // plain directory and not a link to somewhere else.
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(err(
                "setup log",
                format!("{} is not a plain directory", dir.display()),
            ));
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(dir.join(SETUP_LOG_NAME))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(err(
            "setup log",
            format!(
                "{} is not a regular file",
                dir.join(SETUP_LOG_NAME).display()
            ),
        ));
    }
    Ok(file)
}

/// Remove the `setup.log` that earlier setup versions appended to inside the
/// service's log directory. Its name is not an owned service-log name, so
/// while it exists the service refuses the directory and runs without file
/// logging. Returns whether a file was removed.
///
/// Runs before the service (re)starts, when that directory may not be
/// secured yet: the file is opened without following a reparse point and
/// deleted by handle only if it resolves to exactly the expected path, so a
/// junction planted on the way cannot redirect the delete.
pub fn remove_legacy_setup_log() -> Result<bool> {
    remove_file_at_exact_path(&service_data_dir()?.join("logs").join(SETUP_LOG_NAME))
}

fn remove_file_at_exact_path(path: &Path) -> Result<bool> {
    use windows::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let file = match std::fs::OpenOptions::new()
        .access_mode((DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(err(
            "remove legacy setup log",
            format!("{} is not a regular file; left alone", path.display()),
        ));
    }
    let resolved = normalized_path(&file)?;
    if normalize(&resolved) != normalize(path) {
        return Err(err(
            "remove legacy setup log",
            format!(
                "{} resolves to {}; left alone",
                path.display(),
                resolved.display()
            ),
        ));
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&raw const info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|error| err("remove legacy setup log", error.to_string()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HMODULE;

    /// A private copy of a system DLL mapped as an image (resource-only: no
    /// code runs), standing in for the running setup image so renames and
    /// substitutions can be staged without touching the test binary.
    struct MappedCopy {
        module: HMODULE,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl MappedCopy {
        fn new() -> Self {
            use windows::Win32::System::LibraryLoader::{
                LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW,
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("probe.dll");
            let system = known_folder(&windows::Win32::UI::Shell::FOLDERID_System).unwrap();
            std::fs::copy(system.join("version.dll"), &path).unwrap();
            let path_w = wide(&path);
            let module = unsafe {
                LoadLibraryExW(
                    PCWSTR(path_w.as_ptr()),
                    None,
                    LOAD_LIBRARY_AS_IMAGE_RESOURCE,
                )
            }
            .unwrap();
            MappedCopy {
                module,
                path,
                _dir: dir,
            }
        }

        /// Image-resource handles carry tag bits; the mapping starts below.
        fn base(&self) -> *const core::ffi::c_void {
            (self.module.0 as usize & !3) as *const core::ffi::c_void
        }
    }

    impl Drop for MappedCopy {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::Foundation::FreeLibrary(self.module);
            }
        }
    }

    /// `tempfile` may hand out 8.3 short paths (CI profiles); the exact-path
    /// checks compare against resolved long names.
    fn long_temp_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let long = std::fs::canonicalize(dir.path()).unwrap();
        let long = PathBuf::from(strip_verbatim(&long.to_string_lossy()));
        (dir, long)
    }

    fn junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");
    }

    /// Regression: `is_within` was a string prefix check, so the uninstaller
    /// treated `C:\Program Files\TaskManX` as part of the install tree.
    #[test]
    fn is_within_matches_whole_path_components() {
        let tree = Path::new(r"C:\Program Files\TaskMan");
        assert!(is_within(
            Path::new(r"C:\Program Files\TaskMan\taskman-setup.exe"),
            tree
        ));
        assert!(is_within(
            Path::new(r"c:\program files\taskman\sub\x.exe"),
            tree
        ));
        assert!(is_within(Path::new(r"C:\Program Files\TaskMan\"), tree));
        for outside in [
            r"C:\Program Files\TaskManX\taskman-setup.exe",
            r"C:\Program Files\TaskMan.old\taskman-setup.exe",
            r"C:\Program Files",
            r"D:\Program Files\TaskMan\taskman-setup.exe",
        ] {
            assert!(!is_within(Path::new(outside), tree), "{outside}");
        }
    }

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(strip_verbatim(r"\\?\C:\a\b.exe"), r"C:\a\b.exe");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\srv\share\b.exe"),
            r"\\srv\share\b.exe"
        );
        assert_eq!(strip_verbatim(r"C:\a"), r"C:\a");
    }

    /// The running test binary is a real process image: pinning it must
    /// succeed and read the executable's bytes.
    #[test]
    fn the_running_image_can_be_pinned_and_read() {
        let image = setup_image().unwrap();
        assert!(image.read_all().unwrap().starts_with(b"MZ"));
        let exe = std::env::current_exe().unwrap();
        assert_eq!(image.current_path().unwrap().file_name(), exe.file_name());
    }

    /// A launch path that no longer holds the running image must not be
    /// trusted: renaming the image away and putting another file at its path
    /// used to substitute what setup read as its payload.
    #[test]
    fn a_file_substituted_at_the_launch_path_is_not_the_image() {
        let copy = MappedCopy::new();
        let original = std::fs::read(&copy.path).unwrap();
        {
            let image = SetupImage::pin_mapped(copy.base()).unwrap();
            assert_eq!(image.read_all().unwrap(), original);
            // While pinned, the file cannot be renamed, deleted or rewritten.
            assert!(std::fs::rename(&copy.path, copy.path.with_file_name("x.dll")).is_err());
            assert!(std::fs::remove_file(&copy.path).is_err());
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&copy.path)
                    .is_err()
            );
        }
        // Unpinned (the window before setup pins itself), the image is
        // renamed and an impostor appears at its launch path.
        let moved = copy.path.with_file_name("moved.dll");
        std::fs::rename(&copy.path, &moved).unwrap();
        std::fs::write(&copy.path, b"MZ impostor").unwrap();
        let impostor = File::open(&copy.path).unwrap();
        let error = verify_mapped_image(&impostor, copy.base()).unwrap_err();
        assert!(error.to_string().contains("renamed or replaced"), "{error}");
        // Pinning follows the mapping to the real image, never the impostor.
        let image = SetupImage::pin_mapped(copy.base()).unwrap();
        assert_eq!(image.read_all().unwrap(), original);
        assert_eq!(image.current_path().unwrap().file_name(), moved.file_name());
    }

    /// Regression: the uninstaller rollback renamed whatever sat at the
    /// predictable relocation path back into Program Files. Moves now go
    /// through the pinned handle, so only the image itself ever moves, the
    /// relocated file cannot be swapped while pinned, and an existing
    /// destination is never overwritten.
    #[test]
    fn relocation_moves_only_the_pinned_image() {
        let copy = MappedCopy::new();
        let original = std::fs::read(&copy.path).unwrap();
        let image = SetupImage::pin_mapped(copy.base()).unwrap();
        let away = copy.path.with_file_name(".orphan.exe");
        image.rename_to(&away).unwrap();
        assert!(!copy.path.exists());
        assert_eq!(image.current_path().unwrap().file_name(), away.file_name());
        // Nobody else can move the relocated image to make room for a swap.
        assert!(std::fs::rename(&away, copy.path.with_file_name("swap.dll")).is_err());
        // A file planted at the restore target is not replaced.
        std::fs::write(&copy.path, b"planted").unwrap();
        assert!(image.rename_to(&copy.path).is_err());
        assert_eq!(std::fs::read(&copy.path).unwrap(), b"planted");
        std::fs::remove_file(&copy.path).unwrap();
        image.rename_to(&copy.path).unwrap();
        assert_eq!(std::fs::read(&copy.path).unwrap(), original);
    }

    /// Regression: running setup from the install directory copied the
    /// image onto itself, which fails. Identity, not path spelling, decides.
    #[test]
    fn copying_the_image_onto_itself_is_skipped() {
        let copy = MappedCopy::new();
        let original = std::fs::read(&copy.path).unwrap();
        let image = SetupImage::pin_mapped(copy.base()).unwrap();
        assert!(!image.copy_to(&copy.path).unwrap());
        let respelled = PathBuf::from(copy.path.to_string_lossy().to_uppercase());
        assert!(!image.copy_to(&respelled).unwrap());
        let other = copy.path.with_file_name("taskman-setup.exe");
        std::fs::write(&other, b"older uninstaller").unwrap();
        assert!(image.copy_to(&other).unwrap());
        assert_eq!(std::fs::read(&other).unwrap(), original);
    }

    /// The setup log appends into an existing plain folder but refuses a
    /// folder that is a junction: an elevated append must not be redirected.
    #[test]
    fn setup_log_never_follows_a_junction() {
        let (_dir, base) = long_temp_dir();
        let logs = base.join("logs");
        // An earlier run's folder (creating one here would apply the
        // administrator-only DACL and lock this unelevated test out).
        std::fs::create_dir(&logs).unwrap();
        append_log_line(&logs, "first").unwrap();
        append_log_line(&logs, "second").unwrap();
        let text = std::fs::read_to_string(logs.join(SETUP_LOG_NAME)).unwrap();
        assert!(
            text.contains("] first\n") && text.contains("] second\n"),
            "{text}"
        );

        let elsewhere = base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let linked = base.join("linked-logs");
        junction(&linked, &elsewhere);
        assert!(append_log_line(&linked, "redirected").is_err());
        assert!(!elsewhere.join(SETUP_LOG_NAME).exists());
    }

    /// The legacy `setup.log` in the service's log folder is deleted only
    /// at exactly that path: a junction on the way must not redirect the
    /// elevated delete.
    #[test]
    fn legacy_setup_log_is_removed_only_at_its_exact_path() {
        let (_dir, base) = long_temp_dir();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join(SETUP_LOG_NAME), b"old").unwrap();
        let linked = base.join("logs");
        junction(&linked, &real);

        let error = remove_file_at_exact_path(&linked.join(SETUP_LOG_NAME)).unwrap_err();
        assert!(error.to_string().contains("left alone"), "{error}");
        assert!(real.join(SETUP_LOG_NAME).exists());

        assert!(remove_file_at_exact_path(&real.join(SETUP_LOG_NAME)).unwrap());
        assert!(!real.join(SETUP_LOG_NAME).exists());
        assert!(!remove_file_at_exact_path(&real.join(SETUP_LOG_NAME)).unwrap());
    }
}
