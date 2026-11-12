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

## Real hardware

- Follow `PROJECT-STATE.md` §6 "The next action" step by step. Assert the stick's model,
  serial, size and removable flag from its by-id path before anything. Write nothing without
  the owner's explicit go-ahead **in that session**: an earlier session's go-ahead does not
  carry over.
- A session runs `sudo` only with a password the owner gives it for that run, after the
  owner has seen that password work in their own terminal, through the same pipeline the
  session will use. Never read, `cat`, `grep` or list a credential file, and never hand one
  to a subagent. Its one use is being piped into `sudo -S` inside the Bash call that runs a
  step the owner approved. Never put it in argv; never print, log, store or commit it.
  After one failed validation in a session, stop and ask: PAM locks the account after a
  few failures. An untracked file in the tree may hold a credential, which is one more
  reason to commit by explicit path.
- Check what reached a stick independently of the flasher's own read-back: run
  `scripts/check_boot_records.py <capture> --disk /dev/disk/by-id/usb-…` on a capture taken
  after a re-plug, with the stick unmounted, through `dd iflag=direct` on the whole disk.

## Before pushing

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs this across Ubuntu, macOS and Windows. `cargo fmt --check` runs first, so an
unformatted tree hides every later failure. Linux-only imports must be `cfg`-gated or
`clippy -D warnings` fails the other two platforms on unused imports.

If `scripts/boottest.py`, `scripts/boot/`, `crates/core/boot/` or
`crates/core/src/bootcode.rs` changed, also run `./scripts/boottest.py selftest` (needs
`qemu-system-x86_64`, `nasm`, `mkfs.fat`, `sfdisk`, `fsck.fat` and `cargo`; about a minute
for 23 cases once the installer is built). CI's `boot` job runs it too, but a broken boot
sector is cheaper to see here. A changed `.asm` under `crates/core/boot/` needs its `.bin`
reassembled and committed beside it (`nasm -f bin x.asm -o x.bin`, default optimisation):
the selftest and CI both diff the two and fail on drift. A new selftest case is not done
until it has been seen to fail on a *copy* of the tree with one bug put in — each copy with
its own `CARGO_TARGET_DIR`, because a shared one can silently reuse another copy's build.
If `scripts/check_boot_records.py` changed, run `./scripts/check_boot_records.py --selftest`
(a second or two once the installer is built): a fault it stops catching fails it. CI's
`boot` job runs it too.

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
  keep the main model for deep reasoning, the assembly, design calls and final judgement.
  Fan out freely and in parallel; brief each one with exact paths and the question; an
  adversarial review of each finished unit has found more than the unit's own tests every
  time. After a fan-out, check `git status` for files an agent left in the repo, and
  commit by explicit path, never `git add -A`.
- Commits: when a unit is finished and the checks under "Before pushing" pass, commit it —
  one Conventional Commit per unit, by explicit path — and push, without waiting for
  approval. (The owner's rule since 2026-11-12; it replaced proposing each subject and
  waiting for it to be pasted back.) A cloud session pushes to its branch, as above.
- The session scratchpad can vanish mid-session. Anything a later session may need goes
  in the repo (`docs/research/` for research notes) or is re-derivable from the commit
  message.
