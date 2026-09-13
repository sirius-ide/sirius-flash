# Sirius Flash — Architecture

## Shape
- **`crates/core`** — all platform logic as a library: device discovery, safety gate, ISO detection, and the flashers. No UI, fully testable.
- **`crates/cli`** — thin CLI over `core` (`sirius-flash list` / `write`). Ships as a standalone tool and is the test harness for the core.
- **`src-tauri` (later)** — Tauri v2 GUI wrapping `core`. Rust backend keeps privileged disk ops out of the webview.

## Flashing strategy
- **Windows ISO** (has `sources/install.wim`): GPT → single FAT32 partition → copy all files except `install.wim` → `wimlib` split into <4 GB `.swm` chunks → verify `efi/boot/bootx64.efi` + `sources/boot.wim`. Max UEFI compat; Secure Boot signature preserved.
- **Linux / other bootable ISO**: write the hybrid image directly, then verify.

## Per-OS privileged access (the hard part)
- **Linux**: udisks2 + polkit (or the CLI run via pkexec); shell out to `parted`, `mkfs.fat`, `wimlib-imagex`.
- **macOS**: `diskutil` + authorization services; the real differentiator (no good tool exists).
- **Windows**: Win32 `DeviceIoControl` / VDS.

## Distribution
GitHub Actions matrix (linux/mac/win) → signed artifacts → Cloudflare R2 + `dl.siriuside.com` + AUR. See "Cloud" below. AWS only for gaps.

## Cloud (Cloudflare-first)
Cloudflare is the default for everything it can do (existing account + credits, and `siriuside.com` already lives there):
- **R2** — release artifacts + download mirror (S3-compatible, zero egress fees)
- **Pages** — the product landing page (`siriusflash` under siriuside.com)
- **Workers** — update-check + latest-version redirect endpoints
- **CDN** — `dl.siriuside.com` for downloads (already live)

AWS (Clicksora account) is used only for gaps Cloudflare can't cover. Signing/build happen in GitHub Actions; secrets live in repo/org Actions secrets, never in the tree.
