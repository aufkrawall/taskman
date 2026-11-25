//! The presentation gate: the desktop must never compose this window before it
//! holds a real frame.
//!
//! ## The bug this exists for
//!
//! A window that has never painted has no defined client area — winit
//! registers its class with `hbrBackground: 0`, so there is not even a system
//! background colour to fall back on. DWM composes the window the moment
//! Windows considers it shown, and every eframe backend shows the window with
//! a plain `ShowWindow` after the first *painted* frame, which is not the same
//! thing as a *presented* one: while the window is hidden, its frame is
//! blitted into a DC nothing composites. The gap between "shown" and "DWM has
//! a frame" is the white flash, and it is a gap in a race, so it is occasional.
//!
//! ## What the gate does
//!
//! Three things, and all three are needed:
//!
//! * It **cloaks** the window (`DWMWA_CLOAK`) from installation until the app
//!   says a frame is on screen. A cloaked window is still "shown" to Windows —
//!   it keeps its taskbar button and its focus, and it receives `WM_PAINT` —
//!   but the compositor draws nothing for it.
//! * It **re-cloaks from a WndProc subclass** on the messages that make a
//!   window appear, before returning, and then wakes the app so a frame
//!   follows. The app cannot do this part itself: a taskbar restore notifies
//!   the window before the app's next `update()`, so any cloak the app sets is
//!   one frame too late.
//! * It gives the client area the **app's own background colour**, both as the
//!   `WM_ERASEBKGND` fill and as the window class background, so the worst
//!   case is the app's colour and never the system's white.
//!
//! ## Who uncloaks
//!
//! Only [`present_gate_release`]. Everything else in `tm-platform` that could
//! reveal a window asks [`present_gate_held`] first (`window_chrome::set_cloaked`
//! and `window_chrome::ensure_presentable`). One door is the invariant: no code
//! path can uncloak a window the gate still holds, so no code path can turn the
//! flash back on.
//!
//! The app has to call it from the frame *after* the one that painted — a paint
//! is not on screen until its frame ends — and it has to be willing to call it
//! again on every later frame, because a transition that arrives after a release
//! takes a new hold. Release without a hold is a no-op, so "call it at the start
//! of every frame" is the shape that cannot miss a hold, and
//! [`present_gate_needs_frame`] says whether the app must request a repaint to
//! answer the one it may not have noticed. [`remove_present_gate`] is the other
//! way out and uncloaks as part of taking the gate down, so even an app that
//! gives up on frames can make the window visible again.
//!
//! ## The window class
//!
//! The erase brush is installed as the window *class* background
//! (`GCLP_HBRBACKGROUND`) as well as being filled by the `WM_ERASEBKGND`
//! handler, so an erase the handler declines — an empty update rectangle, or the
//! default procedure erasing on its own — is still the app's colour and never
//! the system's white. That attribute belongs to the class, so it covers every
//! window of the class in this process, not just the gated one: today the app
//! has a single winit window, and any winit window or egui viewport added later
//! would inherit the same background, which is the colour a TaskMan window wants
//! anyway. `teardown` hands the previous brush back before it deletes ours, so
//! nothing of the sort outlives the gate.
//!
//! ## What it cannot cover
//!
//! Cloaking is a single bit per window and other parties own it too: a virtual
//! desktop switch, or the shell uncloaking a window, writes the same attribute
//! without sending this thread a message, and a third party can uncloak at any
//! moment. The transitions below are also not all preemptive — `WM_SIZE` is the
//! post-hoc backstop for restores that send nothing else, so it closes the door
//! right after the window is back rather than before. The background colour is
//! what makes those cases survivable: they cost at worst a frame of the app's
//! own colour, never a frame of the system's white.
//!
//! ## Threads
//!
//! The subclass runs inside `DispatchMessage` on the thread that owns the
//! window, and every mutation of the gate — install, hold, release, teardown —
//! is a call from that thread as well: installation happens where the window was
//! created, release follows a frame. Because of that, a transition and a
//! release can never interleave halfway, which is what keeps `held` and the
//! cloak in step. The readers are the undisciplined ones — `ensure_presentable`
//! is reached from the hotkey worker and from cross-process activation — so the
//! state sits behind one mutex, and nothing calls the app's `wake` callback
//! while it is held.
//!
//! Installation must happen before the window is ever shown (eframe's
//! `CreationContext`, not its first `update`): the cloak is what covers the very
//! first `ShowWindow`, and a gate installed after it arrives too late for that
//! one show.

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetUpdateRect, HBRUSH, HDC, HGDIOBJ,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GCLP_HBRBACKGROUND, IsIconic, IsWindow, SC_MAXIMIZE, SC_RESTORE, SET_WINDOW_POS_FLAGS,
    SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED, SWP_SHOWWINDOW, SetClassLongPtrW, WINDOWPOS,
    WM_ERASEBKGND, WM_NCDESTROY, WM_SHOWWINDOW, WM_SIZE, WM_SYSCOMMAND, WM_WINDOWPOSCHANGED,
};

