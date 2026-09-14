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

The VHDs are files rather than inline arrays because qemu rounds a fixed VHD up
to CHS geometry, so 35 kB is the smallest one that exists.

The payload of both is `(i * 167 + 13) mod 256` for `i` in `0..n`, which the
tests regenerate rather than storing a second copy of.

Each file was validated by decompressing it with GNU `gzip`, an independent
implementation, so a fixture cannot silently agree with a bug in our decoder.
