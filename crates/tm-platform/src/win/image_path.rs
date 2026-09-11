//! Image path of a process that will not open.
//!
//! Every ordinary way to ask for an executable's path — sysinfo,
//! `QueryFullProcessImageNameW`, the PEB — needs a handle, and an unelevated
//! session cannot open a SYSTEM or elevated process at all. That is roughly
//! half the process list, and it is why Process Properties showed "—" for the
//! path of the very process the user was trying to identify.
//!
//! `NtQuerySystemInformation(SystemProcessIdInformation)` answers for any PID
//! without a handle. It returns an NT device path
//! (`\Device\HarddiskVolume3\Windows\...`), which is translated back to a
//! drive letter here — the UI must never show a path the user cannot paste
//! into Explorer.

use std::collections::HashMap;
use std::path::PathBuf;

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::Foundation::{HANDLE, UNICODE_STRING};
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::core::PWSTR;

/// `SystemProcessIdInformation`. Not in the `windows` crate's enum, and not
/// documented beyond "reserved"; the layout below has been stable since
/// Windows Vista and is what every process viewer uses for this.
const SYSTEM_PROCESS_ID_INFORMATION_CLASS: i32 = 88;

const STATUS_SUCCESS: i32 = 0;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;

/// A path longer than this did not come from this API.
const MAX_PATH_CHARS: usize = 32_768;

#[repr(C)]
struct SystemProcessIdInformation {
    process_id: HANDLE,
    image_name: UNICODE_STRING,
}

/// Full image path of `pid` as a DOS path, or `None` when the kernel will not
/// name it.
///
/// Test-only: production goes through [`ImagePaths`], which caches both the
/// per-process answer and the volume map. This is the single-shot form the
/// live tests pin the NT layout with.
#[cfg(test)]
fn of_pid(pid: u32) -> Option<PathBuf> {
    let nt_path = nt_path_of_pid(pid)?;
    Some(PathBuf::from(dos_path(&nt_path, &device_map())))
}