/// The subclass id. One gate per process, on one window, so any stable value
/// identifies our own subclass when it has to be removed again.
const SUBCLASS_ID: usize = 1;

/// `WM_SYSCOMMAND` carries the command in the high 12 bits; the low 4 are
/// reserved and Windows documents that they must be masked off.
const SC_COMMAND_MASK: usize = 0xfff0;

/// Why the window was about to become composable.
///
/// Which of these actually arrives is a property of the OS, not of the app, so
/// they are all handled and each one is documented with what it is relied on
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// `WM_SHOWWINDOW` with `wParam == TRUE`. **The reliable one**: Windows
    /// sends it exactly when a window's visibility changes, before the window
    /// is composed, and it is the only notification that a hide→show (the
    /// tray path) produces at all. A window that was hidden has no composited
    /// content, so this is unconditional — no hold state is consulted.
    Shown,
    /// `WM_WINDOWPOSCHANGED` carrying `SWP_SHOWWINDOW`, while the gate holds.
    /// Defensive: something asked for the window to be shown while the app
    /// still owes a frame (a restore's own `SetWindowPos`, an activation).
    /// Gated on the hold because `SWP_SHOWWINDOW` is also passed against
    /// already-visible windows by `window_chrome`'s activation path, and
    /// cloaking those would blink a perfectly painted window.
    ShowPositioned,
    /// `WM_SYSCOMMAND` `SC_RESTORE` / `SC_MAXIMIZE` on a **minimized** window.
    /// The pre-restore notification: it arrives before the window is
    /// un-minimized, which is the only moment a taskbar click can be
    /// intercepted at all. Gated on `IsIconic` because a "Restore Down" of a
    /// visible window is also `SC_RESTORE` and must not blink.
    RestoreRequested,
    /// `WM_SIZE` `SIZE_RESTORED` / `SIZE_MAXIMIZED` while the gate holds, or
    /// right after the window was minimized. The post-restore **backstop**, for
    /// the restores that never produced a `WM_SYSCOMMAND` (a programmatic
    /// `ShowWindow(SW_RESTORE)`, a shell path that skips it). Gated on the hold
    /// or on a remembered minimize because a live resize sends the same message
    /// thousands of times.
    Restored,
}

/// What the window looked like when the message arrived. Gathered from the
/// handle so the decision itself stays pure and testable without one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WindowFacts {
    /// `IsIconic`. A minimized window shows nothing of itself, so a restore is
    /// the one moment it has to be cloaked before Windows draws it.
    iconic: bool,
    /// `WINDOWPOS.flags`, and zero for every message that carries no
    /// `WINDOWPOS`. Unpacking the struct is the caller's job precisely because
    /// dereferencing it cannot be part of a pure function.
    pos_flags: u32,
    /// Whether the gate already holds the window off the compositor.
    held: bool,
    /// Whether a `WM_SIZE(SIZE_MINIMIZED)` arrived and no reveal has been
    /// handled since. This is what makes the `WM_SIZE` backstop work for a
    /// restore that produces no `WM_SYSCOMMAND`: the minimize notification
    /// always arrives, so the next size restore is known to be a reveal without
    /// asking the shell to have sent anything else.
    was_iconic: bool,
}

/// The cloak decision for one message, or `None` when the message cannot make
/// an unpainted window appear.
///
/// Pure on purpose: the whole "can the compositor see something it should not"
/// question is settled here, and every branch below is pinned by a unit test.
fn transition_for(message: u32, wparam: usize, facts: WindowFacts) -> Option<Transition> {
    match message {
        // A window becoming visible. Unconditional — see `Transition::Shown`.
        WM_SHOWWINDOW => (wparam != 0).then_some(Transition::Shown),
        WM_WINDOWPOSCHANGED => {
            let flags = SET_WINDOW_POS_FLAGS(facts.pos_flags);
            (facts.held && flags.contains(SWP_SHOWWINDOW)).then_some(Transition::ShowPositioned)
        }
        WM_SYSCOMMAND => {
            let command = wparam & SC_COMMAND_MASK;
            let restoring = command == SC_RESTORE as usize || command == SC_MAXIMIZE as usize;
            (restoring && facts.iconic).then_some(Transition::RestoreRequested)
        }
        WM_SIZE => {
            let restoring = wparam == SIZE_RESTORED as usize || wparam == SIZE_MAXIMIZED as usize;
            (restoring && (facts.held || facts.was_iconic)).then_some(Transition::Restored)
        }
        _ => None,
    }
}

