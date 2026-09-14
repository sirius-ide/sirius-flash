# Sirius Flash

**A cross-platform bootable-USB creator that actually handles Windows ISOs right — on Linux, macOS, and Windows.**

Part of the [Sirius](https://siriuside.com) family, alongside [Sirius IDE](https://github.com/sirius-ide/sirius-ide).

## Why

Making a Windows install USB from Linux or macOS is still a trap:
- **Etcher / raw `dd`** raw-write the ISO → often a dead stick, and can't handle `install.wim > 4 GB` on FAT32.
- **WoeUSB-ng** works but is rough and Linux-only.
- **macOS** has *nothing* good — no Rufus, no Boot Camp on Apple Silicon.

Sirius Flash does it the correct way, everywhere: GPT + FAT32 + split `install.wim` for maximum UEFI compatibility (Secure Boot stays valid), plain image-write for Linux ISOs, and a clean GUI — with hard safety rails so you can never target the wrong disk.

## Status

🚧 Early development. Linux core first (productized from a proven shell workflow), then macOS, then Windows.

- [x] Reference Linux workflow (`scripts/make-win11-usb.sh`) — GPT+FAT32+wimsplit, by-id safety asserts
- [x] Rust core: safe device enumeration (by-id / removable / size)
- [x] Rust core: Windows-ISO flashing (UDF-aware detection, install.wim/.esd, FAT32 split)
- [x] Rust core: image write + SHA-256 + read-back verification, in-process (no `dd`)
- [x] Windows 11 User Experience — bypass TPM / Secure Boot / RAM / CPU / storage,
      skip the Microsoft account, local admin, no data collection, debloat
- [x] Tauri GUI (pick ISO → pick USB → flash) — dark themed, branded, live progress
- [ ] Format options: MBR/GPT, BIOS/UEFI, filesystem, cluster size, volume label
- [ ] UEFI:NTFS dual-partition layout (removes WIM splitting entirely)
- [x] Image formats — see the table below; detected by content, streamed, never buffered
- [ ] Built-in ISO downloader (Linux catalogue, then Windows)
- [ ] macOS backend
- [ ] Windows backend
- [ ] Signed releases via CI → dl.siriuside.com + AUR

## Image formats

Detected by **content**, not by file extension, and streamed — an 8 GB image is
never held in memory. Concatenated archives (`cat a.gz b.gz`, `pbzip2` output)
are decoded in full rather than stopping at the first stream.

| Format | Extensions | Notes |
|---|---|---|
| Raw | `.iso` `.img` `.usb` `.wic` `.raw` | written verbatim |
| gzip | `.gz` | multi-member |
| xz | `.xz` | multi-stream |
| zstd | `.zst` | multi-frame, large `--long` windows |
| bzip2 | `.bz2` `.bzip2` | multi-stream, so `pbzip2` output works |
| LZMA | `.lzma` | headerless: confirmed by a bounded trial decode |
| Unix compress | `.Z` | LZW, decoder written here |
| zip | `.zip` | the largest member by uncompressed size |
| VHD, fixed | `.vhd` | trailing 512-byte footer trimmed |

Not supported yet, and refused with an explanation rather than written wrongly:
**dynamic** and **differencing** `.vhd` (convert with `qemu-img convert -O vpc
-o subformat=fixed`), `.vhdx`, `.ffu`, and `.7z`. A zip with a prepended
self-extracting stub is not recognised as a zip — unpack it first.

**`.vtsi` is deliberately not supported.** Rufus accepts it, but it is a
proprietary VMware/ThinkPad service-image container with no public
specification; we would rather say so than pretend.

Every write is hashed with SHA-256 as it goes and read back off the device to
confirm it — something neither Rufus nor Etcher does.

## Safety first

Every write is gated on hard asserts: target must be **removable**, within a sane USB size window, resolved via its **stable `/dev/disk/by-id` serial path** (never `/dev/sdX`), and explicitly **not** any known internal/data disk. Kernel device names (`sdb`, `nvme0n1`) are treated as unstable and never trusted for targeting.

## Build

```bash
cargo build --release        # core + CLI
cd app && pnpm install && pnpm tauri dev   # launch the GUI
```

## Credits

Sirius Flash stands on the shoulders of [**Rufus**](https://github.com/pbatard/rufus) by Pete Batard,
also GPL-3.0. Its `wue.c` was studied as the reference for Windows Setup's answer-file
behaviour — in particular that WinPE requires a (possibly empty) `ProductKey` element, that
`unattend.xml` passwords are `Base64(UTF-16LE(password + "Password"))`, and that only a single
`<FirstLogonCommands>` section is permitted. Thanks for two decades of making bootable media
bearable.

## License

GPL-3.0 — © Clicksora, L.L.C. Free and open source.
