//! Physical-disk model for the Linux backend.
//!
//! The Performance page lists PHYSICAL disks the way Windows Task Manager does
//! ("Disk 0 (C: D:)"), not mounted filesystems: active time, throughput and
//! response time are properties of a drive, and a drive with three mounted
//! partitions is one drive. Listing per mount used to repeat the parent disk's
//! counters once per partition, so one busy NVMe drive looked like three busy
//! SSDs.
//!
//! Everything here takes the sysfs root it inspects, so the resolution rules
//! (partition → disk, md/dm → member disks) run against a fake tree in tests on
//! every host. Platform-neutral on purpose, like `linux_telemetry`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tm_core::model::MediaKind;

/// How deep a stacked device may nest (dm on md on partitions…) before the
/// walk gives up. Real stacks are two or three levels; the bound only exists
/// so a malformed or cyclic `slaves` tree cannot recurse forever.
const MAX_STACK_DEPTH: usize = 8;

/// One physical drive, as `/sys/block` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalDisk {
    /// Kernel name (`nvme0n1`, `sda`), which is also the `/proc/diskstats`
    /// row name.
    pub name: String,
    /// Vendor model string when the driver publishes one.
    pub model: String,
    pub media: MediaKind,
    pub size_bytes: u64,
}

/// Every physical drive under `<sys>/block`, sorted by kernel name.
///
/// A physical drive is a block device backed by real hardware: it has a
/// `device` link. Virtual devices — loop, zram, md, dm, nbd — have none and
/// are excluded; their I/O already shows on the drives beneath them. A drive
/// reporting zero size (an empty card reader slot, an optical drive without
/// media) is excluded too.
pub fn physical_disks(sys: &Path) -> Vec<PhysicalDisk> {
    let block = sys.join("block");
    let Ok(entries) = std::fs::read_dir(&block) else {
        return Vec::new();
    };
    let mut disks: Vec<PhysicalDisk> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = block.join(&name);
            if !dir.join("device").exists() {
                return None;
            }
            let sectors = read_trim(dir.join("size"))?.parse::<u64>().ok()?;
            if sectors == 0 {
                return None;
            }
            Some(PhysicalDisk {
                model: read_trim(dir.join("device").join("model")).unwrap_or_default(),
                media: media_kind(&name, &dir),
                size_bytes: sectors.saturating_mul(512),
                name,
            })
        })
        .collect();
    disks.sort_by_key(|disk| natural_key(&disk.name));
    disks
}

/// Bus and rotation, from what sysfs says about the drive itself.
///
/// USB wins over the medium: a USB bridge commonly reports an SSD as
/// rotational, and Windows calls such a disk removable/USB as well.
fn media_kind(name: &str, dir: &Path) -> MediaKind {
    let device_path = std::fs::canonicalize(dir.join("device"))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    if device_path.contains("/usb") {
        return MediaKind::Usb;
    }
    if name.starts_with("sr") {
        return MediaKind::Optical;
    }
    if name.starts_with("nvme") {
        return MediaKind::SsdNvme;
    }
    match read_trim(dir.join("queue").join("rotational")).as_deref() {
        Some("1") => MediaKind::Hdd,
        Some("0") if device_path.contains("/ata") => MediaKind::SsdSata,
        Some("0") => MediaKind::Ssd,
        _ => MediaKind::Unknown,
    }
}

/// The physical drives a block device (by kernel name) is stored on.
///
/// A partition resolves to its disk, an md/dm device to the drives under its
/// `slaves` (recursively, so LUKS on LVM on RAID still lands on the drives),
/// and a whole drive to itself. Unknown or malformed devices resolve to
/// nothing — a volume is never attributed to a guessed drive.
pub fn backing_disks(sys: &Path, device: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    resolve(sys, device, 0, &mut out);
    out
}

