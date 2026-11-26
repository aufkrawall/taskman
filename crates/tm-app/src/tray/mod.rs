//! The system tray icon.
//!
//! One icon, three entries, and one contract the window hides behind: the
//! window may only disappear into the tray once the icon really exists,
//! because a window hidden behind an icon that never appeared cannot be
//! brought back. Each desktop implements that icon its own way — Windows owns a
//! notification-area icon through `tray-icon` next to the Win32 shell code in
//! `main.rs`, Linux serves a StatusNotifierItem over the session bus
//! ([`linux`]) — and this module holds the part both agree on: what a start
//! minimized launch does while the icon is not ready, which menu entry means
//! which action, and the pixel layout the icon is advertised in.
//!
//! The decisions are plain functions with no bus, no window and no platform
//! types, so their tests run in the ordinary `cargo test -p tm-app` run of any
//! host instead of only inside a Linux build.
#![cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub(crate) use linux::TrayShell;

/// How long a start-minimized launch waits for the tray icon before showing
/// the window instead: a window hidden behind an icon that never appears
/// cannot be brought back.
pub(crate) const INITIAL_HIDE_TRAY_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// What a start-minimized launch should do while the tray icon is not ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialHideAction {
    /// The icon exists: it is safe to disappear into it.
    Hide,
    /// Still being created: keep the window hidden and look again shortly.
    Wait,
    /// The icon can never appear (or the wait expired): show the window.
    Show,
}

/// Decide a start-minimized launch's visibility for one frame.
///
/// `Wait` still means "assert hidden": the window is created invisible, but
/// eframe's first-frame reveal keys off the app's own `Visible` command, so
/// sending nothing here would let `post_rendering` show the window until the
/// tray poll notices `READY`.
pub(crate) fn initial_hide_action(
    tray: Option<(bool, bool)>,
    before_deadline: bool,
) -> InitialHideAction {
    match tray {
        Some((true, _)) => InitialHideAction::Hide,
        Some((false, false)) if before_deadline => InitialHideAction::Wait,
        _ => InitialHideAction::Show,
    }
}

/// What a tray menu entry asks the UI thread to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayAction {
    /// Show the window (a left click on the icon is the same request).
    Open,
    /// Show the window with the settings dialog open.
    Settings,
    /// Exit the app through its normal close path.
    Exit,
}

impl TrayAction {
    /// The code `signal_tray` carries into the UI thread.
    ///
    /// The numbers are the wire contract between a desktop's tray menu and
    /// `NativeApp`: they identify the action, not the menu entry, so every menu
    /// that offers "Exit" signals this code however it numbers its own rows.
    pub(crate) const fn signal_code(self) -> u8 {
        match self {
            Self::Open => 1,
            Self::Settings => 2,
            Self::Exit => 3,
        }
    }
}

/// Menu item id of `Open` in the Linux tray's `com.canonical.dbusmenu` object.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MENU_OPEN_ID: i32 = 1;
/// Menu item id of `Settings` in the Linux tray's menu object.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MENU_SETTINGS_ID: i32 = 2;
/// Menu item id of `Exit` in the Linux tray's menu object.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MENU_EXIT_ID: i32 = 3;

/// The entries the tray menu offers, in the order they are shown.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MENU_ITEMS: [(i32, TrayAction); 3] = [
    (MENU_OPEN_ID, TrayAction::Open),
    (MENU_SETTINGS_ID, TrayAction::Settings),
    (MENU_EXIT_ID, TrayAction::Exit),
];

/// Map a menu item id back to the action it stands for.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn action_for_menu_id(id: i32) -> Option<TrayAction> {
    MENU_ITEMS
        .iter()
        .find(|(item, _)| *item == id)
        .map(|(_, action)| *action)
}

/// The translated label of a menu entry.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn menu_label(action: TrayAction) -> &'static str {
    match action {
        TrayAction::Open => tm_core::i18n::tr(tm_core::i18n::K::TrayOpen),
        TrayAction::Settings => tm_core::i18n::tr(tm_core::i18n::K::Settings),
        TrayAction::Exit => tm_core::i18n::tr(tm_core::i18n::K::TrayExit),
    }
}

/// The StatusNotifierItem `Status` value for "the icon can bring the window
/// back" (`true`) or "a panel may hide it" (`false`).
///
/// `Passive` is what a panel that hides passive items reacts to, so it is the
/// only way to make the icon disappear again without closing the connection
/// that serves it.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn status_name(active: bool) -> &'static str {
    if active { "Active" } else { "Passive" }
}

/// Whether a `GetLayout` request at `recursion_depth` should carry the entries
/// below the requested parent.
///
/// dbusmenu reads a negative depth as "every level" and `0` as "this item
/// only"; a positive depth is a level limit this flat menu never reaches.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn children_for_depth(recursion_depth: i32) -> bool {
    recursion_depth != 0
}

/// Whether a dbusmenu event is the click that activates an entry. Only
/// `clicked` carries an action; `hovered` and the rest have nothing to do.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn event_is_click(event_id: &str) -> bool {
    event_id == "clicked"
}

