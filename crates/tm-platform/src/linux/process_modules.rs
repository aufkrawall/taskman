//! Read-only module inventory from `/proc/<pid>/maps`.
//!
//! The action wrapper brackets this read with the sampled process generation.
//! This module owns only the procfs record parsing and file access, so a
//! permission failure is returned as a failure rather than an empty module
//! list.

use crate::actions::ProcessModule;
use std::io::Read;
use std::path::Path;
use tm_core::error::{Result, TmError};

/// Executable mappings are normally well below a megabyte. A hostile procfs
/// replacement or a pathological process must not turn a page query into an
/// unbounded allocation.
const MAX_MAPS_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mapping {
    start: u64,
    end: u64,
    executable: bool,
    device: String,
    inode: u64,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    start: u64,
    end: u64,
    executable: bool,
    device: String,
    inode: u64,
    path: String,
}

fn next_field<'a>(line: &'a str, cursor: &mut usize) -> Option<&'a str> {
    let bytes = line.as_bytes();
    while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
        *cursor += 1;
    }
    let start = *cursor;
    while bytes
        .get(*cursor)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        *cursor += 1;
    }
    (*cursor > start).then_some(&line[start..*cursor])
}

fn parse_hex_u64(value: &str) -> Option<u64> {
    u64::from_str_radix(value, 16).ok()
}

/// Parse one maps line. Only the five fixed columns are tokenized; the
/// optional pathname is taken verbatim because it can contain spaces.
fn parse_maps_line(line: &str) -> Option<Mapping> {
    let mut cursor = 0;
    let range = next_field(line, &mut cursor)?;
    let perms = next_field(line, &mut cursor)?;
    let _offset = next_field(line, &mut cursor)?;
    let device = next_field(line, &mut cursor)?;
    let inode = next_field(line, &mut cursor)?;
    if perms.len() != 4 {
        return None;
    }
    while line
        .as_bytes()
        .get(cursor)
        .is_some_and(u8::is_ascii_whitespace)
    {
        cursor += 1;
    }
    let (start, end) = range.split_once('-')?;
    let start = parse_hex_u64(start)?;
    let end = parse_hex_u64(end)?;
    if end <= start {
        return None;
    }
    Some(Mapping {
        start,
        end,
        executable: perms.as_bytes().get(2) == Some(&b'x'),
        device: device.to_string(),
        inode: inode.parse().ok()?,
        path: line[cursor..]
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string(),
    })
}

fn mapped_name(path: &str) -> String {
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned())
}

/// Group adjacent mappings of the same file into one executable module span.
///
/// File identity is the maps device plus inode, not the display path: two
/// mappings at the same path can belong to different incarnations after a
/// replace, and hard links can share one file under different paths. Rows are
/// emitted only for spans containing executable mappings.
fn modules_from_maps(text: &str) -> Vec<ProcessModule> {
    let mut mappings = text
        .lines()
        .filter_map(parse_maps_line)
        .filter(|mapping| {
            mapping.inode != 0 && mapping.path.starts_with('/') && !mapping.path.is_empty()
        })
        .collect::<Vec<_>>();
    mappings.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.end.cmp(&b.end))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut spans: Vec<Span> = Vec::new();
    for mapping in mappings {
        if let Some(previous) = spans.last_mut()
            && previous.end == mapping.start
            && previous.device == mapping.device
            && previous.inode == mapping.inode
        {
            previous.end = mapping.end;
            previous.executable |= mapping.executable;
            continue;
        }
        spans.push(Span {
            start: mapping.start,
            end: mapping.end,
            executable: mapping.executable,
            device: mapping.device,
            inode: mapping.inode,
            path: mapping.path,
        });
    }

    let mut modules = spans
        .into_iter()
        .filter(|span| span.executable)
        .map(|span| ProcessModule {
            name: mapped_name(&span.path),
            path: span.path,
            base_address: span.start,
            size_bytes: span.end.saturating_sub(span.start),
        })
        .collect::<Vec<_>>();
    modules.sort_by(|a, b| {
        a.base_address
            .cmp(&b.base_address)
            .then_with(|| a.path.cmp(&b.path))
    });
    modules
}

