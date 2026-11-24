use emath::{Rect, Vec2, vec2};

use crate::{InputOptions, Modifiers, MouseWheelUnit, TouchPhase};

/// The current state of scrolling.
///
/// There are two important types of scroll input deviced:
/// * Discreen scroll wheels on a mouse
/// * Smooth scroll input from a trackpad
///
/// Scroll wheels will usually fire one single scroll event,
/// so it is important that egui smooths it out over time.
///
/// On the contrary, trackpads usually provide smooth scroll input,
/// and with kinetic scrolling (which on Mac is implemented by the OS)
/// scroll events can arrive _after_ the user lets go of the trackpad.
///
/// In either case, we consider use to be scrolling until there is no more
/// scroll events expected.
///
/// This means there are a few different states we can be in:
/// * Not scrolling
/// * "Smooth scrolling" (low-pass filter of discreet scroll events)
/// * Trackpad-scrolling (we receive begin/end phases for these)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not scrolling,
    Static,

    /// We're smoothing out previous scroll events
    Smoothing,

    // We're in-between [`TouchPhase::Start`] and [`TouchPhase::End`] of a trackpad scroll.
    InTouch,
}

/// Keeps track of wheel (scroll) input.
#[derive(Clone, Debug)]
pub struct WheelState {
    /// Are we currently in a scroll action?
    ///
    /// This may be true even if no scroll events came in this frame,
    /// but we are in a kinetic scroll or in a smoothed scroll.
    pub status: Status,

    /// The modifiers at the start of the scroll.
    pub modifiers: Modifiers,

    /// Time of the last scroll event.
    pub last_wheel_event: f64,

    /// Used for smoothing the scroll delta.
    pub unprocessed_wheel_delta: Vec2,
    /// Pending non-scroll deltas whose modifiers request zoom.
    pub unprocessed_zoom_delta: Vec2,

    /// How many points the user scrolled, smoothed over a few frames.
    ///
    /// The delta dictates how the _content_ should move.
    ///
    /// A positive X-value indicates the content is being moved right,
    /// as when swiping right on a touch-screen or track-pad with natural scrolling.
    ///
    /// A positive Y-value indicates the content is being moved down,
    /// as when swiping down on a touch-screen or track-pad with natural scrolling.
    ///
    /// [`crate::ScrollArea`] will both read and write to this field, so that
    /// at the end of the frame this will be zero if a scroll-area consumed the delta.
    pub smooth_wheel_delta: Vec2,
    /// Smoothed zoom delta for this pass.
    pub smooth_zoom_delta: Vec2,

    /// `time` of the pass that started the current smoothing burst,
    /// or `None` when no delta is pending.
    ///
    /// TASKMAN-FORK (#17): the burst is bounded in wall clock time from here,
    /// see `WHEEL_BURST_DECAY_SECONDS`.
    pub burst_start: Option<f64>,
    /// How many passes the current smoothing burst has taken so far.
    ///
    /// TASKMAN-FORK (#17): bounds the burst even when the `dt` it is given says
    /// less time passed than really did, see `WHEEL_BURST_MAX_FRAMES`.
    pub burst_frames: u32,
}

impl Default for WheelState {
    fn default() -> Self {
        Self {
            status: Status::Static,
            modifiers: Default::default(),
            last_wheel_event: f64::NEG_INFINITY,
            unprocessed_wheel_delta: Vec2::ZERO,
            unprocessed_zoom_delta: Vec2::ZERO,
            smooth_wheel_delta: Vec2::ZERO,
            smooth_zoom_delta: Vec2::ZERO,
            burst_start: None,
            burst_frames: 0,
        }
    }
}