/// Reorder an RGBA8 image into the bytes a StatusNotifierItem advertises as
/// `IconPixmap`: one big-endian (network order) ARGB32 value per pixel, alpha
/// first, which is the only layout the spec defines for tray pixmaps.
///
/// A trailing partial pixel is dropped rather than padded: an image whose
/// length is not a whole number of pixels has no meaning in either channel
/// order, and inventing the missing bytes could show a colour that was never
/// in the icon.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn argb32_network_order(rgba: &[u8]) -> Vec<u8> {
    let mut argb = Vec::with_capacity(rgba.len() & !3);
    for pixel in rgba.as_chunks::<4>().0 {
        argb.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
    }
    argb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_minimized_keeps_the_window_hidden_until_the_tray_is_ready() {
        use InitialHideAction::{Hide, Show, Wait};
        assert_eq!(initial_hide_action(Some((true, false)), true), Hide);
        assert_eq!(initial_hide_action(Some((true, true)), false), Hide);
        // Regression: a slow tray must still assert hidden; the old wait arm
        // sent no command and eframe's first-frame reveal showed the window.
        assert_eq!(initial_hide_action(Some((false, false)), true), Wait);
        assert_eq!(initial_hide_action(Some((false, false)), false), Show);
        assert_eq!(initial_hide_action(Some((false, true)), true), Show);
        assert_eq!(initial_hide_action(None, true), Show);
        // A tray that failed before the deadline is what a Linux session
        // without a StatusNotifierWatcher looks like: show the window at once
        // instead of hiding it behind an icon nobody will ever see.
        assert_eq!(initial_hide_action(Some((false, true)), false), Show);
    }

    /// The dbusmenu ids are a wire contract: a host reports a click as the
    /// numeric id it was served, so the ids may be reordered only together
    /// with the entries. Each one has to reach the same action the Windows
    /// menu and the taskbar flyout signal.
    #[test]
    fn tray_menu_ids_map_to_the_actions_the_menus_signal() {
        assert_eq!(action_for_menu_id(MENU_OPEN_ID), Some(TrayAction::Open));
        assert_eq!(
            action_for_menu_id(MENU_SETTINGS_ID),
            Some(TrayAction::Settings)
        );
        assert_eq!(action_for_menu_id(MENU_EXIT_ID), Some(TrayAction::Exit));
        // The served order is Open, Settings, Exit.
        let served: Vec<TrayAction> = MENU_ITEMS.iter().map(|(_, action)| *action).collect();
        assert_eq!(
            served,
            [TrayAction::Open, TrayAction::Settings, TrayAction::Exit]
        );
        // Root (0) and unknown ids belong to no action.
        assert_eq!(action_for_menu_id(0), None);
        assert_eq!(action_for_menu_id(4), None);
        assert_eq!(action_for_menu_id(-1), None);
        // Distinct codes: the UI thread keeps one pending action at a time.
        assert_eq!(TrayAction::Open.signal_code(), 1);
        assert_eq!(TrayAction::Settings.signal_code(), 2);
        assert_eq!(TrayAction::Exit.signal_code(), 3);
    }

    #[test]
    fn a_tray_pixmap_is_argb32_in_network_order() {
        // Two pixels: (r, g, b, a) -> (a, r, g, b).
        assert_eq!(
            argb32_network_order(&[1, 2, 3, 4, 5, 6, 7, 8]),
            vec![4, 1, 2, 3, 8, 5, 6, 7]
        );
        // A partial trailing pixel is not a pixel: drop it instead of guessing
        // the missing channels.
        assert_eq!(
            argb32_network_order(&[1, 2, 3, 4, 5, 6, 7]),
            vec![4, 1, 2, 3]
        );
        assert!(argb32_network_order(&[]).is_empty());
        // The real icon keeps its size, so a 64x64 pixmap stays 64x64.
        let rgba = vec![0x11_u8; 64 * 64 * 4];
        let argb = argb32_network_order(&rgba);
        assert_eq!(argb.len(), 64 * 64 * 4);
        assert_eq!(&argb[..4], &[0x11, 0x11, 0x11, 0x11]);
    }

    /// `Status` is the only way to hide an icon without dropping the
    /// connection that serves it, so the two names are a wire contract of
    /// their own (a panel that hides passive items watches for exactly the
    /// `Passive` spelling).
    #[test]
    fn the_served_status_names_are_the_spec_spellings() {
        assert_eq!(status_name(true), "Active");
        assert_eq!(status_name(false), "Passive");
        assert_ne!(status_name(true), status_name(false));
    }

    /// dbusmenu reads a negative depth as "every level" and `0` as "this item
    /// only"; a host that asks for `0` must not be handed the entries, or it
    /// would show the leaves as children of the root it asked about.
    #[test]
    fn layout_children_follow_the_requested_depth() {
        assert!(children_for_depth(-1));
        assert!(children_for_depth(1));
        assert!(children_for_depth(i32::MAX));
        assert!(!children_for_depth(0));
    }

    #[test]
    fn only_the_click_event_carries_an_action() {
        assert!(event_is_click("clicked"));
        assert!(!event_is_click("hovered"));
        assert!(!event_is_click(""));
        // Case matters: the spec spells the event `clicked`.
        assert!(!event_is_click("Clicked"));
    }

    /// The icon's menu is the only way to reach Settings and Exit on Linux, so
    /// every entry keeps a real translated label (an empty one would render as
    /// a blank row a host still has to show).
    #[test]
    fn every_menu_entry_keeps_a_translated_label() {
        for (id, action) in MENU_ITEMS {
            let label = menu_label(action);
            assert!(!label.trim().is_empty(), "menu item {id} has no label");
        }
        assert_eq!(
            menu_label(TrayAction::Open),
            tm_core::i18n::tr(tm_core::i18n::K::TrayOpen)
        );
        assert_eq!(
            menu_label(TrayAction::Settings),
            tm_core::i18n::tr(tm_core::i18n::K::Settings)
        );
        assert_eq!(
            menu_label(TrayAction::Exit),
            tm_core::i18n::tr(tm_core::i18n::K::TrayExit)
        );
    }
}