pub(super) fn list_process_modules(pid: u32) -> Result<Vec<ProcessModule>> {
    let path = format!("/proc/{pid}/maps");
    let maps = match read_maps(&path) {
        Ok(maps) => maps,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(TmError::ProcessNotFound { pid });
        }
        Err(error) => {
            return Err(TmError::platform(
                "read /proc/pid/maps",
                format!("{path}: {error}"),
            ));
        }
    };
    Ok(modules_from_maps_bytes(&maps))
}

fn read_maps(path: &str) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(64 * 1024);
    file.take(MAX_MAPS_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MAPS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{path} exceeds the {MAX_MAPS_BYTES}-byte safety limit"),
        ));
    }
    Ok(bytes)
}

fn modules_from_maps_bytes(bytes: &[u8]) -> Vec<ProcessModule> {
    modules_from_maps(&String::from_utf8_lossy(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_merge_only_contiguous_mappings_of_the_same_file_object() {
        let maps = "\
7f0000000000-7f0000001000 r--p 00000000 08:01 123 /usr/lib/lib demo.so\n\
7f0000001000-7f0000003000 r-xp 00001000 08:01 123 /usr/lib/lib demo.so\n\
7f0000003000-7f0000004000 rw-p 00003000 08:01 123 /usr/lib/lib demo.so\n\
7f0000005000-7f0000006000 r-xp 00000000 08:01 123 /usr/lib/lib demo.so\n\
7f0000006000-7f0000007000 r-xp 00000000 08:01 999 /usr/lib/lib demo.so\n\
7f0000007000-7f0000008000 r-xp 00000000 00:00 0 [vdso]\n\
7f0000008000-7f0000009000 r-xp 00000000 00:00 0\n";
        let modules = modules_from_maps(maps);
        assert_eq!(modules.len(), 3);
        assert_eq!(modules[0].name, "lib demo.so");
        assert_eq!(modules[0].path, "/usr/lib/lib demo.so");
        assert_eq!(modules[0].base_address, 0x7f0000000000);
        assert_eq!(modules[0].size_bytes, 0x4000);
        assert_eq!(modules[1].base_address, 0x7f0000005000);
        assert_eq!(modules[1].size_bytes, 0x1000);
        assert_eq!(modules[2].base_address, 0x7f0000006000);
        assert_eq!(modules[2].size_bytes, 0x1000);
    }

    #[test]
    fn deleted_suffix_is_kept_in_the_path_but_not_in_the_name() {
        let maps =
            "1000-2000 r-xp 00000000 00:01 42 /tmp/plugin with spaces.so (deleted)\n";
        let modules = modules_from_maps(maps);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].path, "/tmp/plugin with spaces.so (deleted)");
        assert_eq!(modules[0].name, "plugin with spaces.so");
        assert_eq!(modules[0].base_address, 0x1000);
        assert_eq!(modules[0].size_bytes, 0x1000);
    }

    #[test]
    fn non_executable_and_anonymous_mappings_are_not_modules() {
        let maps = "\
1000-2000 rw-p 00000000 08:01 42 /tmp/data.bin\n\
2000-3000 r-xp 00000000 00:00 0 [vdso]\n\
3000-4000 r-xp 00000000 00:00 0\n";
        assert!(modules_from_maps(maps).is_empty());
    }

    #[test]
    fn maps_record_requires_fixed_columns_and_an_address_range() {
        assert!(parse_maps_line("not-a-map").is_none());
        assert!(parse_maps_line("1000-2000 rx 0 00:00 1 /a").is_none());
        assert!(parse_maps_line("1000-1000 r-xp 0 00:00 1 /a").is_none());
        assert!(parse_maps_line("2000-1000 r-xp 0 00:00 1 /a").is_none());
    }

    #[test]
    fn non_utf8_mapping_paths_are_reported_lossily_without_losing_the_page() {
        let maps = b"1000-2000 r-xp 00000000 08:01 42 /tmp/lib\xffname.so\n";
        let modules = modules_from_maps_bytes(maps);
        assert_eq!(modules.len(), 1);
        assert!(modules[0].path.contains('\u{fffd}'));
        assert_eq!(modules[0].name, "lib\u{fffd}name.so");
    }

    #[test]
    fn current_process_has_a_readable_executable_inventory() {
        let modules = list_process_modules(std::process::id()).expect("current process maps");
        assert!(
            modules
                .iter()
                .any(|module| module.base_address > 0 && module.size_bytes > 0),
            "the test executable and its runtime must be mapped"
        );
    }
}