// TASKMAN-FORK (#17): a wheel notch is smoothed as a *burst* — a finite decay
// that lands on exactly zero — instead of the asymptotic filter upstream used.
//
// Upstream smoothed with `exponential_smooth_factor(0.90, 0.1, dt)`, i.e. 90 %
// of the notch within 0.1 s, and let the remaining 10 % (4 px of a 40 px notch)
// trickle out over another 0.2 s in ever-smaller discrete steps, ended by a
// `< 1.0` px lump flush. Any frame that arrived later than the `stable_dt` the
// factor was computed from stretched that trickle in wall-clock time — which is
// the delayed mini-scroll users saw after the real motion.
//
// The curve below reaches 99.9 % in the same 0.1 s: `1 - 0.001^(dt / 0.1)` per
// pass is 68.4 % at 60 Hz, 43.8 % at 120 Hz and 25.0 % at 240 Hz, so one 40 px
// notch becomes 27.35 / 8.65 / 2.74 / 0.86 / 0.40 px at 60 Hz — five passes,
// 83 ms, every step smaller than the one before it at that rate and the last one
// exact (the bare curve would still be at 0.04 px after 0.1 s; the epsilon below
// lands that sliver instead of dribbling it out).
//
// What the ending pass can land is bounded, not zero: the epsilon flush adds at
// most `WHEEL_BURST_TAIL_EPSILON` (0.2 pt) to the curve's own step, which
// measures 1.0 % of a notch at 30-60 Hz (0.40 px), 0.56 % at 120-240 Hz
// (0.22 px); the clock budget lands whatever the curve still owes after the
// full 0.1 s of decay, which is 0.1 % of the impulse by construction. Both are
// sub-pixel for a single notch, and neither can leave a residual for a later
// frame.
const WHEEL_BURST_DECAY_SECONDS: f32 = 0.1;

/// Fraction of a burst the decay curve delivers within
/// [`WHEEL_BURST_DECAY_SECONDS`].
const WHEEL_BURST_REACH: f32 = 0.999;

/// Failsafe cap on the passes one burst may take, independent of `dt`.
///
/// The curve needs 5 passes at 60 Hz, 10 at 120 Hz and 19 at 240 Hz, and the
/// clock budget below ends any of those at its own 0.1 s — a 640 px coalesced
/// spin at 500 Hz still lands through the budget, at pass 50, with 0.7 px left.
/// The cap only binds where 64 passes cover less than the decay window, i.e.
/// above ~640 Hz, or where the clock budget is fooled: a loop whose `dt` *and*
/// `time` both advance nominally while its frames really arrive far apart. Both
/// are worth bounding, because there the decay would otherwise crawl for
/// seconds. The capped pass lands the whole remainder instead of decaying
/// another fraction of it; a jump then is strictly better than a crawl, and by
/// then the remainder is sub-pixel whenever the clock was honest.
const WHEEL_BURST_MAX_FRAMES: u32 = 64;

/// A residual this small (in points) is not worth another frame — 0.2 pt is a
/// fifth of a pixel, so delivering it as one step cannot be seen. It must stay
/// comfortably above `WHEEL_SMOOTHING_RESIDUAL_PX` in `input_state/mod.rs`, the
/// size below which `InputState::wants_repaint_after` stops asking for a
/// follow-up pass: this epsilon lands every remainder that would otherwise be
/// below that line, so no burst can leave a sliver for some later, unrelated
/// frame to deliver as a lone mini-step.
const WHEEL_BURST_TAIL_EPSILON: f32 = 0.2;

