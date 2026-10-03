//! DLL-planting guard for `taskman-setup.exe`.
//!
//! Setup runs elevated, typically from a user-writable folder, and the
//! loader resolves a static import that is not a KnownDLL from the
//! executable's own folder before System32. Every such import must therefore
//! be delay-loaded (`build.rs`), to be resolved only after `main` confines
//! the search path to System32. A new dependency that adds a non-KnownDLL
//! import fails this test instead of silently reopening the hole.
#![cfg(all(windows, target_env = "msvc"))]

use std::collections::BTreeSet;

struct Pe {
    data: Vec<u8>,
    /// (virtual address, virtual size, raw pointer, raw size)
    sections: Vec<(u32, u32, u32, u32)>,
    directories: usize,
}

impl Pe {
    fn parse(data: Vec<u8>) -> Pe {
        let u16_at = |at: usize| u16::from_le_bytes(data[at..at + 2].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        assert_eq!(&data[..2], b"MZ");
        let pe = u32_at(0x3c) as usize;
        assert_eq!(&data[pe..pe + 4], b"PE\0\0");
        let coff = pe + 4;
        let sections = u16_at(coff + 2) as usize;
        let optional = coff + 20;
        let optional_size = u16_at(coff + 16) as usize;
        let directories = match u16_at(optional) {
            0x20b => optional + 112, // PE32+
            0x10b => optional + 96,  // PE32
            magic => panic!("unknown optional header magic {magic:#x}"),
        };
        let table = optional + optional_size;
        let sections = (0..sections)
            .map(|index| {
                let at = table + index * 40;
                (
                    u32_at(at + 12),
                    u32_at(at + 8),
                    u32_at(at + 20),
                    u32_at(at + 16),
                )
            })
            .collect();
        Pe {
            data,
            sections,
            directories,
        }
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.data[at..at + 4].try_into().unwrap())
    }

    fn offset(&self, rva: u32) -> usize {
        let (va, _, raw, _) = self
            .sections
            .iter()
            .copied()
            .find(|&(va, size, _, raw_size)| rva >= va && rva < va + size.max(raw_size))
            .unwrap_or_else(|| panic!("RVA {rva:#x} is outside every section"));
        (rva - va + raw) as usize
    }

    fn name_at(&self, rva: u32) -> String {
        let start = self.offset(rva);
        let end = start + self.data[start..].iter().position(|&b| b == 0).unwrap();
        String::from_utf8_lossy(&self.data[start..end]).to_ascii_lowercase()
    }

    /// DLL names from data directory `index`, whose descriptors are `stride`
    /// bytes with the name RVA at `name_field`.
    fn dlls(&self, index: usize, stride: usize, name_field: usize) -> BTreeSet<String> {
        let rva = self.u32_at(self.directories + index * 8);
        let mut names = BTreeSet::new();
        if rva == 0 {
            return names;
        }
        let mut at = self.offset(rva);
        loop {
            let name = self.u32_at(at + name_field);
            if name == 0 {
                return names;
            }
            names.insert(self.name_at(name));
            at += stride;
        }
    }
}

/// The registry's KnownDLLs: the loader maps these from the `\KnownDlls`
/// section directory and never searches the application folder for them.
fn known_dlls() -> BTreeSet<String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\KnownDLLs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "cannot read KnownDLLs");
    let known: BTreeSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once("REG_SZ"))
        .map(|(_, value)| value.trim().to_ascii_lowercase())
        .collect();
    assert!(known.contains("kernel32.dll"), "{known:?}");
    known
}

#[test]
fn setup_statically_imports_only_known_dlls() {
    let exe = env!("CARGO_BIN_EXE_taskman-setup");
    let pe = Pe::parse(std::fs::read(exe).unwrap());
    let imports = pe.dlls(1, 20, 12);
    let delayed = pe.dlls(13, 32, 4);
    let known = known_dlls();

    // ntdll is mapped into every process before any import is resolved, and
    // API sets resolve through the API set schema, never through a search.
    let planted: Vec<&String> = imports
        .iter()
        .filter(|dll| {
            !(known.contains(*dll) || *dll == "ntdll.dll" || dll.starts_with("api-ms-win-"))
        })
        .collect();
    assert!(
        planted.is_empty(),
        "taskman-setup statically imports non-KnownDLLs {planted:?}; add them to \
         DELAY_LOADED in crates/tm-installer/build.rs (static: {imports:?})"
    );
    assert!(
        !imports
            .iter()
            .chain(&delayed)
            .any(|dll| dll.starts_with("vcruntime")),
        "the VC runtime must be linked statically (hybrid CRT): {imports:?} {delayed:?}"
    );
    assert!(
        delayed.contains("uxtheme.dll"),
        "delay-load wiring is missing: {delayed:?}"
    );
}
