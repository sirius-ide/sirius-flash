# Security policy

Sirius Flash writes directly to block devices, so a defect here can destroy
data that is not recoverable. Security and safety are treated as the same
concern.

## Reporting a vulnerability

Report privately through GitHub's **Report a vulnerability** button on the
Security tab, rather than opening a public issue. Please include the OS, the
Sirius Flash version, and what the tool did versus what you expected.

## How this project defends itself

- **Push protection and secret scanning** are enabled, so a credential is
  blocked before it can be published rather than cleaned up afterwards.
- **Dependabot** alerts on vulnerable dependencies, including the Tauri app's
  separate lockfile.
- **`cargo audit`** runs against the RustSec advisory database on every push
  and pull request, plus weekly — advisories are published after a dependency
  was last touched, so a push-only gate would go stale.
- **CI runs on Linux, macOS and Windows** with `clippy -D warnings`, the full
  test suite, and a release build.

## Device-safety invariants

These are the rules the code is written to keep. Changing any of them should
be treated as a security-relevant change:

1. Writes are addressed only via the stable `/dev/disk/by-id` path. Kernel
   names (`sdb`, `nvme0n1`) are unstable across boots and are never trusted
   for targeting.
2. A target must be **removable** and within a sane USB size window. Internal
   disks are never listed and never written.
3. Anything that can fail is checked **before** the first destructive
   operation — image validity, install-image presence, and capacity — so a
   bad input never leaves a wiped drive behind.
4. Writes are verified by reading the device back and comparing digests, with
   the page cache dropped first so the check reflects the medium and not RAM.

## Accepted risks

| Advisory | Component | Why it is accepted |
|---|---|---|
| [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429) | `glib` 0.18 | Unsoundness in `glib::VariantStrIter`. Pinned transitively by `gtk` 0.18 ← `tauri` 2.x and not upgradable until Tauri adopts the gtk-rs 0.20 stack. The affected API is not called by this project. Revisited on every Tauri bump. |
