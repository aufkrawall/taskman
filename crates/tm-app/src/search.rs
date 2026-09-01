//! Shared global-search matching (audit §5): ONE normalized query applied to
//! every tab's candidate fields instead of per-tab string comparisons that
//! drift apart.
//!
//! For processes the fields are everything that identifies the program to a
//! person: binary name, friendly/display name, description, publisher,
//! owning account, PID, image path and full command line. Restricting it to
//! the name meant a search for a folder, a switch, a service host's account
//! or a vendor string found nothing even though the answer was on screen in
//! another column.

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

    /// Every process field a person could reasonably search by.
    ///
    /// Ordered cheapest-first: the short identity strings decide almost every
    /// match, and the command line (which can be kilobytes) is only ever
    /// lowercased when nothing before it matched.
    pub fn matches_process(&self, p: &ProcessEntry) -> bool {
        if self.q.is_empty() {
            return true;
        }
        let pid_s = p.pid.to_string();
        if self.matches_any([
            p.name.as_str(),
            p.shown_name(),
            p.description.as_deref().unwrap_or(""),
            p.company.as_deref().unwrap_or(""),
            p.user.as_deref().unwrap_or(""),
            p.service_name.as_deref().unwrap_or(""),
            pid_s.as_str(),
        ]) {
            return true;
        }
        let path = p.exe_path.as_ref().map(|path| path.to_string_lossy());
        self.matches_any([
            path.as_deref().unwrap_or(""),
            p.command_line.as_deref().unwrap_or(""),
        ])
    }
}

/// How long a type-ahead buffer keeps collecting keystrokes. Windows' list
/// views use roughly one second of silence to end a search word; typing
/// faster than this appends instead of restarting.
const TYPE_AHEAD_TIMEOUT_S: f64 = 1.0;

/// What a burst of typed characters should do to the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeAhead {
    /// Select the FIRST entry whose name starts with this accumulated
    /// prefix. Typing "svc" quickly must land on `svchost.exe`, never on
    /// whatever starts with the last letter alone.
    Prefix(String),
    /// The same letter pressed repeatedly: step to the next entry with that
    /// initial and wrap, exactly like native list views.
    Cycle(char),
}

/// Characters that participate in type-ahead. Letters and digits cover the
/// native behavior; `.`, `-` and `_` let a user type through real process
/// names like `svchost.exe` or `msedge-webview`.
fn type_ahead_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// Collect every plain character typed this frame while no text editor owns
/// keyboard focus. Text events (not key codes) keep keyboard layouts and
/// non-ASCII letters natural; modified input is ignored so application
/// shortcuts never become list-navigation keystrokes.
fn typed_chars(ctx: &egui::Context) -> Vec<char> {
    if ctx.egui_wants_keyboard_input() {
        return Vec::new();
    }
    ctx.input(|i| {
        if i.modifiers.alt || i.modifiers.ctrl || i.modifiers.command {
            return Vec::new();
        }
        i.events
            .iter()
            .filter_map(|event| {
                let egui::Event::Text(text) = event else {
                    return None;
                };
                let mut chars = text.chars();
                let c = chars.next()?;
                (chars.next().is_none() && type_ahead_char(c)).then_some(c)
            })
            .collect()
    })
}

/// Accumulating type-ahead for one list, keyed by `id` so tabs never inherit
/// each other's buffer.
///
/// A single frame can deliver SEVERAL text events when the user types fast,
/// so all of them are appended in order; only the buffer decides what to
/// match. Anything typed after [`TYPE_AHEAD_TIMEOUT_S`] of silence starts a
/// new word.
pub fn list_type_ahead(ctx: &egui::Context, id: &'static str) -> Option<TypeAhead> {
    let typed = typed_chars(ctx);
    if typed.is_empty() {
        return None;
    }
    let now = ctx.input(|i| i.time);
    let key = egui::Id::new(("tm-typeahead", id));
    let previous = ctx.data(|d| d.get_temp::<(String, f64)>(key));
    let mut buffer = match previous {
        Some((buffer, last)) if now - last <= TYPE_AHEAD_TIMEOUT_S => buffer,
        _ => String::new(),
    };
    buffer.extend(typed);
    ctx.data_mut(|d| d.insert_temp(key, (buffer.clone(), now)));

    let mut chars = buffer.chars();
    let first = chars.next()?;
    // Native rule: one letter — or the same letter repeated — cycles through
    // its matches; a real word narrows the match by prefix instead.
    if chars.all(|c| same_letter(c, first)) {
        Some(TypeAhead::Cycle(first))
    } else {
        Some(TypeAhead::Prefix(buffer))
    }
}

