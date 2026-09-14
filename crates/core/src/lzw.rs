//! Streaming decoder for the Unix `compress` format (`.Z`).
//!
//! Rufus accepts `.Z` through bled's LZW decoder, so parity needs one. This is
//! hand-written rather than pulled from crates.io: the only pure-Rust `.Z`
//! decoder published is a single 0.1.0 release from July 2026 with no version
//! history, which is a thin foundation for code that decides which bytes land
//! on somebody's boot drive. The format is small, frozen since the 1980s and
//! fully specified, and the tests check it against archives that GNU `gzip`
//! agrees on.
//!
//! Layout: the two magic bytes `1f 9d`, then a flags byte holding the maximum
//! code width in its low five bits and "block mode" in its top bit. After that
//! comes a stream of LZW codes packed **least-significant-bit first**, starting
//! nine bits wide and growing to the maximum as the dictionary fills.
//!
//! The one genuinely awkward part is padding. `compress` writes codes in groups
//! of eight — eight codes of *n* bits is exactly *n* whole bytes — and whenever
//! the code width grows, or the dictionary is reset, it zero-fills the rest of
//! the current group so the next one starts on a byte boundary. A decoder that
//! ignores this stays bit-aligned for a while and then silently produces
//! garbage, which is the classic way to get this format wrong.

use std::io::{self, BufReader, Read};

/// `1f 9d`, the same two bytes `file(1)` keys on.
pub const MAGIC: [u8; 2] = [0x1f, 0x9d];

/// Codes always start nine bits wide.
const INIT_WIDTH: u32 = 9;

/// The format permits at most sixteen, which caps the dictionary at 65536
/// entries and so bounds everything this decoder allocates.
const MAX_WIDTH: u32 = 16;

/// In block mode this code means "reset the dictionary"; it is never a literal.
const CLEAR: u16 = 256;

/// Codes below this are literal bytes and need no dictionary entry.
const FIRST_ENTRY: u16 = 256;

/// Codes are emitted in groups of this many, which is what makes a group a
/// whole number of bytes at any width.
const GROUP: u64 = 8;

/// Pulls LSB-first codes of a varying width out of a byte stream.
struct BitReader<R> {
    inner: R,
    /// Bits not yet handed out, lowest bit first.
    acc: u64,
    /// How many of `acc`'s bits are valid.
    have: u32,
}

impl<R: Read> BitReader<R> {
    fn new(inner: R) -> Self {
        BitReader {
            inner,
            acc: 0,
            have: 0,
        }
    }