/// Raw NT device path of `pid`'s image.
///
/// Two calls by design: the first is expected to fail with
/// `STATUS_INFO_LENGTH_MISMATCH` and reports the buffer size the name needs in
/// `MaximumLength`, the second fills it.
fn nt_path_of_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let mut info = SystemProcessIdInformation {
        process_id: HANDLE(pid as isize as *mut core::ffi::c_void),
        image_name: UNICODE_STRING::default(),
    };
    let class = SYSTEM_INFORMATION_CLASS(SYSTEM_PROCESS_ID_INFORMATION_CLASS);
    let size = std::mem::size_of::<SystemProcessIdInformation>() as u32;

    let status = unsafe {
        NtQuerySystemInformation(
            class,
            std::ptr::from_mut(&mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    // A zero-length request must come back asking for room. Anything else
    // means the process is gone or the class is unavailable.
    if status.0 != STATUS_INFO_LENGTH_MISMATCH {
        return None;
    }
    let wanted = info.image_name.MaximumLength as usize;
    if wanted == 0 || wanted > MAX_PATH_CHARS * 2 {
        return None;
    }
    let mut buffer: Vec<u16> = vec![0; wanted.div_ceil(2)];
    info.image_name.Buffer = PWSTR(buffer.as_mut_ptr());
    info.image_name.Length = 0;
    let status = unsafe {
        NtQuerySystemInformation(
            class,
            std::ptr::from_mut(&mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if status.0 != STATUS_SUCCESS {
        return None;
    }
    let used = info.image_name.Length as usize / 2;
    let nt_path = String::from_utf16_lossy(buffer.get(..used)?);
    (!nt_path.is_empty()).then_some(nt_path)
}

/// Translate `\Device\HarddiskVolume3\Windows\...` into `C:\Windows\...`.
///
/// An untranslatable path is returned unchanged rather than dropped: a device
/// path still tells the user which image is running, and hiding it would be a
/// worse answer than an ugly one.
fn dos_path(nt_path: &str, map: &[(String, String)]) -> String {
    for (device, letter) in map {
        if let Some(rest) = strip_device_prefix(nt_path, device) {
            return format!("{letter}{rest}");
        }
    }
    nt_path.to_string()
}

/// `\Device\HarddiskVolume1` must not match `\Device\HarddiskVolume10`, so the
/// match only counts when the device name ends at a path separator.
fn strip_device_prefix<'a>(nt_path: &'a str, device: &str) -> Option<&'a str> {
    let rest = nt_path
        .get(..device.len())
        .filter(|head| head.eq_ignore_ascii_case(device))?;
    let _ = rest;
    let tail = &nt_path[device.len()..];
    (tail.is_empty() || tail.starts_with('\\')).then_some(tail)
}

/// Drive letter to NT device name for every mounted volume.
fn device_map() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let dos = format!("{}:", letter as char);
        let wide: Vec<u16> = dos.encode_utf16().chain(std::iter::once(0)).collect();
        let mut target = [0u16; 512];
        let len = unsafe {
            QueryDosDeviceW(
                windows::core::PCWSTR(wide.as_ptr()),
                Some(target.as_mut_slice()),
            )
        };
        if len == 0 {
            continue;
        }
        // The result is a NUL-separated list; only the first entry is the
        // volume this letter currently resolves to.
        let device: String = String::from_utf16_lossy(&target[..len as usize])
            .split('\0')
            .next()
            .unwrap_or_default()
            .to_string();
        if !device.is_empty() {
            out.push((device, dos));
        }
    }
    // Longest device name first, so a prefix can never shadow a longer one.
    out.sort_by_key(|(device, _)| std::cmp::Reverse(device.len()));
    out
}

/// Cached image paths for the processes no handle can be opened for.
///
/// Two caches, because both answers are expensive in different ways: the path
/// of a live process never changes, and the drive-letter map costs 26
/// `QueryDosDeviceW` calls — trivial once, and 7000 calls on the first tick if
/// rebuilt per process.
#[derive(Default)]
pub struct ImagePaths {
    resolved: HashMap<u32, Option<PathBuf>>,
    devices: Vec<(String, String)>,
}

impl ImagePaths {
    /// Path of `pid`, asking the kernel at most once per process.
    pub fn get(&mut self, pid: u32) -> Option<PathBuf> {
        if let Some(known) = self.resolved.get(&pid) {
            return known.clone();
        }
        let resolved = nt_path_of_pid(pid).map(|nt_path| {
            if self.devices.is_empty() {
                self.devices = device_map();
            }
            let mut dos = dos_path(&nt_path, &self.devices);
            if dos == nt_path {
                // A volume mounted since the map was built (a USB disk, a
                // mounted image). Rebuild once and try again before giving up
                // and showing the raw device path.
                self.devices = device_map();
                dos = dos_path(&nt_path, &self.devices);
            }
            PathBuf::from(dos)
        });
        self.resolved.insert(pid, resolved.clone());
        resolved
    }

    /// Drop processes that no longer exist so the map stays bounded.
    pub fn retain_live(&mut self, live: &std::collections::HashSet<u32>) {
        if live.is_empty() {
            return;
        }
        self.resolved.retain(|pid, _| live.contains(pid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_prefixes_match_whole_names_only() {
        assert_eq!(
            strip_device_prefix(
                r"\Device\HarddiskVolume3\Windows",
                r"\Device\HarddiskVolume3"
            ),
            Some(r"\Windows")
        );
        // The classic off-by-one: volume 1 must not swallow volume 10.
        assert_eq!(
            strip_device_prefix(r"\Device\HarddiskVolume10\X", r"\Device\HarddiskVolume1"),
            None
        );
        // An exact match with nothing after it is still a match.
        assert_eq!(
            strip_device_prefix(r"\Device\HarddiskVolume3", r"\Device\HarddiskVolume3"),
            Some("")
        );
        // Case is irrelevant in NT paths.
        assert_eq!(
            strip_device_prefix(r"\device\harddiskvolume3\a", r"\Device\HarddiskVolume3"),
            Some(r"\a")
        );
    }

    /// An unknown device is shown as-is. Dropping it would replace a usable
    /// answer with "—".
    #[test]
    fn an_untranslatable_device_path_survives() {
        let map = [(r"\Device\HarddiskVolume3".to_string(), "C:".to_string())];
        let odd = r"\Device\SomethingUnmapped\app.exe";
        assert_eq!(dos_path(odd, &map), odd);
        assert_eq!(
            dos_path(r"\Device\HarddiskVolume3\Windows\x.exe", &map),
            r"C:\Windows\x.exe"
        );
    }

    /// The volume map must be built once, not once per process: it is 26
    /// `QueryDosDeviceW` calls, and the first tick resolves hundreds of paths.
    #[test]
    fn the_volume_map_is_built_once_per_cache() {
        let mut cache = ImagePaths::default();
        let _ = cache.get(std::process::id());
        let after_first = cache.devices.len();
        assert!(after_first > 0, "no volumes mapped");
        let _ = cache.get(std::process::id());
        assert_eq!(cache.devices.len(), after_first);
        // ...and a repeat lookup does not re-ask the kernel at all.
        assert_eq!(cache.resolved.len(), 1);
    }

    /// The call has to agree with the documented path for our own process —
    /// that pins both the NT structure layout and the device translation.
    #[cfg(target_os = "windows")]
    #[test]
    fn our_own_path_matches_what_the_runtime_reports() {
        let expected = std::env::current_exe().expect("current_exe");
        let got = of_pid(std::process::id()).expect("kernel named our own image");
        assert!(
            got.to_string_lossy()
                .eq_ignore_ascii_case(&expected.to_string_lossy()),
            "{got:?} != {expected:?}"
        );
    }

    /// The whole point: a process this token cannot open must still be named.
    /// `wininit.exe` and `services.exe` are SYSTEM, protected, present on
    /// every Windows, and refuse `OpenProcess` to an ordinary session.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_system_process_is_named_without_a_handle() {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };

        let mut named = Vec::new();
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).expect("snapshot");
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                    if name.eq_ignore_ascii_case("wininit.exe")
                        || name.eq_ignore_ascii_case("services.exe")
                    {
                        named.push((entry.th32ProcessID, name));
                    }
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
        }
        assert!(!named.is_empty(), "neither wininit nor services is running");

        let mut resolved = 0;
        for (pid, name) in &named {
            let Some(path) = of_pid(*pid) else { continue };
            let text = path.to_string_lossy().to_ascii_lowercase();
            assert!(
                text.ends_with(&name.to_ascii_lowercase()),
                "{path:?} does not end in {name}"
            );
            assert!(text.contains(r"\windows\"), "{path:?} is not under Windows");
            resolved += 1;
        }
        assert!(
            resolved > 0,
            "no protected system process could be named - the NT class or layout is wrong"
        );
    }
}
