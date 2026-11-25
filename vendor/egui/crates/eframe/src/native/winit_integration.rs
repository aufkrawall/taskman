use std::{sync::Arc, time::Instant};

use winit::{
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};

use egui::ViewportId;
#[cfg(feature = "accesskit")]
use egui_winit::accesskit_winit;

/// Returns `true` if the window is invisible or minimized.
///
/// These windows don't receive `RedrawRequested` events on Windows,
/// so they need special handling to keep processing viewport commands.
pub fn is_invisible_or_minimized(window: &Window) -> bool {
    window.is_visible() == Some(false) || window.is_minimized() == Some(true)
}

/// On Mac, a minimized window uses up all CPU:
/// <https://github.com/emilk/egui/issues/325>
///
/// On Windows, an invisible window also uses up all CPU:
/// <https://github.com/emilk/egui/issues/7776>
pub fn sleep_if_invisible_or_minimized(window: Option<&Window>) {
    if window.is_some_and(is_invisible_or_minimized) {
        profiling::scope!("minimized_sleep");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// ----------------------------------------------------------------------------
// Resident GPU state while hidden:

/// How long a hidden window keeps its device and its pipelines resident.
///
/// A hide is usually the start of a short detour - a minimize, a tray toggle, another window
/// covering ours - and the restore is what the user waits for. Inside this window the renderer
/// keeps the objects that are expensive to rebuild, so the restore paints on its very next
/// frame. Past it they are released again, so a long-idle app returns to the footprint it had
/// before. The size-dependent targets (swapchain back buffers, depth and MSAA textures) and the
/// CPU-side atlas copies do not wait: they go on the first hidden frame either way, and what
/// the grace window buys is only the device plus the pipelines, at the cost of those staying
/// resident - the wgpu device's own memory blocks and its descriptor heaps included - while
/// the window is hidden.
pub const HIDDEN_RESIDENCY_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// What a renderer should do with the GPU objects that are resident for a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidencyAction {
    /// A viewport can paint: keep every object, surfaces included.
    Keep,

    /// Hidden, still inside [`HIDDEN_RESIDENCY_GRACE`]: release the surfaces and the
    /// size-dependent targets, but keep the device and its pipelines.
    Pause,

    /// Hidden and already paused: there is nothing left to release yet.
    KeepResident,

    /// Hidden for at least [`HIDDEN_RESIDENCY_GRACE`]: release the device and the pipelines too.
    Suspend,
}

/// Decide what happens to the resident GPU objects of a window.
///
/// `visible` is whether a viewport can paint; a minimized or occluded window already counts as
/// hidden there, because [`egui::ViewportInfo::visible`] reports those as not visible.
/// `hidden_for` is how long the window has been hidden and is `None` while it can paint, and
/// `paused` says the pause that [`ResidencyAction::Pause`] asked for is still in effect.
///
/// Pure on purpose: the caller owns the clock, so the policy - and not a timer or a sleep - is
/// what the tests pin.
#[must_use]
pub fn residency_action(
    visible: bool,
    hidden_for: Option<std::time::Duration>,
    paused: bool,
    grace: std::time::Duration,
) -> ResidencyAction {
    if visible {
        ResidencyAction::Keep
    } else if hidden_for.is_some_and(|hidden_for| hidden_for >= grace) {
        ResidencyAction::Suspend
    } else if paused {
        ResidencyAction::KeepResident
    } else {
        ResidencyAction::Pause
    }
}

/// Remembers when a window stopped painting, so [`residency_action`] can be asked without the
/// caller keeping its own timestamps.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResidencyTimer {
    /// `None` while a viewport can paint, otherwise when the first hidden frame was seen.
    hidden_since: Option<Instant>,

    /// The renderer already released its surfaces but kept the device.
    paused: bool,

    /// The renderer already released the device past the grace, so nothing more can go.
    suspended: bool,
}

impl ResidencyTimer {
    /// Note this frame's visibility and return what the renderer should do with its resident
    /// GPU objects.
    ///
    /// This is the only place the clock enters the decision. Turn the remaining grace into a
    /// repaint with [`Self::remaining_grace`], so a hidden window leaves this loop and comes
    /// back once, when the device can go.
    pub fn observe(&mut self, now: Instant, visible: bool) -> ResidencyAction {
        let hidden_for = self.hidden_since.map(|since| now.saturating_duration_since(since));
        let action = residency_action(visible, hidden_for, self.paused, HIDDEN_RESIDENCY_GRACE);

        match action {
            ResidencyAction::Keep => {
                self.hidden_since = None;
                self.paused = false;
                self.suspended = false;
            }
            ResidencyAction::Pause => {
                self.hidden_since.get_or_insert(now);
                self.paused = true;
                self.suspended = false;
            }
            ResidencyAction::KeepResident => {}
            ResidencyAction::Suspend => {
                self.paused = false;
                self.suspended = true;
            }
        }

        action
    }

