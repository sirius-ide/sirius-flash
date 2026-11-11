//! BIOS boot code — our MBR and FAT32 volume boot record — and the installer
//! that puts them on a disk.
//!
//! A BIOS boots by running the first 440 bytes of the disk. Ours find the one
//! active partition and run *its* first sector, and that one finds BOOTMGR in
//! the FAT32 root directory and jumps to it. `boot/mbr.asm` and
//! `boot/fat32.asm` are those two programs; the binaries beside them are what
//! they assemble to (the boot-test selftest and CI both reassemble and
//! compare), embedded here so that no build needs an assembler.
//!
//! The installer is where boot code that works under qemu turns into a stick
//! that does not. `fat32.asm` has no room to check the volume it lands on, so
//! everything it assumes is checked here instead: 512-byte sectors, hidden
//! sectors equal to the partition's start, the first FAT live, 32-bit LBAs, a
//! cluster size it can count. `boottest.py vbr` enforced the same list first,
//! and its first run found the splice right and everything around it silently
//! wrong — it wrote before it checked, accepted any bytes, and left the backup
//! boot sector stale.
//!
//! So checking and writing are separate types. [`prepare`] only reads, and it
//! is the only way to get a [`Prepared`]; only a [`Prepared`] can write. Every
//! refusal therefore happens with the disk untouched, and writing without
//! checking is a compile error rather than a habit to keep.

use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// `boot/mbr.asm`, assembled: exactly 440 bytes, so the disk identifier at 440
/// and the partition table at 446 are never part of it. The type is the check —
/// a binary of any other length does not compile.
pub const MBR_CODE: &[u8; 440] = include_bytes!("../boot/mbr.bin");

/// `boot/fat32.asm`, assembled, with zeros where each volume's BPB goes.
pub const FAT32_VBR: &[u8; 512] = include_bytes!("../boot/fat32.bin");

/// The only sector size the boot code handles: `fat32.asm` turns a byte count
/// into sectors with a shift by 9, finds a cluster's FAT sector with a shift by
/// 7 (128 four-byte entries per sector), and steps its load segment 32
/// paragraphs per sector.
pub const SECTOR: u64 = 512;

/// The largest BOOTMGR the VBR loads whole. `fat32.asm` loads to linear
/// 0x20000 and stops at 0x9F000 whatever the directory entry claims, because
/// past there lie the EBDA, video memory and, once the segment wraps, the
/// interrupt table and the loader itself. Windows 11's is 473,364 bytes. A
/// larger one would arrive truncated and report itself corrupt, so the flasher
/// refuses it while the drive is still intact. Mirrors `LOADSEG` and the
/// `cmp bh, 0x9f` there; change all three together.
pub const BOOTMGR_MAX: u64 = 0x9F000 - 0x20000;

/// The volume's own description inside its boot sector: the BPB and extended
/// BPB, through the `"FAT32   "` type string. Spliced from the formatted volume
/// into our code; the jump, the OEM name and everything from 0x5A on are ours.
const BPB_START: usize = 0x0B;
const BPB_END: usize = 0x5A;

/// What makes a volume FAT32 rather than FAT16 is its cluster count, not its
/// BPB, and `mkfs.fat -F 32` builds one below the floor with nothing more than a
/// warning on stderr and exit status 0.
const FAT32_MIN_CLUSTERS: u64 = 65_525;

/// Cluster numbers at or above this end a FAT32 chain.
const FAT32_END_OF_CHAIN: u64 = 0x0FFF_FFF8;

// The layout the splice relies on, checked when the crate is compiled: a
// fat32.bin that does not keep it fails the build, not an install.
const _: () = {
    let v = FAT32_VBR;
    assert!(
        v[510] == 0x55 && v[511] == 0xAA,
        "fat32.bin must end in 0x55AA"
    );
    // A short jump over the BPB to code at or after 0x5A. A binary laid out
    // as plain code from 0x00 — an MBR, say — has none, and its bytes
    // 0x0B-0x59 are instructions the splice would replace with geometry.
    assert!(
        v[0] == 0xEB && v[2] == 0x90,
        "fat32.bin must start with EB xx 90"
    );
    assert!(
        v[1] < 0x80 && 2 + v[1] as usize >= BPB_END && 2 + (v[1] as usize) < 510,
        "fat32.bin must jump forward to code at or after 0x5A"
    );
    // Anything here is overwritten by the volume's BPB, so anything here was
    // a mistake that an install would quietly erase.
    let mut i = BPB_START;
    while i < BPB_END {
        assert!(v[i] == 0, "fat32.bin must keep 0x0B-0x59 zero for the BPB");
        i += 1;
    }
};

