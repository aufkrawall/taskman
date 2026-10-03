//! LaunchAgents/LaunchDaemons plist listing (read-only inventory).

use std::path::{Path, PathBuf};
use tm_core::model::{StartupImpact, StartupItem};

pub fn list_plists() -> Vec<StartupItem> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dirs = [
        (
            PathBuf::from(format!("{home}/Library/LaunchAgents")),
            "LaunchAgents (user)",
        ),
        (
            PathBuf::from("/Library/LaunchAgents"),
            "LaunchAgents (all users)",
        ),
        (PathBuf::from("/Library/LaunchDaemons"), "LaunchDaemons"),
    ];
    let mut items = Vec::new();
    for (dir, label) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("plist") {
                continue;
            }
            let parsed = parse_plist(&path);
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            items.push(StartupItem {
                id: format!("plist:{}", path.display()),
                name,
                command: parsed
                    .program
                    .unwrap_or_else(|| path.to_string_lossy().to_string()),
                location: format!("{label} ({})", dir.display()),
                publisher: None,
                enabled: !parsed.disabled,
                impact: StartupImpact::Unknown,
            });
        }
    }
    items.sort_by_key(|item| item.name.to_lowercase());
    items
}

#[derive(Debug, Default)]
struct ParseOut_ {
    program: Option<String>,
    /// The job's own `Disabled` key. Overrides recorded by `launchctl
    /// disable` or the Login Items settings live outside the plist and are
    /// not read here.
    disabled: bool,
}

fn parse_plist(path: &Path) -> ParseOut_ {
    std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

fn parse(text: &str) -> ParseOut_ {
    // Minimal XML plist scan for <key>Program</key><string>…</string>
    let key = if text.contains("<key>Program</key>") {
        "<key>Program</key>"
    } else {
        "<key>ProgramArguments</key>"
    };
    let program = text.find(key).and_then(|idx| {
        let rest = &text[idx..];
        let start = rest.find("<string>")? + 8;
        let end = rest[start..].find("</string>")?;
        Some(rest[start..start + end].to_string())
    });
    let disabled = text.find("<key>Disabled</key>").is_some_and(|idx| {
        text[idx + "<key>Disabled</key>".len()..]
            .trim_start()
            .starts_with("<true/>")
    });
    ParseOut_ { program, disabled }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_key_marks_the_job_disabled() {
        let plist = "<dict>\n\t<key>Disabled</key>\n\t<true/>\n\t<key>Program</key>\n\t<string>/usr/local/bin/agent</string>\n</dict>";
        let parsed = parse(plist);
        assert!(parsed.disabled);
        assert_eq!(parsed.program.as_deref(), Some("/usr/local/bin/agent"));

        let parsed = parse(
            "<key>Disabled</key><false/><key>ProgramArguments</key><array><string>/bin/agent</string><string>-x</string></array>",
        );
        assert!(!parsed.disabled);
        assert_eq!(parsed.program.as_deref(), Some("/bin/agent"));

        assert!(!parse("<key>Label</key><string>x</string>").disabled);
    }
}