impl WheelState {
    #[expect(clippy::too_many_arguments)]
    pub fn on_wheel_event(
        &mut self,
        viewport_rect: Rect,
        options: &InputOptions,
        time: f64,
        unit: MouseWheelUnit,
        delta: Vec2,
        phase: TouchPhase,
        latest_modifiers: Modifiers,
    ) {
        self.last_wheel_event = time;
        match phase {
            crate::TouchPhase::Start => {
                self.status = Status::InTouch;
                self.modifiers = latest_modifiers;
            }
            crate::TouchPhase::Move => {
                match self.status {
                    Status::Static | Status::Smoothing => {
                        self.modifiers = latest_modifiers;
                        self.status = Status::Smoothing;
                    }
                    Status::InTouch => {
                        // If the user lets go of a modifier - ignore it.
                        // More kinematic scrolling may arrive.
                        // But if the users presses down new modifiers - heed it!
                        self.modifiers |= latest_modifiers;
                    }
                }

                let mut delta = match unit {
                    MouseWheelUnit::Point => delta,
                    MouseWheelUnit::Line => options.line_scroll_speed * delta,
                    MouseWheelUnit::Page => viewport_rect.height() * delta,
                };

                let is_horizontal = self
                    .modifiers
                    .matches_any(options.horizontal_scroll_modifier);
                let is_vertical = self.modifiers.matches_any(options.vertical_scroll_modifier);

                if is_horizontal && !is_vertical {
                    // Treat all scrolling as horizontal scrolling.
                    // Note: one Mac we already get horizontal scroll events when shift is down.
                    delta = vec2(delta.x + delta.y, 0.0);
                }
                if !is_horizontal && is_vertical {
                    // Treat all scrolling as vertical scrolling.
                    delta = vec2(0.0, delta.x + delta.y);
                }

                // Mouse wheels often go very large steps.
                // A single notch on a logitech mouse wheel connected to a Macbook returns 14.0 raw scroll delta.
                // So we smooth it out over several frames for a nicer user experience when scrolling in egui.
                // BUT: if the user is using a nice smooth mac trackpad, we don't add smoothing,
                // because it adds latency.
                let is_smooth = self.status == Status::InTouch
                    || match unit {
                        MouseWheelUnit::Point => delta.length() < 8.0, // a bit arbitrary here
                        MouseWheelUnit::Line | MouseWheelUnit::Page => false,
                    };

                let is_zoom = self.modifiers.matches_any(options.zoom_modifier);
                if is_smooth {
                    if is_zoom {
                        self.smooth_zoom_delta += delta;
                    } else {
                        self.smooth_wheel_delta += delta;
                    }
                } else {
                    // A new notch starts (or joins) a smoothing burst, so it gets
                    // its own budgets instead of inheriting the frames the
                    // previous notch has already used (TASKMAN-FORK #17). A
                    // zero-delta report — a buffered `Move` with no travel — must
                    // not restart them, or it could hold a pending residual's
                    // clock budget open and clear its frame cap for free.
                    if delta != Vec2::ZERO {
                        self.burst_start = Some(time);
                        self.burst_frames = 0;
                    }
                    if is_zoom {
                        self.unprocessed_zoom_delta += delta;
                    } else {
                        self.unprocessed_wheel_delta += delta;
                    }
                }
            }
            crate::TouchPhase::End => {
                self.status = Status::Static;
                self.modifiers = Default::default();
                // A coalesced batch can contain Start, a real Move and End in
                // one frame. Commit the accumulated line delta before ending
                // so the movement reaches the UI in this same pass.
                //
                // This also lands the residual of a burst that started on an
                // earlier pass, which is upstream's behaviour and the right
                // moment for it: an `End` is the end of the gesture, so the last
                // of its movement belongs to this frame rather than to a
                // follow-up one.
                self.smooth_wheel_delta += self.unprocessed_wheel_delta;
                self.smooth_zoom_delta += self.unprocessed_zoom_delta;
                self.unprocessed_wheel_delta = Default::default();
                self.unprocessed_zoom_delta = Default::default();
                // The batch is delivered, so it must not leave a budget open:
                // `advance_burst` would otherwise report a live burst with
                // nothing pending until its next pass clears the fields.
                self.burst_start = None;
                self.burst_frames = 0;
            }
            crate::TouchPhase::Cancel => {
                self.status = Status::Static;
                self.modifiers = Default::default();
                self.unprocessed_wheel_delta = Default::default();
                self.unprocessed_zoom_delta = Default::default();
                self.smooth_wheel_delta = Default::default();
                self.smooth_zoom_delta = Default::default();
                self.burst_start = None;
                self.burst_frames = 0;
            }
        }
    }

    pub fn after_events(&mut self, time: f64, dt: f32) {
        self.advance_burst(time, dt);

        let time_since_last_scroll = time - self.last_wheel_event;

        if self.status == Status::Smoothing
            && self.smooth_wheel_delta == Vec2::ZERO
            && 0.150 < time_since_last_scroll
        {
            // On certain platforms, like web, we don't get the start & stop scrolling events, so
            // we rely on a timer there.
            //
            // Tested on a mac touchpad 2025, where the largest observed gap between scroll events
            // was 68 ms. But we add some margin to be safe
            self.status = Status::Static;
            self.modifiers = Default::default();
        }
    }