/// One MBR partition entry, as the boot code reads it.
#[derive(Debug, Clone, Copy)]
struct Entry {
    status: u8,
    kind: u8,
    start: u64,
    sectors: u64,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn entry(sector0: &[u8; 512], i: usize) -> Entry {
    let at = 446 + 16 * i;
    Entry {
        status: sector0[at],
        kind: sector0[at + 4],
        start: u64::from(le32(sector0, at + 8)),
        sectors: u64::from(le32(sector0, at + 12)),
    }
}

/// The FAT32 BIOS Parameter Block, as `fat32.asm` reads it.
struct Bpb {
    bytes_per_sec: u16,
    sec_per_clus: u8,
    rsvd: u16,
    num_fats: u8,
    root_ent: u16,
    tot16: u16,
    fat_sz16: u16,
    hidden: u32,
    tot32: u32,
    fat_sz32: u32,
    ext_flags: u16,
    root_clus: u32,
    fs_info: u16,
    bk_boot_sec: u16,
}

impl Bpb {
    fn parse(s: &[u8; 512]) -> Bpb {
        Bpb {
            bytes_per_sec: le16(s, 0x0B),
            sec_per_clus: s[0x0D],
            rsvd: le16(s, 0x0E),
            num_fats: s[0x10],
            root_ent: le16(s, 0x11),
            tot16: le16(s, 0x13),
            fat_sz16: le16(s, 0x16),
            hidden: le32(s, 0x1C),
            tot32: le32(s, 0x20),
            fat_sz32: le32(s, 0x24),
            ext_flags: le16(s, 0x28),
            root_clus: le32(s, 0x2C),
            fs_info: le16(s, 0x30),
            bk_boot_sec: le16(s, 0x32),
        }
    }

    /// The backup boot sector, if the volume names one. 0 and 0xFFFF both
    /// mean none.
    fn backup(&self) -> Option<u16> {
        (self.bk_boot_sec != 0 && self.bk_boot_sec != 0xFFFF).then_some(self.bk_boot_sec)
    }
}

fn read_sector<R: Read + Seek>(r: &mut R, lba: u64) -> Result<[u8; 512]> {
    let mut s = [0u8; 512];
    r.seek(SeekFrom::Start(lba * SECTOR))
        .and_then(|_| r.read_exact(&mut s))
        .with_context(|| format!("reading sector {lba}"))?;
    Ok(s)
}

/// Everything checked, nothing written yet: the three sectors an install will
/// put on this disk, and where.
///
/// Only [`prepare`] makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    partition_start: u64,
    /// Partition-relative: the boot sector, and its backup if the BPB names one.
    vbr_sectors: Vec<u64>,
    /// Our VBR with this volume's BPB spliced in.
    vbr: [u8; 512],
    /// Sector 0 as it will be: our code, then this disk's identifier and table.
    mbr: [u8; 512],
}

