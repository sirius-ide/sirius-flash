# Sirius Flash — Project State

Living handoff document. Anyone (or any new session) picking this up cold should read
this file first, then `README.md`, then `SECURITY.md`.

**Last updated:** 2026-10-22, second checkpoint of the day. For the commit this describes, ask git —
`git log -1 --format=%h PROJECT-STATE.md`. A hash written into the file by hand
names the commit *before* the one containing it, and this one had drifted five
commits before anyone noticed.

---

## 0. Resume here — checkpoint 2026-10-22 (evening)

Branch **`main`**. The units below were each committed separately, in this order: the
dependency bump (`80c0155`), zip integrity (`8d34a85`), the MBR (`f2e3fe6`), the FAT32
VBR (`241f6b5`), the Tauri 2.12 set (`5f17de3`), four rebased Dependabot merges, the
app's sha2 alignment (`7f4d15f`), this checkpoint. Cloud sessions work on a branch
(`CLAUDE.md`).

- **Verified, not just written:**
  - **`Security` is green**: `zip 9.0.5`, `ruzstd 0.9.1`, both lockfiles audited clean. zip 9
    opens an archive by reading it *end to end* unless told where it starts; we tell it
    (`ArchiveOffset::Known(0)`): a 2 GiB zip went from ~2 s and 2.1 million syscalls to
    0.05 ms and 17, measured. A zip member's CRC-32 is now verified and a damaged image
    member is refused rather than replaced by the next-largest file — both checked against
    a corpus of ~7,000 real and mutated archives (§8 has the numbers).
  - **`crates/core/boot/mbr.asm`** (§6): CHS fallback computed from INT 13h AH=08h, DL
    retried once as 0x80 only when the named drive is absent or a floppy number, LBA-0
    entries refused, two silent-loop paths closed. Selftest cases 5-15 under SeaBIOS; every
    new case was seen to fail on a broken copy. **Never on real hardware.**
  - **`crates/core/boot/fat32.asm`** (§6): a one-sector FAT32 VBR that loads BOOTMGR. Cases
    16-20: tidy, hostile (label spelled BOOTMGR, a deleted entry, a near miss, a long-name
    part, root directory across three clusters, 925 scattered clusters), 32 KiB clusters,
    missing file, a 1.2 MB file that must stop loading short of the EBDA. The **real Windows 11
    `bootmgr`** loaded by it reaches "Windows Boot Manager … \Boot\BCD … 0xc000000f", the
    right complaint on a volume with no BCD. **Never on real hardware.**
  - **Tauri 2.12** (tauri 2.12.5, plugins 2.8.3 / 2.7.2, `@tauri-apps/*` 2.12.4, vite 8.3.5):
    `tauri build --no-bundle` passes and the binary runs under Xvfb with `GDK_BACKEND=x11`.
    Closes GHSA-w28w-mhc8-qvjv (tauri ≤ 2.11.5, high, invisible to `cargo audit`) and the
    three vite alerts.
- **Written, not wired:** nothing in Rust references `mbr.bin` or `fat32.bin`; `buildable()`
  still refuses every BIOS target. That is §6 step 3, and it is next.