/// One unmask, and the hold state the subclass saw when it happened.
#[derive(Debug, Clone, Copy)]
struct Decision {
    transition: Transition,
    held_before: bool,
}

/// Everything the gate remembers about the window it is installed on.
///
/// The state methods are the gate's policy — hold, release, pending — kept
/// here so they can be exercised without a window, and so the global below is
/// only ever a place to put this struct.
struct Gate {
    hwnd: isize,
    /// The gate is currently what keeps the window off the compositor.
    held: bool,
    /// A transition happened that the app has not answered with a frame yet.
    pending: bool,
    /// A minimize was seen and no reveal has been handled since — see
    /// `WindowFacts::was_iconic`.
    was_iconic: bool,
    /// Client-area erase brush, as a raw handle rather than [`HBRUSH`]. Shared
    /// with the window class background, so it has to outlive the class's
    /// reference to it.
    ///
    /// It is an integer because this struct sits in a `static`, and [`HBRUSH`]
    /// wraps a pointer: it is neither `Send` nor `Sync`, so a
    /// `Mutex<Gate>` holding one could not be a `static` at all. Nothing is lost
    /// — a GDI handle is only meaningful on the thread that owns the window, and
    /// every use of this one (`install`, `teardown`, `WM_ERASEBKGND`) is there.
    brush: isize,
    /// The class background that was there before installation (winit leaves
    /// it null).
    previous_class_brush: isize,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Gate {
    const fn new() -> Self {
        Gate {
            hwnd: 0,
            held: false,
            pending: false,
            was_iconic: false,
            brush: 0,
            previous_class_brush: 0,
            wake: None,
        }
    }

    /// The window is about to be composable: hold it and ask the app for a
    /// frame. Recorded before the caller cloaks, so `held` is already true
    /// before anything else can observe the window.
    fn on_transition(&mut self) {
        self.held = true;
        self.pending = true;
        // The reveal has been handled, so the minimize that preceded it is
        // spent.
        self.was_iconic = false;
    }

    /// `WM_SIZE(SIZE_MINIMIZED)`: the window left the screen. Not a reveal
    /// itself, but it is the notification that makes the next size restore one
    /// — see `WindowFacts::was_iconic`.
    fn note_minimize(&mut self) {
        self.was_iconic = true;
    }

    /// The app is driving a reveal itself (a restore, a cold start). Any
    /// transition it has not answered yet is superseded by the frame it is
    /// about to paint, so the pending flag is dropped rather than left to
    /// request repaints forever.
    fn hold(&mut self) {
        self.held = true;
        self.pending = false;
        // The reveal the app is driving is the one the minimize was waiting
        // for, so a resize after it is just a resize.
        self.was_iconic = false;
    }

    /// End the hold, but only when there is one. An uncloak nobody asked for
    /// is how a window another party cloaked — a virtual-desktop switch, the
    /// shell — ends up visible with no frame behind it.
    fn release(&mut self) -> bool {
        if !self.held {
            return false;
        }
        self.held = false;
        self.pending = false;
        self.was_iconic = false;
        true
    }

    /// Take one pending transition, if the app has not answered it yet.
    fn take_transition(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }
}

static GATE: Mutex<Gate> = Mutex::new(Gate::new());

/// Install the gate on `hwnd` and hold it cloaked immediately.
///
/// Must be called from the thread that owns the window, as early as possible
/// after the window exists. `background` becomes both the client-area erase
/// colour and the window class background; `wake` is called from inside the
/// WndProc on every transition, and is expected to be something like
/// `egui::Context::request_repaint` — thread-safe, and safe to call while the
/// app holds nothing.
///
/// Installing a second time (the app restarts its window across renderer
/// attempts) fully resets the previous installation first: the old subclass is
/// removed, the old hold dropped and the old brush handed back to the class
/// before it is deleted.
pub fn install_present_gate(hwnd: isize, background: [u8; 3], wake: Arc<dyn Fn() + Send + Sync>) {
    if hwnd == 0 {
        tracing::warn!("cannot install the presentation gate on a null window");
        return;
    }
    let window = HWND(hwnd as *mut std::ffi::c_void);
    // SAFETY: the handle came from the window owner; `IsWindow` only asks.
    if !unsafe { IsWindow(Some(window)) }.as_bool() {
        tracing::warn!(
            hwnd,
            "cannot install the presentation gate on a dead window"
        );
        return;
    }
    remove_present_gate();

    // WM_ERASEBKGND and the class background share one brush, so it cannot be
    // deleted while either still refers to it; `teardown` hands the class back
    // first for exactly that reason.
    // SAFETY: `CreateSolidBrush` allocates a GDI object this module owns.
    let brush = unsafe { CreateSolidBrush(super::window_chrome::colorref(background)) };
    // SAFETY: live window, real brush. Cannot fail; if it ever did, the
    // WM_ERASEBKGND handler below still covers the erase.
    let previous_class_brush =
        unsafe { SetClassLongPtrW(window, GCLP_HBRBACKGROUND, brush.0 as isize) as isize };

    // SAFETY: `gate_subclass_proc` is an `extern "system"` fn with the
    // documented subclass signature and no `dwRefData` to get wrong (0 is
    // never dereferenced).
    let subclassed =
        unsafe { SetWindowSubclass(window, Some(gate_subclass_proc), SUBCLASS_ID, 0) }.as_bool();
    if !subclassed {
        // The background colour still stands on its own, so the gate keeps it
        // and keeps tracking the window. Only the transition messages are
        // lost, which is worth a warning and not a hard failure.
        tracing::warn!(
            hwnd,
            "cannot subclass the window; the presentation gate has no transitions"
        );
    }

    {
        let mut gate = tm_core::sync::lock(&GATE);
        gate.hwnd = hwnd;
        gate.brush = brush.0 as isize;
        gate.previous_class_brush = previous_class_brush;
        gate.wake = Some(wake);
        // A fresh installation is a hold: the frame the app is about to paint
        // answers whatever it had not finished answering on the old window.
        gate.hold();
    }
    // Cloak outside the lock: this is a DWM call, and holding the gate's mutex
    // across it buys nothing. The hold is recorded above and stands even when
    // DWM refuses the write, so this reports the mask the window carries rather
    // than the state the gate asked for: `Some(0)` next to the refusal warning
    // says the write did not land, and a nonzero mask names whichever party
    // cloaks the window.
    tracing::debug!(
        hwnd,
        ?background,
        subclassed,
        cloak_bits = ?cloak_window(hwnd),
        "presentation gate installed; gate holding the window"
    );
}

/// Take the gate down: uncloak if it was holding, remove the subclass, hand the
/// window class its old background back and delete the brush.
///
/// Safe to call twice, on the UI thread or after the window is gone, and safe
/// on a zero or stale handle. Also runs on `WM_NCDESTROY` so a window destroyed
/// by somebody else cannot leave the class pointing at a brush this module has
/// already freed.
pub fn remove_present_gate() {
    teardown(false);
}

/// Take the gate down, and mark the window reachable.
///
/// `window_live` exists because only the `WM_NCDESTROY` path can prove the
/// handle is still valid there and then; every other caller has to ask.
fn teardown(window_live: bool) {
    let (hwnd, brush, previous_class_brush, held) = {
        let mut gate = tm_core::sync::lock(&GATE);
        let recorded = (gate.hwnd, gate.brush, gate.previous_class_brush, gate.held);
        gate.hwnd = 0;
        gate.held = false;
        gate.pending = false;
        gate.was_iconic = false;
        gate.brush = 0;
        gate.previous_class_brush = 0;
        gate.wake = None;
        recorded
    };
    if hwnd == 0 {
        return;
    }
    let window = HWND(hwnd as *mut std::ffi::c_void);
    // SAFETY: `IsWindow` only asks about the handle; the window-owning thread
    // is the one calling this.
    let reachable = window_live || unsafe { IsWindow(Some(window)) }.as_bool();
    if !reachable {
        // The window is gone, and with it the only handle that can hand the
        // class its background back. Deleting the brush now would leave the
        // class holding a dangling one, so one brush is deliberately leaked:
        // the only way to get here is a window that never received
        // `WM_NCDESTROY`, which the app's own teardown does not do.
        tracing::debug!(
            hwnd,
            "presentation gate released a window that no longer exists; brush kept alive"
        );
        return;
    }
    if held {
        super::window_chrome::set_cloaked_raw(hwnd, false);
    }
    // SAFETY: live window this module subclassed; removing a subclass that is
    // not there only reports failure.
    unsafe {
        let _ = RemoveWindowSubclass(window, Some(gate_subclass_proc), SUBCLASS_ID);
        SetClassLongPtrW(window, GCLP_HBRBACKGROUND, previous_class_brush);
    }
    if brush != 0 {
        // SAFETY: `brush` is the `CreateSolidBrush` handle from `install`; the
        // class no longer references it, and nothing else was ever handed it.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(brush as *mut std::ffi::c_void));
        }
    }
    tracing::debug!(hwnd, held, "presentation gate removed");
}