fn resolve(sys: &Path, device: &str, depth: usize, out: &mut BTreeSet<String>) {
    if depth > MAX_STACK_DEPTH || device.is_empty() || device.contains('/') {
        return;
    }
    let class = sys.join("class").join("block").join(device);
    if class.join("partition").exists() {
        // `/sys/class/block/<part>` links into its disk's directory.
        if let Some(parent) = std::fs::canonicalize(&class)
            .ok()
            .and_then(|target| target.parent()?.file_name().map(|n| n.to_owned()))
        {
            resolve(sys, &parent.to_string_lossy(), depth + 1, out);
        }
        return;
    }
    let slaves: Vec<String> = std::fs::read_dir(class.join("slaves"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if !slaves.is_empty() {
        for slave in slaves {
            resolve(sys, &slave, depth + 1, out);
        }
        return;
    }
    if sys.join("block").join(device).join("device").exists() {
        out.insert(device.to_string());
    }
}

/// One mounted filesystem, as the collector saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Kernel block device name the filesystem sits on.
    pub device: String,
    pub mount: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Volumes grouped onto the drives they are stored on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskVolumes {
    /// Mount points, shortest first, without duplicates.
    pub mounts: Vec<String>,
    /// Formatted capacity attributed to this drive. A volume spread over
    /// several drives (RAID, LVM spanning) contributes an equal share to each,
    /// so the drives' totals add up to the volume once.
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Group `volumes` by the physical drives `backing` resolves them to.
///
/// One block device mounted several times (bind mounts, btrfs subvolumes) is
/// counted once for capacity, and lists each mount point.
pub fn group_volumes(
    volumes: &[Volume],
    backing: impl Fn(&str) -> BTreeSet<String>,
) -> BTreeMap<String, DiskVolumes> {
    let mut by_device: BTreeMap<&str, (Vec<&str>, u64, u64)> = BTreeMap::new();
    for volume in volumes {
        let slot = by_device.entry(volume.device.as_str()).or_insert((
            Vec::new(),
            volume.total_bytes,
            volume.free_bytes,
        ));
        slot.0.push(volume.mount.as_str());
    }
    let mut out: BTreeMap<String, DiskVolumes> = BTreeMap::new();
    for (device, (mounts, total, free)) in by_device {
        let disks = backing(device);
        if disks.is_empty() {
            continue;
        }
        let share = disks.len() as u64;
        for disk in disks {
            let entry = out.entry(disk).or_default();
            entry.total_bytes = entry.total_bytes.saturating_add(total / share);
            entry.free_bytes = entry.free_bytes.saturating_add(free / share);
            for mount in &mounts {
                if !entry.mounts.iter().any(|known| known == mount) {
                    entry.mounts.push((*mount).to_string());
                }
            }
        }
    }
    for entry in out.values_mut() {
        entry
            .mounts
            .sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    }
    out
}

/// Average time one completed I/O took in a window, in milliseconds: the
/// growth of the time-spent-reading and -writing counters over the growth of
/// completed reads and writes (iostat's `await`). `None` without a completed
/// I/O — an idle window has no response time, not a zero one.
pub fn average_response_ms(io_ms_delta: u64, ios_delta: u64) -> Option<f64> {
    (ios_delta > 0).then(|| io_ms_delta as f64 / ios_delta as f64)
}

/// `nvme10n1` sorts after `nvme2n1`: digit runs compare numerically.
fn natural_key(name: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut digits = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            if !digits.is_empty() {
                out.push((
                    std::mem::take(&mut text),
                    digits.parse().unwrap_or(u64::MAX),
                ));
                digits.clear();
            }
            text.push(c);
        }
    }
    out.push((text, digits.parse().unwrap_or(0)));
    out
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::PathBuf;

    /// A fake sysfs tree. `unix`-gated like the only test that builds one: the
    /// module itself is platform-neutral, so on a Windows test run this helper
    /// would otherwise be dead code (the gate builds `--all-targets`).
    #[cfg(unix)]
    struct Sysfs {
        root: PathBuf,
    }

    #[cfg(unix)]
    impl Sysfs {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("tm-linux-disks-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn mkdir(&self, relative: &str) {
            std::fs::create_dir_all(self.root.join(relative)).unwrap();
        }

        #[cfg(unix)]
        fn link(&self, relative: &str, target: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(self.root.join(target), path).unwrap();
        }

        /// A drive under a fake device tree, with its partitions.
        #[cfg(unix)]
        fn drive(&self, name: &str, bus: &str, rotational: &str, parts: &[&str]) {
            let dev = format!("devices/{bus}/{name}");
            self.write(&format!("{dev}/size"), "2000\n");
            self.write(&format!("{dev}/queue/rotational"), rotational);
            self.write(&format!("devices/{bus}/hw-{name}/model"), "Model X  \n");
            self.link(
                &format!("{dev}/device"),
                &format!("devices/{bus}/hw-{name}"),
            );
            self.link(&format!("block/{name}"), &dev);
            self.link(&format!("class/block/{name}"), &dev);
            for part in parts {
                self.write(&format!("{dev}/{part}/partition"), "1\n");
                self.link(&format!("class/block/{part}"), &format!("{dev}/{part}"));
            }
        }

        /// A virtual stacked device (md/dm) over `slaves`.
        fn stacked(&self, name: &str, slaves: &[&str]) {
            let dev = format!("devices/virtual/block/{name}");
            self.write(&format!("{dev}/size"), "4000\n");
            for slave in slaves {
                self.mkdir(&format!("{dev}/slaves/{slave}"));
            }
            #[cfg(unix)]
            {
                self.link(&format!("block/{name}"), &dev);
                self.link(&format!("class/block/{name}"), &dev);
            }
        }
    }

    #[cfg(unix)]
    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Regression: a mount-per-row list repeated the parent disk's counters for
    /// every partition, and RAID members never appeared at all. Partitions
    /// collapse onto their disk, an md array resolves to its member drives, and
    /// virtual devices are never listed as drives.
    #[cfg(unix)]
    #[test]
    fn partitions_and_arrays_resolve_to_their_physical_drives() {
        let sys = Sysfs::new("resolve");
        sys.drive("nvme0n1", "pci0/nvme", "0\n", &["nvme0n1p1", "nvme0n1p5"]);
        sys.drive("sda", "pci0/ata1", "0\n", &["sda1"]);
        sys.drive("sdb", "pci0/ata2", "1\n", &["sdb1"]);
        sys.drive("sdc", "pci0/usb2", "1\n", &["sdc2"]);
        sys.stacked("md127", &["sda1", "sdb1"]);
        sys.stacked("dm-0", &["md127"]);

        let names: Vec<_> = physical_disks(&sys.root)
            .into_iter()
            .map(|d| (d.name, d.media, d.model, d.size_bytes))
            .collect();
        assert_eq!(
            names,
            [
                (
                    "nvme0n1".into(),
                    MediaKind::SsdNvme,
                    "Model X".into(),
                    1_024_000
                ),
                (
                    "sda".into(),
                    MediaKind::SsdSata,
                    "Model X".into(),
                    1_024_000
                ),
                ("sdb".into(), MediaKind::Hdd, "Model X".into(), 1_024_000),
                ("sdc".into(), MediaKind::Usb, "Model X".into(), 1_024_000),
            ]
        );

        let set = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(backing_disks(&sys.root, "nvme0n1p5"), set(&["nvme0n1"]));
        assert_eq!(backing_disks(&sys.root, "nvme0n1"), set(&["nvme0n1"]));
        assert_eq!(backing_disks(&sys.root, "md127"), set(&["sda", "sdb"]));
        assert_eq!(backing_disks(&sys.root, "dm-0"), set(&["sda", "sdb"]));
        assert!(backing_disks(&sys.root, "loop0").is_empty());
        assert!(backing_disks(&sys.root, "../etc").is_empty());
    }

    #[test]
    fn volumes_group_per_drive_and_split_spanning_capacity() {
        let volumes = [
            Volume {
                device: "nvme0n1p5".into(),
                mount: "/".into(),
                total_bytes: 100,
                free_bytes: 40,
            },
            Volume {
                device: "nvme0n1p5".into(),
                mount: "/var/lib/bind".into(),
                total_bytes: 100,
                free_bytes: 40,
            },
            Volume {
                device: "nvme0n1p1".into(),
                mount: "/boot/efi".into(),
                total_bytes: 10,
                free_bytes: 5,
            },
            Volume {
                device: "md127".into(),
                mount: "/home/raid".into(),
                total_bytes: 200,
                free_bytes: 100,
            },
            Volume {
                device: "tmpfs".into(),
                mount: "/tmp".into(),
                total_bytes: 50,
                free_bytes: 50,
            },
        ];
        let grouped = group_volumes(&volumes, |device| match device {
            "nvme0n1p5" | "nvme0n1p1" => ["nvme0n1".to_string()].into(),
            "md127" => ["sda".to_string(), "sdb".to_string()].into(),
            _ => BTreeSet::new(),
        });
        let nvme = &grouped["nvme0n1"];
        assert_eq!(nvme.mounts, ["/", "/boot/efi", "/var/lib/bind"]);
        // The bind mount is the same filesystem: counted once.
        assert_eq!((nvme.total_bytes, nvme.free_bytes), (110, 45));
        assert_eq!(grouped["sda"].mounts, ["/home/raid"]);
        assert_eq!(
            (grouped["sda"].total_bytes, grouped["sdb"].free_bytes),
            (100, 50)
        );
        assert!(!grouped.contains_key("tmpfs"));
    }

    #[test]
    fn response_time_needs_a_completed_io() {
        assert_eq!(average_response_ms(30, 10), Some(3.0));
        assert_eq!(average_response_ms(5, 0), None);
    }

    #[test]
    fn drive_names_sort_naturally() {
        let mut names = vec!["nvme10n1", "sdb", "nvme2n1", "sda"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["nvme2n1", "nvme10n1", "sda", "sdb"]);
    }
}