fn same_letter(a: char, b: char) -> bool {
    a.to_lowercase().eq(b.to_lowercase())
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

/// Find the first displayed entry whose name starts with `prefix`
/// (case-insensitively). Unlike [`cycle_match`] this never advances past a
/// match, so extending the prefix keeps refining the SAME selection instead
/// of walking away from it.
pub fn prefix_match<'a, T>(
    items: impl IntoIterator<Item = (T, &'a str)>,
    prefix: &str,
) -> Option<T> {
    let needle = prefix.to_lowercase();
    items.into_iter().find_map(|(id, name)| {
        let name = name.trim_start().to_lowercase();
        name.starts_with(&needle).then_some(id)
    })
}

/// Apply one type-ahead burst to a list, returning the identity to select.
pub fn type_ahead_match<'a, T: PartialEq + Clone>(
    items: impl IntoIterator<Item = (T, &'a str)> + Clone,
    selected: Option<T>,
    typed: &TypeAhead,
) -> Option<T> {
    match typed {
        TypeAhead::Cycle(initial) => cycle_match(items, selected, *initial),
        TypeAhead::Prefix(prefix) => prefix_match(items, prefix),
    }
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

    /// Everything the columns show about a process must be reachable from
    /// the one search box; matching only the name was the complaint.
    #[test]
    fn path_command_line_user_and_description_all_match() {
        let mut p = proc(9, "svchost.exe", Some("Microsoft"), "");
        p.exe_path = Some(std::path::PathBuf::from(
            "C:\\Windows\\System32\\svchost.exe",
        ));
        p.command_line = Some("svchost.exe -k NetworkService -p".into());
        p.user = Some("NETWORK SERVICE".into());
        p.description = Some("Host Process for Windows Services".into());
        p.service_name = Some("Dnscache".into());

        assert!(Query::new("system32").matches_process(&p), "image path");
        assert!(Query::new("-k Network").matches_process(&p), "command line");
        assert!(Query::new("network service").matches_process(&p), "user");
        assert!(
            Query::new("host process").matches_process(&p),
            "description"
        );
        assert!(Query::new("dnscache").matches_process(&p), "service name");
        assert!(!Query::new("chrome").matches_process(&p));
    }

    /// The long command line must not be lowercased on every keystroke for
    /// every process when a short identity field already answered.
    #[test]
    fn a_name_match_short_circuits_before_the_command_line() {
        let mut p = proc(9, "brave.exe", None, "");
        p.command_line = Some("x".repeat(4096));
        assert!(Query::new("brave").matches_process(&p));
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

    fn typed_ctx(ctx: &egui::Context, chars: &[char], time: f64) {
        let events = chars
            .iter()
            .map(|c| egui::Event::Text(c.to_string()))
            .collect();
        let mut out = ctx.run_ui(
            egui::RawInput {
                time: Some(time),
                events,
                ..Default::default()
            },
            |_| {},
        );
        out.textures_delta.clear();
    }

    /// The regression from the report: typing "svc" fast must not leave the
    /// selection on whatever starts with "c".
    #[test]
    fn fast_typing_accumulates_into_a_prefix() {
        let ctx = egui::Context::default();
        typed_ctx(&ctx, &['s'], 0.0);
        assert_eq!(
            list_type_ahead(&ctx, "t"),
            Some(TypeAhead::Cycle('s')),
            "a single letter still cycles like a native list"
        );
        typed_ctx(&ctx, &['v'], 0.05);
        assert_eq!(
            list_type_ahead(&ctx, "t"),
            Some(TypeAhead::Prefix("sv".into()))
        );
        typed_ctx(&ctx, &['c'], 0.10);
        assert_eq!(
            list_type_ahead(&ctx, "t"),
            Some(TypeAhead::Prefix("svc".into()))
        );
    }

    /// Several text events can land in ONE frame when typing fast; all of
    /// them must reach the buffer in order.
    #[test]
    fn several_characters_in_one_frame_all_count() {
        let ctx = egui::Context::default();
        typed_ctx(&ctx, &['s', 'v', 'c'], 0.0);
        assert_eq!(
            list_type_ahead(&ctx, "t"),
            Some(TypeAhead::Prefix("svc".into()))
        );
    }

    #[test]
    fn a_pause_starts_a_new_word() {
        let ctx = egui::Context::default();
        typed_ctx(&ctx, &['s'], 0.0);
        assert_eq!(list_type_ahead(&ctx, "t"), Some(TypeAhead::Cycle('s')));
        typed_ctx(&ctx, &['v'], 0.0 + TYPE_AHEAD_TIMEOUT_S + 0.5);
        assert_eq!(
            list_type_ahead(&ctx, "t"),
            Some(TypeAhead::Cycle('v')),
            "after the timeout the buffer restarts"
        );
    }

    #[test]
    fn repeating_one_letter_keeps_cycling() {
        let ctx = egui::Context::default();
        typed_ctx(&ctx, &['s'], 0.0);
        assert_eq!(list_type_ahead(&ctx, "t"), Some(TypeAhead::Cycle('s')));
        typed_ctx(&ctx, &['s'], 0.05);
        assert_eq!(list_type_ahead(&ctx, "t"), Some(TypeAhead::Cycle('s')));
        typed_ctx(&ctx, &['S'], 0.10);
        assert_eq!(list_type_ahead(&ctx, "t"), Some(TypeAhead::Cycle('s')));
    }

    #[test]
    fn separate_lists_keep_separate_buffers() {
        let ctx = egui::Context::default();
        typed_ctx(&ctx, &['s'], 0.0);
        assert_eq!(
            list_type_ahead(&ctx, "processes"),
            Some(TypeAhead::Cycle('s'))
        );
        assert_eq!(
            list_type_ahead(&ctx, "details"),
            Some(TypeAhead::Cycle('s')),
            "a second list starts its own word from the same keystroke"
        );
    }

    #[test]
    fn prefix_match_takes_the_first_display_order_hit() {
        let rows = [
            (1, "conhost.exe"),
            (2, "svchost.exe"),
            (3, "SVCHOST.EXE"),
            (4, "services.exe"),
        ];
        assert_eq!(prefix_match(rows, "svc"), Some(2));
        assert_eq!(prefix_match(rows, "SVCH"), Some(2));
        assert_eq!(prefix_match(rows, "se"), Some(4));
        assert_eq!(prefix_match(rows, "zz"), None);
    }

    #[test]
    fn type_ahead_match_routes_cycle_and_prefix() {
        let rows = [(1, "svchost.exe"), (2, "services.exe"), (3, "conhost.exe")];
        assert_eq!(
            type_ahead_match(rows, None, &TypeAhead::Cycle('s')),
            Some(1)
        );
        assert_eq!(
            type_ahead_match(rows, Some(1), &TypeAhead::Cycle('s')),
            Some(2),
            "repeating the letter advances"
        );
        assert_eq!(
            type_ahead_match(rows, Some(1), &TypeAhead::Prefix("svc".into())),
            Some(1),
            "extending the word refines instead of advancing"
        );
    }

    #[test]
    fn online_search_url_encodes_utf8_and_reserved_bytes() {
        assert_eq!(
            online_search_url("A&B ä"),
            "https://www.bing.com/search?q=A%26B+%C3%A4"
        );
    }
}
