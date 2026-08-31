//! Shared global-search matching (audit §5): ONE normalized query applied to
//! every tab's candidate fields — binary name, display name, PID and
//! publisher/company for processes — instead of per-tab string comparisons
//! that drift apart. The search hint promises "name, publisher or PID"; this
//! module makes the code keep that promise.

use eframe::egui;
use tm_core::model::ProcessEntry;

/// A normalized global search query.
#[derive(Debug, Clone, Default)]
pub struct Query {
    q: String,
}

impl Query {
    /// Trim + lowercase once per frame/keystroke; empty means "match all".
    pub fn new(raw: &str) -> Self {
        Self {
            q: raw.trim().to_lowercase(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// True when any candidate string contains the query.
    pub fn matches_any<'a>(&self, candidates: impl IntoIterator<Item = &'a str>) -> bool {
        self.q.is_empty()
            || candidates
                .into_iter()
                .any(|c| c.to_lowercase().contains(&self.q))
    }

    /// Process search fields: binary name, shown/display name, PID and
    /// publisher/company ("Nach Namen, Herausgeber oder PID suchen").
    pub fn matches_process(&self, p: &ProcessEntry) -> bool {
        if self.q.is_empty() {
            return true;
        }
        let pid_s = p.pid.to_string();
        self.matches_any([
            p.name.as_str(),
            p.shown_name(),
            p.company.as_deref().unwrap_or(""),
            pid_s.as_str(),
        ])
    }
}

/// Return one plain alphabetic character typed while no text editor owns
/// keyboard focus. This deliberately uses text events instead of key codes so
/// keyboard layouts and non-ASCII letters behave naturally. Modified input is
/// ignored so application shortcuts never become list-navigation keystrokes.
pub fn list_initial(ctx: &egui::Context) -> Option<char> {
    if ctx.egui_wants_keyboard_input() {
        return None;
    }
    ctx.input(|i| {
        if i.modifiers.alt || i.modifiers.ctrl || i.modifiers.command {
            return None;
        }
        i.events.iter().rev().find_map(|event| {
            let egui::Event::Text(text) = event else {
                return None;
            };
            let mut chars = text.chars();
            let c = chars.next()?;
            (chars.next().is_none() && c.is_alphabetic()).then_some(c)
        })
    })
}

/// Keyboard movement understood by virtualized list/table pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListNav {
    Previous,
    Next,
    First,
    Last,
    PageUp,
    PageDown,
}

/// Return one unmodified list-navigation key while no text editor owns
/// keyboard focus. This keeps arrows/Delete available to the search box and
/// dialogs whenever they are actually editing text.
pub fn list_nav(ctx: &egui::Context) -> Option<ListNav> {
    if ctx.egui_wants_keyboard_input() {
        return None;
    }
    ctx.input(|i| {
        if i.modifiers.alt || i.modifiers.ctrl || i.modifiers.command || i.modifiers.shift {
            return None;
        }
        [
            (egui::Key::ArrowUp, ListNav::Previous),
            (egui::Key::ArrowDown, ListNav::Next),
            (egui::Key::Home, ListNav::First),
            (egui::Key::End, ListNav::Last),
            (egui::Key::PageUp, ListNav::PageUp),
            (egui::Key::PageDown, ListNav::PageDown),
        ]
        .into_iter()
        .find_map(|(key, nav)| i.key_pressed(key).then_some(nav))
    })
}

/// Move a selected index through a list. No selection starts at the nearest
/// edge for the requested direction; page movement uses a caller-supplied
/// visible-row estimate and always clamps to a valid entry.
pub fn moved_index(
    len: usize,
    current: Option<usize>,
    nav: ListNav,
    page_rows: usize,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    let page = page_rows.max(1);
    Some(match nav {
        ListNav::Previous => current.unwrap_or(0).saturating_sub(1),
        ListNav::Next => current.map_or(0, |i| (i + 1).min(last)),
        ListNav::First => 0,
        ListNav::Last => last,
        ListNav::PageUp => current.unwrap_or(0).saturating_sub(page),
        ListNav::PageDown => current.map_or(0, |i| (i + page).min(last)),
    })
}

/// URL for the context-menu "Search online" action. Kept here so every tab
/// uses the same UTF-8 percent encoding instead of hand-rolling variants.
pub fn online_search_url(term: &str) -> String {
    let mut encoded = String::with_capacity(term.len());
    for &b in term.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char);
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    format!("https://www.bing.com/search?q={encoded}")
}