    /// Deliver one pass of the current smoothing burst.
    ///
    /// TASKMAN-FORK (#17): the burst is bounded twice — by wall clock, from the
    /// pass that started it, and by a failsafe frame count. The time bound keeps
    /// a late frame from spreading the decay over more seconds than a notch
    /// should take; the frame bound catches a loop that reports a nominal `dt`
    /// while its frames arrive far apart. Either bound lands the whole
    /// remainder in that pass instead of handing back a tail for some later
    /// frame to dump.
    fn advance_burst(&mut self, time: f64, dt: f32) {
        if self.unprocessed_wheel_delta == Vec2::ZERO && self.unprocessed_zoom_delta == Vec2::ZERO {
            self.burst_start = None;
            self.burst_frames = 0;
            return;
        }

        let start = *self.burst_start.get_or_insert(time);
        let elapsed = (time - start).max(0.0);
        // Two separate triggers on purpose: the clock budget may end a burst
        // mid-curve, so it hands over what the curve still owes; the frame cap
        // is the failsafe for a `dt` that under-reports real time, so it has to
        // stop the decay outright or it would keep crawling.
        let over_budget = elapsed + f64::from(dt) >= f64::from(WHEEL_BURST_DECAY_SECONDS);
        let over_cap = self.burst_frames + 1 >= WHEEL_BURST_MAX_FRAMES;

        let factor = crate::emath::exponential_smooth_factor(
            WHEEL_BURST_REACH,
            WHEEL_BURST_DECAY_SECONDS,
            dt,
        );
        if factor.is_nan() || factor <= 0.0 {
            // A pass that did not advance time cannot advance the decay, and it
            // must not spend the frame budget either. A NaN `dt` would otherwise
            // reach `burst_step` and turn both deltas into a residual that never
            // equals zero.
            return;
        }

        // Scroll and zoom share the curve and the budgets, but never each
        // other's deltas.
        for d in 0..2 {
            let wheel_step = burst_step(
                self.unprocessed_wheel_delta[d],
                factor,
                over_budget,
                over_cap,
            );
            self.smooth_wheel_delta[d] += wheel_step;
            self.unprocessed_wheel_delta[d] -= wheel_step;

            let zoom_step = burst_step(
                self.unprocessed_zoom_delta[d],
                factor,
                over_budget,
                over_cap,
            );
            self.smooth_zoom_delta[d] += zoom_step;
            self.unprocessed_zoom_delta[d] -= zoom_step;
        }

        self.burst_frames = self.burst_frames.saturating_add(1);

        if self.unprocessed_wheel_delta == Vec2::ZERO && self.unprocessed_zoom_delta == Vec2::ZERO {
            // Landed exactly: the next notch starts its own budgets.
            self.burst_start = None;
            self.burst_frames = 0;
        }
    }

    /// True if there is an active scroll action that might scroll more when using [`Self::smooth_wheel_delta`].
    pub fn is_scrolling(&self) -> bool {
        self.status != Status::Static
    }

    pub fn ui(&self, ui: &mut crate::Ui) {
        let Self {
            status,
            modifiers,
            last_wheel_event,
            unprocessed_wheel_delta,
            unprocessed_zoom_delta,
            smooth_wheel_delta,
            smooth_zoom_delta,
            burst_start,
            burst_frames,
        } = self;

        let time = ui.input(|i| i.time);

        crate::Grid::new("ScrollState")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("status");
                ui.monospace(format!("{status:?}"));
                ui.end_row();

                ui.label("modifiers");
                ui.monospace(format!("{modifiers:?}"));
                ui.end_row();

                ui.label("last_wheel_event");
                ui.monospace(format!("{:.1}s ago", time - *last_wheel_event));
                ui.end_row();

                ui.label("unprocessed_wheel_delta");
                ui.monospace(unprocessed_wheel_delta.to_string());
                ui.end_row();

                ui.label("smooth_wheel_delta");
                ui.monospace(smooth_wheel_delta.to_string());
                ui.end_row();

                ui.label("unprocessed_zoom_delta");
                ui.monospace(unprocessed_zoom_delta.to_string());
                ui.end_row();

                ui.label("smooth_zoom_delta");
                ui.monospace(smooth_zoom_delta.to_string());
                ui.end_row();

                ui.label("smoothing burst");
                ui.monospace(match burst_start {
                    Some(start) => {
                        format!("{burst_frames} frames, {:.0} ms", (time - *start) * 1e3)
                    }
                    None => "none".to_owned(),
                });
                ui.end_row();
            });
    }
}

