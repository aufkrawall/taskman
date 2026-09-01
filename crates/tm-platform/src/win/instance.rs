//! Coordination between the TaskMan instances of one logon session.
//!
//! Ctrl+Shift+Esc — and every other Task Manager entry the shell offers —
//! starts a NEW process each time. Exactly one of them may become the running
//! task manager; a later launch has to hand its "show yourself" request to
//! the instance that already exists. What makes this more than a named mutex
//! is the failure case: if the existing instance is wedged, gone, or was
//! never a TaskMan at all, the launch must still leave the user with a
//! working task manager. A task manager that cannot be opened while the
//! machine is unresponsive is useless exactly when it is needed.
//!
//! Session-local objects (`Local\`, so per logon session):
//!
//! | object | role |
//! | --- | --- |
//! | `TaskMan.Instance.v2` (mutex) | a primary instance exists |
//! | `TaskMan.Show.v2` (event) | "show your window" request |
//! | `TaskMan.Shown.v2` (event) | the primary's acknowledgement |
//! | `TaskMan.Primary.v2` (section) | the primary's process and window ids |
//!
//! All four carry a low integrity label: the hotkey launch runs at the
//! shell's medium integrity, while the instance it must reach may run
//! elevated ("always start with administrator privileges"). Without the
//! label, no-write-up would deny that launch every access to the objects and
//! the session would silently end up with two task managers. The DACL still
//! keeps them to this user, Administrators and SYSTEM.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    ERROR_TIMEOUT, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_ABANDONED,
    WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, OpenMutexW, OpenProcess, PROCESS_ACCESS_RIGHTS,
    ReleaseMutex, ResetEvent, SYNCHRONIZATION_ACCESS_RIGHTS, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, IsWindow, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_NULL,
};
use windows::core::PCWSTR;

/// SYNCHRONIZE, the one right needed to prove an object exists.
const SYNCHRONIZE_ONLY: u32 = 0x0010_0000;
/// Identifies a section this build wrote; anything else is treated as absent.
const INFO_MAGIC: u32 = 0x544D_5031;

/// How long a hotkey launch waits for the running instance to acknowledge
/// that it has processed the request.
///
/// The first stage is short because the common answer arrives in
/// milliseconds. It is only extended after the running instance has proven
/// it still pumps messages: an instance that is merely slow (a saturated
/// machine is the normal reason to reach for a task manager) must not be
/// duplicated, while a wedged one must not delay the user for long.
const FIRST_ACK_MS: u32 = 1_500;
const EXTENDED_ACK_MS: u32 = 4_000;
/// Deadline for the "does this window still pump messages" probe. This is the
/// same question the shell's "not responding" decoration answers.
const HUNG_PROBE_MS: u32 = 500;
/// Ownership handoff to an explicitly elevated replacement (see `acquire`).
const HANDOFF_MS: u32 = 15_000;

/// What a launch found when it asked the session's existing instance to show
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// Nothing holds the session's coordination objects.
    NoInstance,
    /// A live instance acknowledged the request and is showing its window.
    Activated,
    /// Something holds the objects but never answered: it is wedged, dying,
    /// or an unrelated process squats the name. The caller must start a real
    /// task manager anyway.
    Unresponsive,
}

/// The role this process takes in the session.
pub enum Role {
    /// This process is the session's instance; the guard must stay alive for
    /// as long as it runs.
    Primary(Primary),
    /// The running instance took the request over: exit quietly.
    Deferred,
    /// Coordination was impossible, so this process runs unregistered rather
    /// than leaving the user without a task manager.
    Uncoordinated,
}

/// Handles the primary instance owns for the lifetime of the process.
pub struct Primary {
    mutex: HANDLE,
    show: HANDLE,
    ack: HANDLE,
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    listener: Option<std::thread::JoinHandle<()>>,
    owns_mutex: bool,
}

