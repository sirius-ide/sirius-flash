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

If `scripts/boottest.py`, `scripts/boot/` or `crates/core/boot/` changed, also run
`./scripts/boottest.py selftest` (needs `qemu-system-x86_64`, `nasm`, `mkfs.fat`,
`sfdisk`, `fsck.fat`; ~55 s for 20 cases). CI's `boot` job runs it too, but a broken boot
sector is cheaper to see here. A changed `.asm` under `crates/core/boot/` needs its `.bin`
reassembled and committed beside it (`nasm -f bin x.asm -o x.bin`, default optimisation):
the selftest and CI both diff the two and fail on drift. A new selftest case is not done
until it has been seen to fail on a *copy* of the tree with one bug put in.

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
- Commits: propose one Conventional Commit subject per unit and wait for it to be pasted
  back. If the reply is only "continue", ask once (commit all / commit but leave GitHub
  actions / not yet) rather than guessing.
- The session scratchpad can vanish mid-session. Anything a later session may need goes
  in the repo (`docs/research/` for research notes) or is re-derivable from the commit
  message.
