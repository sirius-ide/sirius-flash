# Sirius Flash — working agreement

**Read `PROJECT-STATE.md` first.** It carries the current phase, what is already built,
and the invariants that must not regress. This file is only the short version.

## What this project is

A cross-platform bootable-USB creator (Linux / macOS / Windows) intended to fully replace
Rufus, Etcher, WoeUSB-ng and `dd`. Feature parity with Rufus is the floor, not the target.
Rust + Tauri v2, GPL-3.0, © Clicksora L.L.C.

## Hard rules

- **Never commit personal identifiers** — no real names, usernames, home directory paths
  or hardware serial numbers. Tests use `TestUser`; scripts take device paths as required
  environment variables.
- **No attribution or co-author trailers** in commit messages or PR descriptions.
- **Nothing destructive runs before the image has been fully probed.** Wiping a drive and
  *then* discovering the image is unusable is the worst bug this project can ship; it has
  happened once already.
- **Target drives only by their `/dev/disk/by-id` path.** Kernel names (`sdb`, `nvme0n1`)
  are unstable and must never be trusted for targeting.
- **Stream everything.** Images reach 8 GB and beyond; never read one into memory.

## Before pushing

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs this across Ubuntu, macOS and Windows. `cargo fmt --check` runs first, so an
unformatted tree hides every later failure. Linux-only imports must be `cfg`-gated or
`clippy -D warnings` fails the other two platforms on unused imports.

## Commits

Conventional Commits (`feat(core):`, `fix(cli):`, `chore(security):`). Keep the subject in
the imperative, and explain *why* in the body when the reason is not obvious from the diff.

## Cloud sessions (claude.ai/code, the Claude app, `claude --cloud`)

A cloud session is a fresh Ubuntu 24.04 VM with this repo cloned from GitHub: no USB
devices, no physical disks, no secrets, no user-level Claude memory or settings.

- **Toolchain**: Rust and cargo are pre-installed. The claude.ai environment's setup
  script is `scripts/cloud-setup.sh` (the same Tauri/webkit libraries CI installs, plus
  QEMU, nasm and dosfstools for `scripts/boottest.py`) — keep the two in sync.
- **Nothing can be flashed, and nothing should be tried.** Device-writing paths can only
  be exercised against image files; real-hardware verification waits for a local session.
- **Work on a branch** and push it; the owner reviews locally before it lands on `main`.

## Handoff — every session ends resumable

Before you stop, commit and push your branch, then update `PROJECT-STATE.md` — what
changed, what is verified versus only written, what's next, and the branch name — and
push that too. The next session, cloud or local (VS Code's Session history → **Web**
tab, or `claude --teleport`), resumes from the branch and the state doc even when the
conversation itself doesn't carry over.

## Working preferences

- Subagents (Agent tool): pass `model: "sonnet"` for research, search and exploration;
  keep the main model for deep reasoning only.