/// The primary's acknowledgement event, so [`acknowledge_show`] can be called
/// from the UI thread without threading a handle through the app.
static ACK_EVENT: AtomicIsize = AtomicIsize::new(0);
/// Mapped view of the published primary info, owned by the primary instance.
static INFO_VIEW: AtomicIsize = AtomicIsize::new(0);
/// Show event, published only when the listener thread could not start.
static SHOW_FALLBACK: AtomicIsize = AtomicIsize::new(0);
static LISTENER_STOP: AtomicBool = AtomicBool::new(false);
/// Set once a pre-elevation probe has established that the instance holding
/// the objects does not answer, so [`acquire`] does not wait a second time.
static PROBE_UNRESPONSIVE: AtomicBool = AtomicBool::new(false);

/// Published position of the primary instance. Written by the primary,
/// read by every later launch; the magic word is stored last so a reader
/// either sees a complete record or none.
#[repr(C)]
struct SharedInfo {
    magic: AtomicU32,
    pid: AtomicU32,
    hwnd: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
struct PrimaryInfo {
    pid: u32,
    hwnd: Option<isize>,
}

struct Names {
    mutex: Vec<u16>,
    show: Vec<u16>,
    ack: Vec<u16>,
    info: Vec<u16>,
}

fn names_for(scope: &str) -> Names {
    let wide = |name: &str| -> Vec<u16> {
        format!(r"Local\TaskMan.{name}.v2{scope}")
            .encode_utf16()
            .chain([0])
            .collect()
    };
    Names {
        mutex: wide("Instance"),
        show: wide("Show"),
        ack: wide("Shown"),
        info: wide("Primary"),
    }
}

fn names() -> Names {
    names_for("")
}

/// Security descriptor plus the attributes block that points at it.
struct ObjectSecurity {
    _descriptor: super::core_service::SecurityDescriptor,
    attributes: SECURITY_ATTRIBUTES,
}

/// Own the objects as this user (at any integrity level), Administrators and
/// SYSTEM, and label them low so an unelevated hotkey launch can still reach
/// an elevated instance. Returns `None` when the descriptor cannot be built;
/// the caller then falls back to default security, which still works for the
/// common same-integrity case.
fn object_security() -> Option<ObjectSecurity> {
    let sid = super::core_service::current_user_sid()
        .inspect_err(|error| tracing::debug!(%error, "no user SID for instance objects"))
        .ok()?;
    let descriptor = super::core_service::security_descriptor(&format!(
        "D:(A;;GA;;;{sid})(A;;GA;;;BA)(A;;GA;;;SY)S:(ML;;NW;;;LW)"
    ))
    .inspect_err(|error| tracing::debug!(%error, "instance object security unavailable"))
    .ok()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: false.into(),
    };
    Some(ObjectSecurity {
        _descriptor: descriptor,
        attributes,
    })
}

fn security_ptr(security: &Option<ObjectSecurity>) -> Option<*const SECURITY_ATTRIBUTES> {
    security
        .as_ref()
        .map(|security| &security.attributes as *const SECURITY_ATTRIBUTES)
}

/// Wait for `handle`, treating an abandoned owner as success — a dead owner
/// is exactly the state the waiter wants to inherit.
fn wait(handle: HANDLE, timeout_ms: u32) -> bool {
    let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
    result == WAIT_OBJECT_0 || result == WAIT_ABANDONED
}

/// Whether a process id still names a live process. An id we cannot open is
/// reported as live: a refused answer must never be read as "gone".
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let process = match unsafe { OpenProcess(PROCESS_ACCESS_RIGHTS(SYNCHRONIZE_ONLY), false, pid) }
    {
        Ok(process) => process,
        // Only "no such id" proves the process is gone; a refused open (a
        // protected or foreign-user process) means it is very much there.
        Err(error) => return error.code() != ERROR_INVALID_PARAMETER.to_hresult(),
    };
    // A process handle is signaled once the process has exited.
    let exited = unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0;
    unsafe {
        let _ = CloseHandle(process);
    }
    !exited
}

