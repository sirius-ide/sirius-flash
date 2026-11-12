#!/usr/bin/env python3
"""Check the BIOS boot records on a written stick, independently of the flasher.

`sirius-flash write --target bios` reads its own boot records back, but a check
that shares the flasher's code shares its bugs. This one reads only a raw
capture of the stick and the binaries committed beside the boot sources:

    crates/core/boot/mbr.bin     440 bytes, the MBR's code
    crates/core/boot/fat32.bin   512 bytes, the FAT32 volume boot record

Run it from the same commit as the flasher binary that wrote the stick: a
rebuilt mbr.bin or fat32.bin is a correct mbr-code or vbr-code failure.

Capture the first 2080 sectors of the WHOLE disk: the MBR, the gap, and the
partition's reserved sectors through the backup boot sector. Take the capture
from the whole-disk by-id node, not -part1. Before capturing:
- re-plug the stick, so neither the host's cache nor the stick's own can answer
  with what was last written;
- unmount it if the desktop mounted it. A mounted FAT volume sets byte 0x41 of
  its boot sector, its dirty flag, and only in the primary.
Then read past the page cache, into a file outside the repo:

    D=/dev/disk/by-id/usb-...                     # the stick: whole disk, never a kernel name
    sudo dd if="$D" bs=4096 count=260 iflag=direct status=none > <scratch>/head.bin
    ./scripts/check_boot_records.py <scratch>/head.bin --disk "$D"

`--disk` resolves the by-id link and reads the disk's size from sysfs; it never
opens the device. `--disk-sectors N` gives the size directly instead. One of the
two is required, because the table's extent cannot be checked without it.

What passing proves: the boot records are what the flasher should have written,
and what mbr.asm and fat32.asm read is lawful. It cannot see the FAT, the root
directory or BOOTMGR, which lie past the capture.

Every expectation is a literal, never taken from the capture:
  - Sector 0: bytes 0-439 are mbr.bin, and the sector ends 55 AA.
  - The table: exactly one entry, entry 0, active, type 0x0C, at LBA 2048, running
    to the disk's last sector. Entries 1-3 are all zero.
  - LBA 2048 equals fat32.bin everywhere outside the BPB (0x0B-0x59).
  - The BPB, matching mkfs.fat -F 32 as the flasher runs it:
      512 bytes per sector, a power-of-two cluster size up to 128 sectors,
      at least 8 reserved sectors, 2 FATs, the FAT16 fields zero, 2048 hidden
      sectors, a volume that fits its partition, at least 65,525 clusters,
      FATs big enough to map them, ext flags 0 (FAT 0 live and mirrored),
      root cluster 2, FSInfo in reserved sector 1, the backup boot sector in 6,
      "FAT32   ", and the dirty flag clear.
  - The backup boot sector equals the primary.
  - FSInfo carries both signatures.

    ./scripts/check_boot_records.py --selftest

This builds and installs an image with boottest.py, which needs cargo, sfdisk,
mkfs.fat and qemu-system-x86_64, though nothing is booted. It checks that image
through the same command line an operator uses, then puts known faults into
copies of its capture:
  - Each fault must fail on the check it targets.
  - Each lawful variation must still pass.
  - A fault that changes nothing is an error, so the self-test cannot pass
    vacuously.
"""

import argparse
import os
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BOOT = ROOT / "crates" / "core" / "boot"
SECTOR = 512
SECTORS = 2080  # LBA 0 through the partition's reserved sectors and backup
PART = 2048  # where the flasher puts partition 1: 1 MiB in
FSINFO = 1  # the reserved sector mkfs.fat puts FSInfo in
BACKUP = 6  # the reserved sector mkfs.fat and the installer use for the backup
BPB_FIRST, BPB_LAST = 0x0B, 0x59  # the BPB the installer keeps from mkfs.fat
MIN_CLUSTERS = 65525  # fewer, and it is FAT16 by definition, whatever it says


