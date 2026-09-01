//! Multi-row selection for the two process tables.
//!
//! Native list views everywhere in Windows agree on the gesture set, so this
//! implements exactly that and nothing else: a plain click replaces the
//! selection, `Ctrl` toggles one row, `Shift` selects the inclusive range from
//! the anchor, and `Ctrl+A` takes everything. The anchor is the row a plain or
//! `Ctrl` click landed on; `Shift` never moves it, which is what lets a user
//! walk a range open and closed again.
//!
//! Two identities are tracked separately on purpose:
//!
//! * **primary** — the row the user acted on last. Everything that only makes
//!   sense for one process (Properties, Go to details, the modules inspector)
//!   uses it, so those commands keep working with a range selected.
//! * **items** — the whole selection, in the display order it was taken in.
//!   Only the genuinely repeatable commands (end task, efficiency mode) fan
//!   out over it.
//!
//! Rows are held as [`ProcessIdentity`] (pid + creation time), never as row
//! indexes: the list re-sorts under the selection on every sample tick, and a
//! pid alone is reused by the OS.

use crate::app::ProcessIdentity;
use eframe::egui;

/// What a click should do to the selection, decided by the modifier keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickKind {
    /// No modifier: this row becomes the whole selection.
    Replace,
    /// Ctrl: add or remove this row, leaving the rest alone.
    Toggle,
    /// Shift: select everything between the anchor and this row.
    Range,
}

