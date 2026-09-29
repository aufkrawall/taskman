//! Self-extracting payload archive appended to `taskman-setup.exe`.
//!
//! The setup artifact is a plain `taskman-setup.exe` with one appended
//! payload region:
//!
//! ```text
//! [ setup executable ][ deflate blob region ][ manifest JSON ][ footer ]
//! ```
//!
//! The footer is 16 bytes at end-of-file: `manifest_len: u64 LE` followed by
//! [`MAGIC`]. Everything before it is derived from those two numbers, so the
//! setup executable itself can change size freely (it is produced by a
//! separate link step) without invalidating the payload.
//!
//! Design constraints:
//! * 100% Rust: deflate via `flate2`'s miniz_oxide backend, hashing via
//!   `sha2`, no native libraries and no external archiver.
//! * The reader treats the archive as UNTRUSTED input: it validates the
//!   magic, the manifest shape, every offset/length pair, rejects path
//!   traversal in entry names, and verifies each entry's SHA-256 after
//!   inflation before a single byte is written to disk.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tm_core::error::{Result, TmError};

/// End-of-file marker identifying a payload-carrying setup executable.
///
/// LE bytes of the ASCII string `TMSUPAYL`.
pub const MAGIC: u64 = u64::from_le_bytes(*b"TMSUPAYL");

/// Footer size: `manifest_len` plus [`MAGIC`].
pub const FOOTER_LEN: u64 = 16;

/// Archive format version; bumped when the manifest shape changes
/// incompatibly.
pub const FORMAT_VERSION: u32 = 1;

/// One embedded file. Offsets are relative to the start of the blob region.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub offset: u64,
    pub compressed: u64,
    pub uncompressed: u64,
    /// Lowercase hex SHA-256 of the uncompressed bytes.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub entries: Vec<Entry>,
}

/// A parsed payload region of a setup executable.
#[derive(Debug)]
pub struct Archive {
    pub manifest: Manifest,
    /// The complete deflate blob region, still compressed.
    blobs: Vec<u8>,
}

fn err(context: &'static str, detail: impl Into<String>) -> TmError {
    TmError::platform(context, detail)
}

/// Reject entry names that could escape the extraction directory.
///
/// Payload entries are always flat file names (`taskman.exe`, `LICENSE`); a
/// separator, drive marker or dot-dot has no legitimate use here and is the
/// classic self-extractor traversal hole.
pub fn validate_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.len() > 128
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.contains("..")
        || name.starts_with('.')
        || name.trim() != name;
    if bad {
        return Err(err(
            "payload entry name",
            format!("unsafe entry name: {name:?}"),
        ));
    }
    Ok(())
}

fn hex_sha256(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Compress and write the blob region for `files`, returning the manifest
/// describing it. The caller writes the manifest and footer after this.
pub fn write_blobs(out: &mut impl Write, files: &[(String, Vec<u8>)]) -> Result<Manifest> {
    let mut entries = Vec::with_capacity(files.len());
    let mut offset = 0u64;
    for (name, data) in files {
        validate_name(name)?;
        if entries.iter().any(|entry: &Entry| &entry.name == name) {
            return Err(err(
                "payload entry",
                format!("duplicate entry name: {name}"),
            ));
        }
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data)?;
        let compressed = encoder.finish()?;
        out.write_all(&compressed)?;
        entries.push(Entry {
            name: name.clone(),
            offset,
            compressed: compressed.len() as u64,
            uncompressed: data.len() as u64,
            sha256: hex_sha256(data),
        });
        offset += compressed.len() as u64;
    }
    Ok(Manifest {
        version: FORMAT_VERSION,
        entries,
    })
}

/// Build the complete payload region (blobs + manifest + footer) for `files`.
pub fn build_payload(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut blobs = Vec::new();
    let manifest = write_blobs(&mut blobs, files)?;
    let manifest_json = serde_json::to_vec(&manifest)?;
    let mut out = blobs;
    out.extend_from_slice(&manifest_json);
    out.extend_from_slice(&(manifest_json.len() as u64).to_le_bytes());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    Ok(out)
}

/// Append the payload for `files` to `setup_exe`, producing `out`.
///
/// Used by the `taskman-payload` build tool: the setup executable is linked
/// for the *target* architecture (possibly ARM64 cross-compiled) while this
/// tool runs on the build host.
pub fn embed(setup_exe: &Path, out: &Path, files: &[(String, PathBuf)]) -> Result<()> {
    let setup = std::fs::read(setup_exe)?;
    if setup.len() < 2 || &setup[..2] != b"MZ" {
        return Err(err(
            "embed payload",
            format!("{} is not a Windows executable", setup_exe.display()),
        ));
    }
    let mut inputs = Vec::with_capacity(files.len());
    for (name, path) in files {
        validate_name(name)?;
        let data = std::fs::read(path)
            .map_err(|error| err("embed payload", format!("{}: {error}", path.display())))?;
        inputs.push((name.clone(), data));
    }
    let payload = build_payload(&inputs)?;
    let mut file = std::fs::File::create(out)?;
    file.write_all(&setup)?;
    file.write_all(&payload)?;
    file.flush()?;
    Ok(())
}