/// Whether the published window still pumps messages — the question the
/// shell answers with its "not responding" decoration, asked with the same
/// primitive.
///
/// `None` means the probe could not answer. That is the normal outcome when
/// an unelevated launch probes an elevated instance, because UIPI refuses
/// the message across integrity levels; only an explicit timeout proves a
/// wedged message loop, and only that may cost the instance its request.
fn window_responds(hwnd: isize) -> Option<bool> {
    let window = HWND(hwnd as *mut core::ffi::c_void);
    if !unsafe { IsWindow(Some(window)) }.as_bool() {
        return None;
    }
    let result = unsafe {
        SendMessageTimeoutW(
            window,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            HUNG_PROBE_MS,
            None,
        )
    };
    if result.0 != 0 {
        return Some(true);
    }
    (unsafe { GetLastError() } == ERROR_TIMEOUT).then_some(false)
}

/// Map the primary's published record, creating it when `security` is given
/// (primary) or opening it read-only when it is not (later launches).
fn open_info(names: &Names, create: Option<&Option<ObjectSecurity>>) -> Option<(HANDLE, *mut u8)> {
    let (mapping, access) = match create {
        Some(security) => (
            unsafe {
                CreateFileMappingW(
                    // Paging-file backed: the documented "no file" handle,
                    // not a null one.
                    INVALID_HANDLE_VALUE,
                    security_ptr(security),
                    PAGE_READWRITE,
                    0,
                    std::mem::size_of::<SharedInfo>() as u32,
                    PCWSTR(names.info.as_ptr()),
                )
            }
            .ok()?,
            FILE_MAP_READ | FILE_MAP_WRITE,
        ),
        None => (
            unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(names.info.as_ptr())) }
                .ok()?,
            FILE_MAP_READ,
        ),
    };
    let view = unsafe { MapViewOfFile(mapping, access, 0, 0, std::mem::size_of::<SharedInfo>()) };
    if view.Value.is_null() {
        unsafe {
            let _ = CloseHandle(mapping);
        }
        return None;
    }
    Some((mapping, view.Value.cast()))
}

/// Read the published record. `None` means "no usable record": no section, a
/// section this build did not write, or one that is not filled in yet.
fn read_primary_info(names: &Names) -> Option<PrimaryInfo> {
    let (mapping, view) = open_info(names, None)?;
    let shared = unsafe { &*view.cast::<SharedInfo>() };
    let info = (shared.magic.load(Ordering::Acquire) == INFO_MAGIC).then(|| PrimaryInfo {
        pid: shared.pid.load(Ordering::Relaxed),
        hwnd: match shared.hwnd.load(Ordering::Relaxed) {
            0 => None,
            handle => Some(handle as isize),
        },
    });
    unsafe {
        let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
            Value: view.cast::<core::ffi::c_void>(),
        });
        let _ = CloseHandle(mapping);
    }
    info
}

/// Publish this process's window handle so later launches can tell a slow
/// instance from a wedged one. Called once, as soon as the handle exists.
pub fn publish_window(hwnd: isize) {
    let view = INFO_VIEW.load(Ordering::Acquire);
    if view == 0 {
        return;
    }
    let shared = unsafe { &*(view as *const SharedInfo) };
    shared.hwnd.store(hwnd as u64, Ordering::Relaxed);
}

/// Acknowledge a show request from the UI thread that actually processed it.
/// The acknowledgement therefore proves the UI is alive, which is what the
/// waiting launch needs to know.
pub fn acknowledge_show() {
    let ack = ACK_EVENT.load(Ordering::Acquire);
    if ack == 0 {
        return;
    }
    unsafe {
        let _ = SetEvent(HANDLE(ack as *mut core::ffi::c_void));
    }
}