def check(head, mbr, vbr, disk_sectors):
    """Return [(check id, ok, description)] for a capture."""
    out = []

    def add(cid, ok, what):
        out.append((cid, bool(ok), what))

    def sector(n):
        return head[n * SECTOR:(n + 1) * SECTOR]

    add("capture-size", len(head) == SECTORS * SECTOR,
        f"the capture is {SECTORS} sectors (got {len(head)} bytes)")
    add("binaries", len(mbr) == 440 and len(vbr) == 512,
        f"mbr.bin is 440 bytes and fat32.bin 512 (got {len(mbr)} and {len(vbr)})")
    if len(head) < (PART + BACKUP + 1) * SECTOR:
        add("capture-size", False, "too short to reach the backup boot sector; nothing more checked")
        return out

    s0 = sector(0)
    if len(mbr) == 440:
        add("mbr-code", s0[:440] == mbr, "sector 0 bytes 0-439 equal mbr.bin")
    add("mbr-signature", s0[510:512] == b"\x55\xaa", "sector 0 ends 55 AA")
    entries = []
    for i in range(4):
        raw = s0[446 + 16 * i:446 + 16 * (i + 1)]
        start, count = struct.unpack_from("<II", raw, 8)
        entries.append((raw, raw[0], raw[4], start, count))
    add("status-bytes", all(e[1] in (0x00, 0x80) for e in entries),
        "every status byte is 0x00 or 0x80: " + ", ".join(f"{e[1]:#04x}" for e in entries))
    actives = [e for e in entries if e[1] == 0x80]
    add("one-active", len(actives) == 1, f"exactly one active entry (got {len(actives)})")
    add("one-entry", sum(1 for e in entries if e[2] != 0) == 1,
        "exactly one entry has a partition type")
    add("unused-entries", all(e[0] == bytes(16) for e in entries[1:]), "entries 1-3 are all zero")
    first = entries[0]
    add("active-first", first[1] == 0x80, "the active entry is entry 0")
    add("active-type", first[2] == 0x0C, f"entry 0 is type 0x0C (got {first[2]:#04x})")
    add("active-start", first[3] == PART, f"entry 0 starts at LBA {PART} (got {first[3]})")
    add("active-extent", first[3] + first[4] == disk_sectors,
        f"entry 0 ends at the disk's last sector: {first[3]} + {first[4]} = "
        f"{first[3] + first[4]}, disk {disk_sectors}")

    p = sector(PART)
    if len(vbr) == 512:
        differ = [i for i in range(SECTOR) if not BPB_FIRST <= i <= BPB_LAST and p[i] != vbr[i]]
        add("vbr-code", not differ, f"LBA {PART} equals fat32.bin outside 0x0B-0x59"
            + (f" (differs at {', '.join(hex(i) for i in differ[:12])})" if differ else ""))
    bps, spc, reserved, nfats, root_entries, total16 = struct.unpack_from("<HBHBHH", p, 0x0B)
    fatsz16 = struct.unpack_from("<H", p, 0x16)[0]
    hidden, total = struct.unpack_from("<II", p, 0x1C)
    fatsz, extflags, _ver, rootclus, fsinfo, bkboot = struct.unpack_from("<IHHIHH", p, 0x24)
    data = total - reserved - nfats * fatsz
    clusters = data // spc if spc and data > 0 else 0
    add("bytes-per-sector", bps == 512, f"512 bytes per sector (got {bps})")
    add("cluster-size", spc in (1, 2, 4, 8, 16, 32, 64, 128),
        f"sectors per cluster a power of two up to 128 (got {spc})")
    add("reserved", reserved >= 8, f"at least 8 reserved sectors, through the backup FSInfo (got {reserved})")
    add("fat-count", nfats == 2, f"2 FATs (got {nfats})")
    add("fat16-fields", root_entries == 0 and total16 == 0 and fatsz16 == 0,
        f"the FAT16 root-entry, total and FAT-size fields are 0 (got {root_entries}, {total16}, {fatsz16})")
    add("hidden-sectors", hidden == PART, f"{PART} hidden sectors (got {hidden})")
    add("volume-fits", 0 < total <= first[4], f"the volume's {total} sectors fit entry 0's {first[4]}")
    add("cluster-count", clusters >= MIN_CLUSTERS,
        f"at least {MIN_CLUSTERS} clusters, or it is not FAT32 (got {clusters})")
    add("fat-size", fatsz * (SECTOR // 4) >= clusters + 2,
        f"each FAT ({fatsz} sectors) maps every cluster ({clusters} + 2 entries)")
    add("ext-flags", extflags == 0, f"ext flags 0: FAT 0 live, FATs mirrored (got {extflags:#06x})")
    add("root-cluster", rootclus == 2, f"root directory at cluster 2 (got {rootclus})")
    add("fsinfo-location", fsinfo == FSINFO, f"FSInfo in reserved sector {FSINFO} (got {fsinfo})")
    add("backup-location", bkboot == BACKUP, f"backup boot sector in reserved sector {BACKUP} (got {bkboot})")
    add("fs-type", p[0x52:0x5A] == b"FAT32   ", f"type string 'FAT32   ' (got {p[0x52:0x5A]!r})")
    add("dirty-flag", p[0x41] == 0,
        f"byte 0x41 (the dirty flag) clear, as on an unmounted volume (got {p[0x41]:#04x})")
    add("backup-equal", sector(PART + BACKUP) == p, f"LBA {PART + BACKUP} equals LBA {PART}")
    fsi = sector(PART + FSINFO)
    add("fsinfo", fsi[0:4] == b"RRaA" and fsi[484:488] == b"rrAa",
        f"FSInfo (LBA {PART + FSINFO}) carries RRaA and rrAa")
    out.append(("info", True,
                f"{reserved} reserved sectors, {nfats} FATs of {fatsz} sectors, {spc} sectors per "
                f"cluster, {clusters} clusters, drive byte {p[0x40]:#04x}; {first[4] - total} "
                f"partition sectors past the volume; "
                f"{sum(1 for b in head[SECTOR:PART * SECTOR] if b)} non-zero bytes in sectors 1-{PART - 1}"))
    return out


def report(results):
    for cid, ok, what in results:
        if cid == "info":
            print(f"     {what}")
        else:
            print(f"{'ok  ' if ok else 'FAIL'} {cid:17} {what}")
    failed = [cid for cid, ok, _ in results if not ok]
    print()
    print("ALL CHECKS PASSED" if not failed else f"{len(failed)} CHECK(S) FAILED: {', '.join(failed)}")
    return failed


def disk_sectors_of(path):
    """A whole disk's size in 512-byte sectors, from sysfs, by its by-id link."""
    if not str(path).startswith("/dev/disk/by-id/") or "-part" in Path(path).name:
        sys.exit(f"--disk wants a whole disk's /dev/disk/by-id/ path, not {path}")
    name = Path(os.path.realpath(path)).name
    size = Path(f"/sys/class/block/{name}/size")
    if not size.exists() or Path(f"/sys/class/block/{name}/partition").exists():
        sys.exit(f"{path} resolves to {name}, which is not a whole disk in /sys/class/block")
    print(f"     {path} is {name} now; its size comes from {size}")
    return int(size.read_text())


def put(at, data):
    def fault(b):
        b[at:at + len(data)] = data
    return fault


def flip(at):
    def fault(b):
        b[at] ^= 0xFF
    return fault


def selftest(workdir):
    mbr, vbr = (BOOT / "mbr.bin").read_bytes(), (BOOT / "fat32.bin").read_bytes()
    work = Path(workdir)
    img = work / "selftest.img"
    harness = str(ROOT / "scripts" / "boottest.py")
    for argv in ([harness, "build", str(img), "--size-mib", "2048", "--label", "WIN11USB",
                  "--sec-per-clus", "16"], [harness, "install", str(img)]):
        r = subprocess.run(argv, capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"{' '.join(argv[1:3])} failed:\n{r.stdout}{r.stderr}")
    disk = img.stat().st_size // SECTOR
    with open(img, "rb") as f:
        clean = f.read(SECTORS * SECTOR)
    bad = []

    # The operator's path: this very script, run as a command, read by its exit
    # status and its FAIL lines.
    def run_cli(capture, *extra):
        path = work / "capture.bin"
        path.write_bytes(capture)
        return subprocess.run([sys.executable, "-I", str(Path(__file__).resolve()), str(path), *extra],
                              capture_output=True, text=True)

    def cli_case(name, capture, extra, rc, must_show):
        r = run_cli(capture, *extra)
        good = r.returncode == rc and all(s in r.stdout + r.stderr for s in must_show)
        print(f"{'ok  ' if good else 'FAIL'} command line: {name} (exit {r.returncode})")
        if not good:
            print((r.stdout + r.stderr).rstrip())
            bad.append(f"command line: {name}")

    dirty = bytearray(clean)
    dirty[PART * SECTOR + 0x41] = 0x01
    other_mbr = work / "other-mbr.bin"
    other_mbr.write_bytes(bytes([mbr[0] ^ 0xFF]) + mbr[1:])
    short_vbr = work / "short-vbr.bin"
    short_vbr.write_bytes(vbr[:100])
    n = str(disk)
    cli_case("the installed image passes", clean, ["--disk-sectors", n], 0,
             ["ok   active-extent", "ALL CHECKS PASSED"])
    cli_case("a dirty volume fails", bytes(dirty), ["--disk-sectors", n], 1, ["FAIL dirty-flag"])
    cli_case("a disk one sector longer fails", clean, ["--disk-sectors", str(disk + 1)], 1,
             ["FAIL active-extent"])
    cli_case("no disk size is refused", clean, [], 2, ["--disk"])
    cli_case("a kernel-name --disk is refused", clean, ["--disk", "/dev/sda"], 1, ["by-id"])
    cli_case("--mbr is the MBR compared", clean, ["--disk-sectors", n, "--mbr", str(other_mbr)], 1,
             ["FAIL mbr-code"])
    cli_case("a short --vbr fails, not crashes", clean, ["--disk-sectors", n, "--vbr", str(short_vbr)],
             1, ["FAIL binaries"])

    v = PART * SECTOR
    bk = (PART + BACKUP) * SECTOR
    entry0_count = struct.unpack_from("<I", clean, 446 + 12)[0]
    reserved, nfats = struct.unpack_from("<HB", clean, v + 0x0E)
    fatsz = struct.unpack_from("<I", clean, v + 0x24)[0]
    spc = clean[v + 0x0D]

    def both(off, data):
        """The same change in the primary and the backup boot sector, so only the
        check aimed at the field can catch it."""
        def fault(b):
            put(v + off, data)(b)
            put(bk + off, data)(b)
        return fault

    def flip_both(off):
        def fault(b):
            b[v + off] ^= 0xFF
            b[bk + off] ^= 0xFF
        return fault

    faults = [
        ("an MBR code byte", flip(100), None, "mbr-code"),
        ("the MBR's first byte", flip(0), None, "mbr-code"),
        ("the MBR's last code byte (439)", flip(439), None, "mbr-code"),
        ("the 55 AA after the table", put(510, b"\x55\xab"), None, "mbr-signature"),
        ("a status byte of 0x01", put(462, b"\x01"), None, "status-bytes"),
        ("entry 3's status byte 0x01", put(494, b"\x01"), None, "status-bytes"),
        ("a second active entry", put(462, b"\x80"), None, "one-active"),
        ("entry 3 active", put(494, b"\x80"), None, "one-active"),
        ("no active entry", put(446, b"\x00"), None, "one-active"),
        ("a second typed entry", put(466, b"\x0c"), None, "one-entry"),
        ("entry 3 typed", put(498, b"\x0c"), None, "one-entry"),
        ("a stale LBA in entry 1", put(462 + 8, b"\x10"), None, "unused-entries"),
        ("a stale LBA in entry 2", put(478 + 8, b"\x10"), None, "unused-entries"),
        ("a stale LBA in entry 3", put(494 + 8, b"\x10"), None, "unused-entries"),
        ("entry 0 typed 0x07", put(450, b"\x07"), None, "active-type"),
        ("entry 0 typed 0x0B", put(450, b"\x0b"), None, "active-type"),
        ("entry 0 at LBA 2304", put(446 + 8, struct.pack("<I", 2304)), None, "active-start"),
        ("a disk one sector longer than the table", None, disk + 1, "active-extent"),
        ("a VBR code byte", flip(v + 0x60), None, "vbr-code"),
        ("the VBR's first byte", flip(v + 0), None, "vbr-code"),
        ("the VBR's jump target (byte 1)", flip(v + 1), None, "vbr-code"),
        ("the OEM name's first byte", flip(v + 3), None, "vbr-code"),
        ("the OEM name's last byte, just before the BPB", flip(v + 0x0A), None, "vbr-code"),
        ("the first code byte, just after the BPB", flip(v + 0x5A), None, "vbr-code"),
        ("the VBR's 55", put(v + 0x1FE, b"\x00"), None, "vbr-code"),
        ("the VBR's AA", put(v + 0x1FF, b"\x00"), None, "vbr-code"),
        ("4096 bytes per sector", both(0x0B, struct.pack("<H", 4096)), None, "bytes-per-sector"),
        ("3 sectors per cluster", both(0x0D, b"\x03"), None, "cluster-size"),
        ("0 sectors per cluster", both(0x0D, b"\x00"), None, "cluster-size"),
        ("0 reserved sectors", both(0x0E, struct.pack("<H", 0)), None, "reserved"),
        ("7 reserved sectors", both(0x0E, struct.pack("<H", 7)), None, "reserved"),
        ("1 FAT", both(0x10, b"\x01"), None, "fat-count"),
        ("FAT16 root entries", both(0x11, struct.pack("<H", 512)), None, "fat16-fields"),
        ("a FAT16 total", both(0x13, struct.pack("<H", 1)), None, "fat16-fields"),
        ("a FAT16 FAT size", both(0x16, struct.pack("<H", 1)), None, "fat16-fields"),
        ("1792 hidden sectors", both(0x1C, struct.pack("<I", 1792)), None, "hidden-sectors"),
        ("0 hidden sectors", both(0x1C, struct.pack("<I", 0)), None, "hidden-sectors"),
        ("a volume one sector past its partition",
         both(0x20, struct.pack("<I", entry0_count + 1)), None, "volume-fits"),
        ("a volume of 0 sectors", both(0x20, struct.pack("<I", 0)), None, "volume-fits"),
        ("too few clusters for FAT32",
         both(0x20, struct.pack("<I", reserved + nfats * fatsz + 16 * (MIN_CLUSTERS - 1))),
         None, "cluster-count"),
        ("a FAT of 0 sectors", both(0x24, struct.pack("<I", 0)), None, "fat-size"),
        ("a FAT too small to map the volume", both(0x24, struct.pack("<I", fatsz // 2)), None, "fat-size"),
        # Exactly one FAT entry short: clusters + 2 entries = fatsz * 128 + 1.
        ("a FAT one entry short of the volume",
         both(0x20, struct.pack("<I", reserved + nfats * fatsz + spc * (fatsz * 128 - 1))),
         None, "fat-size"),
        ("FAT 1 active, mirroring off", both(0x28, struct.pack("<H", 0x81)), None, "ext-flags"),
        ("root directory at cluster 3", both(0x2C, struct.pack("<I", 3)), None, "root-cluster"),
        ("FSInfo in reserved sector 2", both(0x30, struct.pack("<H", 2)), None, "fsinfo-location"),
        ("the backup in reserved sector 7", both(0x32, struct.pack("<H", 7)), None, "backup-location"),
        ("no backup (0)", both(0x32, struct.pack("<H", 0)), None, "backup-location"),
        ("no backup (0xFFFF)", both(0x32, struct.pack("<H", 0xFFFF)), None, "backup-location"),
        ("the dirty flag set", both(0x41, b"\x01"), None, "dirty-flag"),
        ("another bit of byte 0x41 set", both(0x41, b"\x02"), None, "dirty-flag"),
        ("a FAT16 type string", both(0x52, b"FAT16   "), None, "fs-type"),
        ("a backup that differs in code", flip(bk + 0x100), None, "backup-equal"),
        ("a backup that differs in its dirty flag", put(bk + 0x41, b"\x01"), None, "backup-equal"),
        ("a backup that differs in its last byte", flip(bk + 0x1FF), None, "backup-equal"),
        ("a damaged FSInfo lead signature", put((PART + FSINFO) * SECTOR, b"XXXX"), None, "fsinfo"),
        ("a damaged FSInfo struct signature", put((PART + FSINFO) * SECTOR + 484, b"XXXX"), None, "fsinfo"),
        ("a capture one sector short", lambda b: b.__delitem__(slice(-SECTOR, None)), None, "capture-size"),
    ]

    # Lawful variations a correct checker must still pass: a check that is
    # too strict would fail them.
    lawful = [
        ("a volume exactly filling its partition", both(0x20, struct.pack("<I", entry0_count))),
        ("another drive number in both boot sectors (BPB byte 0x40)", both(0x40, b"\x81")),
        # 32, not 128: the 2 GiB image would drop below FAT32's cluster floor.
        ("32 sectors per cluster", both(0x0D, b"\x20")),
        ("non-zero bytes in the MBR-to-partition gap", put(5 * SECTOR, b"\x01")),
        ("a different disk signature (bytes 440-443)", put(440, b"\x12\x34\x56\x78")),
    ]
    for name, fault in lawful:
        b = bytearray(clean)
        fault(b)
        if bytes(b) == clean:
            print(f"FAIL {name}: the variation changed nothing, so this case proves nothing")
            bad.append(name)
            continue
        failed = [cid for cid, ok, _ in check(bytes(b), mbr, vbr, disk) if not ok]
        print(f"{'ok  ' if not failed else 'FAIL'} {name}: still passes"
              + (f" (but fails {', '.join(failed)})" if failed else ""))
        if failed:
            bad.append(name)

    for name, fault, disk_override, expect in faults:
        b = bytearray(clean)
        if fault:
            fault(b)
        if bytes(b) == clean and disk_override is None:
            print(f"FAIL {name}: the fault changed nothing, so this case proves nothing")
            bad.append(name)
            continue
        failed = [cid for cid, ok, _ in check(bytes(b), mbr, vbr,
                                              disk_override if disk_override else disk) if not ok]
        caught = expect in failed
        print(f"{'ok  ' if caught else 'FAIL'} {name}: fails {', '.join(failed) or 'nothing'}"
              + ("" if caught else f" (expected {expect})"))
        if not caught:
            bad.append(name)
    print()
    print(f"SELFTEST PASSED: 7 command-line cases, {len(lawful)} lawful variations and "
          f"{len(faults)} faults" if not bad else f"SELFTEST FAILED: {', '.join(bad)}")
    return 1 if bad else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("capture", nargs="?", help="the first 2080 sectors of the stick")
    size = ap.add_mutually_exclusive_group()
    size.add_argument("--disk", help="the stick's whole-disk /dev/disk/by-id/ path (read via sysfs)")
    size.add_argument("--disk-sectors", type=int, help="the disk's size in 512-byte sectors")
    ap.add_argument("--mbr", default=str(BOOT / "mbr.bin"))
    ap.add_argument("--vbr", default=str(BOOT / "fat32.bin"))
    ap.add_argument("--selftest", action="store_true",
                    help="build an image, check it passes, and check known faults fail")
    a = ap.parse_args()
    if a.selftest:
        with tempfile.TemporaryDirectory(prefix="check-boot-records-") as d:
            return selftest(d)
    if not a.capture:
        ap.error("give a capture, or --selftest")
    if a.disk is None and a.disk_sectors is None:
        ap.error("give --disk /dev/disk/by-id/usb-... or --disk-sectors N: the table's extent "
                 "is checked against the disk's size")
    disk = disk_sectors_of(a.disk) if a.disk else a.disk_sectors
    head = Path(a.capture).read_bytes()
    failed = report(check(head, Path(a.mbr).read_bytes(), Path(a.vbr).read_bytes(), disk))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