/// Check a disk for everything our boot code assumes, and work out what to
/// write. Reads only.
///
/// `disk` is the whole disk, read from LBA 0 — the view the BIOS has.
/// `disk_bytes` is its size and `sector_size` its logical sector size, both as
/// the device reports them.
pub fn prepare<R: Read + Seek>(
    disk: &mut R,
    disk_bytes: u64,
    sector_size: u32,
) -> Result<Prepared> {
    if u64::from(sector_size) != SECTOR {
        bail!(
            "this drive has {sector_size}-byte sectors; the BIOS boot code handles 512-byte \
             sectors only"
        );
    }
    let disk_sectors = disk_bytes / SECTOR;

    // ---- the partition table, as mbr.asm reads it ----
    let sector0 = read_sector(disk, 0)?;
    if sector0[510..512] != [0x55, 0xAA] {
        bail!("sector 0 has no 0x55AA signature, so there is no partition table to boot from");
    }
    let entries: Vec<Entry> = (0..4).map(|i| entry(&sector0, i)).collect();
    // mbr.asm refuses any status byte but 0x00 and 0x80: anything else means
    // this is not a partition table at all.
    if let Some((i, e)) = entries
        .iter()
        .enumerate()
        .find(|(_, e)| e.status != 0x00 && e.status != 0x80)
    {
        bail!(
            "partition entry {} has status 0x{:02X}; a partition table holds only 0x00 and 0x80",
            i + 1,
            e.status
        );
    }
    let active: Vec<&Entry> = entries.iter().filter(|e| e.status == 0x80).collect();
    let part = match active.as_slice() {
        [one] => **one,
        [] => bail!("no partition is marked active, so the MBR would have nothing to boot"),
        more => bail!(
            "{} partitions are marked active; the MBR refuses that rather than guess",
            more.len()
        ),
    };
    if part.kind != 0x0C {
        bail!(
            "the active partition has type 0x{:02X}; the BIOS boot code is for FAT32 (LBA), \
             type 0x0C",
            part.kind
        );
    }
    if part.start == 0 {
        // Reading "the partition's first sector" would read the MBR, verify
        // its 0x55AA, and run it again, forever.
        bail!("the active partition starts at LBA 0, which is the MBR itself");
    }
    if part.sectors == 0 || part.start + part.sectors > disk_sectors {
        bail!(
            "the active partition (LBA {}, {} sectors) does not lie inside the {disk_sectors}-sector \
             disk",
            part.start,
            part.sectors
        );
    }

    // ---- the volume, as fat32.asm reads it ----
    let old = read_sector(disk, part.start)?;
    if old[510..512] != [0x55, 0xAA] {
        bail!(
            "the active partition's first sector has no 0x55AA signature; it is not a \
             formatted volume"
        );
    }
    let b = Bpb::parse(&old);
    if u64::from(b.bytes_per_sec) != SECTOR {
        bail!(
            "the volume says it has {}-byte sectors; the boot code handles 512 only",
            b.bytes_per_sec
        );
    }
    // On FAT12/16 the BPB ends at 0x3E and 0x24 onwards is boot code, so every
    // FAT32 field read below would be a misreading of it.
    if b.fat_sz16 != 0 || b.root_ent != 0 || b.tot16 != 0 || &old[0x52..0x5A] != b"FAT32   " {
        bail!("the active partition is not FAT32");
    }
    if !b.sec_per_clus.is_power_of_two() {
        // Zero is the dangerous one: the loader counts sectors per cluster
        // down from it, so a zero would read 65535 sectors per cluster.
        bail!(
            "the volume claims {} sectors per cluster; it must be a power of two",
            b.sec_per_clus
        );
    }
    if b.num_fats == 0 || b.fat_sz32 == 0 || b.rsvd == 0 {
        bail!("the volume has no FAT, or no reserved sectors to hold its boot sector");
    }
    // mkfs.fat does NOT derive hidden sectors from --offset, and a formatter
    // that misreads the partition start does the same thing on a device:
    // every absolute sector the VBR computes would land that far off.
    if u64::from(b.hidden) != part.start {
        bail!(
            "the volume's hidden-sectors field says it starts at LBA {}, but the partition \
             table says {}; the boot code adds the field to every read, so it would read the \
             wrong sectors",
            b.hidden,
            part.start
        );
    }
    let total = u64::from(b.tot32);
    if total == 0 || total > part.sectors {
        bail!(
            "the volume claims {total} sectors but its partition has {}",
            part.sectors
        );
    }
    // The VBR reads with a 32-bit LBA.
    if part.start + total > 1 << 32 {
        bail!("the volume reaches past LBA 2^32, which the boot code cannot address");
    }
    let meta = u64::from(b.rsvd) + u64::from(b.num_fats) * u64::from(b.fat_sz32);
    let clusters = total.saturating_sub(meta) / u64::from(b.sec_per_clus);
    if clusters < FAT32_MIN_CLUSTERS {
        bail!("the volume has {clusters} clusters; FAT32 needs at least {FAT32_MIN_CLUSTERS}");
    }
    let root = u64::from(b.root_clus);
    if root < 2 || root >= clusters + 2 || root >= FAT32_END_OF_CHAIN {
        bail!(
            "the root directory's cluster {root} is not on the volume (clusters 2-{})",
            clusters + 1
        );
    }
    // The loader follows chains through the first FAT and nothing else.
    // Bit 7 set says only one FAT is live, and bits 0-3 say which.
    if b.ext_flags & 0x80 != 0 && b.ext_flags & 0x0F != 0 {
        bail!(
            "the volume keeps only FAT {} up to date, and the boot code reads FAT 0",
            b.ext_flags & 0x0F
        );
    }

    // ---- the backup boot sector ----
    // It gets the same record. A stale backup makes every fsck.fat on the
    // stick report differences, and a repair that restores from it quietly
    // puts back the code this replaced.
    let mut vbr_sectors = vec![0];
    if let Some(bk) = b.backup() {
        if bk >= b.rsvd || bk == b.fs_info {
            bail!(
                "the BPB puts the backup boot sector at {bk}, which is not a free reserved sector"
            );
        }
        let backup = read_sector(disk, part.start + u64::from(bk))?;
        if backup[BPB_START..BPB_END] != old[BPB_START..BPB_END] {
            bail!(
                "the backup boot sector disagrees with the primary about the volume; it is \
                 damaged, or the volume is mounted and has marked itself dirty"
            );
        }
        vbr_sectors.push(u64::from(bk));
    }

    let mut vbr = *FAT32_VBR;
    vbr[BPB_START..BPB_END].copy_from_slice(&old[BPB_START..BPB_END]);
    let mut mbr = sector0;
    mbr[..MBR_CODE.len()].copy_from_slice(MBR_CODE);
    Ok(Prepared {
        partition_start: part.start,
        vbr_sectors,
        vbr,
        mbr,
    })
}

