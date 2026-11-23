//! Linux fontconfig text settings, parsed from `fc-match -v`.
//!
//! This is intentionally a process-wide, one-shot query: fonts do not change while the
//! GUI runs, and spawning `fc-match` per frame would be both slow and racy. The command
//! is resolved from a small set of root-owned absolute system paths rather than `PATH`,
//! but it inherits the user's environment so `FONTCONFIG_FILE`, `XDG_CONFIG_HOME`, `HOME`
//! and desktop-specific overrides continue to select the user's real configuration.

use super::text_rendering::{LcdFilterChoice, Params};

const TRUSTED_FC_MATCH_PATHS: &[&str] = &[
    "/usr/bin/fc-match",
    "/bin/fc-match",
    "/usr/local/bin/fc-match",
    "/run/current-system/sw/bin/fc-match",
    "/nix/var/nix/profiles/default/bin/fc-match",
];

#[derive(Clone, Copy, Debug)]
struct ProgramMetadata {
    owner_uid: u32,
    mode: u32,
    is_file: bool,
}

fn is_trusted_program(metadata: ProgramMetadata) -> bool {
    metadata.is_file
        && metadata.owner_uid == 0
        && metadata.mode & 0o022 == 0
        && metadata.mode & 0o111 != 0
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FontconfigProperties {
    antialias: Option<bool>,
    rgba: Option<u8>,
    lcdfilter: Option<u8>,
}

fn token(value: &str) -> Option<&str> {
    value
        .trim()
        .split(['(', ' ', '\t'])
        .find(|part| !part.is_empty())
}

fn parse_bool(value: &str) -> Option<bool> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn parse_rgba(value: &str) -> Option<u8> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "unknown" | "0" => Some(0),
        "rgb" | "1" => Some(1),
        "bgr" | "2" => Some(2),
        "vrgb" | "3" => Some(3),
        "vbgr" | "4" => Some(4),
        "none" | "5" => Some(5),
        _ => None,
    }
}

fn parse_lcdfilter(value: &str) -> Option<u8> {
    match token(value)?.to_ascii_lowercase().as_str() {
        "none" | "0" => Some(0),
        "default" | "1" => Some(1),
        "light" | "2" => Some(2),
        "legacy" | "3" => Some(3),
        _ => None,
    }
}

fn parse_properties(text: &str) -> FontconfigProperties {
    let mut properties = FontconfigProperties::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "antialias" => properties.antialias = parse_bool(value),
            "rgba" => properties.rgba = parse_rgba(value),
            "lcdfilter" => properties.lcdfilter = parse_lcdfilter(value),
            _ => {}
        }
    }
    properties
}

fn params_from_output(text: &str) -> Params {
    let properties = parse_properties(text);
    let mut params = Params::default();

    // `antialias: False` is an explicit user choice. Missing or malformed values are not
    // evidence that sub-pixel text is safe, so they fail closed too.
    if properties.antialias != Some(true) {
        return params;
    }

    match properties.rgba {
        Some(1) => params.enabled = true,
        Some(2) => {
            params.enabled = true;
            params.bgr = true;
        }
        _ => return params,
    }

    params.lcd_filter = match properties.lcdfilter {
        Some(0) => LcdFilterChoice::None,
        Some(2) => LcdFilterChoice::Light,
        Some(3) => LcdFilterChoice::Legacy,
        // Fontconfig's default and unknown future values use FreeType's default filter.
        _ => LcdFilterChoice::Default,
    };
    params
}

#[cfg(target_os = "linux")]
fn resolve_fc_match() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    for candidate in TRUSTED_FC_MATCH_PATHS {
        let path = std::path::Path::new(candidate);
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if is_trusted_program(ProgramMetadata {
            owner_uid: metadata.uid(),
            mode: metadata.permissions().mode(),
            is_file: metadata.is_file(),
        }) {
            return Some(path.to_path_buf());
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub(super) fn query() -> Params {
    static CACHE: std::sync::OnceLock<Params> = std::sync::OnceLock::new();
    *CACHE.get_or_init(query_uncached)
}

#[cfg(target_os = "linux")]
fn query_uncached() -> Params {
    let Some(program) = resolve_fc_match() else {
        tracing::debug!("no trusted absolute fc-match found; sub-pixel text disabled");
        return Params::default();
    };
    let Ok(output) = std::process::Command::new(program)
        .args(["-v", "sans-serif"])
        .output()
    else {
        return Params::default();
    };
    if !output.status.success() {
        return Params::default();
    }
    params_from_output(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(antialias: &str, rgba: &str, lcdfilter: &str) -> String {
        format!(
            "Pattern has 34 elts (size 16)\n\
             \tantialias: {antialias}(w)\n\
             \trgba: {rgba}(i)(w)\n\
             \tlcdfilter: {lcdfilter}(i)(w)\n"
        )
    }

    #[test]
    fn horizontal_orders_enable_the_matching_stripe_order() {
        let rgb = params_from_output(&output("True", "1", "1"));
        assert!(rgb.enabled);
        assert!(!rgb.bgr);

        let bgr = params_from_output(&output("True", "2", "1"));
        assert!(bgr.enabled);
        assert!(bgr.bgr);
    }

    #[test]
    fn unknown_vertical_none_and_malformed_orders_fail_closed() {
        for rgba in ["0", "3", "4", "5", "bogus", ""] {
            let params = params_from_output(&output("True", rgba, "1"));
            assert!(!params.enabled, "rgba={rgba:?} must stay grayscale");
        }
        assert!(!params_from_output("rgba: 1(i)(w)\n").enabled);
        assert!(!params_from_output("garbage\n").enabled);
    }

    #[test]
    fn antialias_false_disables_subpixel_text() {
        let params = params_from_output(&output("False", "1", "1"));
        assert!(!params.enabled);
        assert!(!params_from_output(&output("not-a-bool", "1", "1")).enabled);
    }

    #[test]
    fn lcdfilter_maps_to_the_fork_filter_choices() {
        let filter = |value| params_from_output(&output("True", "1", value)).lcd_filter;
        assert_eq!(filter("0"), LcdFilterChoice::None);
        assert_eq!(filter("1"), LcdFilterChoice::Default);
        assert_eq!(filter("2"), LcdFilterChoice::Light);
        assert_eq!(filter("3"), LcdFilterChoice::Legacy);
        assert_eq!(filter("future"), LcdFilterChoice::Default);
    }

    #[test]
    fn trusted_path_policy_rejects_user_or_writable_programs() {
        assert!(is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o755,
            is_file: true,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 1000,
            mode: 0o755,
            is_file: true,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o775,
            is_file: true,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o644,
            is_file: true,
        }));
        assert!(!is_trusted_program(ProgramMetadata {
            owner_uid: 0,
            mode: 0o755,
            is_file: false,
        }));
        assert!(
            TRUSTED_FC_MATCH_PATHS
                .iter()
                .all(|path| path.starts_with('/'))
        );
    }
}
