//! taskman-payload - build-time tool for the self-contained setup artifact.
//!
//! `build.py` runs this on the build host (which may be x64 while the setup
//! target is ARM64):
//!
//! ```text
//! taskman-payload <setup-exe> <output-exe> <name>=<file> [<name>=<file>...]
//! taskman-payload verify <setup-exe>
//! ```
//!
//! The first form appends the payload archive; `verify` re-parses a finished
//! artifact and hash-checks every embedded entry, which `build.py` runs right
//! after packaging. The archive format is defined in
//! `tm_installer::payload`; keeping writer, reader and verifier in one crate
//! means the format cannot drift between them.

use std::path::{Path, PathBuf};

use tm_installer::payload;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.len() == 2 && args[0] == "verify" {
        verify(&PathBuf::from(&args[1]));
        return;
    }

    let usage = "usage: taskman-payload <setup-exe> <output-exe> <name>=<file>...\n       taskman-payload verify <setup-exe>";
    if args.len() < 3 {
        eprintln!("taskman-payload: {usage}");
        std::process::exit(2);
    }
    let setup = PathBuf::from(&args[0]);
    let out = PathBuf::from(&args[1]);
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for spec in &args[2..] {
        let Some((name, path)) = spec.split_once('=') else {
            eprintln!("taskman-payload: entry {spec:?} is not <name>=<file>");
            std::process::exit(2);
        };
        files.push((name.to_string(), PathBuf::from(path)));
    }

    match payload::embed(&setup, &out, &files) {
        Ok(()) => {
            let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!(
                "taskman-payload: wrote {} ({} entries, {} bytes)",
                out.display(),
                files.len(),
                size
            );
        }
        Err(error) => {
            eprintln!("taskman-payload: {error}");
            std::process::exit(1);
        }
    }
}

/// Parse a finished setup artifact and hash-check every embedded entry.
fn verify(setup: &Path) {
    let result = payload::Archive::open(setup).and_then(|archive| {
        let count = archive.verify_all()?;
        let total: u64 = archive
            .manifest
            .entries
            .iter()
            .map(|e| e.uncompressed)
            .sum();
        for entry in &archive.manifest.entries {
            println!(
                "taskman-payload: entry {}: {} -> {} bytes (sha256 {}...)",
                entry.name,
                entry.compressed,
                entry.uncompressed,
                &entry.sha256[..12.min(entry.sha256.len())]
            );
        }
        println!(
            "taskman-payload: verified {} entries, {} uncompressed bytes",
            count, total
        );
        Ok(())
    });
    if let Err(error) = result {
        eprintln!(
            "taskman-payload: verify failed for {}: {error}",
            setup.display()
        );
        std::process::exit(1);
    }
}