/// Find the next displayed entry whose name starts with `initial`. If the
/// current selection is one of the matches, repeated presses cycle and wrap;
/// otherwise the first match is selected. The iterator order is therefore the
/// exact order the caller renders on screen. Identity type `T` is whatever
/// the caller uses to select (PID, resource key, ...).
pub fn cycle_match<'a, T: PartialEq + Clone>(
    items: impl IntoIterator<Item = (T, &'a str)>,
    selected: Option<T>,
    initial: char,
) -> Option<T> {
    let needle = initial.to_lowercase().collect::<String>();
    let matches = items
        .into_iter()
        .filter(|(_, name)| {
            name.trim_start()
                .chars()
                .next()
                .is_some_and(|c| c.to_lowercase().collect::<String>() == needle)
        })
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    selected
        .and_then(|sel| matches.iter().position(|candidate| *candidate == sel))
        .map_or_else(
            || Some(matches[0].clone()),
            |pos| Some(matches[(pos + 1) % matches.len()].clone()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, company: Option<&str>, display: &str) -> ProcessEntry {
        let mut p = ProcessEntry::new(pid, name);
        p.company = company.map(str::to_string);
        p.display = display.to_string();
        p
    }

    #[test]
    fn empty_query_matches_everything() {
        let q = Query::new("   ");
        assert!(q.is_empty());
        assert!(q.matches_process(&proc(1, "x.exe", None, "")));
    }

    #[test]
    fn binary_display_publisher_and_pid_match() {
        // Binary name.
        assert!(Query::new("brave").matches_process(&proc(
            1,
            "brave.exe",
            Some("Brave Software"),
            ""
        )));
        // Display name.
        assert!(Query::new("edge").matches_process(&proc(2, "msedge.exe", None, "Microsoft Edge")));
        // Publisher.
        assert!(Query::new("acme").matches_process(&proc(3, "stuff.exe", Some("ACME Corp"), "")));
        // PID.
        assert!(Query::new("4242").matches_process(&proc(4242, "unrelated.exe", None, "")));
        // ...but not unrelated processes.
        assert!(!Query::new("4242").matches_process(&proc(7, "other.exe", None, "")));
    }

    #[test]
    fn matching_is_case_insensitive_and_trimmed() {
        let q = Query::new("  EDGE  ");
        assert!(q.matches_process(&proc(1, "msedge.exe", None, "")));
    }

    #[test]
    fn non_numeric_query_does_not_match_pid_digits() {
        // A text query must not be treated as a PID candidate prefix beyond
        // plain substring semantics (e.g. "abc" never matches any pid).
        assert!(!Query::new("xyz").matches_process(&proc(12345, "app.exe", None, "")));
    }

    #[test]
    fn list_initial_cycles_and_wraps_in_display_order() {
        let rows = [(1, "Alpha"), (2, "Beta"), (3, "another"), (4, "Äther")];
        assert_eq!(cycle_match(rows, None, 'a'), Some(1));
        assert_eq!(cycle_match(rows, Some(1), 'A'), Some(3));
        assert_eq!(cycle_match(rows, Some(3), 'a'), Some(1));
        assert_eq!(cycle_match(rows, Some(2), 'a'), Some(1));
        assert_eq!(cycle_match(rows, None, 'z'), None);
        assert_eq!(cycle_match(rows, None, 'ä'), Some(4));
    }

    #[test]
    fn cycle_match_works_with_non_pid_identities() {
        let rows = [("cpu", "CPU"), ("disk0", "Disk 0 (C:)"), ("gpu0", "GPU 0")];
        assert_eq!(cycle_match(rows, None, 'd'), Some("disk0"));
        assert_eq!(cycle_match(rows, Some("disk0"), 'd'), Some("disk0"));
        assert_eq!(cycle_match(rows, Some("gpu0"), 'd'), Some("disk0"));
        assert_eq!(cycle_match(rows, None, 'x'), None);
    }

    #[test]
    fn list_navigation_clamps_and_pages() {
        assert_eq!(moved_index(0, None, ListNav::Next, 10), None);
        assert_eq!(moved_index(20, None, ListNav::Next, 10), Some(0));
        assert_eq!(moved_index(20, Some(0), ListNav::Previous, 10), Some(0));
        assert_eq!(moved_index(20, Some(7), ListNav::PageDown, 10), Some(17));
        assert_eq!(moved_index(20, Some(17), ListNav::PageDown, 10), Some(19));
        assert_eq!(moved_index(20, Some(17), ListNav::First, 10), Some(0));
        assert_eq!(moved_index(20, Some(1), ListNav::Last, 10), Some(19));
    }

    #[test]
    fn online_search_url_encodes_utf8_and_reserved_bytes() {
        assert_eq!(
            online_search_url("A&B ä"),
            "https://www.bing.com/search?q=A%26B+%C3%A4"
        );
    }
}