/// Cloak `hwnd` and mark the gate as holding it.
///
/// For a reveal the app drives itself: call this before asking the window to
/// appear, then [`present_gate_release`] once a frame has been presented.
/// Cloak the window and mark the gate as holding it.
///
/// For a reveal the app drives itself: call this before asking the window to
/// appear, then [`present_gate_release`] once a frame has been presented. A
/// zero `hwnd` falls back to the handle the gate was installed on, mirroring
/// [`present_gate_release`], so a hold taken without a cached handle is still
/// a cloak and not just a flag.
pub fn present_gate_hold(hwnd: isize) {
    let held_hwnd = {
        let mut gate = tm_core::sync::lock(&GATE);
        gate.hold();
        gate.hwnd
    };
    let target = if hwnd != 0 { hwnd } else { held_hwnd };
    // The hold is recorded before the cloak and stands even when DWM refuses
    // the write, so this reports the mask the window carries rather than what
    // the gate asked for: `Some(0)` is a window nothing cloaks, `None` a state
    // DWM would not read, and a nonzero mask may be the shell's
    // virtual-desktop cloak rather than this gate's.
    if target == 0 {
        tracing::debug!(
            hwnd = target,
            "presentation gate holding the window; no handle to cloak"
        );
    } else {
        tracing::debug!(
            hwnd = target,
            cloak_bits = ?cloak_window(target),
            "presentation gate holding the window"
        );
    }
}

