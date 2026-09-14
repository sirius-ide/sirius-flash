//! Format options — partition scheme, target firmware, filesystem, cluster
//! size, volume label.
//!
//! This is Rufus's main panel, and most of the pairs it offers are invalid.
//! Rufus greys them out; the rules live here instead, so that the core refuses
//! an impossible combination rather than trusting a UI to have done it. A GUI
//! that wants to grey out a dropdown asks [`target_systems_for`],
//! [`filesystems_for`] and [`cluster_sizes`] rather than reimplementing any of
//! this in TypeScript.
//!
//! Requests are separated from plans on purpose. A [`FormatRequest`] is whatever
//! the user typed and may be nonsense; only [`FormatRequest::validate`] turns
//! one into a [`FormatPlan`], and the flasher takes a plan. Forgetting to
//! validate is therefore a compile error rather than a wiped drive.
//!
//! The numbers come from Rufus (`src/rufus.c:450-612`, GPL-3.0, read at
//! 18ae93bf) reimplemented rather than approximated, and were checked against a
//! transcription of that function across the whole size range.

use anyhow::{bail, Result};
use std::fmt;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;
const TB: u64 = 1024 * GB;

/// How the drive's partition table is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionScheme {
    Mbr,
    Gpt,
}

/// The firmware the resulting media is meant to boot on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSystem {
    /// Legacy BIOS, or UEFI running its Compatibility Support Module.
    Bios,
    /// UEFI proper, no CSM.
    Uefi,
    /// Media that boots either way.
    BiosOrUefi,
}

/// The filesystem to create on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystem {
    Fat32,
    Ntfs,
    ExFat,
}

impl PartitionScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            PartitionScheme::Mbr => "mbr",
            PartitionScheme::Gpt => "gpt",
        }
    }
}

impl TargetSystem {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetSystem::Bios => "bios",
            TargetSystem::Uefi => "uefi",
            TargetSystem::BiosOrUefi => "bios-or-uefi",
        }
    }

    /// Does this target require the firmware to boot the media itself?
    fn needs_uefi(self) -> bool {
        matches!(self, TargetSystem::Uefi | TargetSystem::BiosOrUefi)
    }
}

impl FileSystem {
    pub fn as_str(self) -> &'static str {
        match self {
            FileSystem::Fat32 => "fat32",
            FileSystem::Ntfs => "ntfs",
            FileSystem::ExFat => "exfat",
        }
    }

    /// Largest single file the filesystem can hold.
    ///
    /// FAT32's ceiling is a 32-bit `DIR_FileSize` field, not a formatter
    /// limitation, which is why `install.wim` has to be split for it.
    pub fn max_file_size(self) -> Option<u64> {
        match self {
            FileSystem::Fat32 => Some(4 * GB - 1),
            FileSystem::Ntfs | FileSystem::ExFat => None,
        }
    }

    /// Can firmware boot this filesystem unaided?
    ///
    /// The UEFI specification obliges firmware to implement FAT and nothing
    /// else — §13.3.1.1, "The EFI firmware must support the FAT32, FAT16, and
    /// FAT12 variants". NTFS and exFAT do not appear in the specification at
    /// all. Booting NTFS needs a driver loaded first, which is what Rufus's
    /// UEFI:NTFS second partition is for, and that carries its own Secure Boot
    /// consequences (see PROJECT-STATE §6).
    pub fn uefi_bootable_unaided(self) -> bool {
        matches!(self, FileSystem::Fat32)
    }

    /// Longest volume label the filesystem records.
    pub fn max_label_len(self) -> usize {
        match self {
            // 11 bytes in the boot sector's BS_VolLab / a directory entry.
            FileSystem::Fat32 | FileSystem::ExFat => 11,
            FileSystem::Ntfs => 32,
        }
    }

    /// Does the filesystem store labels folded to upper case?
    fn uppercases_label(self) -> bool {
        matches!(self, FileSystem::Fat32)
    }
}

impl fmt::Display for FileSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The drive being formatted, as far as these rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    pub size_bytes: u64,
    /// Almost always 512; 4096 on 4Kn media, where it removes cluster sizes.
    pub sector_size: u32,
}

