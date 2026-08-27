//! Small native-window state that is not part of user-editable application
//! settings. Window size remains in `config.ini`; the OS-space position is
//! stored separately because it can be negative on multi-monitor desktops.

use std::sync::{Mutex, OnceLock};

static WINDOW_POS: OnceLock<Mutex<Option<[f32; 2]>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<[f32; 2]>> {
    WINDOW_POS.get_or_init(|| Mutex::new(load_position()))
}

fn state_path() -> std::path::PathBuf {
    tm_core::settings::taskman_config_dir().join("window-state.ini")
}

fn load_position() -> Option<[f32; 2]> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    let value = text.lines().find_map(|raw| {
        let line = raw.trim();
        line.strip_prefix("position=")
    })?;
    let (x, y) = value.split_once(',')?;
    let x = x.trim().parse::<f32>().ok()?;
    let y = y.trim().parse::<f32>().ok()?;
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

pub fn window_position() -> Option<[f32; 2]> {
    slot().lock().ok().and_then(|guard| *guard)
}

pub fn set_window_position(pos: [f32; 2]) {
    if pos[0].is_finite() && pos[1].is_finite()
        && let Ok(mut guard) = slot().lock()
    {
        *guard = Some(pos);
    }
}

/// Persist only at a clean app shutdown. The caller applies the application's
/// autosave and remember-window gates before invoking this function.
pub fn save() {
    let Some(pos) = window_position() else { return };
    let path = state_path();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, "failed to create window-state directory");
        return;
    }
    let tmp = path.with_extension("ini.tmp");
    let body = format!("# Native window placement.\nposition={},{}\n", pos[0], pos[1]);
    if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &path)) {
        tracing::warn!(error = %e, "failed to save window position");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn position_format_allows_negative_monitor_coordinates() {
        let text = "position=-1920.5,42\n";
        let value = text.lines().find_map(|line| line.strip_prefix("position=")).unwrap();
        let (x, y) = value.split_once(',').unwrap();
        assert_eq!(x.parse::<f32>().unwrap(), -1920.5);
        assert_eq!(y.parse::<f32>().unwrap(), 42.0);
    }
}
