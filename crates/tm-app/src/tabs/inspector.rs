//! Shared plumbing of the Process Properties list pages (Threads, Modules,
//! Network, Services).
//!
//! The pages poll only while they are on screen: the dialog asks [`Poll::due`]
//! each frame it shows the page, and nothing runs for a page nobody looks at.
//! Queries never run on the UI thread.

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// One background query owned by a Process Properties page.
pub struct Poll<T> {
    slot: Arc<Mutex<Option<Result<T, String>>>>,
    in_flight: InFlight,
    started: Option<Instant>,
}

impl<T> Default for Poll<T> {
    fn default() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
            in_flight: InFlight::default(),
            started: None,
        }
    }
}

impl<T: Send + 'static> Poll<T> {
    /// Whether a new query should start now: none is running, and either
    /// none ever ran or `every` has passed since the last one started
    /// (`None` = query once).
    pub fn due(&self, now: Instant, every: Option<Duration>) -> bool {
        !self.in_flight.busy()
            && match (self.started, every) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(started), Some(every)) => now.duration_since(started) >= every,
            }
    }

    pub fn busy(&self) -> bool {
        self.in_flight.busy()
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
        self.started = Some(Instant::now());
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
        poll.started = Some(now);
        assert!(!poll.due(now + Duration::from_secs(60), None));
        assert!(!poll.due(now, Some(Duration::from_secs(1))));
        assert!(poll.due(now + Duration::from_secs(1), Some(Duration::from_secs(1))));
        assert!(poll.in_flight.begin());
        assert!(!poll.due(now + Duration::from_secs(5), Some(Duration::from_secs(1))));
    }
}
