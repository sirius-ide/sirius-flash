# Test fixtures

Binary inputs that are too large to sit inline in a test module as a byte
array. Everything here is a **real archive**, not a hand-assembled one.

| file | what it is | how it was made |
|---|---|---|
| `lzw-width-growth.Z` | Unix `compress` stream whose codes widen 9 → 10 → 11 → 12 | LZW encoder, output verified with GNU `gzip -dc` |
| `lzw-dictionary-full.Z` | the same payload at `maxbits=10`, so the dictionary saturates and then stays frozen | as above |
| `fixed.vhd` | fixed-size VHD: 34816 bytes of payload plus the 512-byte `conectix` footer | `qemu-img convert -f raw -O vpc -o subformat=fixed` |
| `dynamic.vhd` | dynamic VHD, which we must refuse rather than write | `qemu-img create -f vpc -o subformat=dynamic` |
| `zip-damaged-sibling.zip` | two real members; the *second* member's local header is clobbered, the first is intact | `zip`, then one byte overwritten |
| `zip-size-misdeclared.zip` | a genuine deflate member of 1 MiB that declares 4096 in both size fields | `zipfile`, then the size fields rewritten |
| `bios-fat32-head.bin` | sector 0, then the partition's 32 reserved sectors, of a 64 MiB MBR disk with one active FAT32 partition at LBA 2048 — what `bootcode::prepare` reads | `sfdisk` and `mkfs.fat`; commands below |

The VHDs are files rather than inline arrays because qemu rounds a fixed VHD up
to CHS geometry, so 35 kB is the smallest one that exists.

The payload of both is `(i * 167 + 13) mod 256` for `i` in `0..n`, which the
tests regenerate rather than storing a second copy of.

Each file was validated by decompressing it with GNU `gzip`, an independent
implementation, so a fixture cannot silently agree with a bug in our decoder.

`bios-fat32-head.bin` is real `sfdisk` (util-linux 2.42.4) and `mkfs.fat`
(dosfstools 4.2) output, made reproducible with a fixed disk identifier and
`--invariant`, and checked with `fsck.fat -n` before it was cut down. Rebuilding
it gives the same bytes:

```bash
truncate -s 64M disk.img
printf 'label: dos\nlabel-id: 0x5157ab1e\nunit: sectors\nstart=2048, size=129024, type=c, bootable\n' \
  | sfdisk --quiet disk.img
mkfs.fat -F 32 -s 1 -n FIXTURE -h 2048 --invariant --offset=2048 disk.img 64512
(dd if=disk.img bs=512 count=1; dd if=disk.img bs=512 skip=2048 count=32) > bios-fat32-head.bin
# sha256 7bfc920570a52d78e05c21934d8d2a6e39c98972545bb1df72b8fa358dc4646c
```