/// Ask the instance that holds the objects to show itself, and find out
/// whether it actually did.
///
/// The foreground grant comes first and is the reason this handshake exists
/// at all: only the process the shell just launched holds the right to give
/// the foreground away, so the running instance can only raise its window if
/// this launch hands that right over before asking.
fn handshake(names: &Names, show: HANDLE, ack: HANDLE) -> Activation {
    let info = read_primary_info(names);
    if let Some(info) = info
        && !process_alive(info.pid)
    {
        // A record without its process: the instance died holding the name,
        // or something else squats it. Either way nobody will answer.
        return Activation::Unresponsive;
    }
    if let Some(info) = info {
        let _ = unsafe { AllowSetForegroundWindow(info.pid) };
    }
    unsafe {
        // Clear an acknowledgement left over from an earlier launch that
        // stopped waiting, so a stale signal cannot be mistaken for this
        // request being handled.
        let _ = ResetEvent(ack);
        if SetEvent(show).is_err() {
            return Activation::Unresponsive;
        }
    }
    if wait(ack, FIRST_ACK_MS) {
        return Activation::Activated;
    }
    // Nothing yet. A window that is provably stuck loses its request now; a
    // merely slow instance — the normal state of the machine one reaches for
    // a task manager on — keeps it for the extended deadline.
    if let Some(hwnd) = read_primary_info(names).and_then(|info| info.hwnd)
        && window_responds(hwnd) == Some(false)
    {
        tracing::warn!("the running instance does not pump messages; starting a new one");
        return Activation::Unresponsive;
    }
    if wait(ack, EXTENDED_ACK_MS) {
        Activation::Activated
    } else {
        tracing::warn!("the running instance never acknowledged; starting a new one");
        Activation::Unresponsive
    }
}

/// Hand a launch to the instance this session already runs, without becoming
/// one. Used before the elevation policy re-execs: raising a window that is
/// already open must never cost the user a UAC prompt.
pub fn activate_existing() -> Activation {
    let activation = activate_in(&names());
    if activation == Activation::Unresponsive {
        PROBE_UNRESPONSIVE.store(true, Ordering::Release);
    }
    activation
}

fn activate_in(names: &Names) -> Activation {
    match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_ACCESS_RIGHTS(SYNCHRONIZE_ONLY),
            false,
            PCWSTR(names.mutex.as_ptr()),
        )
    } {
        Ok(mutex) => {
            unsafe {
                let _ = CloseHandle(mutex);
            }
            let security = object_security();
            let events = unsafe {
                let show = CreateEventW(
                    security_ptr(&security),
                    false,
                    false,
                    PCWSTR(names.show.as_ptr()),
                );
                let ack = CreateEventW(
                    security_ptr(&security),
                    false,
                    false,
                    PCWSTR(names.ack.as_ptr()),
                );
                show.and_then(|show| ack.map(|ack| (show, ack)))
            };
            match events {
                Ok((show, ack)) => {
                    let activation = handshake(names, show, ack);
                    unsafe {
                        let _ = CloseHandle(show);
                        let _ = CloseHandle(ack);
                    }
                    activation
                }
                Err(error) => {
                    tracing::warn!(%error, "instance events unavailable");
                    Activation::Unresponsive
                }
            }
        }
        // Only "no such object" proves there is nothing to talk to. Anything
        // else (a denied open, an exhausted handle table) leaves an instance
        // that may exist but cannot be reached, and starting is the answer
        // that still gives the user a task manager.
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Activation::NoInstance,
        Err(error) => {
            tracing::warn!(%error, "instance mutex could not be opened");
            Activation::Unresponsive
        }
    }
}