    /// Top up `acc` until it holds at least `want` bits, or the input ends.
    fn fill(&mut self, want: u32) -> io::Result<()> {
        while self.have < want {
            let mut byte = [0u8; 1];
            match self.inner.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    self.acc |= u64::from(byte[0]) << self.have;
                    self.have += 8;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The next `width`-bit code, or `None` at a clean end of input.
    fn code(&mut self, width: u32) -> io::Result<Option<u16>> {
        self.fill(width)?;
        if self.have < width {
            // Trailing bits too short to form a code are the encoder's padding
            // of the final byte, not a truncated stream.
            return Ok(None);
        }
        let code = (self.acc & ((1u64 << width) - 1)) as u16;
        self.acc >>= width;
        self.have -= width;
        Ok(Some(code))
    }

    /// Throw away `count` bits of inter-group padding.
    fn discard(&mut self, mut count: u32) -> io::Result<()> {
        while count > 0 {
            let step = count.min(32);
            self.fill(step)?;
            let step = step.min(self.have);
            if step == 0 {
                // Padding running into the end of the file is harmless: there
                // are no more codes to misalign.
                return Ok(());
            }
            self.acc >>= step;
            self.have -= step;
            count -= step;
        }
        Ok(())
    }
}

/// Decodes a `.Z` stream as it is read.
///
/// The dictionary is held as parallel prefix/suffix arrays rather than as
/// strings: entry *n* is "entry `prefix[n]`, then the byte `suffix[n]`". An
/// entry is expanded by walking that chain, which yields its bytes backwards,
/// so they are pushed onto `pending` and read out in reverse. Storing strings
/// directly would let a crafted archive allocate without bound; this way the
/// whole decoder is fixed at a few hundred kilobytes however hostile the input.
pub struct Decoder<R> {
    bits: BitReader<BufReader<R>>,
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// Expanded bytes waiting to be read, held in reverse order.
    pending: Vec<u8>,
    /// Largest code width this stream may reach, from the header.
    max_width: u32,
    /// Whether code 256 resets the dictionary.
    block_mode: bool,
    width: u32,
    /// Next dictionary slot to fill.
    free: u16,
    /// Previous code, needed to extend the dictionary and to resolve the
    /// self-referential case below.
    prev: Option<u16>,
    /// Codes read at the current width since the last group boundary.
    in_group: u64,
    done: bool,
}

impl<R: Read> Decoder<R> {
    /// Read and validate the three-byte header, returning a decoder positioned
    /// on the first code.
    pub fn new(inner: R) -> io::Result<Decoder<R>> {
        let mut inner = BufReader::new(inner);
        let mut header = [0u8; 3];
        inner.read_exact(&mut header)?;
        if header[0] != MAGIC[0] || header[1] != MAGIC[1] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a compress (.Z) stream",
            ));
        }
        let max_width = u32::from(header[2] & 0x1f);
        if !(INIT_WIDTH..=MAX_WIDTH).contains(&max_width) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported .Z code width {max_width}, expected 9 to 16"),
            ));
        }
        let block_mode = header[2] & 0x80 != 0;
        let capacity = 1usize << max_width;
        Ok(Decoder {
            bits: BitReader::new(inner),
            prefix: vec![0; capacity],
            suffix: vec![0; capacity],
            pending: Vec::with_capacity(capacity),
            max_width,
            block_mode,
            width: INIT_WIDTH,
            free: first_free(block_mode),
            prev: None,
            in_group: 0,
            done: false,
        })
    }

    /// Skip the encoder's zero padding out to the end of the current group of
    /// eight codes, then start counting a fresh group.
    fn realign(&mut self) -> io::Result<()> {
        let short = self.in_group % GROUP;
        if short != 0 {
            let pad = (GROUP - short) as u32 * self.width;
            self.bits.discard(pad)?;
        }
        self.in_group = 0;
        Ok(())
    }

    /// Back to a dictionary of nothing but the 256 literals.
    fn reset(&mut self) {
        self.free = first_free(self.block_mode);
        self.width = INIT_WIDTH;
        self.prev = None;
    }

    /// Push the bytes of `code` onto `pending` (reversed) and return the first
    /// byte of its expansion, which is what a new dictionary entry records.
    fn expand(&mut self, code: u16) -> io::Result<u8> {
        let mut at = code;
        let start = self.pending.len();
        // The chain strictly decreases, so it cannot loop; the bound is belt
        // and braces against a dictionary corrupted by a malformed stream.
        for _ in 0..=self.prefix.len() {
            if at < FIRST_ENTRY {
                self.pending.push(at as u8);
                return Ok(at as u8);
            }
            let idx = at as usize;
            if idx >= self.prefix.len() {
                break;
            }
            self.pending.push(self.suffix[idx]);
            at = self.prefix[idx];
        }
        self.pending.truncate(start);
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "corrupt .Z stream: dictionary entry does not terminate",
        ))
    }

    /// Decode one code, leaving its bytes in `pending`. Returns false at the
    /// end of the stream.
    fn step(&mut self) -> io::Result<bool> {
        loop {
            let Some(code) = self.bits.code(self.width)? else {
                return Ok(false);
            };
            self.in_group += 1;

            if self.block_mode && code == CLEAR {
                self.realign()?;
                self.reset();
                continue;
            }

            let Some(prev) = self.prev else {
                // The first code of a stream, and of every block after a reset,
                // can only be a literal — there is no dictionary yet to name.
                if code >= FIRST_ENTRY {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "corrupt .Z stream: it opens with a dictionary reference",
                    ));
                }
                self.pending.push(code as u8);
                self.prev = Some(code);
                return Ok(true);
            };

            let start = self.pending.len();
            let first = if code < self.free {
                self.expand(code)?
            } else if code == self.free {
                // The encoder may name the entry it is about to create, when
                // the input repeats as `xIxIx`. Its expansion is the previous
                // string followed by that string's own first byte — which is
                // why `expand` returns that byte.
                let first = self.expand(prev)?;
                // `pending` runs backwards, so the byte that comes *last* on
                // output is inserted at the *front* of what expand just pushed.
                self.pending.insert(start, first);
                first
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt .Z stream: code {code} is past the dictionary"),
                ));
            };

            if u32::from(self.free) < (1u32 << self.max_width) {
                let slot = self.free as usize;
                self.prefix[slot] = prev;
                self.suffix[slot] = first;
                self.free += 1;
            }
            self.prev = Some(code);

            // Widen once the dictionary has outgrown the current code size.
            if self.free > ((1u32 << self.width) - 1) as u16 && self.width < self.max_width {
                self.realign()?;
                self.width += 1;
            }
            return Ok(true);
        }
    }
}