- **Do first:** `gh pr list` — one routine lockfile bump (#28, liblzma 0.4.9) may still be
  open (§8); merge it when green. Then §6 step 3.
- **Before trusting this section:** `git fetch`, `gh run list` (CI *and* Security), `gh pr list`.

## 1. What this is

A cross-platform bootable-USB creator meant to fully replace Rufus, Etcher, WoeUSB-ng
and `dd` on **Linux, macOS and Windows**. GPL-3.0, © Clicksora L.L.C. Companion product
to Sirius IDE, offered alongside it.

Repo: `github.com/sirius-ide/sirius-flash` · Local: `~/Projects/sirius-flash`

**Goal, stated by the product owner:** *"we want our product to be totally replaced by
users for rufus and any other flashing tool for any os out there"* and *"we want to beat
our competitors on all fronts."* Feature parity with Rufus is the floor, not the target.

## 1b. The family

Sirius Flash is the companion product to **Sirius IDE** — same owner, same GitHub org
(`sirius-ide`), same copyright holder (Clicksora, L.L.C.), same Cloudflare-first
infrastructure. They are offered together.

| | Sirius IDE | Sirius Flash |
|---|---|---|
| What | Agentic AI editor, a Code - OSS (VS Code) fork | Bootable-USB creator |
| Repo | `~/Projects/sirius` (branch `sirius`) | `~/Projects/sirius-flash` (branch `main`) |
| Licence | Proprietary (`LicenseRef-Sirius`) | **GPL-3.0** — Rufus is GPL-3.0 and we adapt from it |
| Status | Shipping, `v1.118.4`, live update server | Early development, Linux core first |
| State doc | `PROJECT-STATE.md` at its repo root | this file |

Shared infrastructure: `dl.siriuside.com` (CDN, Cloudflare R2), Pages for the landing
site, Workers for update endpoints, GitHub Actions for CI.
Logo and icon artwork is generated with Gemini Nano Banana and wired in by hand.

The licences differ deliberately and must not be mixed: **no Rufus-derived code may enter
the proprietary IDE repo**, and Flash must stay GPL-3.0 for as long as it adapts from Rufus.

## 2. Layout

```
crates/core/src/lib.rs      device discovery, safety gate, ISO detection,
                            Windows tweaks + autounattend.xml, both flashers
crates/core/src/blockio.rs  streaming I/O: progress, SHA-256, verification,
                            format detection + decoding, recursive tree copy
crates/core/src/format.rs   format options: scheme/target/filesystem/cluster/label,
                            and the rules that make an illegal combination unusable
crates/core/src/lzw.rs      streaming Unix compress (.Z) decoder
crates/core/fixtures/       real binary test archives, with a README on provenance
crates/core/boot/mbr.asm    the MBR: chainloads the active partition (LBA, CHS from the
                            BIOS geometry, a bounded DL retry); mbr.bin beside it is what
                            it assembles to, checked by the selftest and CI
crates/core/boot/fat32.asm  the FAT32 volume boot record: finds and loads BOOTMGR in one
                            sector; fat32.bin likewise
docs/research/              research notes that outlive a session (the Rufus survey)
crates/cli/src/main.rs      `sirius-flash list | write | unattend`
scripts/boottest.py         boot-test harness: build media in a plain file,
                            install boot code, boot it under qemu, read the screen
scripts/boot/marker.asm     a known-good MBR, so the harness can prove it
                            detects a working boot sector as well as a dead one
scripts/boot/marker_vbr.asm a VBR that checks the MBR's handover contract and
                            names the first part of it that is wrong
scripts/boot/marker_bootmgr.asm
                            a stand-in BOOTMGR that checks the VBR's entry contract and
                            that every one of its sectors arrived, in order
app/src-tauri/src/lib.rs    Tauri commands; spawns the CLI under pkexec
app/src/main.ts             frontend logic
app/index.html              UI (cards ①-④)
scripts/make-win11-usb.sh   the original shell workflow this productizes
```

Cargo workspace **excludes** `app/` (the Tauri app has its own lockfile; CI audits both).

## 3. Done

- [x] Safe device enumeration — removable + size window + `/dev/disk/by-id` only
- [x] UDF-aware ISO detection (see invariant 4 below)
- [x] Windows flasher: GPT + FAT32, `install.wim` **or** `.esd`, split only when oversized
- [x] Linux/raw flasher: in-process streaming write, no `dd`
- [x] SHA-256 computed while writing + read-back verification (Rufus has no equivalent)
- [x] Image formats — raw, gzip, xz, zstd, bzip2, lzma, `.Z`, zip, fixed `.vhd` —
      detected by content, streamed, and decoded through **every** stream of a
      concatenated archive; a zip member's CRC-32 is verified like every other format's
      own checksum, and a damaged image member is refused rather than replaced by a sibling
- [x] BIOS boot sectors, written here and boot-tested under qemu: an MBR that chainloads
      the active partition (LBA, CHS from the BIOS geometry, a bounded DL retry) and a
      one-sector FAT32 VBR that loads the real Windows 11 `bootmgr` — **not yet wired into
      the flasher** (§6 step 3)
- [x] Recursive tree copy in-process, no `rsync`
- [x] Windows 11 User Experience: bypass TPM / Secure Boot / RAM / CPU / storage
      (5 bypasses; Rufus offers 3 — and removed its storage one as "a bogus bypass that
      doesn't do anything", so ours needs an on-device test before it stays, §8), skip
      Microsoft account, local admin account, disable data collection, prevent BitLocker
      auto-encryption, debloat
- [x] Live progress: percent, bytes, rate, ETA — CLI and GUI
- [x] Tauri GUI, dark themed, with an `autounattend.xml` preview
- [x] CI green on Ubuntu / macOS / Windows: fmt, `clippy -D warnings`, tests, release build
- [x] Security: `cargo audit` on both lockfiles, Dependabot, secret scanning, push protection

**113 tests** (24 in `lib.rs`, 47 in `blockio.rs`, 12 in `lzw.rs`, 25 in `format.rs`,
5 in the CLI); **98 of 108 passed on a non-Linux target** when last measured, before the
five zip tests were added — that count is the working proxy
for how much of the core is ready for the macOS backend, and it is measured, not estimated:
copy the tree, `sed` the `target_os` guards, and run the suite (see §9).

Binary test fixtures live in `crates/core/fixtures/` with a README saying how each was
produced. Everything there is a **real** archive, cross-checked against an independent
implementation (GNU `gzip` for `.Z`, `qemu-img` for `.vhd`); inline byte arrays in the
test modules are real archive bytes too, never hand-assembled.

## 4. Invariants — do not regress these

Each of these is a bug that already shipped once and was fixed. Re-introducing one is a
data-loss or dead-stick regression.

1. **Probe before you destroy.** `flash_windows_iso` mounts and inspects the ISO *before*
   touching the target. An earlier version wiped the drive and only then discovered the
   image held `install.esd` rather than `install.wim`.
2. **Never trust kernel device names.** `sdb` / `nvme0n1` are unstable across reboots and
   re-plugs. Target only the `/dev/disk/by-id` serial path.
3. **Accept both `install.wim` and `install.esd`** — `pick_install_image()` is the single
   place that decides, and detection and flashing must agree.
4. **`detect_iso_kind` must never conclude "Other" from a stub.** Modern Windows ISOs are
   UDF with a decoy ISO9660 layer containing only `README.TXT`. `bsdtar` reads ISO9660
   only; `7z` and `blkid` read UDF. Detection is layered: bsdtar → 7z → `blkid TYPE=udf`
   (⇒ Windows) → readable (⇒ Other) → filename. Skipping a layer breaks Win11 ISOs.
5. **Stream, never buffer.** Images run to 8 GB+. Everything is a `Read` chained into a
   4 MiB loop.
6. **Drain stdout and stderr concurrently.** Pipes hold 64 KiB; draining one to EOF first
   deadlocks the child (observed: an 18.5-minute hang).
7. **Split log lines on `\r` as well as `\n`.** Progress records are carriage-returned;
   `BufReader::lines()` alone makes the UI look frozen.
8. **Cfg-gate Linux-only imports.** `clippy -D warnings` turns an unused import into a
   hard CI failure on macOS and Windows.
9. **FAT32 caps a file at 4 GiB − 1.** Split with `wimlib-imagex` only past that; splitting
   unconditionally wastes time and loses a plain-copy fast path.
10. **Decode every stream, not just the first.** Concatenating two archives is valid in
    gzip, xz, bzip2 and zstd, and every reference tool unpacks the lot. Stopping after the
    first stream wrote half an image, reported success, and then *passed* read-back
    verification — the digest covers whatever was written, so verification cannot catch a
    truncated decode. Hence `MultiGzDecoder`, `XzDecoder::new_multi_decoder`,
    `MultiBzDecoder` and `ZstdFrames`.
11. **`Compression::None` means "write this file to the device verbatim".** So a *failure*
    to detect a format is not a harmless skip — it dd's the archive onto the drive and
    certifies it. This cuts both ways, and it is why `.lzma` is settled by a bounded trial
    decode rather than by tightening the header test: LZMA-alone has no magic number, and
    every stricter header rule rejected lawful archives (`xz` emits 3, 6 and 12 MiB
    dictionaries; the reference SDK rounds to whole mebibytes).
12. **Sizes an archive declares about itself are claims, not facts.** A zip member
    declaring 4 KiB can expand to gigabytes, and deflate reaches ~1000:1. Bound the decoded
    output to the declared length and treat both overrun and shortfall as errors.
13. **An option the user set must reach the tool that implements it, or be refused.**
    `--cluster-size` was parsed, validated against the mask, resolved into the plan and
    printed in the preflight — and then never passed to `mkfs.ntfs`, which defaults to 4096
    whatever was asked for. Every layer reported success and the volume came out wrong.
    Accepting an option and dropping it is worse than not offering it: the user's check is
    that the tool echoed their choice back. Note the two formatters disagree on units —
    `mkfs.fat -s` counts *sectors*, `mkfs.ntfs -c` counts *bytes*.
14. **A warning after the decision is not a warning.** Fixed once in the CLI, where the
    UEFI:NTFS caveat printed below the `Type YES` prompt; it then shipped again in the GUI,
    which passes `--yes`, so the CLI's copy reached the log pane only once the drive was
    already being repartitioned. Anything the user might act on belongs in the confirm
    dialog, and its text comes from `WindowsLayout::caveat()` so the two cannot drift.
15. **Every decoder verifies the archive's own checksum, and a zip member's is in the
    index.** gzip, xz, bzip2 and zstd carry one their decoder checks at the end of the
    stream; a stored zip member is copied verbatim and a deflated one has no trailer, so
    for years a flipped bit, or an index understating a member's size in both fields,
    wrote a wrong image that read-back verification then *certified*. The central
    directory's CRC-32 is checked when the member ends. In the same breath: tolerating a
    damaged *sibling* must never extend to the image itself — the old selection skipped any
    member it could not locate and so wrote the 35-byte README beside a damaged image as
    the image, verified, and reported done.

## 5. Conventions

- **No personal identifiers anywhere in the repo** — no real names, usernames, home paths
  or hardware serials. Use `TestUser` in tests; make device paths required env vars in
  scripts.
- **No attribution or co-author lines** in commit messages or PR descriptions.
- Commits are Conventional Commits (`feat(core):`, `chore(security):`, …).
- Rufus is GPL-3.0 and so are we — studying and adapting it is legal, and credited in
  `README.md`. Keep crediting it.
- **Boot code: seen to fail before believed.** A new selftest case is not trusted until it
  has gone red on a copy of the tree with one bug put in; the repo's sources are never
  edited to do it. Test-only paths are reached through assemble-time symbols (`-D`), the
  shipped binary is the plain build, and a hook build identical to the plain one is
  reported as dead. The committed `.bin` beside each `.asm` is what the source assembles
  to at nasm's default optimisation; the selftest and CI both diff it.

## 6. Current phase — format options + UEFI:NTFS

Format coverage against Rufus is **done**; what follows is the record of what landed and
what was deliberately left out.

### Covered

`raw` · `.gz` · `.xz` · `.zst` · `.bz2` · `.lzma` · `.Z` · `.zip` · fixed `.vhd`

### Deliberately not covered

| Format | Why |
|---|---|
| `.vhd` dynamic / differencing | needs BAT parsing; **refused with an explanation**, not written wrongly |
| `.vhdx` | a different container entirely |
| `.ffu` | Microsoft full-flash update format |
| `.7z` | large dependency for marginal gain |
| `.vtsi` | Rufus-proprietary (VMware/ThinkPad service images), no public spec — **said openly in `README.md`** rather than silently omitted |
| zip with an SFX stub | prepended junk moves the header off offset 0; a tail probe would risk taking a raw image that merely contains a zip for one |

### Notes for whoever picks this up

- `bzip2-rs` was swapped for the `bzip2` crate. Still pure Rust — its default backend is
  `libbz2-rs-sys`, a Rust reimplementation, not the C library — and it has the
  multi-stream reader `bzip2-rs` lacks and cannot be given, because it buffers past the
  end of a stream.
- The `.Z` decoder is hand-written in `crates/core/src/lzw.rs`. The only pure-Rust crate
  for the format is a single 0.1.0 with no version history, and it decodes into a `Vec`,
  which breaks invariant 5 outright. The two traps are that the encoder pads to groups of
  eight codes on every width change, and that it checks whether to widen *before*
  extending the dictionary — one more code goes out at the old width than the obvious
  reading suggests.
- `zip` is pinned to `default-features = false, features = ["deflate-flate2"]`. The
  umbrella `deflate` feature adds zopfli (a compressor) and switches flate2's backend for
  the whole workspace; the default features add ~40 crates including `zstd-sys`, which
  needs a C toolchain on all three platforms.

### Reference: what Rufus accepts

Rufus's authoritative accepted-format list (`src/rufus.c:2642`):

```
*.iso *.img *.vhd *.vhdx *.usb *.bz2 *.bzip2 *.gz *.lzma *.xz *.Z *.zip *.zst *.wic *.wim *.esd *.vtsi
```

Its `bled` decompression library (`src/bled/bled.h:25-35`) covers:
NONE, ZIP, LZW (`.Z`), GZIP, LZMA, BZIP2, XZ, 7ZIP, VTSI, ZSTD.

We now meet or beat that everywhere except the four container formats listed above, and
`.vtsi`. Two deliberate divergences from Rufus, both improvements:

- **zip**: bled writes the *first* member; we write the largest by uncompressed size.
  On an archive whose image sits second, the first-member rule writes the README.
- **detection**: Rufus dispatches on the file *extension*; we detect by content, because
  plenty of images are served as `ubuntu.iso` while actually being gzip.

A new decoder follows the pattern in `blockio.rs`: extend the `Compression` enum, add
magic bytes to `sniff()` (or, for a format without a magic number, a positive test in
`detect_compression`), wire a streaming reader into `open_image()`, and test against a
real fixture archive — never a hand-built byte string.

**Done:** the format-option *model* — `crates/core/src/format.rs`. A `FormatRequest` is
what the user asked for and may be nonsense; only `validate()` produces a `FormatPlan`, and
the flasher will take a plan, so skipping the check is a compile error. Two structural
tests keep it honest: everything the query functions advertise to a GUI must validate and
nothing else may, and the advertised sets were run through real `mkfs` (147 plans, all
accepted).

Two findings worth keeping:

- **MBR above 2 TiB warns, it does not become illegal.** Refusing looks right and is
  wrong — GPT cannot boot a legacy BIOS either, so rejecting MBR there leaves a BIOS-only
  image with no legal scheme at all. Rufus only moves the default and asks.
- **Reimplement Rufus's cluster computation, not the published Microsoft table.** They
  disagree: on a 256 MB volume the FAT32 default is 512 B, because a default the mask has
  just excluded is reset to the smallest one still allowed.

**The model describes more than this build can make, on purpose.** `needs_boot_code()`
marks the difference and the flasher must honour it:

- A **UEFI** target needs no boot code. The firmware reads FAT itself and loads
  `\EFI\BOOT\BOOTX64.EFI` off the volume, which is exactly why the existing GPT+FAT32
  Windows path works without writing a single byte of bootstrap.
- A **BIOS** target needs an MBR bootstrap *and* a partition boot record for the
  filesystem. Rufus carries `src/ms-sys/` for this (`write_win7_mbr`, `write_fat_32_br`,
  `write_ntfs_br`). **We write none of it.** Formatting for BIOS would succeed and hand the
  user a drive that silently does not boot — so it must be refused, not attempted.

`sirius-flash format-options` prints the legal sets for a drive (or a hypothetical
`--size-gb`) and writes nothing, and `write` takes `--label`, `--cluster-size` and
`--full-format`. `flash_windows_iso` now takes a validated plan, resolved and checked
*before* its "everything from here on is destructive" line, and `assert_buildable` refuses
anything outside GPT + UEFI + FAT32 by name.

**`--full-format` passes `-c` to `mkfs.fat`**: a read-only surface scan that marks
unreadable sectors bad. It finds a *dying* stick. It is **not** a counterfeit check — a
fake-capacity stick's unwritten sectors read back fine and its writes wrap silently, so
catching one needs write-and-read-back. The read-back verification after an image is
written is what actually does that, and it already runs by default.

**Both open decisions are settled, and implemented.**

### UEFI:NTFS — we match Rufus's default, and say what it costs

`WindowsLayout::{Fat32Split, NtfsUefiNtfs}`, defaulting on Rufus's own trigger: the size of
the largest file. Reading the source settled what Rufus actually does, and it was the
opposite of what this document previously assumed — for an image with a file over 4 GiB,
`SetAllowedFileSystems` (`rufus.c:190-207`) removes FAT32 from the dropdown *entirely*,
leaving NTFS, and `format.c:1482` then adds the loader partition off the filesystem alone.
Rufus does not split by default; FAT32+split lives behind the undocumented Alt-E, and its
changelog calls splitting "WAY SLOWER than using UEFI:NTFS".

**Where we differ is the warning.** Rufus shows none: `MSG_129`, the one string that ever
mentioned this, is dead code — retired in 3.17 when the bootloader became signed and never
replaced. `WindowsLayout::caveat()` names the third-party-CA dependency, quotes what the
machine will actually say if it is missing ("No bootable option or device was found"), and
names the way out — and the CLI prints it **before** the confirmation prompt, because after
it the warning is worthless.

`uefi-ntfs.img` is vendored in `crates/core/assets/` because it cannot be built: its value
is a Microsoft signature. Pinned by SHA-256 and checked in the preflight *and* at the write.

### Boot sectors — MBR and FAT32 VBR written and boot-tested; wiring is next

Both sectors exist, assemble reproducibly (nasm 2.16.01 in an Ubuntu 24.04 container and
nasm 3.02 here agree byte for byte, at the default optimisation level), and are committed
as binaries beside their sources — `mbr.bin` (440 bytes, 421 used) and `fat32.bin` (512
bytes, 416 of the 420 code bytes used) — so that core can `include_bytes!` them without
putting an assembler on the macOS and Windows build path. The selftest and a CI step both
reassemble and compare; a stale binary fails the build. Nothing in Rust references either
yet, and `buildable()` still refuses every BIOS target.

**The harness grew to 20 cases.** `boot_and_read` takes several expected strings and a
`floppy` mode (the image as drive A, no hard disk at all); `put` can scatter a file's
clusters across the FAT in descending order, grow the root directory, write a long-name
part with a correct checksum, and plant decoy entries. Every case added this session was
seen to fail on a copy of the tree with one bug put in — the repo's sources were never
touched to do it — and the forced-CHS and wrong-DL cases also fail if their assemble-time
hooks stop assembling anything, because a hook build identical to the shipped one is
reported as dead.

**`mbr.asm` — the three open assumptions, settled:**

- **CHS fallback: kept, computed from the BIOS.** The partition entry's CHS fields are
  filled by partitioners from an assumed 255-head geometry; SeaBIOS gives a 512 MiB disk
  32 heads, so the old fallback read a head that did not exist. The MBR now asks AH=08h and
  converts the start LBA against what it reports — which is what syslinux's and GRUB's MBRs
  do; ReactOS, FreeBSD, Rufus and Windows 7 all use the partition-entry fields, and Rufus's
  own log records that failing on a Dell. No report of a BIOS booting USB as a hard disk
  without INT 13h extensions newer than 2010 was found, so the path may never run; it costs
  80 bytes, guards sectors-per-track 0 and cylinders past 1023 (a wrapped cylinder reads a
  real, wrong sector — LBA 0, this MBR, forever), and falls back after a *failed* AH=42h as
  well, as GRUB does. The VBR research agent argued the fallback is moot because our VBR
  is LBA-only; it stays because the MBR is VBR-agnostic and the syslinux and GRUB boot
  sectors we may chainload for Linux media have CHS paths of their own.
- **DL: trusted, retried once.** The Windows 7 pattern, narrowed: after a failed read the
  MBR tries drive 0x80 once — but only when the drive the BIOS named does not exist (AH=08h
  fails) or is a floppy number (below 0x80; GRUB's documented firmware bug). A hard disk that
  exists and refuses a read is reported as a read error, because the review showed the wider
  rule booting the *other* disk's sector at the same LBA when a genuine 0x81 had a bad
  sector. Nothing records that a substitution happened; the VBR sees DL=0x80.
- **Two active partitions: still refused.** Correction to the earlier record: ReactOS's MBR
  does *not* refuse (it boots the first 0x80), and neither does Windows 7's; syslinux and
  Windows XP do. Media this project writes has exactly one active entry, so the strict
  reading costs nothing and catches a table that is not ours.

Three more things the adversarial review found, all fixed: an active entry at **LBA 0**
read this sector, verified it, and jumped to it forever (now "bad partition table"); a BIOS
that returns CF=0 having transferred nothing would **verify the 0x55AA the BIOS loaded us
with** and run this sector again (the word is cleared before every read); and EDD writes the
**count transferred back into the packet** (0 after a failure), which a retry then reused
(re-armed before every read). The review's own mutation table drove cases 12-15: forced CHS
at LBA 518434 (cylinder 257, head 5 — the first case to exercise cylinder bits 9:8 and a
non-zero head), a partition at cylinder 1024 on an 8 GiB disk (LBA boots it, forced CHS
must say "read error"), an entry past the end of the disk, and a floppy boot with no hard
disk (the only machine on which the retry's give-up guard can be reached).

**`fat32.asm` — one stage, by evidence.** Microsoft's code runs from sector 0 *and*
reserved sector 12 (see the corrected blob facts under "The Rufus source"); FreeDOS's
`boot32lb.asm` and the KolibriOS sector each load a kernel from a FAT32 root in one sector;
a measurement prototype did the full honest walk in 350 code bytes. Ours is 360 + 56 of
text, and walks the whole root chain, every sector of every cluster, every 32-byte entry,
and the whole file chain from the first FAT with one FAT sector cached.

What the real BOOTMGR needs, learnt by loading the Windows 11 one under qemu and taking
things away until it broke — and what the stand-in `marker_bootmgr.asm` therefore checks:

- the **whole file** at linear 0x20000, entered by `jmp 2000:0000` (115 of 116 clusters
  gets "BOOTMGR image is corrupt"; it does not re-read itself);
- this boot sector still at 0:7C00 with **bytes-per-sector (0x0B) and hidden sectors
  (0x1C) intact** — it reads both from memory;
- its drive number in **BPB byte 0x40**: it ignores DL and reads that field, which is why
  Rufus patches 0x80 into it at install time. We store the DL we were given there, so a
  stick booted as 0x81 works.
- Nothing else: DS, ES, SS:SP, SI, BP are set up by its own 16-bit stub, which also raises
  A20 itself.

Design choices worth knowing: the file is loaded by its **directory-entry size**, one sector
per INT 13h call, not by whole clusters — with 64 KiB clusters the last whole cluster of a
473 KB BOOTMGR would end at 0xA0000, on top of the EBDA (Microsoft's loader has that
exposure) — and the load **stops at linear 0x9F000 whatever the size field says**, because
past there lie the EBDA, video memory and, after the segment wraps at 1 MiB, the interrupt
table and the loader itself; the first cluster is read before its chain is consulted, so an
entry whose first cluster is an end-of-chain mark cannot jump with nothing loaded; the
packet is dropped with `add sp,16`, not through SI, in case a BIOS returns with SI changed;
**DS:SI from the MBR is not consulted** to "correct" hidden sectors, because a foreign MBR
may pass anything there and the installer guarantees the field; 512-byte sectors are assumed
and the installer refuses anything else; a volume label is a legal way to put the 11 bytes
`BOOTMGR    ` ahead of the file (a *directory* of that name beside it is a duplicate entry
no real volume can hold — `fsck.fat` said so when the hostile case first tried it), and the
attribute test skips labels, directories and long-name parts alike while matching the 0x27
(read-only, hidden, system, archive) that Windows gives `bootmgr`. The messages are
"Sirius: BOOTMGR not found" and "Sirius: disk error".

The adversarial review of this unit found no boot-stopping defect and four test gaps, all
closed: the hostile case's "deleted BOOTMGR" did not exist (the harness's `put` reused 0xE5
slots, so the next decoy overwrote it — it no longer does, and a loader that takes 0xE5 as
end-of-directory now fails); no case wrote BOOTMGR with Windows' attribute 0x27 (a loader
matching only 0x20 passed); the DL store into BPB byte 0x40 was invisible because `mkfs.fat`
had already put 0x80 there (case 16 now blanks the byte in both boot sectors first); and a
loader that read whole clusters, or one sector too many, passed (the harness now pads the
last cluster with 0xA5 and the stand-in checks that the memory after the file is untouched).
It also showed that the load cap only has a visible effect once the segment wraps — writes
into video memory and ROM below 1 MiB do not stick — which is why case 20's file is 1.2 MB
and not 600 KB.

**The next action: step 3, wire both into the flasher and open the gate.** What the
installer must do, all of it already enforced by `boottest.py vbr` and worth copying
field-for-field: refuse a volume whose bytes-per-sector is not 512 or whose hidden-sectors
field is not the partition's start LBA; splice bytes 0x0B-0x59 of the formatted volume into
`fat32.bin` and write the result to the partition's first sector **and** to the backup at
`BkBootSec` (sector 6 under `mkfs.fat`; a stale backup makes every `fsck` report
differences and a repair restores the old code); write `mbr.bin` as 440 bytes so the disk
identifier and the partition table survive; mark exactly one partition active, type 0x0C.
`format.rs:323` `buildable()` is the single place that refuses BIOS, and its message names
the reason — it must stop being true before it stops being returned. Then the first real
hardware boot: everything above ran under SeaBIOS on an emulated IDE disk, and the
assumptions that only real firmware can test are DL, AH=08h geometry on USB-HDD emulation,
and whether any BIOS still lacks packet reads.

### The Rufus source: where it is and how to read it

Every `rufus.c:NNN` citation in this file points into a checkout that lives in a session
scratchpad and **does not survive**. Recreate it with

```bash
git clone --depth 50 https://github.com/pbatard/rufus
```

All six cited locations (`rufus.c:190-207`, `:450-612`, `:2642`; `format.c:284-288`, `:1482`;
`bled/bled.h:25-35`) were re-verified against upstream `942ed3a` (2026-09-21). If they drift,
`git log -S` on the quoted identifier finds them again.

**The boot-sector blobs in `src/ms-sys/inc/` are C arrays of hex bytes.** A text grep for
`"BOOTMGR is missing"` returns nothing and proves nothing — decode `0x..` tokens to bytes
first, then search. Done that way, the provenance record below holds exactly: the string is
in `br_fat32pe_0x52.h`, which carries **no header comment at all**, while `mbr_rufus.h`
opens with `Copyright © 2012-2014 Pete Batard`.

Three things the decode showed that matter for the work ahead:

- **Microsoft's FAT32 boot code runs from sector 0 and reserved sector 12.** Corrected
  from an earlier reading of the blobs: `_0x52` is 923 bytes (an earlier decode matched the
  "0x52" in the array *name* and came out one byte long — restrict the decode to the braces)
  and covers the sector-0 tail plus an FSInfo template; `_0x3f0` is the FSInfo trailer plus
  a sector 2 that nothing executes; `_0x1800` is sector 12, where the root scan, the FAT walk
  and the `jmp 2000:0` live. Sector 0 is 271 code bytes, sector 12 327, about 96 of them a
  CHS path. Rufus writes the set at sectors 0-2 and 12 and again 6 sectors on. Ours fits in
  one sector because it carries neither CHS nor active-FAT selection.
- **Refusing two active partitions is syslinux's and Windows XP's rule, not everyone's.**
  `mbr_syslinux.h` and `mbr_gpt_syslinux.h` carry `Multiple active partitions`;
  `mbr_reactos.h` carries `no active partition found`, `read error while reading drive`
  and `partition signature != 55AA` — the same three failures ours reports — but its
  source (`dosmbr.S`, ported from FreeDOS fdisk) boots the *first* 0x80 it finds, as does
  Windows 7's. We refuse anyway; see §6.
- **Rufus's own MBR waits for a keypress** (`Press any key to boot from USB.`) and
  otherwise falls through to the next drive. That is a UX choice for a *dual-purpose*
  stick, not a correctness requirement, and it is not ours to copy — but know it exists
  when comparing behaviour on real hardware.

**A wider survey of the Rufus tree** — resources, device safety, image analysis, the
Windows customisation, revocation checks, the write loop, the dialogs — is in
`docs/research/rufus-survey-2026-10-22.md`, with a consolidated gap table and a ranked list
of what to adopt and what not to copy. Four of its findings corrected this file or the
README: Rufus has no read-back (and Etcher does, so the README no longer says otherwise);
our vendored `uefi-ntfs.img` is byte-identical to Rufus's current one; Rufus removed its
storage bypass as a no-op; its FAT32 blobs are laid out as described above.

### Why we write our own rather than lifting them

`ms-sys` is GPL-2.0-or-later, so its *logic* is usable. Its blobs are not the same question:
`br_fat32pe_0x52.h` is Microsoft's boot record (it contains "BOOTMGR is missing"), and the
GPL header covers Henrik Carlqvist's code, not Microsoft's bytes. `mbr_rufus.h` is pbatard's
own and is clean.

So: write our own. An MBR that chainloads the active partition is ~100 bytes of assembly, a
FAT32 boot record that loads `BOOTMGR` ~400; `nasm` is installed, the structures are
documented, and the harness below tells us in seconds whether it boots. That avoids a legal
question nobody can answer confidently, and we would have had to understand the blobs
field-by-field to patch their BPBs anyway.

### Boot testing: possible here, today, with no root and no mtools

An earlier version of this section said we could not boot-test locally, because loop
devices need root and `mtools` is absent. **That was wrong**, and the mistake mattered —
it was being used to argue boot-sector work could not be verified.

The blocker is removed by one flag:

| tool | role |
|---|---|
| `sfdisk` | partitions a **plain file**, unprivileged — no loop device |
| `mkfs.fat --offset=SECTOR` | creates FAT32 *inside* the image at the partition offset. **This is what loop devices were needed for.** `-h`, `-s`, `-R`, `-b`, `-D` set every BPB field boot code reads |
| `python3` | ~40 lines writes a file into the FAT (BPB, FAT chain in both copies, 8.3 root entry, cluster data) — replaces `mcopy` |
| `qemu-system-x86_64` + SeaBIOS | boots it under TCG, so no KVM and no root; works in a CI container |
| `nasm` / `ndisasm` | build boot sectors, and disassemble Rufus's blobs to compare |

Output is read without a display by dumping the VGA text buffer from the qemu monitor —
`memsave 0xb8000 4000` — and decoding the 80x25 char/attr pairs. Deterministic and
greppable; no screenshots.

**This is built: `scripts/boottest.py`.** `build` makes the media, `mbr` and `vbr` install
boot code, `put` writes a file into the FAT32 volume, `run` boots it and prints the screen.
`selftest` runs the whole thing in about eight seconds and is a `boot` job in CI.

The selftest is a **negative** test before it is a positive one. It asserts that media with
no boot code is reported as dead — SeaBIOS reaches `Booting from Hard Disk...` and then
nothing, the silent dead stick §4 warns about — and only then that a known-good sector is
reported as alive. A rig that has only ever seen a pass is indistinguishable from one that
always passes, so both directions were confirmed by breaking the marker and watching the
selftest exit 1.

Five traps, each of which silently produces media that looks fine:

- **`mkfs.fat` does not derive hidden sectors from `--offset`.** Without `-h`, the BPB says
  the volume starts at LBA 0 and every absolute read a boot sector computes lands 2048
  sectors early. `build` sets it and `Bpb.check` refuses media where it disagrees.
- **`BLOCK-COUNT` is in 1024-byte blocks, not sectors.** Passing the sector count builds a
  filesystem twice the size of its partition, which mounts happily until something reads
  past the end.
- **`mkfs.fat -F 32` will build a volume below FAT32's 65525-cluster floor** without saying
  so; only `fsck.fat` mentions it afterwards. Hence a 512 MiB default and an explicit check.
- **qemu's `memsave` rejects an absolute path**, because the HMP parser reads a leading `/`
  as a format specifier: it fails with `invalid char 't' in expression`, writes nothing, and
  leaves qemu's exit status untouched. The harness runs qemu with its cwd set and passes a
  bare filename.
- **Reading a volume back with the code that wrote it proves nothing.** A directory entry
  one byte too long reported a 30000-byte file as 7680000; the harness's own parser agreed
  with itself, and `fsck.fat` and `7z` are what caught it. Case 3 of the selftest keeps
  `fsck.fat` in the loop.

Two deliberate choices about geometry: the default is 8 sectors per cluster rather than the
1 `mkfs.fat` picks, because a 1-sector cluster is the one case where cluster-to-LBA
arithmetic is the identity and a boot sector that multiplies by the wrong thing still
passes; and `mbr` installs 440 bytes, not 446 or 512, so the disk identifier and the
partition table survive.

So testability is **not** a reason to defer boot-sector work. The reasons that remain are
the provenance question above and the fact that the blobs need per-volume BPB patching
rather than being copied verbatim.

## 7. Backlog after that

Wire the boot sectors (§6 step 3) · Built-in ISO downloader (Linux catalogue first, then
Windows) · macOS backend · Windows backend · signed CI releases → `dl.siriuside.com` +
AUR · persistent-partition support · bad-block check.

From the Rufus survey (`docs/research/rufus-survey-2026-10-22.md`), in its priority order:
one typed `ImageReport` from an in-process ISO9660/UDF reader gating every destructive
step (truncated-ISO check, projected size, image-on-target refusal, bootloader inventory,
Windows build from the WIM XML — and the exact largest-file size the GUI cannot get today);
UEFI bootloader revocation (DBX hash, SBAT, SVN) as a warning; the safety rails Rufus has
and we lack (source-on-target, multi-partition and 4Kn confirmations from one source of
truth, naming the process holding the device, a user ignore-list); a quick fake-capacity
test with block-id stamps, then the full destructive pattern test; head-*and*-tail wipe
plus per-partition head zeroing; save-drive-to-image and zero-drive; a hash tool that
compares; signed assets and updates with a rollback guard; Windows 11 customisation
parity *after* testing which bypasses do anything; ISO mode for Linux as an opt-in. And
five things not to copy: hidden cheat-mode toggles for safety gates, index-based disk
identity, running a downloaded script to fetch ISOs, ISO-extract as the default for
hybrid ISOs, and the legacy/packer tail.

## 8. Open items

- **Dependabot: the 17-PR backlog is cleared; what was done and what is left.** GitHub does
  not close a PR when a different commit makes the same change, so every supersession was a
  manual close with a comment naming the commit. Confirm with `gh pr list`.
  - Closed as superseded by `80c0155`: **#25** (ruzstd 0.9.1) and **#24** (zip 9.0.4; we took
    9.0.5, which also carries the opt-in fix for GHSA-m665-v7pm-7grj and reads ZIP64 extra
    fields in spec order).
  - Closed as superseded by `5f17de3`: **#22, #21, #14, #6** (`app/src-tauri`), **#13, #15,
    #17, #18, #26, #16** (`app`) and **#27** (source-map-js, opened from a new alert during
    the session). The Tauri-family PRs could not be merged one at a time: `tauri build`
    refuses a crate/npm pair that differs in major.minor, and CI's `gui` job never runs
    `tauri build`, so a mismatched merge would have stayed green. Dependabot `groups` for
    `tauri*` and `@tauri-apps/*` are now configured so the next family bump arrives as two
    PRs to land together.
  - Rebased on the fixed `main`, CI green, merged with `--rebase`: **#23** (sha2 0.11.1,
    `f38b30f`), **#5** (clap 4.6.7, `4ab2396`), **#9** (actions/setup-node 4→7, `623777c`)
    and **#10** (pnpm/action-setup 4→6, `6544a77`; both majors were read: the workflow pins
    `node-version: 22` and `version: 10`). The app's lockfile was then aligned to sha2
    0.11.1 by hand (`7f4d15f`), because Dependabot bumps the two lockfiles separately and
    `cargo audit --file` reads only what the app's lists.
  - **#20** (serde_json in `app/src-tauri`) lost a base-branch race during the merges;
    Dependabot re-opened it as **#29**, which merged once its checks passed (`e9b858d`).
    **#28** (liblzma 0.4.9, which bumps the bundled xz to 5.8.4 — the library we build
    statically for `.xz`) arrived during the session and is still open: a routine lockfile
    patch bump, merge when green.
