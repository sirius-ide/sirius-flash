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

    /// Longest volume label the filesystem records, **in the unit that
    /// filesystem counts**.
    ///
    /// This is not the same unit for all three, and the difference is not
    /// academic: exFAT stores the label as UTF-16, so `mkfs.exfat` rejects an
    /// 11-*character* label made of astral or accented characters with "input
    /// string is too long". FAT32 counts characters — `BS_VolLab` is eleven OEM
    /// code points and `mkfs.fat` measures with `mbstowcs` before converting.
    pub fn max_label_len(self) -> usize {
        match self {
            // Eleven OEM code points in BS_VolLab.
            FileSystem::Fat32 => 11,
            // Eleven UTF-16 code units in the volume-label directory entry.
            FileSystem::ExFat => 11,
            // Thirty-two UTF-16 code units.
            FileSystem::Ntfs => 32,
        }
    }

    /// Is the label length counted in UTF-16 code units rather than characters?
    fn counts_label_in_utf16(self) -> bool {
        matches!(self, FileSystem::Ntfs | FileSystem::ExFat)
    }

    /// Does the filesystem store labels folded to upper case?
    fn uppercases_label(self) -> bool {
        matches!(self, FileSystem::Fat32)
    }

    /// Must the label be folded to plain ASCII?
    ///
    /// FAT keeps the label as 11 bytes of an OEM code page, and nothing outside
    /// printable ASCII survives that round trip: `mkfs.fat` converts through
    /// CP850 and then rejects the result, because dosfstools tests
    /// `doslabel[i] < 0x20` on a *signed* char, so every byte at or above 0x80
    /// trips it. The error even names a character class the label does not
    /// contain — "characters below 0x20".
    ///
    /// This matters far more than a cosmetic rename. `mkfs.fat` runs *after*
    /// `parted`, so an accented label meant a wiped drive and then a failed
    /// format: invariant 1, exactly. Folding here keeps the whole decision on
    /// the safe side of that boundary. Rufus does the same thing for the same
    /// reason (`src/format.c:284-288`, `if (wLabel[i] >= 0x80) wLabel[k++] = '_'`).
    ///
    /// NTFS and exFAT store UTF-16 and take the label as given, so they are
    /// left alone — folding them would mangle a perfectly legal name.
    fn folds_label_to_ascii(self) -> bool {
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
    /// reads every sector and marks the unreadable ones bad, which finds a
    /// dying stick. It is a *read* scan, so it does not detect a fake-capacity
    /// counterfeit — those need a write-and-read-back pass.
    pub quick: bool,
    /// Will a UEFI:NTFS loader partition be written alongside this filesystem?
    ///
    /// This is the axis that decides whether NTFS under a UEFI target is legal.
    /// Firmware reads only FAT, so NTFS is unbootable *unaided* — and the
    /// loader partition is precisely the aid. Without this the model refuses
    /// the layout the flasher actually writes by default.
    pub uefi_ntfs_helper: bool,
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
    uefi_ntfs_helper: bool,
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
    /// Will a UEFI:NTFS loader partition accompany this filesystem?
    pub fn uefi_ntfs_helper(&self) -> bool {
        self.uefi_ntfs_helper
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

    /// Can *this build* actually produce bootable media for this plan?
    ///
    /// [`needs_boot_code`](Self::needs_boot_code) answers a narrower question,
    /// and a plan can clear it and still be unbuildable — MBR + UEFI needs no
    /// bootstrap yet is declined for want of a way to boot-test it. This is the
    /// single authority on the whole gap between what the model can describe
    /// and what we can make, so that the flasher and anything listing options
    /// cannot drift apart and quietly promise different things.
    ///
    /// `Ok(())` means we would produce media that boots.
    pub fn buildable(&self) -> Result<(), &'static str> {
        if self.needs_boot_code() {
            return Err(
                "BIOS media needs an MBR bootstrap and a partition boot record, which \
                 this build does not write",
            );
        }
        if self.filesystem != FileSystem::Fat32 && !self.uefi_ntfs_helper {
            return Err(
                "UEFI firmware only reads FAT, so this would need a filesystem driver \
                 loaded before boot",
            );
        }
        if self.scheme != PartitionScheme::Gpt {
            return Err(
                "MBR with a UEFI target is a valid layout but is not yet boot-verified here",
            );
        }
        Ok(())
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
            if !(MIN_VOLUME..256 * TB).contains(&size) {
                return 0;
            }
            0x0001_F000
        }
        // 512 B to 32 MiB, flat.
        FileSystem::ExFat => {
            if !(MIN_VOLUME..256 * TB).contains(&size) {
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

/// Smallest volume we will offer NTFS or exFAT on.
///
/// Below this their own structures crowd out the data: a 4 MiB exFAT volume
/// with a 1 MiB cluster is created happily by `mkfs.exfat` and then rejected by
/// `fsck.exfat`, which is worse than refusing it. FAT32 has its own, larger
/// floor of 32 MB built into its mask.
///
/// Nothing reachable by flashing comes near this — `assert_safe_target` already
/// confines real targets to 2–512 GiB — so this only keeps the exploratory
/// `format-options` output honest.
const MIN_VOLUME: u64 = 8 * MB;

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
        .map(|c| {
            if fs.folds_label_to_ascii() && !c.is_ascii() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if fs.uppercases_label() {
        out = out.to_uppercase();
    }
    out = out.trim().to_string();
    // Truncate in the unit the filesystem actually counts, always on a
    // character boundary so a multi-byte character is never cut in half.
    // FAT32 is already ASCII here, where the two units agree; exFAT and NTFS
    // store UTF-16, and an accented or astral character costs one or two units
    // there while costing one character.
    let limit = fs.max_label_len();
    let too_long = if fs.counts_label_in_utf16() {
        out.chars().map(char::len_utf16).sum::<usize>() > limit
    } else {
        out.chars().count() > limit
    };
    if too_long {
        let mut kept = String::new();
        let mut used = 0usize;
        for c in out.chars() {
            let cost = if fs.counts_label_in_utf16() {
                c.len_utf16()
            } else {
                1
            };
            if used + cost > limit {
                break;
            }
            used += cost;
            kept.push(c);
        }
        out = kept.trim_end().to_string();
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
        if self.target.needs_uefi()
            && !self.filesystem.uefi_bootable_unaided()
            && !self.uefi_ntfs_helper
        {
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
            uefi_ntfs_helper: self.uefi_ntfs_helper,
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
            uefi_ntfs_helper: false,
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
        // A multi-byte character is never cut in half — on a filesystem whose
        // label can hold one.
        let (l, _) = sanitise_label(FileSystem::Ntfs, "ÉÉÉÉÉÉÉÉÉÉÉÉÉÉ");
        assert_eq!(l.chars().count(), 14);
        assert_eq!(l, "ÉÉÉÉÉÉÉÉÉÉÉÉÉÉ");
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
                                    uefi_ntfs_helper: false,
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
            uefi_ntfs_helper: false,
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
                uefi_ntfs_helper: false,
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

#[cfg(test)]
mod label_tests {
    use super::*;

    /// `mkfs.fat` runs after `parted`, so a label it refuses meant a wiped
    /// drive and then a failed format. Every one of these was verified to be
    /// rejected by dosfstools 4.2 before this fold existed, and accepted after.
    #[test]
    fn fat_labels_are_folded_to_ascii_rather_than_failing_the_format() {
        for (input, expected) in [
            ("Café", "CAF_"),
            ("MÜNCHEN", "M_NCHEN"),
            ("ÅÄÖ", "___"),
            ("Björk's USB", "BJ_RK'S USB"),
            ("日本語", "___"),
            ("УСТАНОВКА", "_________"),
            ("🚀", "_"),
        ] {
            let (got, _) = sanitise_label(FileSystem::Fat32, input);
            assert_eq!(got, expected, "folding {input:?}");
            assert!(got.is_ascii(), "{got:?} must be plain ASCII for FAT");
            assert!(got.len() <= 11, "{got:?} must fit 11 bytes");
        }
    }

    /// …but NTFS and exFAT store UTF-16 and take these as given, so folding
    /// them would mangle a perfectly legal name for no reason.
    #[test]
    fn other_filesystems_keep_their_accents() {
        for fs in [FileSystem::Ntfs, FileSystem::ExFat] {
            let (got, changed) = sanitise_label(fs, "Café");
            assert_eq!(got, "Café", "{fs} stores UTF-16");
            assert!(!changed);
        }
    }

    /// A FAT label is 11 *bytes*. After folding it is ASCII, so the character
    /// count and the byte count agree and the existing truncation is correct —
    /// but assert it, because that equivalence is the whole reason the fold
    /// has to happen before the truncation.
    #[test]
    fn a_folded_fat_label_never_exceeds_eleven_bytes() {
        for input in [
            "ÉÉÉÉÉÉÉÉÉÉÉÉÉÉ",
            "Windows 安装 media",
            "a very long label indeed",
            "🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀",
        ] {
            let (got, _) = sanitise_label(FileSystem::Fat32, input);
            assert!(
                got.len() <= 11,
                "{input:?} folded to {got:?}, which is {} bytes",
                got.len()
            );
        }
    }

    /// Every printable ASCII character the sanitiser keeps was swept through
    /// `mkfs.fat`, and none is rejected — so the fold is the only thing that
    /// was needed.
    #[test]
    fn printable_ascii_survives_unchanged() {
        let keep: String = (0x20u8..0x7f)
            .map(char::from)
            .filter(|c| !"*?.,;:/\\|+=<>[]\"".contains(*c))
            .collect();
        let (got, _) = sanitise_label(FileSystem::Fat32, &keep);
        assert!(got.is_ascii());
        assert!(got.chars().all(|c| c != '_' || keep.contains('_')));
    }
}

#[cfg(test)]
mod floor_tests {
    use super::*;

    /// `mkfs.exfat` will build a 4 MiB volume with a 1 MiB cluster and
    /// `fsck.exfat` then rejects it, so the model must not offer that pairing.
    /// Measured, not assumed.
    #[test]
    fn filesystems_are_not_offered_below_their_usable_floor() {
        for fs in [FileSystem::Ntfs, FileSystem::ExFat] {
            assert!(
                cluster_sizes(fs, Volume::new(4 * MB, 512)).is_empty(),
                "{fs} on 4 MiB formats but does not fsck clean"
            );
            assert!(
                !cluster_sizes(fs, Volume::new(8 * MB, 512)).is_empty(),
                "{fs} is fine from 8 MiB up"
            );
        }
        // Nothing a real target could hit: the safety gate stops well above.
        for fs in [FileSystem::Ntfs, FileSystem::ExFat, FileSystem::Fat32] {
            assert!(!cluster_sizes(fs, Volume::new(2 * GB, 512)).is_empty());
        }
    }
}

#[cfg(test)]
mod label_unit_tests {
    use super::*;

    /// exFAT and NTFS store the label as UTF-16 and count code units, so an
    /// eleven-*character* accented label is twenty-two units and `mkfs.exfat`
    /// refuses it with "input string is too long". Measured against
    /// exfatprogs, not inferred.
    #[test]
    fn utf16_filesystems_are_truncated_in_code_units() {
        let eleven_accents = "ÉÉÉÉÉÉÉÉÉÉÉ"; // 11 chars, 11 UTF-16 units
        let (got, _) = sanitise_label(FileSystem::ExFat, eleven_accents);
        assert_eq!(
            got.chars().map(char::len_utf16).sum::<usize>(),
            11,
            "{got:?} must fit eleven UTF-16 units"
        );

        // Astral characters cost two units each, so only five fit.
        let rockets = "🚀🚀🚀🚀🚀🚀🚀🚀";
        let (got, _) = sanitise_label(FileSystem::ExFat, rockets);
        let units: usize = got.chars().map(char::len_utf16).sum();
        assert!(units <= 11, "{got:?} is {units} UTF-16 units");
        assert_eq!(got.chars().count(), 5, "five surrogate pairs fit, not more");

        let (got, _) = sanitise_label(FileSystem::Ntfs, &"🚀".repeat(20));
        let units: usize = got.chars().map(char::len_utf16).sum();
        assert!(units <= 32, "{got:?} is {units} UTF-16 units");
    }

    /// FAT32 counts characters, not units — and after the ASCII fold the two
    /// agree anyway, so nothing over-truncates.
    #[test]
    fn fat32_still_counts_characters() {
        let (got, _) = sanitise_label(FileSystem::Fat32, "SÉCURITÉ");
        assert_eq!(got, "S_CURIT_", "all eight fit; none is dropped");
        assert_eq!(got.chars().count(), 8);
    }
}

/// How Windows installation media is laid out.
///
/// The two differ only in how they cope with `install.wim` exceeding FAT32's
/// 4 GiB file limit, and that difference decides which firmware will boot the
/// result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsLayout {
    /// One FAT32 partition, with the install image split into `.swm` chunks if
    /// it is too large for the filesystem.
    ///
    /// Asks the firmware to verify exactly one image: Microsoft's own
    /// bootloader, off the ISO. Every Certified-for-Windows machine trusts that
    /// by definition, so this boots wherever the ISO itself would. The cost is
    /// time — splitting re-compresses the image — and that Windows Setup sees
    /// a split image rather than the original.
    Fat32Split,
    /// An NTFS partition holding the image untouched, plus a small FAT
    /// partition carrying the UEFI:NTFS bootloader and an NTFS driver.
    ///
    /// Faster, and `install.wim` arrives byte-identical. But the firmware must
    /// now also trust the **third-party** `Microsoft Corporation UEFI CA 2011`,
    /// which is optional: OEM guidance says "should consider", the mandatory
    /// `db` for Windows 11 25H2+ omits it, and Secured-core PCs must distrust
    /// it. Where it is missing the firmware rejects the bootloader before it
    /// can print anything, and the user sees only "No bootable option or device
    /// was found" — indistinguishable from a badly written stick.
    NtfsUefiNtfs,
}

impl WindowsLayout {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowsLayout::Fat32Split => "fat32-split",
            WindowsLayout::NtfsUefiNtfs => "ntfs-uefi-ntfs",
        }
    }

    /// Which layout to use when the user has not chosen.
    ///
    /// Matches Rufus's trigger, which is the size of the largest file rather
    /// than anything about the image: below FAT32's limit it uses FAT32 and
    /// nothing exotic is needed; above it, NTFS. Rufus reaches the same answer
    /// by removing FAT32 from its dropdown entirely once an image has a file
    /// over 4 GiB (`SetAllowedFileSystems`, `rufus.c:190-207`), which leaves
    /// NTFS as the only option and pulls in UEFI:NTFS via `format.c:1482`.
    ///
    /// We follow it because it is the right default for almost everyone — but
    /// unlike Rufus we say what it costs, because Rufus says nothing at all.
    /// Its one warning about the third-party CA, `MSG_129`, is dead code:
    /// retired in 3.17 when the bootloader became signed, and never replaced.
    pub fn default_for(largest_file: u64) -> WindowsLayout {
        match FileSystem::Fat32.max_file_size() {
            Some(cap) if largest_file > cap => WindowsLayout::NtfsUefiNtfs,
            _ => WindowsLayout::Fat32Split,
        }
    }

    /// What the user should be told before this layout is written, if anything.
    ///
    /// Deliberately in the register Rufus's own documentation still uses
    /// (`res/uefi/readme.txt`: "you may however have to enable 3rd party
    /// certificates in your Secure Boot settings, as you would to boot Linux")
    /// rather than the alarmist retired `MSG_129` — the blunt version was
    /// removed upstream because it had become untrue.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            WindowsLayout::Fat32Split => None,
            WindowsLayout::NtfsUefiNtfs => Some(
                "This layout boots NTFS through the UEFI:NTFS loader, which is Secure Boot \
                 signed but by Microsoft's *third-party* CA. On most machines it just works. \
                 On some — Secured-core PCs, many corporate and Surface models — that \
                 certificate is not trusted, and the machine will report only \"No bootable \
                 option or device was found\", with no error from the media itself. You may \
                 have to enable third-party certificates in your firmware's Secure Boot \
                 settings, as you would to boot Linux. If you cannot change firmware \
                 settings on the target machine, use the fat32-split layout instead.",
            ),
        }
    }

    /// The filesystem this layout puts on the data partition.
    pub fn filesystem(self) -> FileSystem {
        match self {
            WindowsLayout::Fat32Split => FileSystem::Fat32,
            WindowsLayout::NtfsUefiNtfs => FileSystem::Ntfs,
        }
    }

    /// Does this layout need the vendored UEFI:NTFS image written to a second
    /// partition?
    pub fn needs_uefi_ntfs_partition(self) -> bool {
        matches!(self, WindowsLayout::NtfsUefiNtfs)
    }

    /// Does this layout need `wimlib-imagex` for an oversized install image?
    pub fn needs_splitter(self, largest_file: u64) -> bool {
        self == WindowsLayout::Fat32Split
            && FileSystem::Fat32
                .max_file_size()
                .is_some_and(|cap| largest_file > cap)
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// The trigger is Rufus's: the size of the largest file, not anything about
    /// the image. Below FAT32's limit nothing exotic is needed.
    #[test]
    fn the_default_layout_follows_rufus() {
        let cap = FileSystem::Fat32.max_file_size().unwrap();
        assert_eq!(
            WindowsLayout::default_for(cap),
            WindowsLayout::Fat32Split,
            "a file exactly at the limit still fits"
        );
        assert_eq!(
            WindowsLayout::default_for(cap + 1),
            WindowsLayout::NtfsUefiNtfs,
            "one byte over and FAT32 cannot hold it"
        );
        // The ordinary Windows 11 case: install.wim around 5 GiB.
        assert_eq!(
            WindowsLayout::default_for(5 * GB),
            WindowsLayout::NtfsUefiNtfs
        );
        // An older image with a small install.esd.
        assert_eq!(
            WindowsLayout::default_for(3 * GB),
            WindowsLayout::Fat32Split
        );
    }

    /// The whole point of diverging from Rufus: it writes this layout and says
    /// nothing. We match the default and add the sentence it is missing.
    #[test]
    fn the_layout_with_a_trust_dependency_carries_a_caveat() {
        let caveat = WindowsLayout::NtfsUefiNtfs
            .caveat()
            .expect("the third-party CA requirement must be stated");
        assert!(caveat.contains("third-party"), "name the actual dependency");
        assert!(
            caveat.contains("No bootable option or device was found"),
            "quote what the user would actually see, since the media cannot report it"
        );
        assert!(
            caveat.contains("fat32-split"),
            "name the way out, not just the problem"
        );
        assert!(
            !caveat.contains("MUST DISABLE"),
            "not the retired alarmist register; the bootloader IS signed"
        );
        assert_eq!(
            WindowsLayout::Fat32Split.caveat(),
            None,
            "this one asks the firmware to trust only the ISO's own bootloader"
        );
    }

    /// NTFS under a UEFI target is unbootable *unaided* — and the loader
    /// partition is precisely the aid. Without this axis the model refuses the
    /// layout the flasher writes by default, which is how the plan ended up
    /// claiming FAT32 while the drive was being made NTFS.
    #[test]
    fn ntfs_under_uefi_is_legal_exactly_when_the_loader_comes_with_it() {
        let v = Volume::new(32 * GB, 512);
        let mut r = FormatRequest {
            scheme: PartitionScheme::Gpt,
            target: TargetSystem::Uefi,
            filesystem: FileSystem::Ntfs,
            cluster_size: None,
            label: "SIRIUS".into(),
            quick: true,
            uefi_ntfs_helper: false,
        };
        let err = r.validate(v).unwrap_err();
        assert!(err.to_string().contains("UEFI specification"), "got: {err}");

        r.uefi_ntfs_helper = true;
        let plan = r
            .validate(v)
            .expect("the loader partition is the driver the message asks for");
        assert!(plan.uefi_ntfs_helper());
        plan.buildable()
            .expect("and it is a layout we can actually write");

        // FAT32 needs no such help, and must not start requiring it.
        let fat = FormatRequest {
            filesystem: FileSystem::Fat32,
            uefi_ntfs_helper: false,
            ..r.clone()
        };
        fat.validate(v)
            .expect("FAT32 is what firmware reads natively");
    }

    /// The layout is the authority on the filesystem; they cannot disagree.
    #[test]
    fn each_layout_names_its_own_filesystem() {
        assert_eq!(WindowsLayout::Fat32Split.filesystem(), FileSystem::Fat32);
        assert_eq!(WindowsLayout::NtfsUefiNtfs.filesystem(), FileSystem::Ntfs);
        for l in [WindowsLayout::Fat32Split, WindowsLayout::NtfsUefiNtfs] {
            assert_eq!(
                l.needs_uefi_ntfs_partition(),
                l.filesystem() != FileSystem::Fat32,
                "only the non-FAT layout needs the loader"
            );
        }
    }

    #[test]
    fn each_layout_declares_what_it_needs() {
        let big = 5 * GB;
        let small = 3 * GB;
        assert!(WindowsLayout::NtfsUefiNtfs.needs_uefi_ntfs_partition());
        assert!(!WindowsLayout::Fat32Split.needs_uefi_ntfs_partition());
        // The splitter is only needed when something actually needs splitting.
        assert!(WindowsLayout::Fat32Split.needs_splitter(big));
        assert!(!WindowsLayout::Fat32Split.needs_splitter(small));
        assert!(
            !WindowsLayout::NtfsUefiNtfs.needs_splitter(big),
            "NTFS holds it whole; that is the point"
        );
    }
}