    /// Whether the release [`ResidencyAction::Suspend`] asks for already happened.
    ///
    /// A window that stays hidden past the grace keeps resolving to `Suspend` on every frame
    /// it runs, and releasing an already released device is a no-op, so callers report the
    /// transition instead of every frame.
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// The part of [`HIDDEN_RESIDENCY_GRACE`] that is still left.
    pub fn remaining_grace(&self, now: Instant) -> std::time::Duration {
        let hidden_for = match self.hidden_since {
            Some(hidden_since) => now.saturating_duration_since(hidden_since),
            None => std::time::Duration::ZERO,
        };
        HIDDEN_RESIDENCY_GRACE.saturating_sub(hidden_for)
    }
}

/// Create an egui context, restoring it from storage if possible.
pub fn create_egui_context(storage: Option<&dyn crate::Storage>) -> egui::Context {
    profiling::function_scope!();

    pub const IS_DESKTOP: bool = cfg!(any(
        target_os = "freebsd",
        target_os = "linux",
        target_os = "macos",
        target_os = "openbsd",
        target_os = "windows",
    ));

    let egui_ctx = egui::Context::default();

    egui_ctx.set_embed_viewports(!IS_DESKTOP);

    egui_ctx.options_mut(|o| {
        // eframe supports multi-pass (Context::request_discard).
        #[expect(clippy::unwrap_used)]
        {
            o.max_passes = 2.try_into().unwrap();
        }
    });

    let memory = crate::native::epi_integration::load_egui_memory(storage).unwrap_or_default();
    egui_ctx.memory_mut(|mem| *mem = memory);

    egui_ctx
}

/// The custom even `eframe` uses with the [`winit`] event loop.
#[derive(Debug)]
pub enum UserEvent {
    /// A repaint is requested.
    RequestRepaint {
        /// What to repaint.
        viewport_id: ViewportId,

        /// When to repaint.
        when: Instant,

        /// What the cumulative pass number was when the repaint was _requested_.
        cumulative_pass_nr: u64,
    },

    /// A request related to [`accesskit`](https://accesskit.dev/).
    #[cfg(feature = "accesskit")]
    AccessKitActionRequest(accesskit_winit::Event),
}

#[cfg(feature = "accesskit")]
impl From<accesskit_winit::Event> for UserEvent {
    fn from(inner: accesskit_winit::Event) -> Self {
        Self::AccessKitActionRequest(inner)
    }
}

pub trait WinitApp {
    fn egui_ctx(&self) -> Option<&egui::Context>;

    fn window(&self, window_id: WindowId) -> Option<Arc<Window>>;

    fn window_id_from_viewport_id(&self, id: ViewportId) -> Option<WindowId>;

    fn save(&mut self);

    fn save_and_destroy(&mut self);

    fn run_ui_and_paint(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
    ) -> crate::Result<EventResult>;

    fn suspended(&mut self, event_loop: &ActiveEventLoop) -> crate::Result<EventResult>;

    fn resumed(&mut self, event_loop: &ActiveEventLoop) -> crate::Result<EventResult>;

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) -> crate::Result<EventResult>;

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: winit::event::WindowEvent,
    ) -> crate::Result<EventResult>;

    #[cfg(feature = "accesskit")]
    fn on_accesskit_event(&mut self, event: accesskit_winit::Event) -> crate::Result<EventResult>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventResult {
    Wait,

    /// Causes a synchronous repaint inside the event handler. This should only
    /// be used in special situations if the window must be repainted while
    /// handling a specific event. This occurs on Windows when handling resizes.
    ///
    /// `RepaintNow` creates a new frame synchronously, and should therefore
    /// only be used for extremely urgent repaints.
    RepaintNow(WindowId),

    /// Queues a repaint for once the event loop handles its next redraw. Exists
    /// so that multiple input events can be handled in one frame. Does not
    /// cause any delay like `RepaintNow`.
    RepaintNext(WindowId),

    RepaintAt(WindowId, Instant),

    /// Causes a save of the client state when the persistence feature is enabled.
    Save,

    /// Starts the process of ending eframe execution whilst allowing for proper
    /// clean up of resources.
    ///
    /// # Warning
    /// This event **must** occur before [`Exit`] to correctly exit eframe code.
    /// If in doubt, return this event.
    ///
    /// [`Exit`]: [EventResult::Exit]
    CloseRequested,

    /// The event loop will exit, now.
    /// The correct circumstance to return this event is in response to a winit "Destroyed" event.
    ///
    /// # Warning
    /// The [`CloseRequested`] **must** occur before this event to ensure that winit
    /// is able to remove any open windows. Otherwise the window(s) will remain open
    /// until the program terminates.
    ///
    /// [`CloseRequested`]: EventResult::CloseRequested
    Exit,
}

