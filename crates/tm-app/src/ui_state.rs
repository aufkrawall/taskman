//! Small native-window state that is not part of user-editable application
//! settings. Window size remains in `config.ini`; the OS-space position and
//! maximized flag are stored separately because the position can be negative
//! on multi-monitor desktops and the maximized size must never clobber the
//! stored restore size.

use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, Default)]
struct Placement {
    pos: Option<[f32; 2]>,
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
    let mut p = Placement::default();
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return p;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(value) = line.strip_prefix("position=") {
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
    let Some(pos) = guard.pos else { return };
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
    let body = format!(
        "# Native window placement.\nposition={},{}\nmaximized={}\n",
        pos[0], pos[1], guard.maximized
    );
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
    }

    #[test]
    fn placement_parses_from_state_file_text() {
        let parse = |text: &str| {
            let mut p = Placement::default();
            for raw in text.lines() {
                let line = raw.trim();
                if let Some(v) = line.strip_prefix("position=") {
                    p.pos = parse_position(v);
                } else if let Some(v) = line.strip_prefix("maximized=") {
                    p.maximized = v.trim() == "true";
                }
            }
            p
        };
        let p = parse("position=-8,320\nmaximized=true\n");
        assert_eq!(p.pos, Some([-8.0, 320.0]));
        assert!(p.maximized);
        let p = parse("position=1,2\nmaximized=false\n");
        assert_eq!(p.pos, Some([1.0, 2.0]));
        assert!(!p.maximized);
        // Fresh installs: absent keys fall back to OS placement defaults.
        let p = parse("# nothing yet\n");
        assert_eq!(p.pos, None);
        assert!(!p.maximized);
    }
}
