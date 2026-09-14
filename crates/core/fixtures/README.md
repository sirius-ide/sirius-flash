# Test fixtures

Binary inputs that are too large to sit inline in a test module as a byte
array. Everything here is a **real archive**, not a hand-assembled one.

| file | what it is | how it was made |
|---|---|---|
| `lzw-width-growth.Z` | Unix `compress` stream whose codes widen 9 → 10 → 11 → 12 | LZW encoder, output verified with GNU `gzip -dc` |
| `lzw-dictionary-full.Z` | the same payload at `maxbits=10`, so the dictionary saturates and then stays frozen | as above |

The payload of both is `(i * 167 + 13) mod 256` for `i` in `0..n`, which the
tests regenerate rather than storing a second copy of.

Each file was validated by decompressing it with GNU `gzip`, an independent
implementation, so a fixture cannot silently agree with a bug in our decoder.