#[cfg(feature = "accesskit")]
pub(crate) fn on_accesskit_window_event(
    egui_winit: &mut egui_winit::State,
    window_id: WindowId,
    event: &accesskit_winit::WindowEvent,
) -> EventResult {
    match event {
        accesskit_winit::WindowEvent::InitialTreeRequested => {
            egui_winit.egui_ctx().enable_accesskit();
            // Because we can't provide the initial tree synchronously
            // (because that would require the activation handler to access
            // the same mutable state as the winit event handler), some
            // AccessKit platform adapters will use a placeholder tree
            // until we send the first tree update. To minimize the possible
            // bad effects of that workaround, repaint and send the tree
            // immediately.
            EventResult::RepaintNow(window_id)
        }
        accesskit_winit::WindowEvent::ActionRequested(request) => {
            egui_winit.on_accesskit_action_request(request.clone());
            // As a form of user input, accessibility actions should cause
            // a repaint, but not until the next regular frame.
            EventResult::RepaintNext(window_id)
        }
        accesskit_winit::WindowEvent::AccessibilityDeactivated => {
            egui_winit.egui_ctx().disable_accesskit();
            // Disabling AccessKit support should have no visible effect,
            // so there's no need to repaint.
            EventResult::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn action(visible: bool, hidden_for: Option<Duration>, paused: bool) -> ResidencyAction {
        residency_action(visible, hidden_for, paused, HIDDEN_RESIDENCY_GRACE)
    }

    #[test]
    fn a_paintable_viewport_keeps_every_resident_gpu_object() {
        assert_eq!(action(true, None, false), ResidencyAction::Keep);
        // Coming back from a pause leaves nothing of the pause behind.
        let hidden_for = Some(Duration::from_secs(1));
        assert_eq!(action(true, hidden_for, true), ResidencyAction::Keep);
    }

    #[test]
    fn the_first_hidden_frame_pauses_instead_of_suspending() {
        assert_eq!(action(false, None, false), ResidencyAction::Pause);
        let just_hidden = Some(Duration::from_millis(1));
        assert_eq!(action(false, just_hidden, false), ResidencyAction::Pause);
    }

    #[test]
    fn a_paused_window_stays_resident_until_the_grace_expires() {
        let inside = Some(HIDDEN_RESIDENCY_GRACE - Duration::from_millis(1));
        assert_eq!(action(false, inside, true), ResidencyAction::KeepResident);

        let expired = Some(HIDDEN_RESIDENCY_GRACE);
        assert_eq!(action(false, expired, true), ResidencyAction::Suspend);
        // A hide that never got to pause still ends in a suspend once the grace is over.
        assert_eq!(action(false, expired, false), ResidencyAction::Suspend);
    }

    #[test]
    fn the_timer_restarts_the_grace_every_time_the_window_hides() {
        let mut timer = ResidencyTimer::default();
        let t0 = Instant::now();

        assert_eq!(timer.observe(t0, false), ResidencyAction::Pause);
        assert_eq!(timer.remaining_grace(t0), HIDDEN_RESIDENCY_GRACE);

        let inside = t0 + Duration::from_secs(1);
        assert_eq!(timer.observe(inside, false), ResidencyAction::KeepResident);
        let left = HIDDEN_RESIDENCY_GRACE - Duration::from_secs(1);
        assert_eq!(timer.remaining_grace(inside), left);

        // Restored: the pause is over, and the next hide gets a full grace again.
        assert_eq!(timer.observe(inside, true), ResidencyAction::Keep);
        assert_eq!(timer.remaining_grace(inside), HIDDEN_RESIDENCY_GRACE);
        assert_eq!(timer.observe(inside, false), ResidencyAction::Pause);
    }

    #[test]
    fn the_grace_expiring_suspends_and_arms_nothing_more() {
        let mut timer = ResidencyTimer::default();
        let t0 = Instant::now();
        assert_eq!(timer.observe(t0, false), ResidencyAction::Pause);
        assert!(!timer.is_suspended());

        let after_grace = t0 + HIDDEN_RESIDENCY_GRACE;
        assert_eq!(timer.observe(after_grace, false), ResidencyAction::Suspend);
        assert_eq!(timer.remaining_grace(after_grace), Duration::ZERO);
        // The release is the transition: the timer says so once, so the caller logs it once.
        assert!(timer.is_suspended());
        // Still hidden and still past the grace: nothing more to release.
        assert_eq!(timer.observe(after_grace, false), ResidencyAction::Suspend);
        assert!(timer.is_suspended());

        // Restored, then hidden again: the next hide pauses again from a resident device.
        assert_eq!(timer.observe(after_grace, true), ResidencyAction::Keep);
        assert!(!timer.is_suspended());
        assert_eq!(timer.observe(after_grace, false), ResidencyAction::Pause);
        assert!(!timer.is_suspended());
    }
}