impl Volume {
    pub fn new(size_bytes: u64, sector_size: u32) -> Volume {
        Volume {
            size_bytes,
            sector_size,
        }
    }
}

/// Something the user should be told before the drive is touched, but which is
/// not a reason to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// MBR addresses at most 2 TiB. Rufus asks and proceeds; so do we, because
    /// refusing would leave a BIOS-only image no legal scheme at all on a large
    /// drive — GPT is not bootable by a legacy BIOS either.
    MbrCapsAtTwoTib { unusable_bytes: u64 },
    /// FAT32 past 32 GB is refused by Microsoft's own formatter but is a
    /// perfectly legal volume, and `mkfs.fat` makes it without complaint.
    LargeFat32 { size_bytes: u64 },
    /// The label was too long, or held characters the filesystem cannot store.
    LabelAdjusted { from: String, to: String },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::MbrCapsAtTwoTib { unusable_bytes } => write!(
                f,
                "MBR addresses at most 2 TiB, so {:.1} GiB of this drive will be unusable",
                *unusable_bytes as f64 / GB as f64
            ),
            Warning::LargeFat32 { size_bytes } => write!(
                f,
                "a {:.0} GB FAT32 volume is past what Windows' own formatter will create, \
                 though it is a valid filesystem and Windows reads it fine",
                *size_bytes as f64 / GB as f64
            ),
            Warning::LabelAdjusted { from, to } => {
                write!(f, "volume label {from:?} was adjusted to {to:?}")
            }
        }
    }
}

/// What the user asked for. May be impossible; [`FormatRequest::validate`] is
/// the only way to find out, and the only way to get something the flasher will
/// accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatRequest {
    pub scheme: PartitionScheme,
    pub target: TargetSystem,
    pub filesystem: FileSystem,
    /// `None` asks for the filesystem's default for this volume.
    pub cluster_size: Option<u32>,
    pub label: String,
    /// A quick format writes only the filesystem structures. A full one also
    /// reads every sector, which is how a counterfeit or dying stick is caught.
    pub quick: bool,
}

/// A [`FormatRequest`] that has been checked against a specific drive.
///
/// Only [`FormatRequest::validate`] constructs one, so a function taking a plan
/// cannot be handed an impossible combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatPlan {
    scheme: PartitionScheme,
    target: TargetSystem,
    filesystem: FileSystem,
    /// Always concrete: the default has been resolved against the volume.
    cluster_size: u32,
    label: String,
    quick: bool,
    volume: Volume,
    warnings: Vec<Warning>,
}

impl FormatPlan {
    pub fn scheme(&self) -> PartitionScheme {
        self.scheme
    }
    pub fn target(&self) -> TargetSystem {
        self.target
    }
    pub fn filesystem(&self) -> FileSystem {
        self.filesystem
    }
    pub fn cluster_size(&self) -> u32 {
        self.cluster_size
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn quick(&self) -> bool {
        self.quick
    }
    pub fn volume(&self) -> Volume {
        self.volume
    }
    /// Things the user must be shown before anything destructive runs.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Does making this plan *bootable* require boot code we do not write?
    ///
    /// This is the line between what the options model can describe and what
    /// this build can actually produce, and it is not the same line.
    ///
    /// A UEFI target needs no boot code at all: the firmware reads FAT itself
    /// and loads `\EFI\BOOT\BOOTX64.EFI` off the volume, which is why the
    /// existing GPT+FAT32 Windows path works without writing a single byte of
    /// bootstrap. A BIOS target is the opposite — the firmware only executes
    /// sector 0, so the media needs an MBR bootstrap *and* a partition boot
    /// record for the filesystem, which is what Rufus carries `src/ms-sys/`
    /// for (`write_win7_mbr`, `write_fat_32_br`, `write_ntfs_br`).
    ///
    /// We have none of that yet. Formatting for BIOS would succeed and produce
    /// a drive that silently does not boot, so the flasher refuses instead.
    pub fn needs_boot_code(&self) -> bool {
        matches!(self.target, TargetSystem::Bios | TargetSystem::BiosOrUefi)
    }
}

/// Which target systems make sense for a partition scheme.
///
/// One pair is genuinely impossible rather than merely discouraged: **GPT can
/// never boot a legacy BIOS**. A BIOS boots by executing the 512 bytes at LBA 0
/// and chain-loading whichever MBR entry is flagged active; a GPT disk's LBA 0
/// is a *protective* MBR holding one `0xEE` entry that is never active and
/// describes no filesystem. There is nothing for the BIOS to chain to. Hybrid
/// MBR/GPT tricks exist, are out of spec, and break on real firmware.
pub fn target_systems_for(scheme: PartitionScheme) -> &'static [TargetSystem] {
    match scheme {
        PartitionScheme::Mbr => &[
            TargetSystem::Bios,
            TargetSystem::Uefi,
            TargetSystem::BiosOrUefi,
        ],
        PartitionScheme::Gpt => &[TargetSystem::Uefi],
    }
}

