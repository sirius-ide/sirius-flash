# Sirius Flash — Project State

Living handoff document. Anyone (or any new session) picking this up cold should read
this file first, then `README.md`, then `SECURITY.md`.

**Last updated:** 2026-09-14 · at commit `db8ab21`

---

## 1. What this is

A cross-platform bootable-USB creator meant to fully replace Rufus, Etcher, WoeUSB-ng
and `dd` on **Linux, macOS and Windows**. GPL-3.0, © Clicksora L.L.C. Companion product
to Sirius IDE, offered alongside it.

Repo: `github.com/sirius-ide/sirius-flash` · Local: `~/Projects/sirius-flash`

**Goal, stated by the product owner:** *"we want our product to be totally replaced by
users for rufus and any other flashing tool for any os out there"* and *"we want to beat
our competitors on all fronts."* Feature parity with Rufus is the floor, not the target.

## 2. Layout

```
crates/core/src/lib.rs      device discovery, safety gate, ISO detection,
                            Windows tweaks + autounattend.xml, both flashers
crates/core/src/blockio.rs  streaming I/O: progress, SHA-256, verification,
                            decompression, recursive tree copy
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
- [x] Compressed images — gzip, xz, zstd, bzip2 — sniffed by magic bytes, streamed
- [x] Recursive tree copy in-process, no `rsync`
- [x] Windows 11 User Experience: bypass TPM / Secure Boot / RAM / CPU / storage
      (5 bypasses; Rufus offers 3), skip Microsoft account, local admin account,
      disable data collection, prevent BitLocker auto-encryption, debloat
- [x] Live progress: percent, bytes, rate, ETA — CLI and GUI
- [x] Tauri GUI, dark themed, with an `autounattend.xml` preview
- [x] CI green on Ubuntu / macOS / Windows: fmt, `clippy -D warnings`, tests, release build
- [x] Security: `cargo audit` on both lockfiles, Dependabot, secret scanning, push protection

**34 tests** (18 in `lib.rs`, 16 in `blockio.rs`); 28 are platform-independent — that count
is the working proxy for how much of the core is ready for the macOS backend.

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

## 5. Conventions

- **No personal identifiers anywhere in the repo** — no real names, usernames, home paths
  or hardware serials. Use `TestUser` in tests; make device paths required env vars in
  scripts.
- **No attribution or co-author lines** in commit messages or PR descriptions.
- Commits are Conventional Commits (`feat(core):`, `chore(security):`, …).
- Rufus is GPL-3.0 and so are we — studying and adapting it is legal, and credited in
  `README.md`. Keep crediting it.

## 6. Current phase — format coverage, then format options + UEFI:NTFS

Rufus's authoritative accepted-format list (`src/rufus.c:2642`):

```
*.iso *.img *.vhd *.vhdx *.usb *.bz2 *.bzip2 *.gz *.lzma *.xz *.Z *.zip *.zst *.wic *.wim *.esd *.vtsi
```

Its `bled` decompression library (`src/bled/bled.h:25-35`) covers:
NONE, ZIP, LZW (`.Z`), GZIP, LZMA, BZIP2, XZ, 7ZIP, VTSI, ZSTD.

**Covered:** raw (`.iso .img .usb .wic .raw`), `.gz`, `.xz`, `.zst`, `.bz2`
**Missing, in planned order:**

| Format | Difficulty | Notes |
|---|---|---|
| `.lzma` | easy | `liblzma` is already a dependency |
| `.zip` | easy | needs the `zip` crate; pick the single largest member |
| `.Z` (LZW) | moderate | legacy `compress`; needs a decoder crate |
| `.vhd` fixed | easy | raw image + 512-byte trailing footer to strip |
| `.vhd` dynamic | moderate | requires BAT parsing |
| `.vhdx` | hard | different container entirely |
| `.ffu` | hard | Microsoft full-flash update format |
| `.7z` | hard | large dependency for marginal gain |
| `.vtsi` | skip | Rufus-proprietary (VMware/ThinkPad service images); say so openly |

New decoders follow the existing pattern in `blockio.rs`: extend the `Compression` enum,
add magic bytes to `sniff()`, wire a streaming reader into `open_image()`, and add a test
against a real fixture archive — never a hand-built byte string.

**After formats:** format options (MBR/GPT, BIOS/UEFI target, FAT32/NTFS/exFAT, cluster
size, volume label, quick format) and the **UEFI:NTFS dual-partition layout** — the latter
removes WIM splitting entirely and should land together with the options panel, since they
are one screen in Rufus.

## 7. Backlog after that

Built-in ISO downloader (Linux catalogue first, then Windows) · macOS backend ·
Windows backend · signed CI releases → `dl.siriuside.com` + AUR · persistent-partition
support · bad-block check.

## 8. Open items

- Four Dependabot PRs are waiting: `ruzstd 0.9.0`, `sha2 0.11.0`, `actions/checkout@7`,
  `typescript 7.0.2`. `sha2` and `ruzstd` are majors — read the changelogs.
- `RUSTSEC-2024-0429` (`glib`) is accepted and documented in `SECURITY.md`; it arrives via
  Tauri's GTK stack and has no fixed version reachable from here. Re-check periodically.
- A USB serial and a home path remain in commit `8b95dda`. Deliberately **not** rewritten:
  they are not credentials, and GitHub keeps force-pushed commits reachable by SHA, so a
  rewrite would not remove them. The real remediation — enabling secret scanning, push
  protection and Dependabot — is done.

## 9. Build and check

```bash
cargo build --release                       # core + CLI
cargo test --workspace                      # 34 tests
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cd app && pnpm install && pnpm tauri dev    # GUI
```

CI mirrors exactly that across the three-OS matrix. `cargo fmt --check` runs first, so an
unformatted tree masks every later failure — format before pushing.

To exercise non-Linux `cfg` paths without a macOS box, copy the tree to a scratch dir and
`sed 's/target_os = "linux"/target_os = "macos"/g'` the sources, then build there.
