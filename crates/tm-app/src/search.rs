//! Shared global-search matching (audit §5): ONE normalized query applied to
//! every tab's candidate fields — binary name, display name, PID and
//! publisher/company for processes — instead of per-tab string comparisons
//! that drift apart. The search hint promises "name, publisher or PID"; this
//! module makes the code keep that promise.

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
}