/// Which filesystems can be used for a scheme and target on this volume.
///
/// The constraint that does the work is firmware support: a target that must
/// boot without our help can only use a filesystem the firmware can read, and
/// that means FAT32.
pub fn filesystems_for(
    scheme: PartitionScheme,
    target: TargetSystem,
    volume: Volume,
) -> Vec<FileSystem> {
    if !target_systems_for(scheme).contains(&target) {
        return Vec::new();
    }
    [FileSystem::Fat32, FileSystem::Ntfs, FileSystem::ExFat]
        .into_iter()
        .filter(|fs| {
            if target.needs_uefi() && !fs.uefi_bootable_unaided() {
                return false;
            }
            !cluster_sizes(*fs, volume).is_empty()
        })
        .collect()
}

/// Rufus keeps the legal cluster sizes for a filesystem as a bitmask, bit *n*
/// meaning a cluster of 2^n bytes. Only FAT32's mask depends on the volume
/// size; NTFS and exFAT are constants whose *default* moves instead.
fn cluster_mask(fs: FileSystem, volume: Volume) -> u32 {
    let size = volume.size_bytes;
    let mut allowed = match fs {
        FileSystem::Fat32 => {
            // Under 32 MB there is no room for the structures, and past 2 TiB
            // the 32-bit sector count in BPB_TotSec32 runs out.
            if !(32 * MB..2 * TB).contains(&size) {
                return 0;
            }
            let mut allowed: u32 = 0x0000_01F8;
            let mut i: u64 = 32;
            while i <= 32 * 1024 {
                if (size as f64) < (i * MB) as f64 * FAT32_CLUSTER_THRESHOLD {
                    break;
                }
                allowed <<= 1;
                i <<= 1;
            }
            allowed &= 0x0001_FE00;
            if size >= 32 * GB {
                allowed &= 0x0001_C000;
            }
            allowed
        }
        // 4 KiB to 64 KiB, flat.
        FileSystem::Ntfs => {
            if size >= 256 * TB {
                return 0;
            }
            0x0001_F000
        }
        // 512 B to 32 MiB, flat.
        FileSystem::ExFat => {
            if size >= 256 * TB {
                return 0;
            }
            0x03FF_FE00
        }
    };
    // A cluster cannot be smaller than a sector.
    allowed &= !(volume.sector_size - 1);

    // …nor so large that the volume holds almost none of them. Rufus's masks
    // have no such floor, so they offer sizes the formatter then refuses — a
    // dropdown entry that fails at format time is worse than one that was never
    // there. Measured against `mkfs.exfat`, which accepts a cluster exactly
    // when four of them fit and refuses when fewer do: 8 MiB takes a 1 MiB
    // cluster but not 8 MiB, and 32 MiB takes 8 MiB but not 32 MiB.
    let ceiling = volume.size_bytes / 4;
    for bit in 0..32u32 {
        if u64::from(1u32 << bit) > ceiling {
            allowed &= (1u32 << bit) - 1;
            break;
        }
    }
    allowed
}

