#!/usr/bin/env python3
"""Boot-test harness: build bootable media in a plain file, boot it, read the screen.

Everything here runs **unprivileged**. An earlier version of PROJECT-STATE said
local boot testing was impossible because loop devices need root and `mtools` is
not installed, and used that to argue boot-sector work could not be verified.
That was wrong twice over:

  * `sfdisk` partitions a plain file, and `mkfs.fat --offset=SECTOR` creates the
    filesystem *inside* that file at the partition offset. Between them there is
    nothing left for a loop device to do.
  * `put` below writes a file into the FAT32 volume directly — BPB, both FAT
    copies, an 8.3 root entry, cluster data — which is all `mcopy` was wanted for.

Output is read without a display by dumping the VGA text buffer from the qemu
monitor (`memsave 0xb8000 4000`) and decoding the 80x25 char/attribute pairs, so
results are greppable strings rather than screenshots.

    ./scripts/boottest.py selftest          # proves the harness detects both outcomes

Requires: sfdisk, mkfs.fat, qemu-system-x86_64, nasm (selftest only), python3.
"""

import argparse
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

SECTOR = 512
TOOLS = ["sfdisk", "mkfs.fat", "qemu-system-x86_64"]


def need_tools(extra=()):
    missing = [t for t in list(TOOLS) + list(extra) if shutil.which(t) is None]
    if missing:
        sys.exit(f"missing required tools: {', '.join(missing)}")


def sh(*cmd, stdin=None):
    p = subprocess.run(cmd, input=stdin, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"{cmd[0]} failed ({p.returncode}):\n{p.stderr.strip()}")
    return p.stdout


# --------------------------------------------------------------------------- #
# BPB
# --------------------------------------------------------------------------- #


class Bpb:
    """The FAT32 BIOS Parameter Block, as boot code reads it."""

    def __init__(self, vbr, part_start):
        u = lambda o, n: int.from_bytes(vbr[o : o + n], "little")
        self.part_start = part_start
        self.bytes_per_sec = u(0x0B, 2)
        self.sec_per_clus = u(0x0D, 1)
        self.rsvd_sec = u(0x0E, 2)
        self.num_fats = u(0x10, 1)
        self.fat_sz16 = u(0x16, 2)
        self.hidden = u(0x1C, 4)
        self.tot_sec32 = u(0x20, 4)
        self.fat_sz32 = u(0x24, 4)
        self.root_clus = u(0x2C, 4)
        self.fs_info = u(0x30, 2)
        self.bk_boot_sec = u(0x32, 2)
        if vbr[510:512] != b"\x55\xaa":
            sys.exit("volume has no 0x55AA signature — is the offset right?")
        if self.bytes_per_sec != SECTOR:
            sys.exit(f"unsupported sector size {self.bytes_per_sec}")

    @property
    def clusters(self):
        data_sectors = self.tot_sec32 - self.rsvd_sec - self.num_fats * self.fat_sz32
        return data_sectors // self.sec_per_clus

    @property
    def fat_lba(self):
        return self.part_start + self.rsvd_sec

    @property
    def data_lba(self):
        return self.fat_lba + self.num_fats * self.fat_sz32

    def cluster_lba(self, n):
        return self.data_lba + (n - 2) * self.sec_per_clus

    def check(self, part_sectors):
        # Both of these are silent-corruption traps rather than errors any tool
        # reports. `mkfs.fat` does NOT derive hidden sectors from --offset, so
        # without -h the BPB says the volume starts at LBA 0 and every absolute
        # read a boot sector computes lands 2048 sectors early. And BLOCK-COUNT
        # is in 1024-byte blocks, not sectors, so passing the sector count makes
        # a filesystem twice the size of its partition, which mounts happily
        # until something reads past the end.
        if self.hidden != self.part_start:
            sys.exit(f"BPB hidden sectors is {self.hidden}, expected {self.part_start}")
        if self.tot_sec32 > part_sectors:
            sys.exit(f"filesystem claims {self.tot_sec32} sectors, partition has {part_sectors}")
        # On FAT12/16 the BPB ends at 0x3E and 0x24 onwards is boot code, so
        # every FAT32 field read above would be a misreading of it.
        if self.fat_sz16 != 0:
            sys.exit("this is FAT12/16, not FAT32 (BPB_FATSz16 is non-zero)")
        # What separates FAT32 from FAT16 is the cluster count, not the BPB
        # fields, and `mkfs.fat -F 32` will build a volume below the floor
        # without complaining — only `fsck.fat` mentions it afterwards. Media
        # that no Windows tool considers valid FAT32 is a bad place to start
        # debugging a boot sector, so refuse it here.
        if self.clusters < 65525:
            sys.exit(f"only {self.clusters} clusters; FAT32 needs 65525. "
                     f"Use a larger --size-mib or a smaller --sec-per-clus.")


