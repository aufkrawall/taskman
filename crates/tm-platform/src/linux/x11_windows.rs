//! X11/EWMH window ownership for process classification.
//!
//! The window manager publishes its managed clients in `_NET_CLIENT_LIST`
//! (initial mapping order) and `_NET_CLIENT_LIST_STACKING` (bottom-to-top
//! stacking order). Each client's `_NET_WM_PID` provides the process owner.
//! Both lists are merged because either property can be temporarily missing
//! or stale in smaller window managers.
//!
//! Only windows that still exist, are viewable, are not override-redirect,
//! and do not have an explicit ICCCM withdrawn state contribute a PID. A
//! missing or malformed `_NET_WM_PID` is unknown ownership, not PID zero.
//!
//! Collection opens one X11 connection per collector sample and closes it
//! afterwards. That costs a connection handshake, four atom lookups, two root
//! property reads, and up to three round trips for every unique client window
//! (attributes, `WM_STATE`, and `_NET_WM_PID`). The inventory is deliberately
//! not cached: windows are short-lived, and stale ownership would misclassify
//! a later window.
//!
//! Native Wayland clients have no portable system-wide window inventory.
//! Under XWayland only the X11 subset is observable and is reported as such;
//! native Wayland windows are never guessed.

use std::collections::HashSet;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, MapState, Window};
use x11rb::rust_connection::RustConnection;

/// Bound property reads so a corrupt root property cannot force an unbounded
/// allocation. Truncated properties are treated as unavailable.
const MAX_PROPERTY_LONGS: u32 = 16_384;

/// ICCCM `WM_STATE` value for a withdrawn client.
const WM_STATE_WITHDRAWN: u32 = 0;

#[derive(Debug, Clone, Copy)]
struct Atoms {
    client_list: Option<Atom>,
    stacking: Option<Atom>,
    pid: Option<Atom>,
    state: Option<Atom>,
}

/// Return the PIDs that currently own an eligible top-level X11 window.
///
/// No `DISPLAY`, an unreachable server, a missing EWMH property, and an X11
/// protocol error all produce an empty observation. The caller must not treat
/// that as proof that a process has no window, especially in a native Wayland
/// session.
pub(crate) fn window_owner_pids() -> HashSet<u32> {
    let has_display = std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    if !has_display {
        return HashSet::new();
    }

    query_window_owner_pids().unwrap_or_default()
}

fn query_window_owner_pids() -> Option<HashSet<u32>> {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let atoms = Atoms {
        client_list: intern_atom(&conn, b"_NET_CLIENT_LIST"),
        stacking: intern_atom(&conn, b"_NET_CLIENT_LIST_STACKING"),
        pid: intern_atom(&conn, b"_NET_WM_PID"),
        state: intern_atom(&conn, b"WM_STATE"),
    };

    if atoms.client_list.is_none() && atoms.stacking.is_none() {
        return Some(HashSet::new());
    }
    let Some(pid_atom) = atoms.pid else {
        return Some(HashSet::new());
    };

    let client_windows = atoms
        .client_list
        .and_then(|atom| {
            read_u32_property(
                &conn,
                root,
                atom,
                Some(AtomEnum::WINDOW.into()),
                MAX_PROPERTY_LONGS,
            )
        })
        .unwrap_or_default();
    let stacking_windows = atoms
        .stacking
        .and_then(|atom| {
            read_u32_property(
                &conn,
                root,
                atom,
                Some(AtomEnum::WINDOW.into()),
                MAX_PROPERTY_LONGS,
            )
        })
        .unwrap_or_default();

    let mut owners = HashSet::new();
    for window in merge_window_lists(&client_windows, &stacking_windows) {
        if let Some(pid) = window_owner_pid(&conn, window, atoms.state, pid_atom) {
            owners.insert(pid);
        }
    }
    Some(owners)
}

fn intern_atom(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
    conn.intern_atom(true, name)
        .ok()?
        .reply()
        .ok()
        .map(|reply| reply.atom)
        .filter(|&atom| atom != x11rb::NONE)
}

fn read_u32_property(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    expected_type: Option<Atom>,
    max_longs: u32,
) -> Option<Vec<u32>> {
    let reply = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, max_longs)
        .ok()?
        .reply()
        .ok()?;
    if reply.format != 32 || reply.bytes_after != 0 {
        return None;
    }
    if expected_type.is_some_and(|expected| reply.type_ != expected) {
        return None;
    }
    Some(reply.value32()?.collect())
}

fn window_owner_pid(
    conn: &RustConnection,
    window: Window,
    state_atom: Option<Atom>,
    pid_atom: Atom,
) -> Option<u32> {
    let attributes = conn.get_window_attributes(window).ok()?.reply().ok()?;
    let wm_state = state_atom
        .and_then(|atom| read_u32_property(conn, window, atom, None, 2))
        .and_then(|values| values.first().copied());
    if !window_is_eligible(attributes.map_state, attributes.override_redirect, wm_state) {
        return None;
    }

    let pid_values = read_u32_property(
        conn,
        window,
        pid_atom,
        Some(AtomEnum::CARDINAL.into()),
        1,
    )?;
    parse_window_pid(&pid_values)
}

fn merge_window_lists(client_list: &[Window], stacking_list: &[Window]) -> Vec<Window> {
    let capacity = client_list.len().saturating_add(stacking_list.len());
    let mut seen = HashSet::with_capacity(capacity);
    let mut windows = Vec::with_capacity(capacity);
    for &window in client_list.iter().chain(stacking_list.iter()) {
        if window != x11rb::NONE && seen.insert(window) {
            windows.push(window);
        }
    }
    windows
}

fn window_is_eligible(
    map_state: MapState,
    override_redirect: bool,
    wm_state: Option<u32>,
) -> bool {
    map_state == MapState::VIEWABLE
        && !override_redirect
        && wm_state != Some(WM_STATE_WITHDRAWN)
}

fn parse_window_pid(values: &[u32]) -> Option<u32> {
    match values {
        [pid] if *pid != 0 => Some(*pid),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_lists_are_merged_without_duplicates_or_none() {
        assert_eq!(
            merge_window_lists(&[10, 20, 10, x11rb::NONE], &[20, 30, x11rb::NONE]),
            vec![10, 20, 30]
        );
        assert!(merge_window_lists(&[], &[]).is_empty());
    }

    #[test]
    fn only_viewable_managed_non_withdrawn_windows_are_eligible() {
        assert!(window_is_eligible(MapState::VIEWABLE, false, None));
        // ICCCM normal/iconic and unknown non-withdrawn states are not
        // promoted to "withdrawn"; the X map state remains the primary fact.
        assert!(window_is_eligible(MapState::VIEWABLE, false, Some(1)));
        assert!(window_is_eligible(MapState::VIEWABLE, false, Some(2)));
        assert!(window_is_eligible(MapState::VIEWABLE, false, Some(3)));

        assert!(!window_is_eligible(MapState::UNMAPPED, false, None));
        assert!(!window_is_eligible(MapState::UNVIEWABLE, false, None));
        assert!(!window_is_eligible(MapState::VIEWABLE, true, None));
        assert!(!window_is_eligible(MapState::VIEWABLE, false, Some(WM_STATE_WITHDRAWN)));
    }

    #[test]
    fn window_pid_requires_exactly_one_nonzero_cardinal() {
        assert_eq!(parse_window_pid(&[42]), Some(42));
        assert_eq!(parse_window_pid(&[]), None);
        assert_eq!(parse_window_pid(&[0]), None);
        assert_eq!(parse_window_pid(&[42, 43]), None);
    }
}
