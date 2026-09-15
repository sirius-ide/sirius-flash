# Vendored assets

## `uefi-ntfs.img`

The FAT12 partition image Rufus writes as a second partition so that UEFI
firmware can boot an NTFS volume. 1,048,576 bytes,
`sha256 72683fa1250eeea772d3399277b434d4e55ba8dd0dc926e52d817e701fc2eb9e`.

Taken verbatim from [Rufus](https://github.com/pbatard/rufus) `res/uefi/`,
GPL-3.0. **It is vendored rather than built because it cannot be built**: its
value is the Microsoft Secure Boot signature on the binaries inside, and only
Microsoft can produce that. A version we compiled ourselves would not boot on a
machine with Secure Boot enabled.

Per Rufus's own `res/uefi/readme.txt`, it holds three separate upstreams:

| contents | source | licence |
|---|---|---|
| NTFS UEFI drivers, read-only build, Secure Boot signed | [pbatard/ntfs-3g](https://github.com/pbatard/ntfs-3g) 1.9 | GPL-2.0-or-later |
| exFAT and ARM/RISC NTFS drivers, **unsigned** | [pbatard/efifs](https://github.com/pbatard/efifs) 1.12 | GPL-3.0 — cannot be Secure Boot signed |
| UEFI:NTFS bootloader binaries, Secure Boot signed (except 32-bit ARM) | [pbatard/uefi-ntfs](https://github.com/pbatard/uefi-ntfs) 2.8 | GPL-3.0 |

All three are GPL-3.0 compatible.

### What it requires of the target machine

The signed binaries chain to **`Microsoft Corporation UEFI CA 2011`** — the
*third-party* CA, which is optional in a machine's `db`. Where it is absent
(Secured-core PCs, many corporate and Surface SKUs), the firmware rejects the
bootloader before it can print anything and the user sees only "No bootable
option or device was found". That is why the layout choice is surfaced to the
user rather than made silently; see `PROJECT-STATE.md` §6.

Re-verify the hash against `res/uefi/uefi-ntfs.img` on each Rufus bump, and
re-check the current UEFI DBX for revocations before any release.
