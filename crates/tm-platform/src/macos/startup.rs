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
                enabled: true,
                impact: StartupImpact::Unknown,
            });
        }
    }
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
}

struct ParseOut_ {
    program: Option<String>,
}

fn parse_plist(path: &Path) -> ParseOut_ {
    // Minimal XML plist scan for <key>Program</key><string>…</string>
    parse(path)
}

fn parse(path: &Path) -> ParseOut_ {
    // Minimal XML plist scan for <key>Program</key><string>…</string>
    let mut out = ParseOut_ { program: None };
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Some(idx) = text.find("<key>Program</key>") {
            let rest = &text[idx..];
            if let Some(start) = rest.find("<string>") {
                if let Some(end) = rest[start + 8..].find("</string>") {
                    out.program = Some(rest[start + 8..start + 8 + end].to_string());
                }
            }
        } else if let Some(idx) = text.find("<key>ProgramArguments</key>") {
            let rest = &text[idx..];
            if let Some(start) = rest.find("<string>") {
                if let Some(end) = rest[start + 8..].find("</string>") {
                    out.program = Some(rest[start + 8..start + 8 + end].to_string());
                }
            }
        }
    }
    out
}
