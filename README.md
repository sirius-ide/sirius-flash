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
- [ ] Rust core: safe device enumeration (by-id / removable / size) — **in progress**
- [ ] Rust core: Windows-ISO flashing (port of the reference workflow)
- [ ] Rust core: Linux-ISO image write + verify
- [ ] Tauri GUI (pick ISO → pick USB → flash)
- [ ] macOS backend
- [ ] Windows backend
- [ ] Signed releases via CI → dl.siriuside.com + AUR

## Safety first

Every write is gated on hard asserts: target must be **removable**, within a sane USB size window, resolved via its **stable `/dev/disk/by-id` serial path** (never `/dev/sdX`), and explicitly **not** any known internal/data disk. Kernel device names (`sdb`, `nvme0n1`) are treated as unstable and never trusted for targeting.

## Build

```bash
cargo build --release        # core + CLI
# GUI (later): pnpm install && pnpm tauri dev
```

## License

GPL-3.0 — © Clicksora, L.L.C. Free and open source.