/// Uncloak `hwnd`, clear the hold and report whether a hold was released.
///
/// Refuses to act without a hold: see [`Gate::release`]. This is the only
/// uncloak the gate allows, so an app that calls it once per presented frame
/// can never reveal an unpainted window.
///
/// A zero `hwnd` falls back to the handle the gate was installed on. The gate
/// already owns that handle, and clearing the hold without uncloaking would
/// leave the window invisible with nothing left to reveal it: a caller that
/// lost its cached handle must still be able to let the window out.
pub fn present_gate_release(hwnd: isize) -> bool {
    let (released, held_hwnd) = {
        let mut gate = tm_core::sync::lock(&GATE);
        let released = gate.release();
        (released, gate.hwnd)
    };
    if !released {
        return false;
    }
    let target = if hwnd != 0 { hwnd } else { held_hwnd };
    if target != 0 {
        super::window_chrome::set_cloaked_raw(target, false);
    }
    tracing::debug!(hwnd = target, "presentation gate released the window");
    true
}

/// Whether the gate is currently what keeps the window off the compositor.
pub fn present_gate_held() -> bool {
    tm_core::sync::lock(&GATE).held
}

/// Take one pending transition, answering `true` once per transition.
pub fn present_gate_take_transition() -> bool {
    tm_core::sync::lock(&GATE).take_transition()
}

/// Whether the app owes a frame because a transition is unanswered.
///
/// The app asks this to request a repaint even when no paint was queued: the
/// window was cloaked by a message, so nothing would otherwise draw it.
pub fn present_gate_needs_frame() -> bool {
    tm_core::sync::lock(&GATE).pending
}

/// Classify one message and, when it unmasked the window, record the hold.
///
/// Returns what to log so the caller can re-cloak and wake the app outside the
/// lock.
fn decide(hwnd: HWND, message: u32, wparam: usize, lparam: LPARAM) -> Option<Decision> {
    // Cheap filter first. Hit-testing and input messages are the bulk of what a
    // task-manager window sees, and two user32 calls per message would be
    // waste.
    if !matches!(
        message,
        WM_SHOWWINDOW | WM_WINDOWPOSCHANGED | WM_SYSCOMMAND | WM_SIZE
    ) {
        return None;
    }
    // Going away is not a reveal, but it is the notification that makes the next
    // size restore one, and it is the only one that always arrives.
    if message == WM_SIZE && wparam == SIZE_MINIMIZED as usize {
        let mut gate = tm_core::sync::lock(&GATE);
        if gate.hwnd == hwnd.0 as isize {
            gate.note_minimize();
        }
        return None;
    }
    let pos_flags = if message == WM_WINDOWPOSCHANGED && lparam.0 != 0 {
        // SAFETY: Windows passes a live `WINDOWPOS` for `WM_WINDOWPOSCHANGED`.
        unsafe { (*(lparam.0 as *const WINDOWPOS)).flags.0 }
    } else {
        0
    };
    let (held, was_iconic) = {
        let gate = tm_core::sync::lock(&GATE);
        (gate.held, gate.was_iconic)
    };
    let facts = WindowFacts {
        // SAFETY: the message proves this handle is live on this thread.
        iconic: unsafe { IsIconic(hwnd) }.as_bool(),
        pos_flags,
        held,
        was_iconic,
    };
    let transition = transition_for(message, wparam, facts)?;
    {
        let mut gate = tm_core::sync::lock(&GATE);
        // A transition for a window the gate no longer owns is a late message
        // from a torn-down window; re-cloaking it would resurrect a hold the
        // app has already released.
        if gate.hwnd != hwnd.0 as isize {
            return None;
        }
        gate.on_transition();
    }
    Some(Decision {
        transition,
        held_before: facts.held,
    })
}

