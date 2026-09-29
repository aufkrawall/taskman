//! taskman-payload - build-time tool for the self-contained setup artifact.
//!
//! `build.py` runs this on the build host (which may be x64 while the setup
//! target is ARM64) to append the payload archive to a linked
//! `taskman-setup.exe`:
//!
//! ```text
//! taskman-payload <setup-exe> <output-exe> <name>=<file> [<name>=<file>...]
//! ```
//!
//! The archive format is defined in `tm_installer::payload`; keeping both
//! sides in one crate means the format cannot drift between writer and
//! reader.

use std::path::PathBuf;

use tm_installer::payload;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: taskman-payload <setup-exe> <output-exe> <name>=<file>...";
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
