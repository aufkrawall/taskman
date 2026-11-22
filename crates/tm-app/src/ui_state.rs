//! Small native-window state that is not part of user-editable application
//! settings. Window size remains in `config.ini`; the OS-space position and
//! maximized flag are stored separately because the position can be negative
//! on multi-monitor desktops and the maximized size must never clobber the
//! stored restore size.

use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, Default)]
struct Placement {
    pos: Option<[f32; 2]>,
    /// Outer position in PHYSICAL screen pixels. The logical `pos` above is
    /// scaled by whichever monitor's DPI the window was created on, so on a
    /// mixed-DPI layout it does not name the same pixel at the next start.
    pos_px: Option<[i32; 2]>,
    maximized: bool,
}

static PLACEMENT: OnceLock<Mutex<Placement>> = OnceLock::new();

fn slot() -> &'static Mutex<Placement> {
    PLACEMENT.get_or_init(|| Mutex::new(load_placement()))
}

fn state_path() -> std::path::PathBuf {
    tm_core::settings::taskman_config_dir().join("window-state.ini")
}

fn load_placement() -> Placement {
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return Placement::default();
    };
    parse_placement(&text)
}

fn parse_placement(text: &str) -> Placement {
    let mut p = Placement::default();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(value) = line.strip_prefix("position_px=") {
            p.pos_px = parse_position_px(value);
        } else if let Some(value) = line.strip_prefix("position=") {
            p.pos = parse_position(value);
        } else if let Some(value) = line.strip_prefix("maximized=") {
            p.maximized = value.trim() == "true";
        }
    }
    p
}

fn parse_position(value: &str) -> Option<[f32; 2]> {
    let (x, y) = value.split_once(',')?;
    let x = x.trim().parse::<f32>().ok()?;
    let y = y.trim().parse::<f32>().ok()?;
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

fn parse_position_px(value: &str) -> Option<[i32; 2]> {
    let (x, y) = value.split_once(',')?;
    Some([x.trim().parse().ok()?, y.trim().parse().ok()?])
}

pub fn window_position() -> Option<[f32; 2]> {
    slot().lock().ok().and_then(|guard| guard.pos)
}

pub fn set_window_position(pos: [f32; 2]) {
    if pos[0].is_finite()
        && pos[1].is_finite()
        && let Ok(mut guard) = slot().lock()
    {
        guard.pos = Some(pos);
    }
}

pub fn window_position_px() -> Option<[i32; 2]> {
    slot().lock().ok().and_then(|guard| guard.pos_px)
}

/// Record the physical-pixel position. Windows-only: that is the only backend
/// that can read the outer rect in pixels, so elsewhere the logical position
/// stays the only one written.
#[cfg(target_os = "windows")]
pub fn set_window_position_px(pos: [i32; 2]) {
    if let Ok(mut guard) = slot().lock() {
        guard.pos_px = Some(pos);
    }
}

pub fn window_maximized() -> bool {
    slot().lock().map(|guard| guard.maximized).unwrap_or(false)
}

pub fn set_window_maximized(maximized: bool) {
    if let Ok(mut guard) = slot().lock() {
        guard.maximized = maximized;
    }
}

/// Persist only at a clean app shutdown. The caller applies the application's
/// autosave and remember-window gates before invoking this function.
pub fn save() {
    let Ok(guard) = slot().lock() else { return };
    if guard.pos.is_none() && guard.pos_px.is_none() {
        return;
    }
    let path = state_path();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, "failed to create window-state directory");
        return;
    }
    // This tiny file is written only during a clean shutdown. Write it in
    // place instead of rename-over-existing: `rename` replacement semantics
    // differ by platform and can fail for an existing destination on Windows.
    let mut body = String::from("# Native window placement.\n");
    if let Some(pos) = guard.pos {
        body.push_str(&format!("position={},{}\n", pos[0], pos[1]));
    }
    if let Some(pos) = guard.pos_px {
        body.push_str(&format!("position_px={},{}\n", pos[0], pos[1]));
    }
    body.push_str(&format!("maximized={}\n", guard.maximized));
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(error = %e, "failed to save window position");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_format_allows_negative_monitor_coordinates() {
        assert_eq!(parse_position("-1920.5,42"), Some([-1920.5, 42.0]));
        assert_eq!(parse_position("0,0"), Some([0.0, 0.0]));
    }

    #[test]
    fn position_parse_rejects_garbage() {
        assert_eq!(parse_position("x,y"), None);
        assert_eq!(parse_position("1"), None);
        assert_eq!(parse_position("inf,2"), None);
        assert_eq!(parse_position_px("1.5,2"), None);
        assert_eq!(parse_position_px("3"), None);
    }

    #[test]
    fn placement_parses_from_state_file_text() {
        let p = parse_placement("position=-8,320\nmaximized=true\n");
        assert_eq!(p.pos, Some([-8.0, 320.0]));
        assert_eq!(p.pos_px, None);
        assert!(p.maximized);
        let p = parse_placement("position=1,2\nposition_px=-1920,40\nmaximized=false\n");
        assert_eq!(p.pos, Some([1.0, 2.0]));
        assert_eq!(p.pos_px, Some([-1920, 40]));
        assert!(!p.maximized);
        // Fresh installs: absent keys fall back to OS placement defaults.
        let p = parse_placement("# nothing yet\n");
        assert_eq!(p.pos, None);
        assert_eq!(p.pos_px, None);
        assert!(!p.maximized);
    }
}