/// The `DWMWA_CLOAKED` mask DWM reports for `hwnd`, or `None` when the query
/// fails.
///
/// The value is a mask over every party cloaking the window, not only this
/// gate: `DWM_CLOAKED_APP` (1) is this process's own cloak, `DWM_CLOAKED_SHELL`
/// (2) a virtual-desktop switch, and `DWM_CLOAKED_INHERITED` (4) a cloak
/// inherited from a cloaked owner. Reading the mask is what lets a gate log
/// report what the window carries instead of what the gate asked for, and what
/// keeps a failed read from reading as "not cloaked".
fn read_cloak_bits(hwnd: isize) -> Option<u32> {
    let mut value: u32 = 0;
    // SAFETY: `value` is a live `u32` of the size the call is told to write,
    // and DWM reports a handle it cannot read as an error instead of writing.
    let read = unsafe {
        DwmGetWindowAttribute(
            HWND(hwnd as *mut std::ffi::c_void),
            DWMWA_CLOAKED,
            (&raw mut value).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    read.is_ok().then_some(value)
}

/// Cloak `hwnd` through the shared helper and report the cloak state the
/// compositor holds afterwards.
///
/// `window_chrome::set_cloaked_raw` can only warn about a refused attribute
/// write, so a gate log that repeated its own intent would sit next to that
/// warning contradicting it. Reading the mask back costs one
/// `DwmGetWindowAttribute` per reveal and reports what the compositor holds,
/// not what the gate asked for: `None` the read failed, `Some(bits)` the raw
/// mask.
fn cloak_window(hwnd: isize) -> Option<u32> {
    super::window_chrome::set_cloaked_raw(hwnd, true);
    read_cloak_bits(hwnd)
}

/// Re-cloak before Windows can compose the window, then ask for a frame.
///
/// The wake runs with no gate lock held: it is the app's repaint request, and
/// the frame it schedules is free to take the gate lock itself.
fn unmask(hwnd: HWND, message: u32, decision: Decision) {
    let raw = hwnd.0 as isize;
    // Re-cloaking BEFORE returning from the WndProc is the whole point: the
    // compositor must see a cloaked window at the moment Windows decides it is
    // shown, not one frame later.
    let cloak_bits = cloak_window(raw);
    tracing::debug!(
        hwnd = raw,
        message,
        cause = ?decision.transition,
        held_before = decision.held_before,
        held_after = true,
        ?cloak_bits,
        "presentation gate re-cloaked the window"
    );
    wake_app();
}

/// Run the app's repaint request, if one is installed.
fn wake_app() {
    // The guard is a temporary of this statement, so the callback runs with the
    // gate's mutex released.
    let wake = tm_core::sync::lock(&GATE).wake.clone();
    if let Some(wake) = wake {
        wake();
    }
}

/// Fill the update rectangle with the app's background colour so an erase can
/// never show the system's white. Returns whether a brush was available.
fn erase_with_gate_brush(hwnd: HWND, hdc: HDC) -> bool {
    let brush = tm_core::sync::lock(&GATE).brush;
    if brush == 0 {
        return false;
    }
    let mut rect = RECT::default();
    // SAFETY: live window from the message; `false` leaves the update region to
    // be validated by the real paint, which eframe's software backend relies on
    // (`os_update_rect` reads the region before it validates it).
    if !unsafe { GetUpdateRect(hwnd, Some(&raw mut rect), false) }.as_bool() {
        return false;
    }
    // SAFETY: `hdc` is the device context `WM_ERASEBKGND` was sent with, and the
    // brush is the one `install` created — `teardown` is the only thing that
    // deletes it.
    unsafe {
        FillRect(hdc, &rect, HBRUSH(brush as *mut std::ffi::c_void));
    }
    true
}

/// The subclass that keeps the gate ahead of every window that appears.
///
/// Chains to `DefSubclassProc` for everything it does not own, which is
/// everything but the erase and the four transition messages.
unsafe extern "system" fn gate_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_ERASEBKGND {
        // SAFETY: `wParam` is the device context to erase into.
        if erase_with_gate_brush(hwnd, HDC(wparam.0 as *mut std::ffi::c_void)) {
            // Returning 1 claims the erase; the client area is now the app's
            // own colour rather than an undefined one.
            return LRESULT(1);
        }
        // SAFETY: chaining to the window's own procedure.
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    }
    if message == WM_NCDESTROY {
        // The window is being destroyed and this is its last message. The
        // original procedure must receive it — winit frees the window and drops
        // its per-window state there — so the chain runs FIRST. `DefSubclassProc`
        // only knows what to forward to while this subclass is still installed:
        // comctl32 resolves the next procedure from the subclass record the
        // message arrived through, so a `RemoveWindowSubclass` before the chain
        // leaves nothing to walk and can swallow the notification outright. The
        // handle stays usable for `RemoveWindowSubclass` and `SetClassLongPtrW`
        // until the last procedure returns from this message, which is where
        // `teardown` now runs.
        // SAFETY: chaining to the window's own procedure.
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        teardown(true);
        return result;
    }
    if let Some(decision) = decide(hwnd, message, wparam.0, lparam) {
        unmask(hwnd, message, decision);
    }
    // SAFETY: chaining to the window's own procedure.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(held: bool, iconic: bool, pos_flags: u32) -> WindowFacts {
        WindowFacts {
            iconic,
            pos_flags,
            held,
            was_iconic: false,
        }
    }

    /// A hidden window becoming visible is the one message Windows sends
    /// exactly when visibility changes, and the window has nothing composed to
    /// show, so it cloaks whatever the gate thinks it is holding. The whole
    /// tray path is this message.
    #[test]
    fn a_show_always_cloaks() {
        assert_eq!(
            transition_for(WM_SHOWWINDOW, 1, facts(false, false, 0)),
            Some(Transition::Shown)
        );
        assert_eq!(
            transition_for(WM_SHOWWINDOW, 1, facts(true, false, 0)),
            Some(Transition::Shown)
        );
        assert_eq!(
            transition_for(WM_SHOWWINDOW, 0, facts(true, false, 0)),
            None,
            "WM_SHOWWINDOW(FALSE) is the tray hide, not a reveal"
        );
    }

    /// The taskbar restore is delivered as a command on a still-minimized
    /// window: cloak before Windows un-minimizes it. The same command on a
    /// visible window is "Restore Down", and cloaking that would blink.
    #[test]
    fn a_minimized_restore_command_cloaks_before_the_window_appears() {
        assert_eq!(
            transition_for(WM_SYSCOMMAND, SC_RESTORE as usize, facts(false, true, 0)),
            Some(Transition::RestoreRequested)
        );
        assert_eq!(
            transition_for(WM_SYSCOMMAND, SC_MAXIMIZE as usize, facts(false, true, 0)),
            Some(Transition::RestoreRequested)
        );
        assert_eq!(
            transition_for(WM_SYSCOMMAND, SC_RESTORE as usize, facts(true, false, 0)),
            None,
            "Restore Down of a visible window is not a reveal"
        );
        assert_eq!(
            transition_for(WM_SYSCOMMAND, SC_MAXIMIZE as usize, facts(false, false, 0)),
            None
        );
        // The low four bits are reserved; Windows documents that the command
        // has to be masked off before it is compared.
        assert_eq!(
            transition_for(
                WM_SYSCOMMAND,
                (SC_RESTORE as usize) | 0x0001,
                facts(false, true, 0)
            ),
            Some(Transition::RestoreRequested)
        );
        // Some other system command on a minimized window shows nothing.
        assert_eq!(
            transition_for(WM_SYSCOMMAND, 0xf060, facts(false, true, 0)),
            None,
            "SC_CLOSE is not a reveal"
        );
    }

    /// The post-restore backstop: the size change lands after the window is
    /// already back, so it can only be told apart from a live resize by the
    /// hold or by the minimize it followed. A live resize sends this thousands
    /// of times and must never cloak.
    #[test]
    fn a_restore_size_change_is_a_backstop_only_while_held() {
        assert_eq!(
            transition_for(WM_SIZE, SIZE_RESTORED as usize, facts(true, false, 0)),
            Some(Transition::Restored)
        );
        assert_eq!(
            transition_for(WM_SIZE, SIZE_MAXIMIZED as usize, facts(true, false, 0)),
            Some(Transition::Restored)
        );
        assert_eq!(
            transition_for(WM_SIZE, SIZE_RESTORED as usize, facts(false, false, 0)),
            None,
            "a live resize must not re-cloak the window it is resizing"
        );
        assert_eq!(
            transition_for(WM_SIZE, SIZE_MAXIMIZED as usize, facts(false, false, 0)),
            None
        );
        // SIZE_MINIMIZED (1) is the window going away, not coming back.
        assert_eq!(transition_for(WM_SIZE, 1, facts(true, false, 0)), None);
    }

    /// `window_chrome`'s activation path passes `SWP_SHOWWINDOW` against an
    /// already-visible window on every foreground grab. Cloaking that would
    /// blink a fully painted window, so the hold decides.
    #[test]
    fn a_show_position_never_cloaks_an_unheld_window() {
        let show = SWP_SHOWWINDOW.0;
        assert_eq!(
            transition_for(WM_WINDOWPOSCHANGED, 0, facts(true, false, show)),
            Some(Transition::ShowPositioned)
        );
        assert_eq!(
            transition_for(WM_WINDOWPOSCHANGED, 0, facts(false, false, show)),
            None,
            "activation sets SWP_SHOWWINDOW on visible windows"
        );
        assert_eq!(
            transition_for(WM_WINDOWPOSCHANGED, 0, facts(true, false, 0)),
            None,
            "a move or resize without a show request is not a reveal"
        );
    }

    /// The restore that produces no system command at all — a shell path that
    /// just un-minimizes the window. The minimize notification always arrived,
    /// so the next size restore is known to be a reveal without being held and
    /// without `IsIconic` still being true.
    #[test]
    fn a_size_restore_after_a_minimize_is_a_reveal_without_the_command() {
        let after_minimize = WindowFacts {
            was_iconic: true,
            ..facts(false, false, 0)
        };
        assert_eq!(
            transition_for(WM_SIZE, SIZE_RESTORED as usize, after_minimize),
            Some(Transition::Restored)
        );
        assert_eq!(
            transition_for(WM_SIZE, SIZE_MAXIMIZED as usize, after_minimize),
            Some(Transition::Restored)
        );
        // Not held and nothing was minimized: an ordinary resize.
        assert_eq!(
            transition_for(WM_SIZE, SIZE_RESTORED as usize, facts(false, false, 0)),
            None
        );
    }

    /// The minimize notification is remembered until a reveal consumes it, so a
    /// resize later in the same session cannot be mistaken for a restore.
    #[test]
    fn a_reveal_spends_the_remembered_minimize() {
        let mut gate = Gate::new();
        gate.note_minimize();
        assert!(gate.was_iconic);
        gate.on_transition();
        assert!(!gate.was_iconic, "the reveal that followed it is handled");

        gate.note_minimize();
        gate.hold();
        assert!(!gate.was_iconic, "a reveal the app drives consumes it too");

        gate.note_minimize();
        assert!(gate.release());
        assert!(!gate.was_iconic, "and so does the release");
    }

    /// Everything else must be free. A task-manager window sees a great deal of
    /// input and hit-testing traffic.
    #[test]
    fn unrelated_messages_are_ignored() {
        for message in [15u32, 0x0200, 0x0084, WM_NCDESTROY, WM_ERASEBKGND] {
            assert_eq!(transition_for(message, 1, facts(true, true, 0)), None);
        }
    }

    /// Release is the one door out of the hold, and it must refuse to act when
    /// there is no hold of its own: a stray uncloak is exactly how a window
    /// another party cloaked appears with no frame behind it.
    #[test]
    fn release_only_acts_on_a_hold() {
        let mut gate = Gate::new();
        assert!(!gate.held);
        assert!(!gate.release(), "release without a hold must not act");
        gate.hold();
        assert!(gate.held);
        assert!(gate.release(), "a hold is released exactly once");
        assert!(!gate.held);
        assert!(!gate.release(), "release is idempotent");
    }

    /// The app calls release on every frame, so the release that acts on nothing
    /// must also change nothing: the remembered minimize is what makes the next
    /// size restore a reveal when the shell sends no command for it.
    #[test]
    fn a_release_without_a_hold_keeps_the_remembered_minimize() {
        let mut gate = Gate::new();
        gate.note_minimize();
        assert!(!gate.release(), "no hold, so nothing to release");
        assert!(
            gate.was_iconic,
            "a no-op release must not spend the minimize it never answered"
        );
    }

    /// A transition takes the hold and leaves one frame owed, reported once.
    #[test]
    fn a_transition_holds_the_window_and_is_reported_once() {
        let mut gate = Gate::new();
        assert!(!gate.pending);
        assert!(!gate.take_transition(), "nothing pending, nothing to take");
        gate.on_transition();
        assert!(gate.held, "an unmask takes the hold");
        assert!(gate.pending, "the app owes a frame");
        assert!(gate.take_transition(), "the transition is reported once");
        assert!(!gate.pending);
        assert!(!gate.take_transition(), "and never twice");
    }

    /// Two transitions are two frames; the second one must not be swallowed by
    /// the first being consumed.
    #[test]
    fn every_unmask_is_its_own_frame() {
        let mut gate = Gate::new();
        gate.on_transition();
        assert!(gate.take_transition());
        gate.on_transition();
        assert!(gate.take_transition());
    }

    /// The app holding the window itself supersedes whatever it had not
    /// answered: the frame it is about to paint answers it.
    #[test]
    fn the_app_holding_the_window_drops_a_transition_it_supersedes() {
        let mut gate = Gate::new();
        gate.on_transition();
        assert!(gate.pending);
        gate.hold();
        assert!(gate.held);
        assert!(!gate.pending);
    }

    /// Releasing ends the episode: nothing may still request repaints after the
    /// window is visible again.
    #[test]
    fn release_ends_the_hold_and_any_pending_request() {
        let mut gate = Gate::new();
        gate.on_transition();
        assert!(gate.release());
        assert!(!gate.held);
        assert!(!gate.pending);
    }
}