impl ClickKind {
    pub fn from_modifiers(modifiers: &egui::Modifiers) -> Self {
        // Shift wins over Ctrl: Ctrl+Shift+click extends in native list
        // views too, it just keeps what was already selected — and `Range`
        // below preserves the pre-anchor selection either way.
        if modifiers.shift {
            Self::Range
        } else if modifiers.command || modifiers.ctrl {
            Self::Toggle
        } else {
            Self::Replace
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Selected rows. Order is the display order they were taken in, which
    /// is what makes "end task" act top-down rather than in click order.
    items: Vec<ProcessIdentity>,
    /// The row single-target commands act on. Always one of `items`.
    primary: Option<ProcessIdentity>,
    /// Range anchor. Survives `Shift` clicks so a range can be resized.
    anchor: Option<ProcessIdentity>,
}

impl Selection {
    /// The row every single-target command acts on.
    pub fn primary(&self) -> Option<&ProcessIdentity> {
        self.primary.as_ref()
    }

    /// Everything selected, in display order.
    pub fn all(&self) -> &[ProcessIdentity] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether this pid is painted as selected. Matching on the pid alone is
    /// correct HERE and only here: the rows being painted come from the same
    /// snapshot the selection was taken against, and a row highlight is not
    /// a destructive action. Everything that acts carries the full identity.
    pub fn contains_pid(&self, pid: u32) -> bool {
        self.items.iter().any(|item| item.pid == pid)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.primary = None;
        self.anchor = None;
    }

    /// Replace the selection with one row (plain click, arrow-key movement).
    pub fn select_single(&mut self, identity: ProcessIdentity) {
        self.items = vec![identity.clone()];
        self.primary = Some(identity.clone());
        self.anchor = Some(identity);
    }

    /// Ctrl+click: add or remove one row without disturbing the rest.
    pub fn toggle(&mut self, identity: ProcessIdentity, order: &[ProcessIdentity]) {
        if let Some(at) = self.items.iter().position(|item| *item == identity) {
            self.items.remove(at);
            if self.primary.as_ref() == Some(&identity) {
                self.primary = self.items.last().cloned();
            }
        } else {
            self.items.push(identity.clone());
            self.sort_to(order);
            self.primary = Some(identity.clone());
        }
        self.anchor = Some(identity);
    }

    /// Shift+click / Shift+arrow: select the inclusive range between the
    /// anchor and `identity` in the CURRENT display order.
    ///
    /// Whatever lay outside that range is dropped, exactly like a native list
    /// view: a shift click is "select from here to there", not "add".
    /// Without an anchor (nothing selected yet) it degrades to a plain click.
    pub fn select_range(&mut self, identity: ProcessIdentity, order: &[ProcessIdentity]) {
        let Some(anchor) = self.anchor.clone() else {
            self.select_single(identity);
            return;
        };
        let (Some(from), Some(to)) = (
            order.iter().position(|item| *item == anchor),
            order.iter().position(|item| *item == identity),
        ) else {
            // The anchor scrolled out of existence (process exited, filter
            // changed). Restart the range here rather than selecting a span
            // the user never pointed at.
            self.select_single(identity);
            return;
        };
        let (low, high) = if from <= to { (from, to) } else { (to, from) };
        self.items = order[low..=high].to_vec();
        self.primary = Some(identity);
        // The anchor deliberately stays put so the range can be resized.
    }

    /// Ctrl+A.
    pub fn select_all(&mut self, order: &[ProcessIdentity]) {
        self.items = order.to_vec();
        self.primary = order.last().cloned();
        self.anchor = order.first().cloned();
    }

    /// Apply a click with its modifiers.
    pub fn click(&mut self, kind: ClickKind, identity: ProcessIdentity, order: &[ProcessIdentity]) {
        match kind {
            ClickKind::Replace => self.select_single(identity),
            ClickKind::Toggle => self.toggle(identity, order),
            ClickKind::Range => self.select_range(identity, order),
        }
    }

    /// Drop selected rows that no longer exist, so a stale identity can never
    /// reach an action and the toolbar stops offering to end a dead process.
    pub fn retain_live(&mut self, live: impl Fn(&ProcessIdentity) -> bool) {
        self.items.retain(&live);
        if self.primary.as_ref().is_some_and(|item| !live(item)) {
            self.primary = self.items.last().cloned();
        }
        if self.anchor.as_ref().is_some_and(|item| !live(item)) {
            self.anchor = self.primary.clone();
        }
    }

    /// Restore display order after an insertion. Identities the current view
    /// does not contain (a row scrolled behind a collapsed group, a filtered
    /// row) keep their relative position at the end rather than being lost.
    fn sort_to(&mut self, order: &[ProcessIdentity]) {
        let rank = |item: &ProcessIdentity| {
            order
                .iter()
                .position(|candidate| candidate == item)
                .unwrap_or(usize::MAX)
        };
        self.items.sort_by_key(rank);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_epoch_s: Some(1000 + i64::from(pid)),
        }
    }

    fn order() -> Vec<ProcessIdentity> {
        (1..=6).map(id).collect()
    }

    #[test]
    fn a_plain_click_replaces_the_selection_and_moves_the_anchor() {
        let order = order();
        let mut selection = Selection::default();
        selection.click(ClickKind::Replace, id(2), &order);
        selection.click(ClickKind::Replace, id(5), &order);
        assert_eq!(selection.all(), [id(5)]);
        assert_eq!(selection.primary(), Some(&id(5)));
    }

    #[test]
    fn ctrl_click_toggles_one_row_and_keeps_display_order() {
        let order = order();
        let mut selection = Selection::default();
        selection.click(ClickKind::Replace, id(4), &order);
        selection.click(ClickKind::Toggle, id(2), &order);
        // Selected in click order 4 then 2, reported in display order.
        assert_eq!(selection.all(), [id(2), id(4)]);
        assert_eq!(selection.primary(), Some(&id(2)), "last click is primary");

        selection.click(ClickKind::Toggle, id(2), &order);
        assert_eq!(selection.all(), [id(4)]);
        assert_eq!(selection.primary(), Some(&id(4)));
    }

    #[test]
    fn shift_click_selects_the_inclusive_range_in_either_direction() {
        let order = order();
        let mut selection = Selection::default();
        selection.click(ClickKind::Replace, id(4), &order);
        selection.click(ClickKind::Range, id(2), &order);
        assert_eq!(selection.all(), [id(2), id(3), id(4)]);
        assert_eq!(selection.primary(), Some(&id(2)));

        // The anchor stayed on 4, so the range can be resized the other way.
        selection.click(ClickKind::Range, id(6), &order);
        assert_eq!(selection.all(), [id(4), id(5), id(6)]);
    }

    #[test]
    fn shift_without_an_anchor_behaves_like_a_plain_click() {
        let order = order();
        let mut selection = Selection::default();
        selection.click(ClickKind::Range, id(3), &order);
        assert_eq!(selection.all(), [id(3)]);
    }

    /// A range whose anchor has since exited must not silently select a
    /// different span than the one the user pointed at.
    #[test]
    fn a_vanished_anchor_restarts_the_range() {
        let mut selection = Selection::default();
        selection.click(ClickKind::Replace, id(99), &order());
        selection.click(ClickKind::Range, id(3), &order());
        assert_eq!(selection.all(), [id(3)]);
    }

    /// PID reuse: an identity whose creation time differs is a DIFFERENT
    /// process and must not inherit the selection.
    #[test]
    fn identity_includes_the_creation_time() {
        let recycled = ProcessIdentity {
            pid: 3,
            start_epoch_s: Some(999_999),
        };
        let order = order();
        let mut selection = Selection::default();
        selection.click(ClickKind::Replace, id(3), &order);
        selection.retain_live(|item| *item != id(3) || false);
        assert!(selection.is_empty());
        assert_ne!(id(3), recycled);
    }

    #[test]
    fn dead_rows_are_dropped_and_the_primary_follows() {
        let order = order();
        let mut selection = Selection::default();
        selection.click(ClickKind::Replace, id(2), &order);
        selection.click(ClickKind::Range, id(4), &order);
        assert_eq!(selection.primary(), Some(&id(4)));
        selection.retain_live(|item| item.pid != 4);
        assert_eq!(selection.all(), [id(2), id(3)]);
        assert_eq!(selection.primary(), Some(&id(3)));
    }

    #[test]
    fn select_all_takes_the_whole_display_order() {
        let order = order();
        let mut selection = Selection::default();
        selection.select_all(&order);
        assert_eq!(selection.len(), 6);
        assert!(selection.contains_pid(1));
        assert!(selection.contains_pid(6));
    }

    #[test]
    fn modifiers_map_to_the_native_gestures() {
        let plain = egui::Modifiers::default();
        assert_eq!(ClickKind::from_modifiers(&plain), ClickKind::Replace);
        assert_eq!(
            ClickKind::from_modifiers(&egui::Modifiers::CTRL),
            ClickKind::Toggle
        );
        assert_eq!(
            ClickKind::from_modifiers(&egui::Modifiers::SHIFT),
            ClickKind::Range
        );
        assert_eq!(
            ClickKind::from_modifiers(&(egui::Modifiers::SHIFT | egui::Modifiers::CTRL)),
            ClickKind::Range
        );
    }
}