- **GHSA-w28w-mhc8-qvjv is the kind of advisory `cargo audit` cannot see.** It is published
  in Tauri's repository advisories, not in RustSec, and GitHub raised no Dependabot alert for
  it either. Fixed by the Tauri 2.12 commit; the class remains: for the GUI's framework,
  read `gh api repos/tauri-apps/tauri/security-advisories` on each Tauri bump.
- **zip: what the integrity work does and does not catch.** Measured on a verification
  corpus (120 real-tool archives, 4,808 seeded header mutants, 2,000 ZIP64 variants) built
  with `zip`, `7z` and Python's `zipfile` plus a byte-patching script, run through a small
  example binary that called `open_image` and printed the decoded digest; the scratch
  directory holding it did not survive the session, so re-creating it means re-writing the
  generators (the recipe is in the two commits' messages). Result: every case that used to
  write wrong bytes silently now refuses,
  except 24 malformed archives whose index declares the image zero-size, so the README
  outranks it and its own CRC verifies. 71 mutants whose index CRC is itself corrupt went
  from "OK" to refused; every reference tool reports them damaged too. The CRC fires when
  the member ends, so on a real write the device has been written by then — the same
  shape as a truncated stream turning a false success into a reported failure.
- **The Windows 11 bypass list needs an on-device test.** Rufus removed `BypassStorageCheck`
  as a no-op and never had a CPU one; we offer both. Invariant 13 says an option that does
  nothing is worse than none. Also untested on a real install: our `autounattend.xml` at the
  media root (Rufus puts it inside `boot.wim` index 2 or under `sources\$OEM$\$$\Panther`,
  and patches the bypass into the offline registry to avoid a command window during Setup).
- `RUSTSEC-2024-0429` (`glib`) is accepted and documented in `SECURITY.md`; it arrives via
  Tauri's GTK stack and has no fixed version reachable from here. Still true at tauri 2.12.5.
- **The Tauri app was invisible to CI, and that had already cost something.** It is
  excluded from the Cargo workspace, so `cargo clippy --workspace` and `cargo build
  --workspace --release` both exit 0 with a hard type error in `app/src-tauri/src/lib.rs`
  — confirmed by putting one there. Worse, `app/src-tauri/Cargo.lock` had not been
  refreshed since the core gained its decoders, and `cargo audit --file` can only report
  on crates the lockfile lists: `zip`, `liblzma`, `ruzstd` and `bzip2` were absent from it
  while the GUI linked all four through `sirius-flash-core`. `security.yml` was auditing a
  smaller program than the one we ship. A `gui` job in `ci.yml` now type-checks the
  frontend and lints the crate, which also keeps that lockfile honest. It still does not
  run `tauri build`, which is what would catch a crate/npm version mismatch.
- **The GUI cannot name the Windows layout exactly, only the condition.** Choosing
  between `Fat32Split` and `NtfsUefiNtfs` needs the size of the largest file in the image,
  and `windows_install_image_size` gets it by `mount -o loop,ro` — root-only, and the GUI
  process deliberately has no privileges; only the CLI under `pkexec` does. So the confirm
  dialog states the trigger ("if this image holds a file larger than 4 GB") rather than the
  outcome. The in-process ISO reader in §7 is the real fix; `7z l` is the stopgap, and that
  wants testing against a real Windows ISO.
- A USB serial and a home path remain in commit `8b95dda`. Deliberately **not** rewritten:
  they are not credentials, and GitHub keeps force-pushed commits reachable by SHA, so a
  rewrite would not remove them. The real remediation — enabling secret scanning, push
  protection and Dependabot — is done.

## 9. Build and check

```bash
cargo build --release                       # core + CLI
cargo test --workspace                      # 113 tests
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cd app && pnpm install && pnpm tauri dev    # GUI
./scripts/boottest.py selftest              # 20 boot cases under qemu, ~55 s
```

CI mirrors exactly that across the three-OS matrix. `cargo fmt --check` runs first, so an
unformatted tree masks every later failure — format before pushing.

To exercise non-Linux `cfg` paths without a macOS box, copy the tree to a scratch dir and
`sed 's/target_os = "linux"/target_os = "macos"/g'` the sources, then build there.