/// In block mode code 256 is reserved for CLEAR, so entries start one later.
fn first_free(block_mode: bool) -> u16 {
    if block_mode {
        FIRST_ENTRY + 1
    } else {
        FIRST_ENTRY
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < buf.len() {
            if self.pending.is_empty() {
                if self.done {
                    break;
                }
                if !self.step()? {
                    self.done = true;
                    break;
                }
                continue;
            }
            // `pending` holds the expansion backwards, so the next byte out is
            // the last one in.
            let byte = self.pending.pop().expect("pending is not empty");
            buf[written] = byte;
            written += 1;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(z: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Decoder::new(z).unwrap().read_to_end(&mut out).unwrap();
        out
    }

    /// The payload behind the checked-in fixtures. Regenerating it beats
    /// storing a second copy of 24 kB next to the archive.
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 167 + 13) % 256) as u8).collect()
    }

    const PAYLOAD: &[u8] = b"SIRIUS-FLASH-COMPRESSION-TEST-PAYLOAD-0123456789";

    /// PAYLOAD as a real `.Z`: block mode, 16-bit maximum. Short enough that
    /// the code width never leaves 9 bits.
    const SMALL: &[u8] = &[
        0x1f, 0x9d, 0x90, 0x53, 0x92, 0x48, 0x49, 0x52, 0x65, 0x4a, 0x0b, 0x23, 0x4c, 0x82, 0x4c,
        0x41, 0xd2, 0x62, 0xc8, 0x93, 0x26, 0x50, 0xa4, 0x14, 0x99, 0x12, 0xf0, 0x89, 0x93, 0x16,
        0x54, 0x26, 0x52, 0x69, 0x01, 0x25, 0x48, 0x16, 0x26, 0x4f, 0x82, 0x10, 0x69, 0x01, 0x23,
        0x86, 0x8c, 0x19, 0x34, 0x6a, 0xd8, 0xb8, 0x81, 0x23, 0x07,
    ];

    /// The same payload with block mode off, so there is no CLEAR code and
    /// dictionary entries begin at 256 rather than 257.
    const NO_BLOCK_MODE: &[u8] = &[
        0x1f, 0x9d, 0x10, 0x53, 0x92, 0x48, 0x49, 0x52, 0x65, 0x4a, 0x0b, 0x23, 0x4c, 0x82, 0x4c,
        0x41, 0xd2, 0x62, 0xc8, 0x93, 0x26, 0x50, 0xa4, 0x14, 0x99, 0x02, 0xf0, 0x89, 0x93, 0x16,
        0x54, 0x24, 0x52, 0x69, 0x01, 0x25, 0x48, 0x16, 0x26, 0x4f, 0x82, 0x10, 0x69, 0x01, 0x23,
        0x86, 0x8c, 0x19, 0x34, 0x6a, 0xd8, 0xb8, 0x81, 0x23, 0x07,
    ];

    /// `A` then `BA` six hundred times — input shaped to make the encoder
    /// emit the code for an entry it has not finished creating.
    const SELF_REFERENTIAL: &[u8] = &[
        0x1f, 0x9d, 0x90, 0x41, 0x84, 0x04, 0x1c, 0x28, 0xb0, 0x20, 0xc1, 0x83, 0x06, 0x13, 0x22,
        0x5c, 0xa8, 0xb0, 0x21, 0xc3, 0x87, 0x0e, 0x23, 0x42, 0x9c, 0x28, 0xb1, 0x22, 0xc5, 0x8b,
        0x16, 0x33, 0x62, 0xdc, 0xa8, 0xb1, 0x23, 0xc7, 0x8f, 0x1e, 0x43, 0x82, 0x1c, 0x29, 0xb2,
        0x24, 0xc9, 0x93, 0x26, 0x53, 0xa2, 0x5c, 0xa9, 0xb2, 0x25, 0xcb, 0x97, 0x2e, 0x63, 0xc2,
        0x9c, 0x29, 0xb3, 0x26, 0xcd, 0x9b, 0x36, 0x73, 0xe2, 0xdc, 0xa9, 0xb3, 0x27, 0xcf, 0x9f,
        0x3e, 0x83, 0x02, 0x1d, 0x2a, 0x11,
    ];

    #[test]
    fn decodes_a_real_archive() {
        assert_eq!(decode(SMALL), PAYLOAD);
    }

    #[test]
    fn decodes_without_block_mode() {
        // With block mode off there is no CLEAR code, so entries begin at 256
        // rather than 257. Getting that wrong shifts every later code by one.
        assert_eq!(decode(NO_BLOCK_MODE), PAYLOAD);
    }

    #[test]
    fn decodes_the_self_referential_case() {
        // `xIxIx`: the encoder names the entry it is in the middle of adding,
        // whose expansion is the previous string plus that string's own first
        // byte. A decoder that simply looks the code up finds nothing there.
        let mut expected = vec![b'A'];
        for _ in 0..600 {
            expected.extend_from_slice(b"BA");
        }
        assert_eq!(decode(SELF_REFERENTIAL), expected);
    }

    #[test]
    fn follows_the_code_width_as_it_grows() {
        // Three transitions, 9 -> 10 -> 11 -> 12. Each one is preceded by
        // padding out to the end of a group of eight codes; a decoder that
        // ignores that padding stays aligned briefly and then produces
        // plausible-looking garbage.
        let z = include_bytes!("../fixtures/lzw-width-growth.Z");
        assert_eq!(decode(z), pattern(24000));
    }

    #[test]
    fn keeps_going_once_the_dictionary_is_full() {
        // maxbits=10 caps the dictionary at 1024 entries. Past that the
        // encoder stops adding and the decoder must stop too, in lockstep,
        // or every subsequent code resolves to the wrong string.
        let z = include_bytes!("../fixtures/lzw-dictionary-full.Z");
        assert_eq!(decode(z), pattern(3000));
    }

    #[test]
    fn rejects_a_stream_that_is_not_compress() {
        let err = Decoder::new(&b"\x1f\x8b\x08not gzip"[..])
            .err()
            .expect("a gzip stream is not a .Z stream");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_an_impossible_code_width() {
        // The low five bits hold the width; 8 and 17 are both out of range.
        for flags in [0x88u8, 0x91] {
            let err = Decoder::new(&[0x1f, 0x9d, flags][..])
                .err()
                .unwrap_or_else(|| panic!("width {} must be rejected", flags & 0x1f));
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn refuses_a_dictionary_reference_before_there_is_a_dictionary() {
        // First code 511: nothing has been defined yet, so this is corrupt.
        // Accepting it would read uninitialised dictionary slots.
        let stream = [0x1f, 0x9d, 0x90, 0xff, 0x01];
        let mut out = Vec::new();
        let err = Decoder::new(&stream[..])
            .unwrap()
            .read_to_end(&mut out)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_truncated_archive_is_an_error_or_a_short_read_never_a_hang() {
        // Every prefix of a real archive must terminate promptly, either with
        // an error or with the bytes decoded so far.
        let full = include_bytes!("../fixtures/lzw-width-growth.Z");
        for cut in [4usize, 17, 100, 1000, 4000] {
            let mut out = Vec::new();
            let _ = Decoder::new(&full[..cut]).unwrap().read_to_end(&mut out);
            assert!(out.len() < 24000, "a truncated archive cannot be complete");
        }
    }

    #[test]
    fn survives_arbitrary_trailing_bytes() {
        // Fuzz-shaped: a valid header followed by noise must not panic.
        for seed in 0u32..64 {
            let mut stream = vec![0x1f, 0x9d, 0x90];
            let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
            for _ in 0..96 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                stream.push(x as u8);
            }
            let mut out = Vec::new();
            let _ = Decoder::new(&stream[..]).unwrap().read_to_end(&mut out);
        }
    }

    #[test]
    fn honours_small_read_buffers() {
        // `stream()` reads in 4 MiB chunks, but Read must be correct for any
        // buffer size, including one byte at a time.
        let z = include_bytes!("../fixtures/lzw-dictionary-full.Z");
        let mut d = Decoder::new(&z[..]).unwrap();
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match d.read(&mut one).unwrap() {
                0 => break,
                _ => out.push(one[0]),
            }
        }
        assert_eq!(out, pattern(3000));
    }
}