/// Cluster sizes change slightly above each power-of-two boundary rather than
/// exactly on it (`FAT32_CLUSTER_THRESHOLD`, `src/rufus.h:116`). Computed in
/// f64 rather than Rufus's f32, which only differs on sizes where f32 could not
/// represent the drive exactly.
const FAT32_CLUSTER_THRESHOLD: f64 = 1.011;

/// Every cluster size this filesystem allows on this volume, smallest first.
///
/// Empty means the filesystem cannot be used on this volume at all.
pub fn cluster_sizes(fs: FileSystem, volume: Volume) -> Vec<u32> {
    let mask = cluster_mask(fs, volume);
    (0..32)
        .filter(|bit| mask & (1u32 << bit) != 0)
        .map(|bit| 1u32 << bit)
        .collect()
}

/// The cluster size to use when the user expresses no preference.
pub fn default_cluster_size(fs: FileSystem, volume: Volume) -> Option<u32> {
    let allowed = cluster_mask(fs, volume);
    if allowed == 0 {
        return None;
    }
    let size = volume.size_bytes;
    let preferred: u32 = match fs {
        FileSystem::Fat32 => {
            let mut d: u32 = 0;
            let mut i: u64 = 32;
            while i <= 32 * 1024 {
                if (size as f64) < (i * MB) as f64 * FAT32_CLUSTER_THRESHOLD {
                    d = 8 * i as u32;
                    break;
                }
                i <<= 1;
            }
            // Between 256 MB and 32 GB the defaults do not follow that rule.
            if (256 * MB..32 * GB).contains(&size) {
                let mut i: u64 = 8;
                while i <= 32 {
                    if (size as f64) < (i * GB) as f64 * FAT32_CLUSTER_THRESHOLD {
                        d = (i as u32 / 2) * KB as u32;
                        break;
                    }
                    i <<= 1;
                }
            }
            if size >= 32 * GB {
                d = 32 * KB as u32;
            }
            d
        }
        FileSystem::Ntfs => {
            let mut d: u32 = 0;
            let mut i: u64 = 16;
            while i <= 256 {
                if size < i * TB {
                    d = (i as u32 / 4) * KB as u32;
                    break;
                }
                i <<= 1;
            }
            d
        }
        FileSystem::ExFat => {
            if size < 256 * MB {
                4 * KB as u32
            } else if size < 32 * GB {
                32 * KB as u32
            } else {
                128 * KB as u32
            }
        }
    };
    // If the sector-size mask took the preferred size away, fall to the
    // smallest one still allowed — the same recovery Rufus makes.
    if preferred != 0 && allowed & preferred != 0 {
        Some(preferred)
    } else {
        Some(allowed.isolate_lowest_one())
    }
}

/// Trim and fold a volume label into what the filesystem can actually store.
///
/// Returns the label and whether anything had to change, so the caller can say
/// so rather than silently renaming the user's drive.
pub fn sanitise_label(fs: FileSystem, label: &str) -> (String, bool) {
    // Characters FAT reserves in 8.3 names, plus control characters. NTFS is
    // far more permissive but these are worth removing everywhere: a label is
    // shown in file managers and typed into shells.
    const FORBIDDEN: &[char] = &[
        '*', '?', '.', ',', ';', ':', '/', '\\', '|', '+', '=', '<', '>', '[', ']', '"',
    ];
    let mut out: String = label
        .chars()
        .filter(|c| !c.is_control() && !FORBIDDEN.contains(c))
        .collect();
    if fs.uppercases_label() {
        out = out.to_uppercase();
    }
    out = out.trim().to_string();
    // Truncate by characters, not bytes, so a multi-byte character is never
    // cut in half.
    if out.chars().count() > fs.max_label_len() {
        out = out.chars().take(fs.max_label_len()).collect();
        out = out.trim_end().to_string();
    }
    let changed = out != label;
    (out, changed)
}