impl Archive {
    /// Parse the payload region out of the complete setup executable bytes.
    pub fn parse(data: Vec<u8>) -> Result<Archive> {
        let len = data.len() as u64;
        if len < FOOTER_LEN {
            return Err(err("payload", "file is too small to carry a payload"));
        }
        let footer_at = len - FOOTER_LEN;
        let manifest_len = u64::from_le_bytes(
            data[footer_at as usize..footer_at as usize + 8]
                .try_into()
                .expect("8-byte slice"),
        );
        let magic = u64::from_le_bytes(
            data[footer_at as usize + 8..footer_at as usize + 16]
                .try_into()
                .expect("8-byte slice"),
        );
        if magic != MAGIC {
            return Err(err(
                "payload",
                "no embedded payload found (bad footer magic): this is a bare \
                 taskman-setup build output, not the packaged installer - run \
                 taskman-v<version>-windows-<arch>-setup.exe from dist/ (or \
                 append the payload with taskman-payload)",
            ));
        }
        if manifest_len < 2 || manifest_len > footer_at {
            return Err(err(
                "payload",
                format!("implausible manifest length {manifest_len}"),
            ));
        }
        let manifest_start = footer_at - manifest_len;
        let manifest: Manifest =
            serde_json::from_slice(&data[manifest_start as usize..footer_at as usize])
                .map_err(|error| err("payload", format!("manifest is not valid JSON: {error}")))?;
        if manifest.version != FORMAT_VERSION {
            return Err(err(
                "payload",
                format!(
                    "unsupported payload format {} (this setup understands {})",
                    manifest.version, FORMAT_VERSION
                ),
            ));
        }
        if manifest.entries.is_empty() {
            return Err(err("payload", "payload manifest has no entries"));
        }
        // Derive the blob region from the entries, then require it to end
        // exactly where the manifest begins: any mismatch means the numbers
        // in the manifest do not describe this file.
        let mut blob_len = 0u64;
        for entry in &manifest.entries {
            validate_name(&entry.name)?;
            if entry.uncompressed == 0 || entry.compressed == 0 {
                return Err(err("payload", format!("entry {} is empty", entry.name)));
            }
            let end = entry
                .offset
                .checked_add(entry.compressed)
                .ok_or_else(|| err("payload", format!("entry {} overflows", entry.name)))?;
            blob_len = blob_len.max(end);
        }
        if manifest_start < blob_len {
            return Err(err("payload", "payload manifest overlaps the blob region"));
        }
        let blob_start = manifest_start - blob_len;
        // Entries may not overlap each other: overlapping ranges would let one
        // stream's bytes double as another's, and the hash check would only
        // cover the concatenated view.
        let mut ranges: Vec<(u64, u64, &str)> = manifest
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.offset,
                    entry.offset + entry.compressed,
                    entry.name.as_str(),
                )
            })
            .collect();
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(err(
                    "payload",
                    format!("entries {} and {} overlap", pair[0].2, pair[1].2),
                ));
            }
        }
        let blobs = data[blob_start as usize..manifest_start as usize].to_vec();
        Ok(Archive { manifest, blobs })
    }

    /// Open the payload embedded in the setup executable at `path`.
    pub fn open(path: &Path) -> Result<Archive> {
        let data = std::fs::read(path)?;
        Archive::parse(data)
    }

    pub fn entry(&self, name: &str) -> Result<&Entry> {
        self.manifest
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| err("payload", format!("payload has no entry named {name}")))
    }

    /// Inflate and hash-verify one entry. Nothing is written to disk before
    /// this returns `Ok`.
    pub fn read_entry(&self, entry: &Entry) -> Result<Vec<u8>> {
        let start = entry.offset as usize;
        let end = (entry.offset + entry.compressed) as usize;
        let slice = self
            .blobs
            .get(start..end)
            .ok_or_else(|| err("payload", format!("entry {} out of range", entry.name)))?;
        let mut decoder = flate2::read::DeflateDecoder::new(slice);
        let mut out = Vec::with_capacity(entry.uncompressed.min(64 * 1024 * 1024) as usize);
        decoder.read_to_end(&mut out)?;
        if out.len() as u64 != entry.uncompressed {
            return Err(err(
                "payload",
                format!(
                    "entry {} inflates to {} bytes, manifest says {}",
                    entry.name,
                    out.len(),
                    entry.uncompressed
                ),
            ));
        }
        if hex_sha256(&out) != entry.sha256 {
            return Err(err(
                "payload",
                format!("entry {} fails its SHA-256 check", entry.name),
            ));
        }
        Ok(out)
    }

    /// Inflate and hash-verify every entry without writing anything to disk.
    ///
    /// This is what `taskman-payload verify` runs against a finished setup
    /// artifact: packaging is not "done" until the produced file re-parses
    /// and every embedded entry verifies.
    pub fn verify_all(&self) -> Result<usize> {
        for entry in &self.manifest.entries {
            self.read_entry(entry)?;
        }
        Ok(self.manifest.entries.len())
    }

    /// Extract every entry into `dir`, returning the written paths.
    ///
    /// All entries are inflated and verified in memory first; a corrupt
    /// payload therefore never leaves a half-written install directory behind.
    pub fn extract_to(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut verified = Vec::with_capacity(self.manifest.entries.len());
        for entry in &self.manifest.entries {
            let data = self.read_entry(entry)?;
            verified.push((entry.name.clone(), data));
        }
        std::fs::create_dir_all(dir)?;
        let mut paths = Vec::with_capacity(verified.len());
        for (name, data) in verified {
            let path = dir.join(&name);
            std::fs::write(&path, &data)?;
            paths.push(path);
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<(String, Vec<u8>)> {
        vec![
            ("taskman.exe".to_string(), b"MZ fake gui binary".to_vec()),
            ("taskman-service.exe".to_string(), vec![0u8; 4096]),
            ("LICENSE".to_string(), b"MIT".to_vec()),
        ]
    }

    fn fake_setup(payload: &[u8]) -> Vec<u8> {
        let mut exe = b"MZ fake setup executable".to_vec();
        exe.extend_from_slice(&vec![0x90; 1234]);
        exe.extend_from_slice(payload);
        exe
    }

    /// The full round trip: build a payload, stick it behind a fake
    /// executable, parse it back, extract, and compare every byte.
    #[test]
    fn payload_round_trips_through_a_setup_executable() {
        let payload = build_payload(&files()).unwrap();
        let exe = fake_setup(&payload);
        let archive = Archive::parse(exe).unwrap();
        assert_eq!(archive.manifest.version, FORMAT_VERSION);
        assert_eq!(archive.manifest.entries.len(), 3);

        let dir = tempfile::tempdir().unwrap();
        let written = archive.extract_to(dir.path()).unwrap();
        assert_eq!(written.len(), 3);
        for (name, data) in files() {
            assert_eq!(
                std::fs::read(dir.path().join(&name)).unwrap(),
                data,
                "{name}"
            );
        }
        let license = archive.entry("LICENSE").unwrap();
        assert_eq!(license.uncompressed, 3);
        assert_eq!(license.sha256.len(), 64);
    }

    /// Compression must actually compress: the repetitive service stub has to
    /// come out far smaller than its input.
    #[test]
    fn payload_entries_are_deflate_compressed() {
        let payload = build_payload(&files()).unwrap();
        let archive = Archive::parse(fake_setup(&payload)).unwrap();
        let service = archive.entry("taskman-service.exe").unwrap();
        assert!(service.compressed < service.uncompressed / 2);
    }

    #[test]
    fn missing_payload_is_reported_not_guessed() {
        let error = Archive::parse(b"MZ plain exe with no payload".to_vec()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no embedded payload"), "{error}");
        // The message must route a human to the packaged installer instead of
        // asking about build tooling.
        assert!(message.contains("bare"), "{error}");
        assert!(message.contains("dist/"), "{error}");
    }

    #[test]
    fn truncated_file_is_rejected() {
        assert!(Archive::parse(Vec::new()).is_err());
        assert!(Archive::parse(b"MZ".to_vec()).is_err());
        let payload = build_payload(&files()).unwrap();
        let exe = fake_setup(&payload);
        // Cut into the footer.
        assert!(Archive::parse(exe[..exe.len() - 4].to_vec()).is_err());
    }

    /// A tampered blob must be caught by the hash check, and no file may be
    /// written for a corrupt archive.
    #[test]
    fn tampered_blob_fails_the_hash_check_before_extraction() {
        let payload = build_payload(&files()).unwrap();
        let mut exe = fake_setup(&payload);
        // Flip a byte in the middle of the blob region.
        let blob_index = exe.len() - payload.len() + 8;
        exe[blob_index] ^= 0xff;
        let archive = Archive::parse(exe).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let error = archive.extract_to(dir.path()).unwrap_err();
        assert!(error.to_string().contains("SHA-256"), "{error}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "nothing may be written for a corrupt payload"
        );
    }

    /// The manifest describes the blobs it was written with; a manifest whose
    /// lengths do not reach the manifest itself proves the file was edited.
    #[test]
    fn inconsistent_manifest_is_rejected() {
        let payload = build_payload(&files()).unwrap();
        let mut exe = fake_setup(&payload);
        let footer_at = exe.len() - FOOTER_LEN as usize;
        // Claim a longer manifest than exists: the blob region computation
        // then lands before the manifest and the overlap check fires (or the
        // JSON itself is garbage - either way it must error).
        exe[footer_at..footer_at + 8].copy_from_slice(&((payload.len() as u64) * 2).to_le_bytes());
        assert!(Archive::parse(exe).is_err());
    }

    /// `verify` (used by build.py after packaging) proves a finished setup
    /// artifact re-parses and every entry hash-checks, without writing files.
    #[test]
    fn verify_all_checks_every_entry_without_writing() {
        let payload = build_payload(&files()).unwrap();
        let archive = Archive::parse(fake_setup(&payload)).unwrap();
        assert_eq!(archive.verify_all().unwrap(), 3);
    }

    #[test]
    fn verify_all_rejects_a_tampered_entry() {
        let payload = build_payload(&files()).unwrap();
        let mut exe = fake_setup(&payload);
        let blob_index = exe.len() - payload.len() + 8;
        exe[blob_index] ^= 0xff;
        let archive = Archive::parse(exe).unwrap();
        assert!(archive.verify_all().is_err());
    }

    /// The build path used by `taskman-payload`: append a payload to a real
    /// setup executable on disk, then read it back with `Archive::open`.
    #[test]
    fn embed_writes_a_readable_payload_onto_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let setup = dir.path().join("setup.exe");
        std::fs::write(&setup, b"MZ fake setup executable").unwrap();
        let gui = dir.path().join("taskman.exe");
        std::fs::write(&gui, b"MZ fake gui binary").unwrap();
        let service = dir.path().join("taskman-service.exe");
        std::fs::write(&service, b"MZ fake service binary").unwrap();
        let out = dir.path().join("setup-full.exe");

        embed(
            &setup,
            &out,
            &[
                ("taskman.exe".to_string(), gui),
                ("taskman-service.exe".to_string(), service),
            ],
        )
        .unwrap();

        let archive = Archive::open(&out).unwrap();
        assert_eq!(archive.manifest.entries.len(), 2);
        let extracted = archive
            .read_entry(archive.entry("taskman.exe").unwrap())
            .unwrap();
        assert_eq!(extracted, b"MZ fake gui binary");
    }

    /// `embed` must refuse a source that is not a Windows executable: a
    /// misconfigured build would otherwise produce a "setup" that silently
    /// is not runnable.
    #[test]
    fn embed_rejects_a_non_executable_setup() {
        let dir = tempfile::tempdir().unwrap();
        let setup = dir.path().join("setup.txt");
        std::fs::write(&setup, b"not an executable").unwrap();
        let gui = dir.path().join("taskman.exe");
        std::fs::write(&gui, b"MZ").unwrap();
        let out = dir.path().join("out.exe");
        let error = embed(&setup, &out, &[("taskman.exe".to_string(), gui)]).unwrap_err();
        assert!(
            error.to_string().contains("not a Windows executable"),
            "{error}"
        );
    }

    #[test]
    fn traversal_entry_names_are_refused() {
        for name in [
            "../evil", "a/b", "a\\b", "C:evil", ".hidden", "", " padded ",
        ] {
            assert!(validate_name(name).is_err(), "{name:?} must be rejected");
        }
        for name in ["taskman.exe", "taskman-service.exe", "LICENSE"] {
            validate_name(name).unwrap();
        }
    }

    /// A manifest that smuggles a traversal name past the writer must still
    /// be refused by the reader.
    #[test]
    fn parser_rejects_smuggled_traversal_names() {
        let payload = build_payload(&files()).unwrap();
        let exe = fake_setup(&payload);
        let footer_at = exe.len() - FOOTER_LEN as usize;
        let manifest_len =
            u64::from_le_bytes(exe[footer_at..footer_at + 8].try_into().unwrap()) as usize;
        let manifest_start = footer_at - manifest_len;
        let mut manifest: Manifest =
            serde_json::from_slice(&exe[manifest_start..footer_at]).unwrap();
        manifest.entries[0].name = "../taskman.exe".into();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let mut rebuilt = exe[..manifest_start].to_vec();
        rebuilt.extend_from_slice(&manifest_json);
        rebuilt.extend_from_slice(&(manifest_json.len() as u64).to_le_bytes());
        rebuilt.extend_from_slice(&MAGIC.to_le_bytes());
        let error = Archive::parse(rebuilt).unwrap_err();
        assert!(error.to_string().contains("unsafe entry name"), "{error}");
    }
}