def read_bpb(image, part_start):
    with open(image, "rb") as f:
        f.seek(part_start * SECTOR)
        return Bpb(f.read(SECTOR), part_start)


def partition_sectors(image, start):
    """Size of the MBR partition beginning at `start`; exits if none does."""
    with open(image, "rb") as f:
        mbr = f.read(SECTOR)
    if mbr[510:512] != b"\x55\xaa":
        sys.exit("no partition table at LBA 0")
    # Each entry: status, CHS first, type, CHS last, LBA start, sector count.
    entries = [struct.unpack_from("<B3xB3xII", mbr, 446 + 16 * i) for i in range(4)]
    used = [(lba, count) for _, ptype, lba, count in entries if ptype]
    for lba, count in used:
        if lba == start:
            return count
    sys.exit(f"no partition starts at LBA {start}; "
             f"the table has {[lba for lba, _ in used] or 'none'}")


# --------------------------------------------------------------------------- #
# build
# --------------------------------------------------------------------------- #


def cmd_build(a):
    need_tools()
    part_sectors = (a.size_mib * 1024 * 1024) // SECTOR - a.start
    if os.path.exists(a.image):
        os.remove(a.image)
    with open(a.image, "wb") as f:
        f.truncate(a.size_mib * 1024 * 1024)

    sh("sfdisk", "--quiet", a.image,
       stdin=f"label: dos\nunit: sectors\nstart={a.start}, size={part_sectors}, type=c, bootable\n")

    # -s 8 rather than whatever mkfs.fat picks: it chooses 1 sector per cluster
    # at this size, which is the one geometry where cluster-to-LBA arithmetic is
    # the identity. A boot sector that multiplies by the wrong thing passes on a
    # 1-sector cluster and fails on every real stick.
    sh("mkfs.fat", "-F", "32", "-n", a.label, "-s", str(a.sec_per_clus),
       "-h", str(a.start), f"--offset={a.start}", a.image, str(part_sectors // 2))

    bpb = read_bpb(a.image, a.start)
    bpb.check(part_sectors)
    print(f"{a.image}: {a.size_mib} MiB, partition at LBA {a.start} ({part_sectors} sectors), "
          f"FAT32 label {a.label}, {bpb.sec_per_clus} sec/cluster, data at LBA {bpb.data_lba}")


# --------------------------------------------------------------------------- #
# install boot code
# --------------------------------------------------------------------------- #


def cmd_mbr(a):
    code = open(a.binary, "rb").read()
    # 440, not 446: 440-443 is the disk identifier and 444-445 are reserved.
    # Overwriting them is what makes Windows think it is a different disk.
    if len(code) > 440:
        sys.exit(f"{a.binary} is {len(code)} bytes; MBR boot code must fit in 440")
    with open(a.image, "r+b") as f:
        f.seek(0)
        f.write(code.ljust(440, b"\0"))
        f.seek(510)
        if f.read(2) != b"\x55\xaa":
            sys.exit("MBR lost its 0x55AA signature")
    print(f"installed {len(code)} bytes of MBR boot code (partition table untouched)")


def cmd_vbr(a):
    """Install a volume boot record, keeping the BPB this volume already has.

    Boot code and filesystem geometry live in the same 512 bytes, which is why
    ms-sys and Rufus patch a BPB into their blobs rather than writing them
    whole. Bytes 0x0B-0x59 describe *this* volume and must survive; the jump at
    0x00-0x02, the OEM name and the code from 0x5A on come from the new record.
    (ms-sys keeps only 0x0B-0x51 and writes the "FAT32   " type string itself;
    it refuses any volume where that string is not already there, so on every
    volume either would touch the two produce the same bytes.)

    Every check runs before the first write. The first version wrote and then
    re-parsed, so `--start 0` replaced the partition table and only afterwards
    reported "unsupported sector size 0".
    """
    new = open(a.binary, "rb").read()
    if len(new) != SECTOR:
        sys.exit(f"{a.binary} is {len(new)} bytes; a VBR must be exactly {SECTOR}")
    if new[510:512] != b"\x55\xaa":
        sys.exit(f"{a.binary} does not end in 0x55AA — not built as a boot sector")
    # The jump is what carries the CPU over the BPB. A binary laid out as plain
    # code from 0x00 — an MBR, say — has none, and its bytes 0x0B-0x59 are
    # instructions the splice below would replace with geometry.
    if new[0] == 0xEB:
        target = 2 + int.from_bytes(new[1:2], "little", signed=True)
    elif new[0] == 0xE9:
        target = 3 + int.from_bytes(new[1:3], "little", signed=True)
    else:
        sys.exit(f"{a.binary} does not start with a jump over the BPB")
    if not 0x5A <= target < 510:
        sys.exit(f"{a.binary} jumps to 0x{target:X}; code must start at or after 0x5A")
    # Anything here is overwritten, so anything here was a mistake: the splice
    # would replace it with this volume's geometry and the install would still
    # report success. Reserve the region with zeros.
    if any(new[0x0B:0x5A]):
        sys.exit(f"{a.binary} has non-zero bytes in 0x0B-0x59, which the installer "
                 f"overwrites with this volume's BPB; reserve them with zeros")

    # Partition table first: pointed at the wrong LBA, "no partition starts
    # there" is the answer, not whatever a BPB parse makes of the bytes found.
    sectors = partition_sectors(a.image, a.start)
    bpb = read_bpb(a.image, a.start)
    bpb.check(sectors)
    with open(a.image, "r+b") as f:
        f.seek(a.start * SECTOR)
        old = f.read(SECTOR)
        targets = [a.start]
        # The backup boot sector gets the same record. Rufus writes both
        # (format.c WritePBR, `_offset += 6 * SectorSize`). A stale backup makes
        # every `fsck.fat` on the stick report differences, and a repair that
        # restores from it quietly puts back the code this replaced. 0 and
        # 0xFFFF both mean "no backup".
        if bpb.bk_boot_sec not in (0, 0xFFFF):
            if bpb.bk_boot_sec >= bpb.rsvd_sec or bpb.bk_boot_sec == bpb.fs_info:
                sys.exit(f"BPB puts the backup boot sector at {bpb.bk_boot_sec}, "
                         f"which is not a free reserved sector")
            f.seek((a.start + bpb.bk_boot_sec) * SECTOR)
            if f.read(SECTOR)[0x0B:0x5A] != old[0x0B:0x5A]:
                sys.exit("the backup boot sector's BPB differs from the primary's; "
                         "run fsck.fat on the volume first")
            targets.append(a.start + bpb.bk_boot_sec)

        merged = bytearray(new)
        merged[0x0B:0x5A] = old[0x0B:0x5A]
        for lba in targets:
            f.seek(lba * SECTOR)
            f.write(merged)
    print(f"installed VBR at LBA {', '.join(map(str, targets))}, BPB preserved")


# --------------------------------------------------------------------------- #
# put a file into the FAT32 volume
# --------------------------------------------------------------------------- #


def cmd_put(a):
    data = open(a.source, "rb").read()
    bpb = read_bpb(a.image, a.start)
    name = a.name.upper()
    stem, _, ext = name.partition(".")
    if len(stem) > 8 or len(ext) > 3:
        sys.exit(f"{a.name!r} is not an 8.3 name")
    entry_name = stem.ljust(8)[:8] + ext.ljust(3)[:3]

    clus_bytes = bpb.sec_per_clus * SECTOR
    needed = max(1, -(-len(data) // clus_bytes))

    with open(a.image, "r+b") as f:
        def fat_read(n):
            f.seek(bpb.fat_lba * SECTOR + n * 4)
            return int.from_bytes(f.read(4), "little") & 0x0FFFFFFF

        def fat_write(n, val):
            # The top 4 bits of a FAT32 entry are reserved and must be kept.
            for copy in range(bpb.num_fats):
                off = (bpb.fat_lba + copy * bpb.fat_sz32) * SECTOR + n * 4
                f.seek(off)
                old = int.from_bytes(f.read(4), "little")
                f.seek(off)
                f.write(((old & 0xF0000000) | (val & 0x0FFFFFFF)).to_bytes(4, "little"))

        total = bpb.fat_sz32 * SECTOR // 4
        chain, n = [], 2
        while len(chain) < needed and n < total:
            if fat_read(n) == 0 and n != bpb.root_clus:
                chain.append(n)
            n += 1
        if len(chain) < needed:
            sys.exit(f"no room: need {needed} clusters, found {len(chain)}")

        for i, c in enumerate(chain):
            fat_write(c, 0x0FFFFFFF if i == len(chain) - 1 else chain[i + 1])
            f.seek(bpb.cluster_lba(c) * SECTOR)
            f.write(data[i * clus_bytes : (i + 1) * clus_bytes].ljust(clus_bytes, b"\0"))

        # Root directory: first free slot in the cluster chain. Deliberately no
        # extension of the chain — failing loudly beats half-writing a directory.
        slot = None
        clus = bpb.root_clus
        while slot is None and 2 <= clus < 0x0FFFFFF8:
            base = bpb.cluster_lba(clus) * SECTOR
            f.seek(base)
            block = f.read(clus_bytes)
            for i in range(0, clus_bytes, 32):
                if block[i] in (0x00, 0xE5):
                    slot = base + i
                    break
            clus = fat_read(clus)
        if slot is None:
            sys.exit("root directory is full")

        # A directory entry is exactly 32 bytes and every field is positional:
        #   0-10 name   11 attr   12-19 NT/creation/access   20-21 cluster hi
        #   22-25 write time+date   26-27 cluster lo   28-31 size
        # One byte too many in the padding shifts the tail by one, and the size
        # then reads 256x too large while the cluster number points at nothing.
        entry = (entry_name.encode("ascii") + bytes([0x20]) + b"\0" * 8
                 + struct.pack("<H", chain[0] >> 16) + b"\0" * 4
                 + struct.pack("<HI", chain[0] & 0xFFFF, len(data)))
        assert len(entry) == 32, len(entry)
        f.seek(slot)
        f.write(entry)

        # FSInfo's free-cluster count is now stale. It is advisory — boot code
        # never reads it — but leaving it wrong makes `fsck.fat` report a
        # problem that is ours, not the volume's, on media meant for debugging
        # somebody else's bug.
        if bpb.fs_info:
            free = sum(1 for n in range(2, bpb.clusters + 2) if fat_read(n) == 0)
            f.seek((bpb.part_start + bpb.fs_info) * SECTOR + 0x1E8)
            f.write(struct.pack("<II", free, chain[-1]))

    print(f"wrote {a.name} ({len(data)} bytes, {needed} cluster(s) from {chain[0]})")


# --------------------------------------------------------------------------- #
# run
# --------------------------------------------------------------------------- #


def decode_vga(raw):
    """80x25 char/attribute pairs -> a list of text lines."""
    out = []
    for row in range(25):
        line = "".join(
            chr(raw[(row * 80 + col) * 2]) if 32 <= raw[(row * 80 + col) * 2] < 127 else " "
            for col in range(80)
        )
        out.append(line.rstrip())
    return out


def boot_and_read(image, expect=None, timeout=25, settle=0.0):
    """Boot the image and return the screen as 25 lines of text.

    Stops as soon as `expect` appears, so a passing run costs a second or two
    and only a failing one waits out the timeout.

    `settle` keeps reading for that many seconds *after* `expect` appears. It
    exists for the case where the thing being waited for is the handover rather
    than the result: SeaBIOS prints "Booting from Hard Disk" and only then jumps
    to the MBR, so a screen captured the instant that text appears has not yet
    given the boot code a chance to run, and would show an empty screen for
    working and broken media alike.
    """
    need_tools()
    with tempfile.TemporaryDirectory() as tmp:
        # The dump filename passed to `memsave` must be RELATIVE, so qemu is
        # started with its working directory here. The HMP parser treats a
        # leading "/" as the start of a format specifier, so an absolute path
        # fails with `invalid char 't' in expression` — pointing at /tmp, not at
        # anything wrong with the command — and writes nothing. Nothing else
        # reports the failure: the monitor's reply is swallowed by terminal echo
        # and qemu's exit status is unaffected, so the only symptom is a dump
        # that never appears.
        sock, dump = os.path.join(tmp, "mon"), "vga.bin"
        qemu = subprocess.Popen(
            ["qemu-system-x86_64", "-machine", "pc", "-accel", "tcg", "-m", "64",
             "-drive", f"file={os.path.abspath(image)},format=raw,if=ide",
             "-display", "none", "-vga", "std",
             # Freeze rather than reboot or exit. A boot sector that faults
             # resets the machine; -no-reboot would make qemu exit and take the
             # screen — the only evidence of what went wrong — with it, and a
             # plain reset would clear it. qemu has no reboot=pause, so route
             # reset to shutdown and pause on that. panic=pause covers the
             # triple fault qemu reports as a guest panic rather than a reset.
             "-action", "reboot=shutdown", "-action", "shutdown=pause",
             "-action", "panic=pause",
             "-monitor", f"unix:{sock},server,nowait"],
            cwd=tmp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.time() + timeout
            while not os.path.exists(sock) and time.time() < deadline:
                time.sleep(0.05)
            mon = socket.socket(socket.AF_UNIX)
            mon.connect(sock)
            mon.settimeout(1)

            # Poll rather than sleeping a fixed time: the first qemu run on a
            # cold cache is far slower than later ones, and a fixed sleep that
            # is long enough to be reliable makes every later run pay for it.
            screen = []
            while time.time() < deadline:
                mon.sendall(f"memsave 0xb8000 4000 {dump}\n".encode())
                time.sleep(0.25)
                try:
                    with open(os.path.join(tmp, dump), "rb") as f:
                        raw = f.read()
                    if len(raw) == 4000:
                        screen = decode_vga(raw)
                except OSError:
                    continue
                if expect and any(expect in l for l in screen):
                    if settle <= 0:
                        break
                    deadline, expect, settle = time.time() + settle, None, 0
            try:
                mon.sendall(b"quit\n")
            except OSError:
                pass
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                qemu.kill()

    return screen


def show(screen):
    for i, line in enumerate(screen):
        if line.strip():
            print(f"{i:2d}| {line}")


def cmd_run(a):
    screen = boot_and_read(a.image, a.expect, a.timeout, a.settle)
    if not screen:
        print("no VGA dump — qemu produced nothing readable", file=sys.stderr)
        return 1
    show(screen)
    if a.expect:
        hit = any(a.expect in l for l in screen)
        print(f"\n{'PASS' if hit else 'FAIL'}: {a.expect!r} "
              f"{'found' if hit else 'not on screen'}")
        return 0 if hit else 1
    return 0


# --------------------------------------------------------------------------- #
# selftest
# --------------------------------------------------------------------------- #


def cmd_selftest(a):
    """Prove the harness reports both outcomes, not just the happy one.

    A test rig that has only ever seen a pass is indistinguishable from one
    that always passes, so this checks a known-good boot sector is detected AND
    that media with no boot code is detected as the failure it is.
    """
    need_tools(["nasm"])
    here = os.path.dirname(os.path.abspath(__file__))
    tmp = tempfile.mkdtemp(prefix="boottest-")
    img = os.path.join(tmp, "disk.img")
    mbr = os.path.join(tmp, "marker.bin")
    failed = []
    try:
        cmd_build(argparse.Namespace(image=img, size_mib=512, label="BOOTTEST",
                                     start=2048, sec_per_clus=8))

        print("\n--- case 1: no boot code (what this build produces for MBR+BIOS) ---")
        # Asserting only that the marker is absent would also pass if the
        # machine had simply not got that far yet, which makes the result a
        # function of the timeout. Requiring SeaBIOS's own "Booting from Hard
        # Disk" first pins the machine to the moment it handed control to the
        # MBR, so the absent marker means there was nothing there to run.
        screen = boot_and_read(img, expect="Booting from Hard Disk",
                               timeout=a.timeout, settle=3)
        show(screen)
        if not any("Booting from Hard Disk" in l for l in screen):
            failed.append("case 1 never reached the boot attempt — the run proves nothing")
        elif any("SIRIUS-MBR-OK" in l for l in screen):
            failed.append("case 1 found a marker on media that has no boot code")
        else:
            print("\nas expected: SeaBIOS handed over, and there was nothing to run — "
                  "the silent dead stick, in a few seconds")

        print("\n--- case 2: a known-good MBR ---")
        sh("nasm", "-f", "bin", os.path.join(here, "boot", "marker.asm"), "-o", mbr)
        cmd_mbr(argparse.Namespace(image=img, binary=mbr))
        screen = boot_and_read(img, expect="SIRIUS-MBR-OK", timeout=a.timeout)
        show(screen)
        if any("SIRIUS-MBR-OK" in l for l in screen):
            print("\nthe marker is on screen: a booting MBR is detected as one")
        else:
            failed.append("case 2 did not find the marker a known-good MBR prints")

        # Case 3 checks `put` against something that did not write the volume.
        # Reading it back with the same BPB code that wrote it would round-trip
        # any misunderstanding intact — which is exactly what happened: a
        # directory entry one byte too long reported a 30000-byte file as
        # 7680000, and only fsck.fat and 7z noticed.
        print("\n--- case 3: the FAT writer, checked by fsck.fat ---")
        if shutil.which("fsck.fat") is None:
            print("fsck.fat not installed — skipping (this case is being skipped, "
                  "not passing)")
        else:
            src = os.path.join(tmp, "payload.bin")
            part = os.path.join(tmp, "part.img")
            with open(src, "wb") as f:
                f.write(bytes(range(256)) * 200)   # 51200 bytes: several clusters
            cmd_put(argparse.Namespace(image=img, source=src, name="BOOTMGR", start=2048))
            sh("dd", f"if={img}", f"of={part}", "bs=512", "skip=2048",
               "conv=sparse", "status=none")
            check = subprocess.run(["fsck.fat", "-n", part], capture_output=True, text=True)
            print(check.stdout.strip())
            if check.returncode != 0:
                failed.append(f"case 3: fsck.fat rejected the volume "
                              f"({check.returncode})")
            else:
                print("fsck.fat reads the volume clean")

        # Case 4 checks `vbr`, which every boot-record case after it relies on.
        # Its failures were all silent the first time it was run. Pointed at the
        # wrong sector it wrote first and validated after: `--start 0` replaced
        # the partition table and only then said "unsupported sector size 0".
        # Given a binary with code in 0x0B-0x59 it overwrote that code with
        # this volume's geometry and reported success. And it left the backup
        # boot sector holding the old code. So it has to refuse with the image
        # untouched before its install is worth checking at all.
        print("\n--- case 4: the VBR installer ---")
        # MBR through the partition's reserved area: every sector `vbr` can
        # reach from the two starts used below.
        with open(img, "rb") as f:
            head = f.read((2048 + read_bpb(img, 2048).rsvd_sec) * SECTOR)

        fixture = bytearray(SECTOR)
        fixture[0:3] = b"\xeb\x58\x90"                  # jmp short 0x5A; nop
        fixture[3:11] = b"SIRIUSVB"                     # OEM name: ours, not the volume's
        # Position-dependent bytes, so a write shifted by one cannot match.
        fixture[0x5A:510] = bytes(i & 0xFF for i in range(0x5A, 510))
        fixture[510:] = b"\x55\xaa"
        stray = bytearray(fixture)
        stray[0x3E] = 0x90                              # one byte of code inside the BPB
        mbr_as_vbr = open(mbr, "rb").read().ljust(510, b"\0") + b"\x55\xaa"
        vbr = os.path.join(tmp, "try.vbr")

        for binary, start, what in [
            (mbr_as_vbr, 2048, "an MBR padded to 512 bytes"),
            (stray, 2048, "a VBR with code in the BPB region"),
            (fixture, 0, "--start 0, which is the MBR"),
        ]:
            with open(vbr, "wb") as f:
                f.write(binary)
            try:
                cmd_vbr(argparse.Namespace(image=img, binary=vbr, start=start))
                why = None
            except SystemExit as e:
                why = e.code
            with open(img, "r+b") as f:
                now = f.read(len(head))
                if now != head:
                    f.seek(0)
                    f.write(head)                       # restore, so later checks start clean
            if now != head:
                failed.append(f"case 4: {what} was "
                              f"{'refused only after writing' if why else 'accepted and written'}")
            elif why is None:
                failed.append(f"case 4: accepted {what}")
            else:
                print(f"refused {what}, image untouched: {why}")

        with open(vbr, "wb") as f:
            f.write(fixture)
        cmd_vbr(argparse.Namespace(image=img, binary=vbr, start=2048))
        with open(img, "rb") as f:
            now = f.read(len(head))
        primary = 2048 * SECTOR
        old = head[primary : primary + SECTOR]
        backup = (2048 + int.from_bytes(old[0x32:0x34], "little")) * SECTOR
        want = bytes(fixture[:0x0B]) + old[0x0B:0x5A] + bytes(fixture[0x5A:])
        for name, off in (("primary", primary), ("backup", backup)):
            if now[off : off + SECTOR] != want:
                failed.append(f"case 4: the {name} boot sector is not jump + OEM from the "
                              f"binary, 0x0B-0x59 from the volume, code from the binary")
        unchanged = lambda b: b[:primary] + b[primary + SECTOR : backup] + b[backup + SECTOR :]
        if unchanged(now) != unchanged(head):
            failed.append("case 4: vbr wrote outside the two boot sectors")

        if shutil.which("fsck.fat") is None:
            print("fsck.fat not installed — skipping its half of case 4 (skipped, not passing)")
        else:
            sh("dd", f"if={img}", f"of={part}", "bs=512", "skip=2048",
               "conv=sparse", "status=none")
            check = subprocess.run(["fsck.fat", "-n", part], capture_output=True, text=True)
            print(check.stdout.strip())
            # fsck.fat calls a stale backup boot sector "mostly harmless" and
            # exits 0, so its exit status alone passes exactly the bug this is
            # looking for. The text is the only signal.
            if check.returncode != 0 or "differences between boot sector" in check.stdout:
                failed.append("case 4: fsck.fat objects to the volume after vbr")
    finally:
        if a.keep:
            print(f"\nkept: {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    if failed:
        print("\nSELFTEST FAILED:", file=sys.stderr)
        for f in failed:
            print(f"  - {f}", file=sys.stderr)
        return 1
    print("\nSELFTEST PASSED: the harness detects a booting MBR and a dead one")
    return 0


# --------------------------------------------------------------------------- #

def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("build", help="make a partitioned FAT32 image, unprivileged")
    b.add_argument("image")
    b.add_argument("--size-mib", type=int, default=512,
                   help="default clears FAT32's 65525-cluster floor at 4 KiB clusters")
    b.add_argument("--label", default="BOOTTEST")
    b.add_argument("--start", type=int, default=2048)
    b.add_argument("--sec-per-clus", type=int, default=8)
    b.set_defaults(func=cmd_build)

    m = sub.add_parser("mbr", help="install MBR boot code, keeping the partition table")
    m.add_argument("image")
    m.add_argument("binary")
    m.set_defaults(func=cmd_mbr)

    v = sub.add_parser("vbr", help="install a volume boot record, keeping the BPB")
    v.add_argument("image")
    v.add_argument("binary")
    v.add_argument("--start", type=int, default=2048)
    v.set_defaults(func=cmd_vbr)

    t = sub.add_parser("put", help="write a file into the FAT32 root directory")
    t.add_argument("image")
    t.add_argument("source")
    t.add_argument("name", help="8.3 name, e.g. BOOTMGR")
    t.add_argument("--start", type=int, default=2048)
    t.set_defaults(func=cmd_put)

    r = sub.add_parser("run", help="boot the image and print the screen")
    r.add_argument("image")
    r.add_argument("--expect", help="text that must appear; sets the exit status")
    r.add_argument("--timeout", type=int, default=25)
    r.add_argument("--settle", type=float, default=0.0,
                   help="keep reading this many seconds after --expect appears")
    r.set_defaults(func=cmd_run)

    s = sub.add_parser("selftest", help="prove the harness detects pass AND fail")
    s.add_argument("--keep", action="store_true")
    s.add_argument("--timeout", type=int, default=60,
                   help="generous: a cold TCG boot in CI is far slower than a warm one, "
                        "and a passing case stops as soon as it sees its marker")
    s.set_defaults(func=cmd_selftest)

    a = p.parse_args()
    sys.exit(a.func(a) or 0)


if __name__ == "__main__":
    main()
