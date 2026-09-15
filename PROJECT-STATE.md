# Sirius Flash — Project State

Living handoff document. Anyone (or any new session) picking this up cold should read
this file first, then `README.md`, then `SECURITY.md`.

**Last updated:** 2026-09-15. For the commit this describes, ask git —
`git log -1 --format=%h PROJECT-STATE.md`. A hash written into the file by hand
names the commit *before* the one containing it, and this one had drifted five
commits before anyone noticed.

---

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
crates/cli/src/main.rs      `sirius-flash list | write | unattend`
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
      concatenated archive
- [x] Recursive tree copy in-process, no `rsync`
- [x] Windows 11 User Experience: bypass TPM / Secure Boot / RAM / CPU / storage
      (5 bypasses; Rufus offers 3), skip Microsoft account, local admin account,
      disable data collection, prevent BitLocker auto-encryption, debloat
- [x] Live progress: percent, bytes, rate, ETA — CLI and GUI
- [x] Tauri GUI, dark themed, with an `autounattend.xml` preview
- [x] CI green on Ubuntu / macOS / Windows: fmt, `clippy -D warnings`, tests, release build
- [x] Security: `cargo audit` on both lockfiles, Dependabot, secret scanning, push protection

**108 tests** (24 in `lib.rs`, 42 in `blockio.rs`, 12 in `lzw.rs`, 25 in `format.rs`,
5 in the CLI); **98 of them pass on a non-Linux target** — that count is the working proxy
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

## 5. Conventions

- **No personal identifiers anywhere in the repo** — no real names, usernames, home paths
  or hardware serials. Use `TestUser` in tests; make device paths required env vars in
  scripts.
- **No attribution or co-author lines** in commit messages or PR descriptions.
- Commits are Conventional Commits (`feat(core):`, `chore(security):`, …).
- Rufus is GPL-3.0 and so are we — studying and adapting it is legal, and credited in
  `README.md`. Keep crediting it.

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

### Boot sectors — next phase, and we write our own

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

Verified here: `sfdisk` + `mkfs.fat --offset=2048` on a 64 MiB file produces a bootable-flagged
type-0x0c entry at LBA 2048 and a valid VBR with the right label, entirely unprivileged.

Demonstrated on the same harness: media with **no** boot code — which is what this build
produces for an MBR+BIOS target — gives `Booting from Hard Disk...` and then silence. The
silent dead stick §4 warns about, reproducible in seconds.

So testability is **not** a reason to defer boot-sector work. The reasons that remain are
the provenance question below and the fact that the blobs need per-volume BPB patching
rather than being copied verbatim.

## 7. Backlog after that

**BIOS boot sectors** (see §6) · Built-in ISO downloader (Linux catalogue first, then Windows) · macOS backend ·
Windows backend · signed CI releases → `dl.siriuside.com` + AUR · persistent-partition
support · bad-block check.

## 8. Open items

- Dependabot is clear and **no PRs are open**. `actions/checkout@7`, `typescript 7.0.2`
  and `sha2 0.11.0` merged unchanged. `ruzstd 0.9.0` needed a code change, so it was made
  here and PR #1 closed — and it did **not** close itself when the push landed: a different
  commit making the same change is invisible to GitHub, so that had to be done by hand.
  Expect the same of any future Dependabot PR whose upgrade needs code. 0.9 moved
  `StreamingDecoder` into `ruzstd::decoding` **and** started applying its 100 MB window
  cap to the first frame, which would have silently refused every `zstd --long` image —
  hence `MAX_ZSTD_WINDOW`.
- `RUSTSEC-2024-0429` (`glib`) is accepted and documented in `SECURITY.md`; it arrives via
  Tauri's GTK stack and has no fixed version reachable from here. Re-check periodically.
- **The Tauri app was invisible to CI, and that had already cost something.** It is
  excluded from the Cargo workspace, so `cargo clippy --workspace` and `cargo build
  --workspace --release` both exit 0 with a hard type error in `app/src-tauri/src/lib.rs`
  — confirmed by putting one there. Worse, `app/src-tauri/Cargo.lock` had not been
  refreshed since the core gained its decoders, and `cargo audit --file` can only report
  on crates the lockfile lists: `zip`, `liblzma`, `ruzstd` and `bzip2` were absent from it
  while the GUI linked all four through `sirius-flash-core`. `security.yml` was auditing a
  smaller program than the one we ship. A `gui` job in `ci.yml` now type-checks the
  frontend and lints the crate, which also keeps that lockfile honest.
- **The GUI cannot name the Windows layout exactly, only the condition.** Choosing
  between `Fat32Split` and `NtfsUefiNtfs` needs the size of the largest file in the image,
  and `windows_install_image_size` gets it by `mount -o loop,ro` — root-only, and the GUI
  process deliberately has no privileges; only the CLI under `pkexec` does. So the confirm
  dialog states the trigger ("if this image holds a file larger than 4 GB") rather than the
  outcome. To make it exact, read the size unprivileged — `7z` already reads UDF for
  `detect_iso_kind`, so `7z l` is the obvious candidate — and that wants testing against a
  real Windows ISO, which is not available on this machine.
- A USB serial and a home path remain in commit `8b95dda`. Deliberately **not** rewritten:
  they are not credentials, and GitHub keeps force-pushed commits reachable by SHA, so a
  rewrite would not remove them. The real remediation — enabling secret scanning, push
  protection and Dependabot — is done.

## 9. Build and check

```bash
cargo build --release                       # core + CLI
cargo test --workspace                      # 108 tests
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cd app && pnpm install && pnpm tauri dev    # GUI
```

CI mirrors exactly that across the three-OS matrix. `cargo fmt --check` runs first, so an
unformatted tree masks every later failure — format before pushing.

To exercise non-Linux `cfg` paths without a macOS box, copy the tree to a scratch dir and
`sed 's/target_os = "linux"/target_os = "macos"/g'` the sources, then build there.