/// Take this process's place in the session.
///
/// `on_show` runs on a dedicated waiter thread whenever another launch asks
/// this instance to show itself; it must only wake the UI, never block.
///
/// `elevation_handoff` marks the replacement started by an explicit
/// "restart elevated": the outgoing instance queues its close right after
/// launching it, so this process waits for ownership instead of bouncing off
/// its own predecessor.
pub fn acquire(elevation_handoff: bool, on_show: impl Fn() + Send + 'static) -> Role {
    let names = names();
    let security = object_security();
    let mutex = match unsafe {
        CreateMutexW(security_ptr(&security), true, PCWSTR(names.mutex.as_ptr()))
    } {
        Ok(mutex) => mutex,
        Err(error) => {
            tracing::warn!(%error, "single-instance coordination unavailable");
            return Role::Uncoordinated;
        }
    };
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let events = unsafe {
        let show = CreateEventW(
            security_ptr(&security),
            false,
            false,
            PCWSTR(names.show.as_ptr()),
        );
        let ack = CreateEventW(
            security_ptr(&security),
            false,
            false,
            PCWSTR(names.ack.as_ptr()),
        );
        show.and_then(|show| ack.map(|ack| (show, ack)))
    };
    let (show, ack) = match events {
        Ok(handles) => handles,
        Err(error) => {
            tracing::warn!(%error, "single-instance events unavailable");
            unsafe {
                let _ = CloseHandle(mutex);
            }
            return Role::Uncoordinated;
        }
    };
    let close_all = || unsafe {
        let _ = CloseHandle(show);
        let _ = CloseHandle(ack);
        let _ = CloseHandle(mutex);
    };

    let mut owns_mutex = !existed;
    if existed {
        if elevation_handoff {
            if !wait(mutex, HANDOFF_MS) {
                tracing::warn!("the outgoing instance never released ownership");
                close_all();
                return Role::Uncoordinated;
            }
            owns_mutex = true;
        } else if PROBE_UNRESPONSIVE.load(Ordering::Acquire) {
            // The pre-elevation probe already waited out this instance.
            close_all();
            return Role::Uncoordinated;
        } else {
            match handshake(&names, show, ack) {
                Activation::Activated => {
                    close_all();
                    return Role::Deferred;
                }
                _ => {
                    close_all();
                    return Role::Uncoordinated;
                }
            }
        }
    }

    // Publishing is best effort: without it later launches fall back to the
    // acknowledgement alone, which is slower to give up on a wedged instance
    // but never wrong.
    let published = open_info(&names, Some(&security));
    if let Some((_, view)) = published {
        let shared = unsafe { &*view.cast::<SharedInfo>() };
        shared.pid.store(std::process::id(), Ordering::Relaxed);
        shared.hwnd.store(0, Ordering::Relaxed);
        shared.magic.store(INFO_MAGIC, Ordering::Release);
        INFO_VIEW.store(view as isize, Ordering::Release);
    }
    ACK_EVENT.store(show_handle_value(ack), Ordering::Release);

    LISTENER_STOP.store(false, Ordering::Release);
    let show_raw = show_handle_value(show);
    let listener = match std::thread::Builder::new()
        .name("tm-show-listener".into())
        .spawn(move || {
            loop {
                let show = HANDLE(show_raw as *mut core::ffi::c_void);
                if unsafe { WaitForSingleObject(show, INFINITE) } != WAIT_OBJECT_0 {
                    break;
                }
                if LISTENER_STOP.load(Ordering::Acquire) {
                    break;
                }
                on_show();
            }
        }) {
        Ok(listener) => Some(listener),
        Err(error) => {
            // Extremely low-resource fallback: the UI polls the same event on
            // its repaint cadence, so a failed thread allocation does not
            // permanently break the hotkey.
            tracing::warn!(%error, "show listener could not start; using UI polling");
            SHOW_FALLBACK.store(show_raw, Ordering::Release);
            None
        }
    };
    Role::Primary(Primary {
        mutex,
        show,
        ack,
        mapping: published.map_or(HANDLE(std::ptr::null_mut()), |(mapping, _)| mapping),
        view: MEMORY_MAPPED_VIEW_ADDRESS {
            Value: published.map_or(std::ptr::null_mut(), |(_, view)| {
                view.cast::<core::ffi::c_void>()
            }),
        },
        listener,
        owns_mutex,
    })
}

fn show_handle_value(handle: HANDLE) -> isize {
    handle.0 as isize
}

/// Whether the UI must poll for show requests (the listener thread could not
/// be created).
pub fn show_polling_armed() -> bool {
    SHOW_FALLBACK.load(Ordering::Acquire) != 0
}

/// Poll the show event on the UI's own cadence. Only ever true on the
/// fallback path; otherwise the listener thread delivers the request.
pub fn poll_show_request() -> bool {
    let raw = SHOW_FALLBACK.load(Ordering::Acquire);
    if raw == 0 {
        return false;
    }
    unsafe { WaitForSingleObject(HANDLE(raw as *mut core::ffi::c_void), 0) == WAIT_OBJECT_0 }
}