/// How much of one axis of a smoothing burst to hand to the UI this pass.
///
/// TASKMAN-FORK (#17): returns the part of `left` to deliver and leaves the rest
/// as residual, so the caller can zero it explicitly once `left` is `0.0`.
///
/// `over_budget` ends a burst that has spent its 0.1 s of decay. What is left
/// there is `(1 - reach)^(1 - dt / decay)` of the impulse — 0.32 % at 60 Hz,
/// 1 % at 30 Hz — because the budget fires as soon as `elapsed + dt` reaches the
/// window, so handing the rest over in this pass is a sub-pixel step for a
/// single notch and reads as the decay reaching zero. `over_cap` is the
/// failsafe: it stops the decay outright, see [`WHEEL_BURST_MAX_FRAMES`].
fn burst_step(left: f32, factor: f32, over_budget: bool, over_cap: bool) -> f32 {
    let remaining = left.abs();
    if remaining == 0.0 {
        return 0.0;
    }
    if over_cap {
        return left;
    }

    // A natural pass delivers `factor` of what is left; the pass that ends the
    // budget delivers all of it.
    let step = if over_budget {
        remaining
    } else {
        factor * remaining
    };

    // Land early rather than leave a sliver that only a much later frame would
    // deliver (see `WHEEL_BURST_TAIL_EPSILON`). The flush can be up to one
    // epsilon larger than the step the curve wanted, which is at most a fifth of
    // a pixel and therefore invisible.
    let step = if remaining - step <= WHEEL_BURST_TAIL_EPSILON {
        remaining
    } else {
        step
    };

    step * left.signum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use emath::Pos2;

    const DT_60: f32 = 1.0 / 60.0;
    const DT_120: f32 = 1.0 / 120.0;

    /// One `Line` notch, in lines, and the points it is worth at the scroll
    /// speed these tests pin.
    const NOTCH_LINES: f32 = 1.0;
    const NOTCH_PX: f32 = 40.0;

    /// The `InputOptions` these tests feed the kernel.
    ///
    /// `line_scroll_speed` is pinned instead of taken from `InputOptions::default`,
    /// which is 8.0 (not 40.0) on a wasm target — every notch expectation below
    /// would silently be wrong there.
    fn options() -> InputOptions {
        InputOptions {
            line_scroll_speed: 40.0,
            ..Default::default()
        }
    }

    fn viewport() -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))
    }

    /// One wheel notch, fed the way `InputState::begin_pass` feeds it.
    fn wheel_notch(state: &mut WheelState, time: f64, modifiers: Modifiers, lines: f32) {
        state.on_wheel_event(
            viewport(),
            &options(),
            time,
            MouseWheelUnit::Line,
            vec2(0.0, lines),
            TouchPhase::Move,
            modifiers,
        );
    }

    /// Run `passes` smoothing passes like `InputState::begin_pass` does, and
    /// report the scroll delta each pass handed to the UI.
    fn run_passes(state: &mut WheelState, time: &mut f64, dt: f32, passes: usize) -> Vec<f32> {
        let mut steps = Vec::with_capacity(passes);
        for _ in 0..passes {
            // `begin_pass` clears the smooth deltas before every pass.
            state.smooth_wheel_delta = Vec2::ZERO;
            state.smooth_zoom_delta = Vec2::ZERO;
            state.after_events(*time, dt);
            steps.push(state.smooth_wheel_delta.y);
            *time += f64::from(dt);
        }
        steps
    }

    fn notch_and_run(dt: f32, lines: f32, passes: usize) -> (WheelState, Vec<f32>) {
        let mut state = WheelState::default();
        let mut time = 0.0;
        wheel_notch(&mut state, time, Modifiers::NONE, lines);
        let steps = run_passes(&mut state, &mut time, dt, passes);
        (state, steps)
    }

    /// One notch is delivered in full, inside the frame budget, with every step
    /// of the decay at most the one before it.
    #[test]
    fn single_line_notch_lands_within_the_frame_budget() {
        let (state, steps) = notch_and_run(DT_60, NOTCH_LINES, WHEEL_BURST_MAX_FRAMES as usize);

        let total: f32 = steps.iter().sum();
        assert!(
            (total - NOTCH_PX).abs() < 0.01,
            "delivered {total} px instead of {NOTCH_PX} px: {steps:?}"
        );
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);
        assert!(
            steps[0] > 0.0 && steps[0] < NOTCH_PX,
            "first pass was not a partial step: {steps:?}"
        );
        for pair in steps.windows(2) {
            assert!(pair[1] <= pair[0], "step grew again: {steps:?}");
        }
    }

    /// The decay never speeds up again, and a large burst is still spread over
    /// several passes rather than dumped in one.
    #[test]
    fn per_pass_steps_are_monotonically_non_increasing() {
        // 400 px keeps the curve busy for the whole frame budget.
        let (state, steps) =
            notch_and_run(DT_60, 10.0 * NOTCH_LINES, WHEEL_BURST_MAX_FRAMES as usize);

        let total: f32 = steps.iter().sum();
        assert!(
            (total - 400.0).abs() < 0.01,
            "delivered {total} px instead of 400 px: {steps:?}"
        );
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);

        let spread_over = steps.iter().filter(|step| **step > 0.0).count();
        assert!(
            spread_over >= 3,
            "burst was an instant jump, {spread_over} pass(es): {steps:?}"
        );
        for pair in steps.windows(2) {
            assert!(pair[1] <= pair[0], "step grew again: {steps:?}");
        }
    }

    /// A landed burst leaves nothing behind, so no later frame can emit a lone
    /// mini-step.
    #[test]
    fn converged_burst_delivers_nothing_on_later_passes() {
        let mut state = WheelState::default();
        let mut time = 0.0;
        wheel_notch(&mut state, time, Modifiers::NONE, NOTCH_LINES);
        run_passes(
            &mut state,
            &mut time,
            DT_60,
            WHEEL_BURST_MAX_FRAMES as usize,
        );
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);
        assert!(state.burst_start.is_none(), "burst budget was left open");

        let later = run_passes(&mut state, &mut time, DT_60, 10);
        assert!(later.iter().all(|step| *step == 0.0), "{later:?}");
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);
        assert_eq!(state.unprocessed_zoom_delta, Vec2::ZERO);
    }

    /// The same wall-clock of decay delivers the same amount of scroll, no
    /// matter how many frames it is cut into.
    #[test]
    fn burst_total_is_frame_rate_independent() {
        let at_60 = notch_and_run(DT_60, NOTCH_LINES, 5).1.iter().sum::<f32>();
        let at_120 = notch_and_run(DT_120, NOTCH_LINES, 10).1.iter().sum::<f32>();

        // Both runs cover 83 ms of the 100 ms curve.
        assert!(
            (at_60 - at_120).abs() < 0.5,
            "{at_60} px at 60 Hz vs {at_120} px at 120 Hz"
        );
    }

    /// The frame budget is a real cap: even when the reported `dt` under-reports
    /// real time, the burst ends on the capped pass instead of decaying another
    /// fraction of the remainder — which is what let the old tail crawl.
    #[test]
    fn frame_cap_ends_the_burst_instead_of_crawling() {
        // 1 ms reported frames: the bare curve needs ~100 passes to land 40 px,
        // so only the frame cap can end this burst.
        const DT_1MS: f32 = 0.001;
        let mut state = WheelState::default();
        let mut time = 0.0;
        wheel_notch(&mut state, time, Modifiers::NONE, NOTCH_LINES);

        let mut passes = 0;
        while state.unprocessed_wheel_delta != Vec2::ZERO {
            state.smooth_wheel_delta = Vec2::ZERO;
            state.after_events(time, DT_1MS);
            time += f64::from(DT_1MS);
            passes += 1;
            assert!(
                passes <= WHEEL_BURST_MAX_FRAMES as usize,
                "frame cap never ended the burst"
            );
        }

        assert_eq!(passes, WHEEL_BURST_MAX_FRAMES as usize);
        assert!(state.burst_start.is_none(), "burst budget was left open");
    }

    /// Whatever the refresh rate, one notch is completely delivered inside the
    /// clock budget and never steps back up — the tail the user saw is gone.
    #[test]
    fn every_refresh_rate_lands_the_notch_quickly() {
        for (name, dt) in [
            ("60 Hz", DT_60),
            ("120 Hz", DT_120),
            ("240 Hz", 1.0 / 240.0),
        ] {
            let mut state = WheelState::default();
            let mut time = 0.0;
            wheel_notch(&mut state, time, Modifiers::NONE, NOTCH_LINES);

            let mut steps = Vec::new();
            let mut elapsed = 0.0_f32;
            while state.unprocessed_wheel_delta != Vec2::ZERO {
                state.smooth_wheel_delta = Vec2::ZERO;
                state.after_events(time, dt);
                steps.push(state.smooth_wheel_delta.y);
                time += f64::from(dt);
                elapsed += dt;
                assert!(
                    elapsed <= WHEEL_BURST_DECAY_SECONDS + dt,
                    "{name}: burst still alive after {elapsed} s: {steps:?}"
                );
            }

            let total: f32 = steps.iter().sum();
            assert!(
                (total - NOTCH_PX).abs() < 0.01,
                "{name}: delivered {total} px instead of {NOTCH_PX} px: {steps:?}"
            );
            for pair in steps.windows(2) {
                // The early-landing flush may deliver up to one epsilon more
                // than the curve wanted (a fifth of a pixel, invisible); beyond
                // the ending pass the decay never steps back up. (The budget and
                // cap passes deliberately land the whole remainder, so this
                // allowance does not cover them - neither is reached at these
                // rates.)
                assert!(
                    pair[1] <= pair[0] + WHEEL_BURST_TAIL_EPSILON,
                    "{name}: step grew again: {steps:?}"
                );
            }
        }
    }

    /// A late frame must not stretch the burst: once the clock budget is spent,
    /// the remaining delta is delivered instead of trickling on.
    #[test]
    fn a_late_frame_still_lands_on_the_clock_budget() {
        // 50 ms frames: two passes cover the 100 ms budget, so the notch must be
        // complete after them even though the decay curve has barely started.
        const DT_STARVED: f32 = 0.05;
        let (state, steps) = notch_and_run(DT_STARVED, NOTCH_LINES, 2);

        let total: f32 = steps.iter().sum();
        assert!(
            (total - NOTCH_PX).abs() < 0.01,
            "delivered {total} px instead of {NOTCH_PX} px: {steps:?}"
        );
        assert!(steps[1] <= steps[0], "{steps:?}");
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);
    }

    /// Zoom keeps its own residual and its own accounting, on the same pacing.
    #[test]
    fn zoom_burst_stays_out_of_the_scroll_delta() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let mut state = WheelState::default();
        let mut time = 0.0;
        wheel_notch(&mut state, time, ctrl, NOTCH_LINES);

        assert_eq!(state.unprocessed_zoom_delta.y, NOTCH_PX);
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);

        let mut zoom_steps = Vec::with_capacity(WHEEL_BURST_MAX_FRAMES as usize);
        for _ in 0..WHEEL_BURST_MAX_FRAMES as usize {
            state.smooth_wheel_delta = Vec2::ZERO;
            state.smooth_zoom_delta = Vec2::ZERO;
            state.after_events(time, DT_60);
            assert_eq!(
                state.smooth_wheel_delta,
                Vec2::ZERO,
                "zoom leaked into the scroll delta"
            );
            zoom_steps.push(state.smooth_zoom_delta.y);
            time += f64::from(DT_60);
        }

        let total: f32 = zoom_steps.iter().sum();
        assert!(
            (total - NOTCH_PX).abs() < 0.01,
            "delivered {total} px of zoom instead of {NOTCH_PX} px: {zoom_steps:?}"
        );
        assert_eq!(state.unprocessed_zoom_delta, Vec2::ZERO);
        assert_eq!(state.unprocessed_wheel_delta, Vec2::ZERO);

        // … which is exactly the pacing a scroll notch gets.
        let (_, scroll_steps) = notch_and_run(DT_60, NOTCH_LINES, WHEEL_BURST_MAX_FRAMES as usize);
        for (zoom, scroll) in zoom_steps.iter().zip(&scroll_steps) {
            assert!(
                (zoom - scroll).abs() < 1e-6,
                "zoom {zoom_steps:?} vs scroll {scroll_steps:?}"
            );
        }
    }

    /// The kernel's landing epsilon must stay visible to the follow-up-frame
    /// chain.
    ///
    /// `InputState::wants_repaint_after` keeps asking for display-cadence
    /// frames while a residual exceeds `WHEEL_SMOOTHING_RESIDUAL_PX` (0.01 px)
    /// in `input_state`. The kernel lands every remainder at or below
    /// `WHEEL_BURST_TAIL_EPSILON`, so a
    /// residual is always exactly `0` or comfortably above that threshold and no
    /// sliver can hide below it forever. Raising the threshold past the epsilon,
    /// or lowering the epsilon below the threshold, would strand a tail with
    /// nothing left to deliver it.
    #[test]
    fn the_landing_epsilon_stays_above_the_repaint_residual() {
        assert!(
            WHEEL_BURST_TAIL_EPSILON > super::super::WHEEL_SMOOTHING_RESIDUAL_PX,
            "the landing epsilon must leave nothing hidden below the repaint residual"
        );
    }
}