impl Prepared {
    /// The LBA the active partition, and its volume, start at.
    pub fn partition_start(&self) -> u64 {
        self.partition_start
    }

    /// Partition-relative sectors that receive the VBR: 0, and the backup.
    pub fn vbr_sectors(&self) -> &[u64] {
        &self.vbr_sectors
    }

    /// Write the VBR to the boot sector and its backup.
    ///
    /// `part` must address the partition with its first byte at offset `base`:
    /// 0 for the partition's own device, `partition_start() * 512` for a whole
    /// disk or an image file. A real drive gets the former — see `lib.rs` for
    /// why that matters once the volume is mounted.
    pub fn write_vbr<W: Write + Seek>(&self, part: &mut W, base: u64) -> Result<()> {
        for &rel in &self.vbr_sectors {
            part.seek(SeekFrom::Start(base + rel * SECTOR))
                .and_then(|_| part.write_all(&self.vbr))
                .with_context(|| format!("writing the volume boot record to sector {rel}"))?;
        }
        part.flush().context("flushing the volume boot record")
    }

    /// Write our code into the first 440 bytes of the disk, and nothing else:
    /// the disk identifier and the partition table stay as they are.
    pub fn write_mbr<W: Write + Seek>(&self, disk: &mut W) -> Result<()> {
        disk.seek(SeekFrom::Start(0))
            .and_then(|_| disk.write_all(&self.mbr[..MBR_CODE.len()]))
            .and_then(|_| disk.flush())
            .context("writing the MBR boot code")
    }

    /// Read every sector an install writes back through the whole disk, and
    /// confirm each holds exactly what was meant — the table included, since
    /// a boot sector that survives beside a changed table proves nothing.
    ///
    /// On a real drive, drop the page cache first, or this checks RAM.
    pub fn verify<R: Read + Seek>(&self, disk: &mut R) -> Result<()> {
        self.verify_mbr(disk)?;
        self.verify_vbr(disk)
    }

    /// Sector 0 only: the code and the table.
    pub fn verify_mbr<R: Read + Seek>(&self, disk: &mut R) -> Result<()> {
        if read_sector(disk, 0)? != self.mbr {
            bail!(
                "sector 0 does not read back as written: the MBR boot code or the partition \
                 table is not what was installed"
            );
        }
        Ok(())
    }

    /// The volume boot record and its backup only. The flasher reads these
    /// back before it writes the MBR; see `lib.rs`.
    pub fn verify_vbr<R: Read + Seek>(&self, disk: &mut R) -> Result<()> {
        for &rel in &self.vbr_sectors {
            let lba = self.partition_start + rel;
            if read_sector(disk, lba)? != self.vbr {
                bail!("the volume boot record at LBA {lba} does not read back as written");
            }
        }
        Ok(())
    }
}

