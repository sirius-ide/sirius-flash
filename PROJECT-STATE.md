# Sirius Flash — Project State

Living handoff document. Anyone (or any new session) picking this up cold should read
this file first, then `README.md`, then `SECURITY.md`.

**Last updated:** 2026-09-14 · at commit `5bfc482`

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

**71 tests** (18 in `lib.rs`, 41 in `blockio.rs`, 12 in `lzw.rs`); 65 are
platform-independent — that count is the working proxy for how much of the core is ready
for the macOS backend.

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

**Next:** format options (MBR/GPT, BIOS/UEFI target, FAT32/NTFS/exFAT, cluster size,
volume label, quick format).

The **UEFI:NTFS dual-partition layout** comes after, and the earlier note here claiming it
"removes WIM splitting entirely" was wrong on both halves. Researched 2026-09-14; keep this
summary, because the stale version of it is all over the internet.

- It **is** Secure Boot signed, and has been since Rufus 3.17 (2021-10-23). Both the
  loader (`bootx64.efi`) and the ntfs-3g driver (`ntfs_x64.efi`) carry real Authenticode
  signatures. Advice saying "disable Secure Boot for UEFI:NTFS" describes the pre-2021
  GPL-3.0 EfiFs driver and is obsolete.
- But both chain to **`Microsoft Corporation UEFI CA 2011`** (succeeded by
  `Microsoft UEFI CA 2023`) — the *third-party* CA, which is **optional**. Microsoft's OEM
  guidance says OEMs "should consider" shipping it; the mandatory `db` for Windows 11
  25H2+ contains only `Windows UEFI CA 2023`; and Secured-core PCs must **distrust** it by
  default. `arm`, `riscv64` and every `exfat_*.efi` are unsigned outright.
- **When that CA is absent the failure is silent.** The loader and the driver share one
  signing leaf, so the firmware rejects the loader at `LoadImage` and UEFI:NTFS never runs
  to print anything. The user sees only `No bootable option or device was found` — the same
  thing a badly written stick produces. Verified by booting Rufus's exact layout under OVMF
  across four `db` configurations.
- **There is therefore no automatic fallback.** The stick is written on one machine for
  another; the target's `db` is unknowable at flash time, and probing our own would answer
  about the wrong machine.

So GPT+FAT32+split stays the default: it asks the firmware to trust only the ISO's own
Microsoft-signed bootloader, a strict subset of what UEFI:NTFS needs. Rufus reached the
same conclusion and still ships a full WIM splitter at HEAD, keeping FAT32+split behind its
Alt-E cheat mode. If UEFI:NTFS lands here it is an **opt-in expert option** with a blunt
in-product warning about the firmware requirement — never a routine layout choice, because
the bootloader is never given the chance to explain itself.

## 7. Backlog after that

Built-in ISO downloader (Linux catalogue first, then Windows) · macOS backend ·
Windows backend · signed CI releases → `dl.siriuside.com` + AUR · persistent-partition
support · bad-block check.

## 8. Open items

- Dependabot is clear. `actions/checkout@7`, `typescript 7.0.2` and `sha2 0.11.0` merged
  unchanged; `ruzstd 0.9.0` needed a code change and is committed locally but its PR (#1)
  is still open on GitHub because these commits have not been pushed. 0.9 moved
  `StreamingDecoder` into `ruzstd::decoding` **and** started applying its 100 MB window
  cap to the first frame, which would have silently refused every `zstd --long` image —
  hence `MAX_ZSTD_WINDOW`.
- `RUSTSEC-2024-0429` (`glib`) is accepted and documented in `SECURITY.md`; it arrives via
  Tauri's GTK stack and has no fixed version reachable from here. Re-check periodically.
- A USB serial and a home path remain in commit `8b95dda`. Deliberately **not** rewritten:
  they are not credentials, and GitHub keeps force-pushed commits reachable by SHA, so a
  rewrite would not remove them. The real remediation — enabling secret scanning, push
  protection and Dependabot — is done.

## 9. Build and check

```bash
cargo build --release                       # core + CLI
cargo test --workspace                      # 71 tests
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cd app && pnpm install && pnpm tauri dev    # GUI
```

CI mirrors exactly that across the three-OS matrix. `cargo fmt --check` runs first, so an
unformatted tree masks every later failure — format before pushing.

To exercise non-Linux `cfg` paths without a macOS box, copy the tree to a scratch dir and
`sed 's/target_os = "linux"/target_os = "macos"/g'` the sources, then build there.