impl Drop for Primary {
    fn drop(&mut self) {
        SHOW_FALLBACK.store(0, Ordering::Release);
        ACK_EVENT.store(0, Ordering::Release);
        INFO_VIEW.store(0, Ordering::Release);
        unsafe {
            if let Some(listener) = self.listener.take() {
                LISTENER_STOP.store(true, Ordering::Release);
                let _ = SetEvent(self.show);
                let _ = listener.join();
            }
            if !self.view.Value.is_null() {
                let _ = UnmapViewOfFile(self.view);
            }
            if !self.mapping.is_invalid() {
                let _ = CloseHandle(self.mapping);
            }
            if self.owns_mutex {
                let _ = ReleaseMutex(self.mutex);
            }
            let _ = CloseHandle(self.ack);
            let _ = CloseHandle(self.show);
            let _ = CloseHandle(self.mutex);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Objects for one test, torn down with the guard.
    struct Fixture {
        names: Names,
        mutex: HANDLE,
        mapping: Option<HANDLE>,
        view: *mut u8,
    }

    impl Fixture {
        fn new(scope: &str) -> Self {
            let names = names_for(scope);
            let mutex =
                unsafe { CreateMutexW(None, true, PCWSTR(names.mutex.as_ptr())) }.expect("mutex");
            Self {
                names,
                mutex,
                mapping: None,
                view: std::ptr::null_mut(),
            }
        }

        /// Publish a record the way a primary instance would.
        fn publish(&mut self, pid: u32, hwnd: u64) {
            let (mapping, view) = open_info(&self.names, Some(&None)).expect("section");
            let shared = unsafe { &*view.cast::<SharedInfo>() };
            shared.pid.store(pid, Ordering::Relaxed);
            shared.hwnd.store(hwnd, Ordering::Relaxed);
            shared.magic.store(INFO_MAGIC, Ordering::Release);
            self.mapping = Some(mapping);
            self.view = view;
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe {
                if !self.view.is_null() {
                    let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.view.cast::<core::ffi::c_void>(),
                    });
                }
                if let Some(mapping) = self.mapping {
                    let _ = CloseHandle(mapping);
                }
                let _ = ReleaseMutex(self.mutex);
                let _ = CloseHandle(self.mutex);
            }
        }
    }

    fn scope(name: &str) -> String {
        format!(".test-{}-{name}", std::process::id())
    }

    #[test]
    fn objects_are_session_local_and_versioned() {
        let names = names_for("");
        let text = String::from_utf16_lossy(&names.mutex[..names.mutex.len() - 1]);
        assert_eq!(text, r"Local\TaskMan.Instance.v2");
    }

    /// A launch that finds nothing must start; it must never mistake an
    /// absent instance for one that is merely quiet.
    #[test]
    fn nothing_registered_reports_no_instance() {
        let names = names_for(&scope("empty"));
        assert_eq!(activate_in(&names), Activation::NoInstance);
    }

    /// The record survives the round trip through shared memory, and an
    /// unwritten section reads as absent rather than as a zeroed record.
    #[test]
    fn published_record_round_trips() {
        let mut fixture = Fixture::new(&scope("record"));
        assert!(read_primary_info(&fixture.names).is_none());
        fixture.publish(4321, 0x1234);
        let info = read_primary_info(&fixture.names).expect("published record");
        assert_eq!(info.pid, 4321);
        assert_eq!(info.hwnd, Some(0x1234));
    }

    /// A record whose process is gone must not cost the launch its whole
    /// acknowledgement deadline: the hotkey has to produce a task manager.
    #[test]
    fn a_dead_owner_is_not_waited_for() {
        let mut fixture = Fixture::new(&scope("dead"));
        // Never a live process: high, unassigned, and a multiple of four.
        fixture.publish(0xFFFF_FFF0, 0);
        let started = std::time::Instant::now();
        assert_eq!(activate_in(&fixture.names), Activation::Unresponsive);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(FIRST_ACK_MS as u64),
            "the dead-owner check waited for the acknowledgement deadline"
        );
    }

    #[test]
    fn liveness_follows_the_process_and_not_the_record() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0xFFFF_FFF0));
        assert!(!process_alive(0));
    }

    /// Nothing published means "cannot tell", which must keep the launch on
    /// the acknowledgement path rather than duplicating the instance.
    #[test]
    fn a_missing_record_is_not_treated_as_a_dead_owner() {
        let fixture = Fixture::new(&scope("norecord"));
        assert!(read_primary_info(&fixture.names).is_none());
    }
}
