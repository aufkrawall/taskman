//! Shared plumbing of the Process Properties list pages (Threads, Modules,
//! Network, Services).
//!
//! The pages poll only while they are on screen: the dialog asks [`Poll::due`]
//! each frame it shows the page, and nothing runs for a page nobody looks at.
//! They follow the app's update speed (paused = frozen) and F5 refreshes the
//! page on demand. Queries never run on the UI thread.

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tm_core::settings::UpdateSpeed;

use crate::app::InFlight;
use crate::theme;
use crate::widgets::tablekit;

/// Paint a right-aligned value into column `column` of a table row.
pub fn num_cell(
    ui: &egui::Ui,
    table: &tablekit::TmTable,
    row: egui::Rect,
    column: usize,
    text: &str,
    pal: &theme::Palette,
    dim: bool,
) {
    let cell = table.col_rect(column, row);
    ui.painter_at(cell).text(
        egui::pos2(cell.right() - 10.0, cell.center().y),
        egui::Align2::RIGHT_CENTER,
        text,
        egui::FontId::proportional(tablekit::FONT_ROW),
        if dim { pal.text_dim } else { pal.text },
    );
}

/// How often a properties page re-queries: never faster than its own `floor`
/// or than the app's update speed, and not at all while updates are paused
/// (`None`; F5 still refreshes on demand, see [`Pace::request`]).
pub fn page_interval(speed: UpdateSpeed, floor: Duration) -> Option<Duration> {
    (speed != UpdateSpeed::Paused).then(|| speed.interval().max(floor))
}

/// When a page queries: once when first shown, then every interval, and
/// whenever the user asks (F5).
#[derive(Default)]
pub struct Pace {
    started: Option<Instant>,
    requested: bool,
}

impl Pace {
    pub fn due(&self, now: Instant, every: Option<Duration>) -> bool {
        self.requested
            || match (self.started, every) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(started), Some(every)) => now.duration_since(started) >= every,
            }
    }

    /// A query started now; it also answers any pending request.
    pub fn mark_started(&mut self, now: Instant) {
        self.started = Some(now);
        self.requested = false;
    }

    /// Query again as soon as nothing is running, regardless of the interval.
    pub fn request(&mut self) {
        self.requested = true;
    }
}

/// Fold one query result into a page. A failure replaces only a list that is
/// not there yet; over a shown list it becomes the `stale` notice, because a
/// transient failure must not blank a page the user is reading.
pub fn merge<T>(
    rows: &mut Option<Result<T, String>>,
    stale: &mut Option<String>,
    result: Result<T, String>,
) {
    match result {
        Err(error) if matches!(rows, Some(Ok(_))) => *stale = Some(error),
        result => {
            *rows = Some(result);
            *stale = None;
        }
    }
}

/// One background query owned by a Process Properties page.
pub struct Poll<T> {
    slot: Arc<Mutex<Option<Result<T, String>>>>,
    in_flight: InFlight,
    pace: Pace,
}

impl<T> Default for Poll<T> {
    fn default() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
            in_flight: InFlight::default(),
            pace: Pace::default(),
        }
    }
}

impl<T: Send + 'static> Poll<T> {
    /// Whether a new query should start now: none is running, and the
    /// [`Pace`] says so for `every` (`None` = paused: first query and F5 only).
    pub fn due(&self, now: Instant, every: Option<Duration>) -> bool {
        !self.in_flight.busy() && self.pace.due(now, every)
    }

    pub fn busy(&self) -> bool {
        self.in_flight.busy()
    }

    /// See [`Pace::request`].
    pub fn request(&mut self) {
        self.pace.request();
    }

    /// Run `job` on its own short-lived thread; its result is picked up by
    /// [`Poll::take`] on a later frame, which the finished job wakes.
    pub fn start(
        &mut self,
        ctx: &egui::Context,
        name: &'static str,
        job: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) {
        if !self.in_flight.begin() {
            return;
        }
        self.pace.mark_started(Instant::now());
        let slot = self.slot.clone();
        let in_flight = self.in_flight.clone();
        let wake = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let result = job();
                *tm_core::sync::lock(&slot) = Some(result);
                in_flight.end();
                wake.request_repaint();
            });
        if let Err(error) = spawned {
            *tm_core::sync::lock(&self.slot) = Some(Err(error.to_string()));
            self.in_flight.end();
        }
    }

    /// The newest finished result, once.
    pub fn take(&self) -> Option<Result<T, String>> {
        tm_core::sync::lock(&self.slot).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_respects_the_interval_and_once_mode() {
        let mut poll = Poll::<()>::default();
        let now = Instant::now();
        assert!(poll.due(now, None));
        poll.pace.mark_started(now);
        assert!(!poll.due(now + Duration::from_secs(60), None));
        assert!(!poll.due(now, Some(Duration::from_secs(1))));
        assert!(poll.due(now + Duration::from_secs(1), Some(Duration::from_secs(1))));
        assert!(poll.in_flight.begin());
        assert!(!poll.due(now + Duration::from_secs(5), Some(Duration::from_secs(1))));
    }

    #[test]
    fn a_request_overrides_the_interval_and_a_pause_once() {
        let mut pace = Pace::default();
        let now = Instant::now();
        pace.mark_started(now);
        assert!(!pace.due(now, None));
        pace.request();
        assert!(pace.due(now, None));
        assert!(pace.due(now, Some(Duration::from_secs(60))));
        pace.mark_started(now);
        assert!(!pace.due(now, None), "a request is answered by one query");
    }

    #[test]
    fn a_failed_refresh_keeps_the_shown_rows() {
        let mut rows = None;
        let mut stale = None;
        merge(&mut rows, &mut stale, Err::<u8, _>("first".into()));
        assert_eq!(rows, Some(Err("first".into())));
        merge(&mut rows, &mut stale, Ok(1));
        merge(&mut rows, &mut stale, Err("later".into()));
        assert_eq!(rows, Some(Ok(1)));
        assert_eq!(stale.as_deref(), Some("later"));
        merge(&mut rows, &mut stale, Ok(2));
        assert_eq!((rows, stale), (Some(Ok(2)), None));
    }

    #[test]
    fn pages_follow_the_update_speed_and_stop_while_paused() {
        let floor = Duration::from_secs(2);
        assert_eq!(page_interval(UpdateSpeed::High, floor), Some(floor));
        assert_eq!(page_interval(UpdateSpeed::Normal, floor), Some(floor));
        assert_eq!(
            page_interval(UpdateSpeed::Low, floor),
            Some(Duration::from_secs(4))
        );
        assert_eq!(page_interval(UpdateSpeed::Paused, floor), None);
    }
}