impl FormatRequest {
    /// Check this request against a specific drive.
    ///
    /// Errors are combinations that cannot work; warnings are ones that will,
    /// with a consequence the user should hear about first. Everything here
    /// runs before anything destructive, which is invariant 1.
    pub fn validate(&self, volume: Volume) -> Result<FormatPlan> {
        if volume.size_bytes == 0 {
            bail!("the target reports a size of zero");
        }
        if !volume.sector_size.is_power_of_two() || volume.sector_size < 512 {
            bail!(
                "a sector size of {} is not usable; it must be a power of two of at least 512",
                volume.sector_size
            );
        }

        // ---- scheme x target ----
        if !target_systems_for(self.scheme).contains(&self.target) {
            bail!(
                "{} cannot boot a {} target: a legacy BIOS boots by running the code at \
                 sector 0 and chain-loading the active MBR entry, and a GPT disk has only a \
                 protective MBR — no boot code and no active entry. Use MBR for BIOS, or \
                 target UEFI.",
                self.scheme.as_str().to_uppercase(),
                self.target.as_str()
            );
        }

        // ---- filesystem ----
        if self.target.needs_uefi() && !self.filesystem.uefi_bootable_unaided() {
            bail!(
                "{} cannot be booted by UEFI firmware on its own — the UEFI specification \
                 requires firmware to implement FAT and nothing else, so {} media needs an \
                 NTFS driver loaded first. Use FAT32, or target BIOS.",
                self.filesystem,
                self.filesystem
            );
        }
        let allowed = cluster_sizes(self.filesystem, volume);
        if allowed.is_empty() {
            bail!(
                "{} cannot be created on a {:.1} GiB volume",
                self.filesystem,
                volume.size_bytes as f64 / GB as f64
            );
        }

        // ---- cluster size ----
        let cluster_size = match self.cluster_size {
            None => default_cluster_size(self.filesystem, volume)
                .expect("a non-empty allowed set always has a default"),
            Some(requested) => {
                if !allowed.contains(&requested) {
                    bail!(
                        "a {} cluster is not valid for {} on this volume; allowed sizes are {}",
                        human_bytes(requested),
                        self.filesystem,
                        allowed
                            .iter()
                            .map(|c| human_bytes(*c))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                requested
            }
        };

        // ---- warnings ----
        let mut warnings = Vec::new();
        if self.scheme == PartitionScheme::Mbr && volume.size_bytes > 2 * TB {
            warnings.push(Warning::MbrCapsAtTwoTib {
                unusable_bytes: volume.size_bytes - 2 * TB,
            });
        }
        if self.filesystem == FileSystem::Fat32 && volume.size_bytes > 32 * GB {
            warnings.push(Warning::LargeFat32 {
                size_bytes: volume.size_bytes,
            });
        }
        let (label, changed) = sanitise_label(self.filesystem, &self.label);
        if changed {
            warnings.push(Warning::LabelAdjusted {
                from: self.label.clone(),
                to: label.clone(),
            });
        }

        Ok(FormatPlan {
            scheme: self.scheme,
            target: self.target,
            filesystem: self.filesystem,
            cluster_size,
            label,
            quick: self.quick,
            volume,
            warnings,
        })
    }
}

/// "4 KB", "32 MB" — for messages about cluster sizes.
fn human_bytes(n: u32) -> String {
    if n >= MB as u32 {
        format!("{} MB", n / MB as u32)
    } else if n >= KB as u32 {
        format!("{} KB", n / KB as u32)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb(gib: u64) -> Volume {
        Volume::new(gib * GB, 512)
    }

    fn req(scheme: PartitionScheme, target: TargetSystem, fs: FileSystem) -> FormatRequest {
        FormatRequest {
            scheme,
            target,
            filesystem: fs,
            cluster_size: None,
            label: "SIRIUS".into(),
            quick: true,
        }
    }

    /// The one combination that is physically impossible rather than merely
    /// discouraged. A BIOS boots by running the code at sector 0 and
    /// chain-loading the active MBR entry; a GPT disk's sector 0 is a
    /// protective MBR with no boot code and no active entry.
    #[test]
    fn gpt_can_never_target_a_legacy_bios() {
        for target in [TargetSystem::Bios, TargetSystem::BiosOrUefi] {
            let err = req(PartitionScheme::Gpt, target, FileSystem::Fat32)
                .validate(usb(32))
                .unwrap_err();
            assert!(err.to_string().contains("protective MBR"), "got: {err}");
        }
        assert_eq!(
            target_systems_for(PartitionScheme::Gpt),
            &[TargetSystem::Uefi]
        );
    }

    /// UEFI firmware is only obliged to implement FAT. Booting NTFS needs a
    /// driver loaded first, which is a separate feature with its own Secure
    /// Boot consequences — not something to let through quietly here.
    #[test]
    fn uefi_targets_refuse_a_filesystem_the_firmware_cannot_read() {
        for fs in [FileSystem::Ntfs, FileSystem::ExFat] {
            let err = req(PartitionScheme::Gpt, TargetSystem::Uefi, fs)
                .validate(usb(32))
                .unwrap_err();
            assert!(err.to_string().contains("UEFI specification"), "got: {err}");
        }
        // …and they are fine for a BIOS target, which does not ask the
        // firmware to read the filesystem at all.
        for fs in [FileSystem::Ntfs, FileSystem::ExFat] {
            req(PartitionScheme::Mbr, TargetSystem::Bios, fs)
                .validate(usb(32))
                .unwrap_or_else(|e| panic!("{fs} on BIOS should be fine: {e}"));
        }
    }

    /// Above 2 TiB, MBR warns — it does not become illegal. Rufus only changes
    /// the *default* there and asks for confirmation at format time.
    ///
    /// Getting this wrong is not cosmetic: GPT cannot boot a legacy BIOS, so
    /// refusing MBR above 2 TiB would leave a BIOS-only image with no legal
    /// partition scheme at all on a large drive.
    #[test]
    fn mbr_past_two_tib_warns_rather_than_refusing() {
        let plan = req(PartitionScheme::Mbr, TargetSystem::Bios, FileSystem::Ntfs)
            .validate(Volume::new(4 * TB, 512))
            .expect("MBR stays legal above 2 TiB");
        let warned = plan
            .warnings()
            .iter()
            .any(|w| matches!(w, Warning::MbrCapsAtTwoTib { .. }));
        assert!(warned, "the user must be told about the lost capacity");
        assert!(
            !target_systems_for(PartitionScheme::Mbr).is_empty(),
            "a BIOS-only image on a 4 TiB drive must still have somewhere to go"
        );
    }

    /// Cluster tables, checked against a transcription of Rufus's own
    /// computation. The 256 MB row is the interesting one: the default is
    /// 512 B, not the 4 KB the published Microsoft table implies, because
    /// Rufus resets a default that its own mask has just excluded.
    #[test]
    fn cluster_tables_match_rufus() {
        let cases: &[(u64, u32, &[u32], u32)] = &[
            // volume, sector, allowed, default
            (63 * MB, 512, &[512], 512),
            (127 * MB, 512, &[512, 1024], 1024),
            (256 * MB, 512, &[512, 1024, 2048], 512),
            (511 * MB, 512, &[512, 1024, 2048, 4096], 4096),
            (8 * GB, 512, &[2048, 4096, 8192, 16384, 32768, 65536], 4096),
            (15 * GB, 512, &[4096, 8192, 16384, 32768, 65536], 8192),
            (31 * GB, 512, &[8192, 16384, 32768, 65536], 16384),
            (32 * GB, 512, &[16384, 32768, 65536], 32768),
            (TB, 512, &[16384, 32768, 65536], 32768),
            // 4Kn media loses every cluster below the sector size.
            (GB, 4096, &[4096, 8192], 4096),
            (511 * MB, 4096, &[4096], 4096),
        ];
        for (size, sector, allowed, default) in cases {
            let v = Volume::new(*size, *sector);
            assert_eq!(
                cluster_sizes(FileSystem::Fat32, v),
                *allowed,
                "fat32 allowed at {size} bytes / {sector} B sectors"
            );
            assert_eq!(
                default_cluster_size(FileSystem::Fat32, v),
                Some(*default),
                "fat32 default at {size} bytes / {sector} B sectors"
            );
        }
        // FAT32 does not exist below 32 MB or at 2 TiB and beyond.
        assert!(cluster_sizes(FileSystem::Fat32, Volume::new(8 * MB, 512)).is_empty());
        assert!(cluster_sizes(FileSystem::Fat32, Volume::new(2 * TB, 512)).is_empty());
        // NTFS and exFAT masks are flat; only their defaults move.
        assert_eq!(
            default_cluster_size(FileSystem::ExFat, usb(16)),
            Some(32 * 1024)
        );
        assert_eq!(
            default_cluster_size(FileSystem::ExFat, usb(64)),
            Some(128 * 1024)
        );
        assert_eq!(default_cluster_size(FileSystem::Ntfs, usb(64)), Some(4096));
    }

    /// Rufus's masks have no floor, so they offer cluster sizes the formatter
    /// then refuses. Measured against `mkfs.exfat`, which takes a cluster
    /// exactly when four of them fit on the volume.
    #[test]
    fn a_cluster_too_large_for_the_volume_is_not_offered() {
        let tiny = Volume::new(8 * MB, 512);
        let offered = cluster_sizes(FileSystem::ExFat, tiny);
        assert!(
            offered.contains(&(1024 * 1024)),
            "mkfs.exfat makes a 1 MiB cluster on 8 MiB"
        );
        assert!(
            !offered.contains(&(8 * 1024 * 1024)),
            "mkfs.exfat refuses an 8 MiB cluster on 8 MiB"
        );
        let small = Volume::new(32 * MB, 512);
        let offered = cluster_sizes(FileSystem::ExFat, small);
        assert!(offered.contains(&(8 * 1024 * 1024)), "8 MiB fits on 32 MiB");
        assert!(!offered.contains(&(32 * 1024 * 1024)), "32 MiB does not");
    }

    #[test]
    fn an_unavailable_cluster_size_is_refused_and_the_message_lists_the_real_ones() {
        let mut r = req(PartitionScheme::Gpt, TargetSystem::Uefi, FileSystem::Fat32);
        r.cluster_size = Some(512);
        let err = r.validate(usb(32)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("512 B"), "got: {msg}");
        assert!(
            msg.contains("16 KB"),
            "the allowed sizes must be named: {msg}"
        );
    }

    #[test]
    fn labels_are_folded_to_what_the_filesystem_can_store() {
        // FAT32 uppercases and keeps 11 characters.
        let (l, changed) = sanitise_label(FileSystem::Fat32, "Sirius Flash Boot");
        assert_eq!(l, "SIRIUS FLAS");
        assert!(changed);
        // NTFS keeps case and allows 32.
        let (l, changed) = sanitise_label(FileSystem::Ntfs, "Sirius Flash");
        assert_eq!(l, "Sirius Flash");
        assert!(!changed);
        // Reserved characters go, on every filesystem.
        let (l, _) = sanitise_label(FileSystem::Ntfs, "a/b\\c:d*e?f");
        assert_eq!(l, "abcdef");
        // A multi-byte character is never cut in half.
        let (l, _) = sanitise_label(FileSystem::Fat32, "ÉÉÉÉÉÉÉÉÉÉÉÉÉÉ");
        assert_eq!(l.chars().count(), 11);
    }

    #[test]
    fn a_sanitised_label_is_reported_rather_than_silently_applied() {
        let mut r = req(PartitionScheme::Gpt, TargetSystem::Uefi, FileSystem::Fat32);
        r.label = "My Install Drive".into();
        let plan = r.validate(usb(32)).unwrap();
        // Truncated to eleven characters, then the trailing space trimmed —
        // a label ending in a space is a nuisance in every file manager.
        assert_eq!(plan.label(), "MY INSTALL");
        assert!(plan
            .warnings()
            .iter()
            .any(|w| matches!(w, Warning::LabelAdjusted { .. })));
    }

    /// The structural guarantee: everything the query functions tell a GUI it
    /// may offer must actually validate. If these ever diverge, the UI greys
    /// out the wrong things or offers a combination the core then refuses.
    #[test]
    fn every_advertised_combination_validates() {
        for size in [64 * MB, 512 * MB, 8 * GB, 32 * GB, 512 * GB, 4 * TB] {
            for sector in [512u32, 4096] {
                let v = Volume::new(size, sector);
                for scheme in [PartitionScheme::Mbr, PartitionScheme::Gpt] {
                    for target in target_systems_for(scheme) {
                        for fs in filesystems_for(scheme, *target, v) {
                            for cluster in cluster_sizes(fs, v) {
                                let r = FormatRequest {
                                    scheme,
                                    target: *target,
                                    filesystem: fs,
                                    cluster_size: Some(cluster),
                                    label: "SIRIUS".into(),
                                    quick: true,
                                };
                                r.validate(v).unwrap_or_else(|e| {
                                    panic!(
                                        "advertised {scheme:?}/{target:?}/{fs}/{cluster} at \
                                         {size} bytes but rejected it: {e}"
                                    )
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// …and the converse: a filesystem that is not advertised must not slip
    /// through validation by another route.
    #[test]
    fn nothing_unadvertised_validates() {
        let v = usb(32);
        for scheme in [PartitionScheme::Mbr, PartitionScheme::Gpt] {
            for target in [
                TargetSystem::Bios,
                TargetSystem::Uefi,
                TargetSystem::BiosOrUefi,
            ] {
                let advertised = filesystems_for(scheme, target, v);
                for fs in [FileSystem::Fat32, FileSystem::Ntfs, FileSystem::ExFat] {
                    let accepted = req(scheme, target, fs).validate(v).is_ok();
                    assert_eq!(
                        accepted,
                        advertised.contains(&fs),
                        "{scheme:?}/{target:?}/{fs}: advertised and accepted disagree"
                    );
                }
            }
        }
    }

    #[test]
    fn a_plan_carries_the_resolved_cluster_size_not_the_request() {
        let plan = req(PartitionScheme::Gpt, TargetSystem::Uefi, FileSystem::Fat32)
            .validate(usb(32))
            .unwrap();
        assert_eq!(plan.cluster_size(), 32 * 1024, "the default was resolved");
        assert_eq!(plan.filesystem(), FileSystem::Fat32);
        assert!(plan.quick());
    }

    #[test]
    fn a_nonsense_volume_is_refused() {
        let r = req(PartitionScheme::Gpt, TargetSystem::Uefi, FileSystem::Fat32);
        assert!(r.validate(Volume::new(0, 512)).is_err());
        assert!(r.validate(Volume::new(8 * GB, 500)).is_err());
        assert!(r.validate(Volume::new(8 * GB, 256)).is_err());
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    /// The options model deliberately describes more than this build can make.
    /// A UEFI target needs no boot code — the firmware reads FAT and loads the
    /// EFI binary itself — while a BIOS target needs an MBR bootstrap and a
    /// partition boot record that we do not write. Formatting for BIOS would
    /// succeed and hand the user a drive that silently does not boot, so the
    /// distinction has to be visible to the flasher.
    #[test]
    fn bios_targets_are_flagged_as_needing_boot_code_we_do_not_write() {
        let v = Volume::new(32 * GB, 512);
        let uefi = FormatRequest {
            scheme: PartitionScheme::Gpt,
            target: TargetSystem::Uefi,
            filesystem: FileSystem::Fat32,
            cluster_size: None,
            label: "SIRIUS".into(),
            quick: true,
        }
        .validate(v)
        .unwrap();
        assert!(
            !uefi.needs_boot_code(),
            "UEFI boots FAT unaided; this is the path that already ships"
        );

        for target in [TargetSystem::Bios, TargetSystem::BiosOrUefi] {
            let plan = FormatRequest {
                scheme: PartitionScheme::Mbr,
                target,
                filesystem: FileSystem::Fat32,
                cluster_size: None,
                label: "SIRIUS".into(),
                quick: true,
            }
            .validate(v)
            .unwrap();
            assert!(
                plan.needs_boot_code(),
                "{target:?} media needs bootstrap we cannot write yet"
            );
        }
    }
}