/// Install both on a disk image: the whole disk in one regular file.
///
/// The tests and the boot-test harness use this; the flasher takes the same
/// steps against a real drive in `lib.rs`. Anything that is not a regular file
/// is refused, so this cannot become a way around the flasher's checks.
pub fn install_on_image(path: &Path) -> Result<Prepared> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !meta.file_type().is_file() {
        bail!(
            "{} is not a regular file; this installs on disk images only",
            path.display()
        );
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let prepared = prepare(&mut f, meta.len(), SECTOR as u32)?;
    prepared.write_vbr(&mut f, prepared.partition_start * SECTOR)?;
    prepared.write_mbr(&mut f)?;
    f.sync_all().context("syncing the image")?;
    prepared.verify(&mut f)?;
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Real tool output, not hand-assembled: sector 0 and the partition's 32
    /// reserved sectors of a 64 MiB image made by `sfdisk` and `mkfs.fat`
    /// (fixtures/README.md has the commands). The partition starts at 2048.
    const HEAD: &[u8] = include_bytes!("../fixtures/bios-fat32-head.bin");
    const START: u64 = 2048;
    const DISK_BYTES: u64 = 64 * 1024 * 1024;

    /// The fixture laid out where it came from, in a buffer just long enough
    /// to hold the sectors an install touches.
    fn disk() -> Vec<u8> {
        let mut d = vec![0u8; ((START + 32) * SECTOR) as usize];
        d[..512].copy_from_slice(&HEAD[..512]);
        let p = (START * SECTOR) as usize;
        d[p..p + 32 * 512].copy_from_slice(&HEAD[512..]);
        d
    }

    fn sector(d: &[u8], lba: u64) -> &[u8] {
        &d[(lba * SECTOR) as usize..((lba + 1) * SECTOR) as usize]
    }

    fn put16(d: &mut [u8], at: usize, v: u16) {
        d[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put32(d: &mut [u8], at: usize, v: u32) {
        d[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Byte offset of a BPB field in the primary boot sector.
    fn bpb(field: usize) -> usize {
        (START * SECTOR) as usize + field
    }

    /// The same field in both the primary and the backup, as a formatter
    /// would have written it, so only the check under test can object.
    fn both(d: &mut [u8], f: impl Fn(&mut [u8], usize)) {
        f(d, bpb(0));
        f(d, bpb(6 * 512));
    }

    fn install(d: &mut Vec<u8>) -> Result<Prepared> {
        let p = prepare(&mut Cursor::new(&d[..]), DISK_BYTES, 512)?;
        let mut c = Cursor::new(d);
        p.write_vbr(&mut c, p.partition_start() * SECTOR)?;
        p.write_mbr(&mut c)?;
        p.verify(&mut c)?;
        Ok(p)
    }

    #[test]
    fn the_fixture_is_what_its_readme_says() {
        let d = disk();
        assert_eq!(&sector(&d, 0)[510..], &[0x55, 0xAA]);
        assert_eq!(sector(&d, 0)[446], 0x80, "one active entry");
        assert_eq!(sector(&d, 0)[446 + 4], 0x0C, "of type 0x0C");
        assert_eq!(le32(sector(&d, 0), 446 + 8), START as u32);
        assert_eq!(&sector(&d, START)[0x52..0x5A], b"FAT32   ");
        assert_eq!(le16(sector(&d, START), 0x32), 6, "backup boot sector at 6");
    }

    /// The splice, field for field: jump and OEM name from our binary, 0x0B
    /// through 0x59 from the volume, code from ours — in the boot sector and
    /// in its backup — and the MBR's 440 bytes, with the disk identifier and
    /// the table untouched. And nothing anywhere else.
    #[test]
    fn installs_on_what_the_real_tools_make() {
        let before = disk();
        let mut d = before.clone();
        let p = install(&mut d).expect("a fresh mkfs.fat volume is exactly what this is for");
        assert_eq!(p.partition_start(), START);
        assert_eq!(p.vbr_sectors(), &[0, 6]);

        let mut want_vbr = FAT32_VBR.to_vec();
        // Literal offsets, not the module's constants: a wrong constant would
        // otherwise move the expectation along with the bug.
        want_vbr[0x0B..0x5A].copy_from_slice(&sector(&before, START)[0x0B..0x5A]);
        for lba in [START, START + 6] {
            assert_eq!(sector(&d, lba), &want_vbr[..], "the VBR at LBA {lba}");
        }
        assert_eq!(&sector(&d, 0)[..440], &MBR_CODE[..]);
        assert_eq!(
            &sector(&d, 0)[440..],
            &sector(&before, 0)[440..],
            "the disk identifier, the table and the signature survive"
        );

        let touched = [0, START, START + 6];
        for lba in 0..(d.len() as u64 / SECTOR) {
            if !touched.contains(&lba) {
                assert_eq!(
                    sector(&d, lba),
                    sector(&before, lba),
                    "LBA {lba} was not ours to write"
                );
            }
        }
    }

    /// Installing twice changes nothing the second time: the BPB spliced in is
    /// the volume's, whatever code surrounds it.
    #[test]
    fn a_second_install_is_a_no_op() {
        let mut d = disk();
        install(&mut d).unwrap();
        let once = d.clone();
        install(&mut d).unwrap();
        assert_eq!(d, once);
    }

    /// Every refusal names its reason, and happens before a single byte is
    /// written. Each mutation is one a formatter, a partitioner or another
    /// tool could actually leave behind.
    #[test]
    #[allow(clippy::type_complexity)]
    fn refuses_what_the_boot_code_cannot_boot_and_writes_nothing() {
        let cases: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>, &str)> = vec![
            ("no table", Box::new(|d| d[511] = 0), "0x55AA"),
            (
                "no active partition",
                Box::new(|d| d[446] = 0),
                "no partition is marked active",
            ),
            (
                "two active partitions",
                Box::new(|d| {
                    d[462] = 0x80;
                    d[462 + 4] = 0x0C;
                }),
                "2 partitions are marked active",
            ),
            (
                "a status byte that is not one",
                Box::new(|d| d[478] = 0x01),
                "status 0x01",
            ),
            (
                "an NTFS partition",
                Box::new(|d| d[446 + 4] = 0x07),
                "type 0x07",
            ),
            (
                "a GPT protective entry",
                Box::new(|d| d[446 + 4] = 0xEE),
                "type 0xEE",
            ),
            (
                "an entry at LBA 0",
                Box::new(|d| put32(d, 446 + 8, 0)),
                "LBA 0",
            ),
            (
                "an entry past the end of the disk",
                Box::new(|d| put32(d, 446 + 12, 200_000)),
                "does not lie inside",
            ),
            (
                "an unformatted partition",
                Box::new(|d| d[bpb(510)] = 0),
                "not a formatted volume",
            ),
            (
                "hidden sectors that are not the partition start",
                Box::new(|d| both(d, |d, o| put32(d, o + 0x1C, 0))),
                "hidden-sectors field",
            ),
            (
                "hidden sectors one off",
                Box::new(|d| both(d, |d, o| put32(d, o + 0x1C, 2047))),
                "hidden-sectors field",
            ),
            (
                "4096-byte sectors in the BPB",
                Box::new(|d| both(d, |d, o| put16(d, o + 0x0B, 4096))),
                "4096-byte sectors",
            ),
            (
                "FAT16",
                Box::new(|d| both(d, |d, o| put16(d, o + 0x16, 200))),
                "not FAT32",
            ),
            (
                "no FAT32 type string",
                Box::new(|d| both(d, |d, o| d[o + 0x52..o + 0x5A].copy_from_slice(b"FAT16   "))),
                "not FAT32",
            ),
            (
                "zero sectors per cluster",
                Box::new(|d| both(d, |d, o| d[o + 0x0D] = 0)),
                "power of two",
            ),
            (
                "three sectors per cluster",
                Box::new(|d| both(d, |d, o| d[o + 0x0D] = 3)),
                "power of two",
            ),
            (
                "no FATs",
                Box::new(|d| both(d, |d, o| d[o + 0x10] = 0)),
                "no FAT",
            ),
            (
                "a volume larger than its partition",
                Box::new(|d| both(d, |d, o| put32(d, o + 0x20, 129_025))),
                "its partition has",
            ),
            (
                "below FAT32's cluster floor",
                Box::new(|d| both(d, |d, o| d[o + 0x0D] = 2)),
                "FAT32 needs at least",
            ),
            (
                "a root directory off the volume",
                Box::new(|d| both(d, |d, o| put32(d, o + 0x2C, 1))),
                "root directory",
            ),
            (
                "only the second FAT live",
                Box::new(|d| both(d, |d, o| put16(d, o + 0x28, 0x81))),
                "reads FAT 0",
            ),
            (
                "a backup boot sector outside the reserved area",
                Box::new(|d| both(d, |d, o| put16(d, o + 0x32, 32))),
                "not a free reserved sector",
            ),
            (
                "a backup boot sector on top of FSInfo",
                Box::new(|d| both(d, |d, o| put16(d, o + 0x32, 1))),
                "not a free reserved sector",
            ),
            (
                "a backup that disagrees with the primary",
                Box::new(|d| d[bpb(6 * 512 + 0x41)] = 1),
                "backup boot sector disagrees",
            ),
        ];
        for (what, mutate, expect) in cases {
            let mut d = disk();
            mutate(&mut d);
            let before = d.clone();
            match install(&mut d) {
                Ok(_) => panic!("{what}: installed, and should have been refused"),
                Err(e) => assert!(
                    e.to_string().contains(expect),
                    "{what}: refused, but the reason does not say {expect:?}: {e}"
                ),
            }
            assert_eq!(d, before, "{what}: refused only after writing");
        }
    }

    /// The VBR reads with a 32-bit LBA. A table and a BPB can both describe a
    /// volume that ends past sector 2^32 on a big enough disk, and every field
    /// would check out except that one.
    #[test]
    fn a_volume_past_lba_2_pow_32_is_refused() {
        let mut d = disk();
        put32(&mut d, 446 + 12, u32::MAX);
        both(&mut d, |d, o| put32(d, o + 0x20, u32::MAX));
        let err = prepare(&mut Cursor::new(&d[..]), 4 << 40, 512).unwrap_err();
        assert!(err.to_string().contains("past LBA 2^32"), "got: {err}");
    }

    #[test]
    fn a_4kn_drive_is_refused_before_its_table_is_read() {
        let d = disk();
        let err = prepare(&mut Cursor::new(&d[..]), DISK_BYTES, 4096).unwrap_err();
        assert!(err.to_string().contains("4096-byte sectors"), "got: {err}");
    }

    /// A volume that names no backup gets the primary only, and the sector a
    /// backup would have used is left alone.
    #[test]
    fn no_backup_is_written_when_the_volume_names_none() {
        for none in [0u16, 0xFFFF] {
            let mut d = disk();
            put16(&mut d, bpb(0x32), none);
            let sector6 = sector(&d, START + 6).to_vec();
            let p = install(&mut d).unwrap();
            assert_eq!(p.vbr_sectors(), &[0]);
            assert_eq!(sector(&d, START + 6), &sector6[..], "BkBootSec {none:#x}");
        }
    }

    /// The read-back has to be able to fail, or it is decoration.
    #[test]
    fn verify_notices_a_sector_that_did_not_take() {
        for (what, lba) in [
            ("MBR", 0),
            ("primary VBR", START),
            ("backup VBR", START + 6),
        ] {
            let mut d = disk();
            let p = install(&mut d).unwrap();
            d[(lba * SECTOR) as usize + 300] ^= 0xFF;
            assert!(
                p.verify(&mut Cursor::new(&d[..])).is_err(),
                "a flipped byte in the {what} passed verification"
            );
        }
        // …and the partition table counts: a boot sector beside a table that
        // changed under it proves nothing.
        let mut d = disk();
        let p = install(&mut d).unwrap();
        d[446] = 0;
        assert!(p.verify(&mut Cursor::new(&d[..])).is_err());
    }

    #[test]
    fn a_disk_too_short_to_hold_its_partition_is_an_error_not_a_panic() {
        let d = disk();
        let short = &d[..(START * SECTOR) as usize];
        assert!(prepare(&mut Cursor::new(short), DISK_BYTES, 512).is_err());
    }

    #[test]
    fn install_on_image_writes_a_real_file_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("sirius-bootcode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("disk.img");
        let mut d = disk();
        d.resize(DISK_BYTES as usize, 0);
        std::fs::write(&img, &d).unwrap();
        install_on_image(&img).expect("installs on an image file");
        let mut want = d.clone();
        install(&mut want).unwrap();
        assert_eq!(std::fs::read(&img).unwrap(), want);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The image path is for image files. A device goes through the flasher,
    /// with all of its checks, or not at all.
    #[cfg(unix)]
    #[test]
    fn install_on_image_refuses_a_device() {
        let err = install_on_image(Path::new("/dev/null")).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "got: {err}");
    }
}
