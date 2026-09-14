//! Streaming image writes with progress, hashing and read-back verification.
//!
//! This replaces shelling out to `dd`, which cost us three things: no usable
//! progress (its records are carriage-return terminated), no digest, and no
//! portability — `oflag=sync status=progress` is GNU coreutils only and does
//! not exist on macOS.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 4 MiB: large enough to keep USB writes near line rate, small enough that
/// progress stays responsive.
const BUF_SIZE: usize = 4 * 1024 * 1024;

/// How often progress is reported. Fast enough to feel live, slow enough not
/// to flood the GUI's event channel.
const TICK: Duration = Duration::from_millis(250);

/// Largest zstd window we will decode.
///
/// ruzstd defaults to a 100 MB cap and, since 0.9, applies it to the *first*
/// frame as well as later ones — which rejects images written with
/// `zstd --long` (windowLog 27 is already 128 MiB). 4 GiB covers everything
/// the reference encoder emits. The window buffer grows on demand, so a larger
/// ceiling costs nothing until a frame actually needs it, and anything past it
/// still fails in `open_image`, before the device has been touched.
const MAX_ZSTD_WINDOW: u64 = 4 * 1024 * 1024 * 1024;

/// Which long-running stage a [`Progress`] update belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Write,
    Verify,
    Copy,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Write => "write",
            Stage::Verify => "verify",
            Stage::Copy => "copy",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub stage: Stage,
    pub bytes: u64,
    pub total: u64,
    pub bytes_per_sec: u64,
}

impl Progress {
    pub fn percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.bytes as f64 * 100.0 / self.total as f64).min(100.0)
        }
    }

    /// Seconds remaining at the current rate, or `None` before a rate is known.
    pub fn eta_secs(&self) -> Option<u64> {
        if self.bytes_per_sec == 0 || self.bytes > self.total {
            return None;
        }
        Some((self.total - self.bytes) / self.bytes_per_sec)
    }
}

/// Lowercase hex, for displaying a digest.
pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compression wrapping a disk image, identified by magic bytes rather than
/// file extension — plenty of images are served as `ubuntu.iso` while actually
/// being gzip, and extensions lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    Lzma,
    Zip,
    Lzw,
    VhdFixed,
}

impl Compression {
    pub fn as_str(self) -> &'static str {
        match self {
            Compression::None => "raw",
            Compression::Gzip => "gzip",
            Compression::Xz => "xz",
            Compression::Zstd => "zstd",
            Compression::Bzip2 => "bzip2",
            Compression::Lzma => "lzma",
            Compression::Zip => "zip",
            Compression::Lzw => "compress",
            Compression::VhdFixed => "vhd",
        }
    }

    /// Identify compression from a file's leading bytes.
    pub fn sniff(magic: &[u8]) -> Compression {
        if magic.starts_with(&[0x1f, 0x8b]) {
            Compression::Gzip
        } else if magic.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Compression::Xz
        } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Compression::Zstd
        } else if magic.starts_with(b"BZh") {
            Compression::Bzip2
        } else if magic.starts_with(&crate::lzw::MAGIC) {
            Compression::Lzw
        // "PK\x03\x04" is a local file header; "PK\x05\x06" is the end-of-central-
        // directory record an archive with no members starts with. Recognising
        // the second lets us say "this zip is empty" instead of writing the
        // archive itself to the device.
        } else if magic.starts_with(b"PK\x03\x04") || magic.starts_with(b"PK\x05\x06") {
            Compression::Zip
        } else if looks_like_lzma_alone(magic) {
            Compression::Lzma
        } else {
            Compression::None
        }
    }
}

impl Compression {
    /// Was this settled by a magic number, rather than guessed from a header
    /// that could belong to something else?
    fn has_magic(self) -> bool {
        !matches!(self, Compression::None | Compression::Lzma)
    }

    /// Do the bytes reach the device unchanged, so that the size on disk is
    /// also the size that lands? True for a raw image, and for a fixed VHD once
    /// its trailing footer is trimmed.
    pub fn is_verbatim(self) -> bool {
        matches!(self, Compression::None | Compression::VhdFixed)
    }
}

/// Largest LZMA dictionary we will accept, and the basis of the memory bound
/// below. Real encoders top out at 64 MiB (`lzma -9`), so 1 GiB is generous.
const MAX_LZMA_DICT: u32 = 1 << 30;

/// Smallest dictionary the LZMA format permits.
const MIN_LZMA_DICT: u32 = 1 << 12;

/// Memory ceiling handed to liblzma: the largest dictionary we accept, plus
/// headroom for the decoder's own structures.
const LZMA_MEMLIMIT: u64 = (MAX_LZMA_DICT as u64) * 2;

/// Does this look like a headerless LZMA ("alone") stream?
///
/// This is the one format here with **no magic number**. Its 13-byte header is
/// a properties byte, a u32 dictionary size and a u64 uncompressed size — all
/// of which a raw disk image can match by accident. Getting this wrong is not
/// a cosmetic bug: liblzma decodes a zero-filled header to an *empty stream
/// without erroring*, so a false positive would wipe the target, write nothing,
/// and report success. Detection is therefore deliberately strict, and every
/// check below earns its place against a real image format:
///
/// | header                    | rejected by              |
/// |---------------------------|--------------------------|
/// | ISO9660 / ext4 / MBR / GPT (13 zero bytes) | dictionary size 0 |
/// | FAT32 / NTFS / exFAT (`eb ..` jump)        | properties byte ≥ 225 |
/// | squashfs, VMDK, QCOW2, DMG, HFS+, ELF      | dictionary not a power of two |
///
/// The power-of-two requirement is the load-bearing one: a plain range check
/// admits VMDK and QCOW2. Every real encoder emits a power of two (verified
/// across `lzma -0..-9` and `xz --format=lzma`), so this costs us nothing.
fn looks_like_lzma_alone(magic: &[u8]) -> bool {
    if magic.len() < 13 {
        return false;
    }
    // (pb * 5 + lp) * 9 + lc, with lc < 9, lp < 5, pb < 5.
    if magic[0] >= 225 {
        return false;
    }
    let dict = u32::from_le_bytes([magic[1], magic[2], magic[3], magic[4]]);
    if !dict.is_power_of_two() || !(MIN_LZMA_DICT..=MAX_LZMA_DICT).contains(&dict) {
        return false;
    }
    // Either "unknown" or a size we could plausibly write to a USB stick.
    let uncompressed = u64::from_le_bytes([
        magic[5], magic[6], magic[7], magic[8], magic[9], magic[10], magic[11], magic[12],
    ]);
    uncompressed == u64::MAX || uncompressed <= MAX_PLAUSIBLE_IMAGE
}

/// 4 TiB. Larger than any bootable image, smaller than the random u64 an
/// unrelated file's bytes would produce.
const MAX_PLAUSIBLE_IMAGE: u64 = 4 << 40;

/// Fill `buf` as far as the file allows, returning how many bytes were read.
///
/// A single `read` is permitted to return fewer bytes than asked for. The
/// magic-number formats tolerate that, but LZMA detection needs all 13 of its
/// header bytes — a short read would silently downgrade a `.lzma` image to
/// "raw" and write the compressed bytes to the device.
fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            got => n += got,
        }
    }
    Ok(n)
}

/// A VHD hard-disk footer.
///
/// Microsoft, *Virtual Hard Disk Image Format Specification* v1.0, 11 October
/// 2006. Every field is **big-endian**, and the footer sits at the END of the
/// file — which is why detection cannot be a head sniff like every other
/// format here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VhdFooter {
    /// 512, or 511 for images written before Virtual PC 2004.
    footer_len: u64,
    disk_type: u32,
}

impl VhdFooter {
    const FIXED: u32 = 2;
    const DYNAMIC: u32 = 3;
    const DIFFERENCING: u32 = 4;
    const COOKIE: &'static [u8; 8] = b"conectix";

    /// Ones-complement of the 32-bit wrapping sum of the footer, with the
    /// checksum field itself read as zero. Straight from the spec's appendix.
    fn checksum(footer: &[u8]) -> u32 {
        let mut sum: u32 = 0;
        for (i, byte) in footer.iter().enumerate() {
            if (64..68).contains(&i) {
                continue;
            }
            sum = sum.wrapping_add(u32::from(*byte));
        }
        !sum
    }

    /// Parse the last 512 bytes of a file as a footer, if that is what they are.
    ///
    /// Four independent conditions must hold — the cookie, the stored checksum,
    /// a major version of 1, and the reserved feature bit. A raw disk image
    /// that happens to contain the word "conectix" in its last sector will not
    /// survive the checksum, so this is not a heuristic in the way the LZMA
    /// header check is.
    fn parse_tail(tail: &[u8; 512], file_size: u64) -> Option<VhdFooter> {
        // Pre-2004 images have a 511-byte footer. The two can never both match:
        // a 512-byte footer read at -511 starts "onectix", and a 511-byte one
        // read at -512 starts with a payload byte.
        let (footer, footer_len) = if tail[..8] == *Self::COOKIE {
            (&tail[..512], 512u64)
        } else if tail[1..9] == *Self::COOKIE {
            (&tail[1..512], 511u64)
        } else {
            return None;
        };
        if file_size <= footer_len {
            // A bare footer with no disk behind it is not an image.
            return None;
        }
        let stored = u32::from_be_bytes([footer[64], footer[65], footer[66], footer[67]]);
        if stored != Self::checksum(footer) {
            return None;
        }
        let version = u32::from_be_bytes([footer[12], footer[13], footer[14], footer[15]]);
        if version >> 16 != 1 {
            return None;
        }
        let features = u32::from_be_bytes([footer[8], footer[9], footer[10], footer[11]]);
        if features & 0x2 == 0 {
            return None;
        }
        Some(VhdFooter {
            footer_len,
            disk_type: u32::from_be_bytes([footer[60], footer[61], footer[62], footer[63]]),
        })
    }
}

/// Read and validate the trailing VHD footer, if there is one.
fn vhd_footer(f: &mut File, file_size: u64) -> Result<Option<VhdFooter>> {
    if file_size < 512 {
        return Ok(None);
    }
    f.seek(SeekFrom::End(-512))?;
    let mut tail = [0u8; 512];
    read_full(f, &mut tail)?;
    Ok(VhdFooter::parse_tail(&tail, file_size))
}

/// Detect how `path` is encoded.
///
/// Layered, and the order is load-bearing. A real magic number wins outright.
/// Failing that, the VHD footer is checked *before* the LZMA-alone guess,
/// because a footer is confirmed by a checksum over 512 bytes while the LZMA
/// header is only a plausibility test — and a VHD's payload begins with
/// whatever filesystem it holds, which could pass that test.
pub fn detect_compression(path: &Path) -> Result<Compression> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // 16 rather than 8: the LZMA-alone header is 13 bytes and has no magic
    // number, so all of it is needed to tell it from a raw image.
    let mut magic = [0u8; 16];
    let n = read_full(&mut f, &mut magic).with_context(|| format!("reading {}", path.display()))?;
    let head = Compression::sniff(&magic[..n]);
    if head.has_magic() {
        return Ok(head);
    }

    let file_size = f.metadata()?.len();
    if let Some(footer) = vhd_footer(&mut f, file_size)
        .with_context(|| format!("reading the trailer of {}", path.display()))?
    {
        return match footer.disk_type {
            VhdFooter::FIXED => Ok(Compression::VhdFixed),
            // Refused here, at image-selection time, rather than after a target
            // device has been chosen and wiped.
            VhdFooter::DYNAMIC => bail!(
                "{} is a dynamic VHD, which stores its data in scattered blocks. \
                 Only fixed-size VHDs can be written so far — convert it with \
                 `qemu-img convert -O vpc -o subformat=fixed`",
                path.display()
            ),
            VhdFooter::DIFFERENCING => bail!(
                "{} is a differencing VHD, which holds only the changes against \
                 a parent disk and cannot be written on its own",
                path.display()
            ),
            other => bail!("{} is a VHD of unsupported type {other}", path.display()),
        };
    }
    Ok(head)
}

/// Counts bytes pulled from the file underneath a decompressor.
///
/// The decompressed size of a compressed image is not knowable up front, so
/// progress is reported against how much of the *compressed* file has been
/// consumed — which is both known and monotonic.
#[derive(Clone, Default)]
pub struct ByteCounter(Arc<AtomicU64>);

impl ByteCounter {
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Either the whole file, or the slice of it holding one zip member.
enum EitherSource {
    Whole(File),
    Member(std::io::Take<File>),
}

impl Read for EitherSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            EitherSource::Whole(f) => f.read(buf),
            EitherSource::Member(t) => t.read(buf),
        }
    }
}

struct Counted<R> {
    inner: R,
    counter: ByteCounter,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counter.0.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// A reader that can hand back one byte it has already taken.
///
/// Needed to tell "the frame ended and the file ended with it" from "another
/// frame follows", without swallowing the byte that would begin it.
struct Peek<R> {
    inner: R,
    head: Option<u8>,
}

impl<R: Read> Peek<R> {
    /// Is there at least one more byte? Reads it if so, and holds on to it.
    fn more(&mut self) -> std::io::Result<bool> {
        if self.head.is_some() {
            return Ok(true);
        }
        let mut byte = [0u8; 1];
        loop {
            return match self.inner.read(&mut byte) {
                Ok(0) => Ok(false),
                Ok(_) => {
                    self.head = Some(byte[0]);
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => Err(e),
            };
        }
    }
}

impl<R: Read> Read for Peek<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(byte) = self.head.take() {
            buf[0] = byte;
            return Ok(1);
        }
        self.inner.read(buf)
    }
}

type ZstdFrame<R> = ruzstd::decoding::StreamingDecoder<Peek<R>, ruzstd::decoding::FrameDecoder>;

fn open_zstd_frame<R: Read>(source: Peek<R>) -> Result<ZstdFrame<R>> {
    ruzstd::decoding::StreamingDecoder::new_with_max_window_size(source, MAX_ZSTD_WINDOW)
        .map_err(|e| anyhow::anyhow!("not a readable zstd image: {e}"))
}

/// Decodes **every** frame of a zstd stream, not only the first.
///
/// A `.zst` file is a sequence of frames, and two of them concatenated is a
/// valid archive that `zstd -d` unpacks whole. ruzstd's `StreamingDecoder`
/// stops at the end of one frame and reports a clean end of file, so the
/// remaining frames were simply never written.
///
/// The other decoders here have their own version of this problem and their own
/// fix; zstd is the one with no ready-made multi-stream reader.
struct ZstdFrames<R: Read> {
    frame: Option<ZstdFrame<R>>,
}

impl<R: Read> ZstdFrames<R> {
    fn new(source: R) -> Result<ZstdFrames<R>> {
        let peek = Peek {
            inner: source,
            head: None,
        };
        Ok(ZstdFrames {
            frame: Some(open_zstd_frame(peek)?),
        })
    }
}

impl<R: Read> Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let Some(frame) = self.frame.as_mut() else {
                return Ok(0);
            };
            match frame.read(buf)? {
                0 => {}
                n => return Ok(n),
            }
            // This frame is spent; start the next one if the file holds one.
            let mut source = self.frame.take().expect("frame was Some").into_inner();
            if !source.more()? {
                return Ok(0);
            }
            self.frame = Some(open_zstd_frame(source).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?);
        }
    }
}

/// Where the payload of the zip member we intend to write actually lives.
struct ZipMember {
    /// Byte offset of the member's data, past its local header.
    data_start: u64,
    /// Bytes on disk. Doubles as the progress total and as the bound that stops
    /// a corrupt member from reading into whatever follows it.
    compressed_size: u64,
    method: zip::CompressionMethod,
    /// Checked because the raw scan below does not decrypt.
    encrypted: bool,
    name: String,
}

/// Pick the member to write out of a zip: the largest one, measured
/// **uncompressed**.
///
/// Uncompressed is the size that matters — it is what lands on the device, and
/// a disk image compresses far better than the README, signature or checksum
/// file packed beside it, so ranking by size on disk reliably picks the wrong
/// entry.
///
/// (Rufus's bled takes the *first* member rather than the largest. That is a
/// deliberate divergence, not an oversight: on an archive whose image sits
/// second, the first-member rule writes the README.)
///
/// The scan uses `by_index_raw`, which reads the central directory without
/// building a decoder for each entry. `by_index` would construct a real
/// decryptor and decompressor per entry, so a single sibling the crate cannot
/// handle — one encrypted note, one bzip2-compressed README — aborts the whole
/// archive even though the image beside it is perfectly writable. The cost is
/// that `by_index_raw` will happily stream ciphertext, so the chosen member is
/// checked for encryption explicitly below.
///
/// Only the central directory is read here; the archive is dropped before any
/// payload streams, which is what lets the returned reader be owned rather than
/// borrowed from it.
fn locate_zip_member(file: &mut File, path: &Path, file_len: u64) -> Result<ZipMember> {
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| anyhow::anyhow!("not a readable zip image: {e}"))?;
    if archive.is_empty() {
        bail!(
            "{} is an empty zip — there is no image inside it to write",
            path.display()
        );
    }

    let mut best: Option<(u64, ZipMember)> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| anyhow::anyhow!("reading the index of {}: {e}", path.display()))?;
        if entry.is_dir() {
            continue;
        }
        let size = entry.size();
        if best
            .as_ref()
            .is_some_and(|(best_size, _)| size <= *best_size)
        {
            continue;
        }
        let name = entry.name().to_string();
        // Absent only if the local header was never located. Skipping the entry
        // would quietly fall back to a smaller member and write the wrong
        // payload, so this is fatal.
        let data_start = entry
            .data_start()
            .with_context(|| format!("{name} in {} has no local header", path.display()))?;
        best = Some((
            size,
            ZipMember {
                data_start,
                compressed_size: entry.compressed_size(),
                method: entry.compression(),
                encrypted: entry.encrypted(),
                name,
            },
        ));
    }

    let Some((size, member)) = best else {
        bail!(
            "{} holds only directories — there is no image inside it to write",
            path.display()
        );
    };

    // `by_index_raw` does not decrypt, and would hand us ciphertext to write to
    // the device verbatim.
    if member.encrypted {
        bail!(
            "{} in {} is encrypted, and Sirius Flash cannot unlock it",
            member.name,
            path.display()
        );
    }
    if !matches!(
        member.method,
        zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
    ) {
        // Display, not Debug: with only the deflate feature enabled, Debug
        // renders bzip2 as `Unsupported(12)` while Display names it.
        bail!(
            "{} in {} is {}-compressed, which is not supported inside a zip \
             — re-pack it as deflate or store",
            member.name,
            path.display(),
            member.method
        );
    }
    // A truncated archive would otherwise be discovered by the decoder partway
    // through the write, with the drive already half-overwritten.
    let end = member.data_start.saturating_add(member.compressed_size);
    if end > file_len {
        bail!(
            "{} is truncated: {} needs {end} bytes but the file is {file_len}",
            path.display(),
            member.name
        );
    }
    if size == 0 {
        bail!(
            "{} in {} is empty — there is nothing to write",
            member.name,
            path.display()
        );
    }
    Ok(member)
}

/// Open an image for reading, transparently decompressing it.
///
/// Returns the reader, the compressed size on disk, the detected compression,
/// and a counter tracking consumption of the underlying file.
pub fn open_image(path: &Path) -> Result<(Box<dyn Read>, u64, Compression, ByteCounter)> {
    let compression = detect_compression(path)?;
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut size = file.metadata()?.len();
    if size == 0 {
        bail!("image {} is empty", path.display());
    }
    let counter = ByteCounter::default();

    // A zip is the one container whose payload does not start at byte zero, so
    // the file is positioned on the chosen member and bounded to it before the
    // counter is attached. Progress is then reported against that member rather
    // than the whole archive, which is what makes it finish at 100%.
    let mut zip_method = zip::CompressionMethod::Stored;
    let source = if compression == Compression::VhdFixed {
        // A fixed VHD is a raw disk image with a 512-byte footer bolted on the
        // end. Writing the footer to the device would append 512 bytes of
        // metadata past the last sector of the filesystem, so it is trimmed --
        // and `size` becomes the payload, which keeps progress ending at 100%
        // rather than at 99.95%.
        let footer = vhd_footer(&mut file, size)?
            .with_context(|| format!("re-reading the footer of {}", path.display()))?;
        size -= footer.footer_len;
        file.rewind()
            .with_context(|| format!("rewinding {}", path.display()))?;
        EitherSource::Member(file.take(size))
    } else if compression == Compression::Zip {
        let member = locate_zip_member(&mut file, path, size)?;
        file.seek(SeekFrom::Start(member.data_start))
            .with_context(|| format!("seeking to {} in {}", member.name, path.display()))?;
        size = member.compressed_size;
        zip_method = member.method;
        // `take` is a bound, not a convenience: without it a member whose length
        // is understated would keep reading into the next member and the central
        // directory.
        EitherSource::Member(file.take(member.compressed_size))
    } else {
        EitherSource::Whole(file)
    };

    let counted = Counted {
        inner: source,
        counter: counter.clone(),
    };

    let reader: Box<dyn Read> = match compression {
        Compression::Zip => match zip_method {
            zip::CompressionMethod::Stored => Box::new(counted),
            // Zip members hold *raw* deflate, with no zlib wrapper, so this is
            // DeflateDecoder and never ZlibDecoder.
            _ => Box::new(flate2::read::DeflateDecoder::new(counted)),
        },
        Compression::None => Box::new(counted),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(counted)),
        // `new_multi_decoder`, not `new`: concatenated .xz streams form a valid
        // .xz file that `xz -d` unpacks in full, and the single-stream decoder
        // stops after the first without saying so.
        Compression::Xz => Box::new(liblzma::read::XzDecoder::new_multi_decoder(counted)),
        // `MultiBzDecoder`, not `BzDecoder`: pbzip2 and friends emit several
        // concatenated streams, and `bunzip2` unpacks the lot.
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(counted)),
        Compression::Lzw => Box::new(
            crate::lzw::Decoder::new(counted)
                .map_err(|e| anyhow::anyhow!("not a readable compress (.Z) image: {e}"))?,
        ),
        // The LZMA-alone container, as produced by `lzma` and `xz --format=lzma`.
        // The memory limit is derived from the dictionary bound that `sniff`
        // already enforces, so a malformed header cannot make liblzma allocate
        // without bound. Both checks are deliberate: neither relies on the other.
        Compression::Lzma => Box::new(liblzma::read::XzDecoder::new_stream(
            counted,
            liblzma::stream::Stream::new_lzma_decoder(LZMA_MEMLIMIT)
                .map_err(|e| anyhow::anyhow!("not a readable lzma image: {e}"))?,
        )),
        Compression::Zstd => Box::new(ZstdFrames::new(counted)?),
        // Already trimmed to the payload above; the bytes themselves are raw.
        Compression::VhdFixed => Box::new(counted),
    };
    Ok((reader, size, compression, counter))
}

/// How many bytes this image will put on the device, when that is knowable
/// before decoding it.
///
/// `None` for anything compressed: the decompressed length is not in the
/// container, so the only thing that catches an oversized image is ENOSPC
/// partway through the write. A raw image, and a fixed VHD minus its footer,
/// can be checked up front — which is the difference between refusing the job
/// and wiping a drive before finding out.
pub fn payload_len(path: &Path) -> Result<Option<u64>> {
    let compression = detect_compression(path)?;
    if !compression.is_verbatim() {
        return Ok(None);
    }
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = f.metadata()?.len();
    Ok(Some(match compression {
        Compression::VhdFixed => match vhd_footer(&mut f, size)? {
            Some(footer) => size - footer.footer_len,
            None => size,
        },
        _ => size,
    }))
}

/// What a write actually produced.
#[derive(Debug)]
pub struct WriteOutcome {
    /// SHA-256 of the bytes placed on the device (decompressed, if applicable).
    pub digest: [u8; 32],
    /// How many bytes were written — the decompressed length.
    pub bytes_written: u64,
    pub compression: Compression,
}

/// Report one progress sample.
fn emit(
    started: &Instant,
    at: u64,
    total: u64,
    stage: Stage,
    on_progress: &mut dyn FnMut(Progress),
) {
    let secs = started.elapsed().as_secs_f64();
    on_progress(Progress {
        stage,
        bytes: at,
        total,
        bytes_per_sec: if secs > 0.0 {
            (at as f64 / secs) as u64
        } else {
            0
        },
    });
}

/// Drive a read/write loop, hashing as it goes and reporting progress.
///
/// `sink` receives each chunk; returning the total lets the caller decide
/// whether it is writing to a device or only reading one back.
fn stream<R, S>(
    mut src: R,
    total: u64,
    stage: Stage,
    on_progress: &mut dyn FnMut(Progress),
    // When set, report progress from this counter rather than from the number
    // of bytes produced — needed for compressed input, whose output size is
    // unknown until the last byte.
    track: Option<&ByteCounter>,
    mut sink: S,
) -> Result<[u8; 32]>
where
    R: Read,
    S: FnMut(&[u8]) -> Result<()>,
{
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF_SIZE];
    let mut done: u64 = 0;
    let started = Instant::now();
    let mut last_tick = Instant::now();

    loop {
        let n = src.read(&mut buf).context("reading image")?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        sink(chunk)?;
        hasher.update(chunk);
        done += n as u64;

        if last_tick.elapsed() >= TICK {
            emit(
                &started,
                track.map_or(done, |c| c.get()),
                total,
                stage,
                on_progress,
            );
            last_tick = Instant::now();
        }
    }

    emit(
        &started,
        track.map_or(done, |c| c.get()),
        total,
        stage,
        on_progress,
    );
    Ok(hasher.finalize().into())
}

/// Write `image` to the block device at `dest`, decompressing on the fly if
/// it is gzip, xz, zstd or bzip2.
///
/// The returned digest covers the bytes actually placed on the device (i.e.
/// decompressed), which is what verification must compare against. Hashing
/// rides the same buffers, so it costs no extra I/O.
pub fn write_image(
    image: &Path,
    dest: &Path,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<WriteOutcome> {
    let (reader, compressed_size, compression, counter) = open_image(image)?;
    let mut out = OpenOptions::new()
        .write(true)
        .open(dest)
        .with_context(|| format!("opening {} for writing", dest.display()))?;

    let mut written = 0u64;
    // Verbatim input produces exactly as many bytes as it consumes, so the
    // output count is already the right progress signal. Everything else has an
    // output size that is unknown until the last byte, so progress is reported
    // against how much of the *input* has been consumed instead.
    let track = if compression.is_verbatim() {
        None
    } else {
        Some(&counter)
    };
    let digest = stream(
        reader,
        compressed_size,
        Stage::Write,
        on_progress,
        track,
        |chunk| {
            written += chunk.len() as u64;
            out.write_all(chunk).map_err(|e| {
                // ENOSPC. A compressed image's real size is unknown until it is
                // unpacked, so this is the first moment we can detect it.
                if e.raw_os_error() == Some(28) {
                    anyhow::anyhow!(
                        "{} ran out of space after {written} bytes — the image is larger than the drive",
                        dest.display()
                    )
                } else {
                    anyhow::Error::new(e).context("writing to device")
                }
            })
        },
    )?;

    // A decoder that yields nothing is not a successful write of an empty
    // image — the file was non-empty, so this is a container we misread or one
    // that was truncated in transit. Without this the device would be wiped,
    // nothing written, and success reported along with the SHA-256 of no bytes
    // at all. liblzma makes the risk concrete: it answers a zero-filled header
    // with an empty stream and no error whatsoever.
    if written == 0 {
        bail!(
            "{} decoded to zero bytes — it is not a usable {} image",
            image.display(),
            compression.as_str()
        );
    }

    // Push everything to the medium before anyone calls this done.
    out.flush().context("flushing device")?;
    out.sync_all().context("syncing device")?;
    Ok(WriteOutcome {
        digest,
        bytes_written: written,
        compression,
    })
}

/// Re-read the first `len` bytes of `dest` and confirm they hash to `expected`.
///
/// The page cache is dropped first, otherwise this would verify RAM rather
/// than the device and pass even for a stick that never took the data.
pub fn verify_written(
    dest: &Path,
    expected: &[u8; 32],
    len: u64,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    drop_cache(dest);
    let src =
        File::open(dest).with_context(|| format!("reopening {} to verify", dest.display()))?;
    let actual = stream(src.take(len), len, Stage::Verify, on_progress, None, |_| {
        Ok(())
    })?;
    if actual != *expected {
        bail!(
            "verification FAILED: device reads back as {} but {} was written \
             — the drive may be faulty, counterfeit, or was removed early",
            hex(&actual),
            hex(expected)
        );
    }
    Ok(())
}

/// Ask the kernel to forget its cached view of a block device.
#[cfg(target_os = "linux")]
fn drop_cache(dest: &Path) {
    let _ = std::process::Command::new("blockdev")
        .arg("--flushbufs")
        .arg(dest)
        .status();
}

#[cfg(not(target_os = "linux"))]
fn drop_cache(_dest: &Path) {}

/// Does `rel` name exactly this excluded path? Compared case-insensitively,
/// because ISO9660 and UDF disagree about the case of the same filename.
fn excluded(rel: &Path, exclude: &[String]) -> bool {
    let r = rel.to_string_lossy().replace('\\', "/").to_lowercase();
    exclude.iter().any(|e| e.to_lowercase() == r)
}

/// Every regular file under `root` (relative paths), the directories that hold
/// them, and the total byte count — so progress can be a real percentage
/// rather than a spinner.
#[allow(clippy::type_complexity)]
fn collect_tree(
    root: &Path,
    exclude: &[String],
) -> Result<(Vec<PathBuf>, Vec<(PathBuf, u64)>, u64)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![PathBuf::new()];

    while let Some(rel_dir) = stack.pop() {
        let abs = root.join(&rel_dir);
        for entry in fs::read_dir(&abs).with_context(|| format!("reading {}", abs.display()))? {
            let entry = entry?;
            let rel = rel_dir.join(entry.file_name());
            if excluded(&rel, exclude) {
                continue;
            }
            let ft = entry.file_type()?;
            if ft.is_dir() {
                dirs.push(rel.clone());
                stack.push(rel);
            } else if ft.is_file() {
                let len = entry.metadata()?.len();
                total += len;
                files.push((rel, len));
            }
            // Symlinks are skipped on purpose: FAT32 cannot represent them and
            // Windows install media does not use them.
        }
    }
    Ok((dirs, files, total))
}

/// Recursively copy `src` into `dst`, skipping `exclude` (paths relative to
/// `src`, e.g. `sources/install.wim`), reporting progress as it goes.
///
/// Replaces `rsync`, which reported nothing without `--info=progress2` and left
/// the flagship Windows path looking frozen for minutes. Ownership and
/// permissions are deliberately not preserved — the destination is FAT32,
/// which cannot store them.
pub fn copy_tree(
    src: &Path,
    dst: &Path,
    exclude: &[String],
    on_progress: &mut dyn FnMut(Progress),
) -> Result<u64> {
    let (dirs, files, total) = collect_tree(src, exclude)?;
    for d in &dirs {
        fs::create_dir_all(dst.join(d))
            .with_context(|| format!("creating {}", dst.join(d).display()))?;
    }

    let mut buf = vec![0u8; BUF_SIZE];
    let mut done = 0u64;
    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut tick = |done: u64, force: bool, on: &mut dyn FnMut(Progress)| {
        if force || last_tick.elapsed() >= TICK {
            let secs = started.elapsed().as_secs_f64();
            on(Progress {
                stage: Stage::Copy,
                bytes: done,
                total,
                bytes_per_sec: if secs > 0.0 {
                    (done as f64 / secs) as u64
                } else {
                    0
                },
            });
            last_tick = Instant::now();
        }
    };

    for (rel, _) in &files {
        let from = src.join(rel);
        let to = dst.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut r = File::open(&from).with_context(|| format!("reading {}", from.display()))?;
        let mut w = File::create(&to).with_context(|| format!("writing {}", to.display()))?;
        loop {
            let n = r
                .read(&mut buf)
                .with_context(|| format!("reading {}", from.display()))?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])
                .with_context(|| format!("writing {}", to.display()))?;
            done += n as u64;
            tick(done, false, on_progress);
        }
        w.flush()?;
    }
    tick(done, true, on_progress);
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(_: Progress) {}

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sirius-blockio-{name}"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn writes_and_hashes_a_file() {
        let src = tmp("src");
        let dst = tmp("dst");
        std::fs::write(&src, b"sirius flash").unwrap();
        std::fs::write(&dst, b"").unwrap();
        let o = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"sirius flash");
        assert_eq!(o.bytes_written, 12);
        assert_eq!(o.compression, Compression::None);
        assert_eq!(hex(&o.digest).len(), 64);
        verify_written(&dst, &o.digest, o.bytes_written, &mut noop).unwrap();
    }

    #[test]
    fn verification_catches_corruption() {
        let src = tmp("src2");
        let dst = tmp("dst2");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let o = write_image(&src, &dst, &mut noop).unwrap();
        // Simulate a drive that silently stored something else.
        std::fs::write(&dst, vec![9u8; 4096]).unwrap();
        let err = verify_written(&dst, &o.digest, 4096, &mut noop).unwrap_err();
        assert!(err.to_string().contains("verification FAILED"));
    }

    #[test]
    fn empty_image_is_rejected() {
        let src = tmp("empty");
        let dst = tmp("dstempty");
        std::fs::write(&src, b"").unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert!(write_image(&src, &dst, &mut noop).is_err());
    }

    #[test]
    fn final_progress_reports_completion() {
        let src = tmp("prog");
        let dst = tmp("progdst");
        // Two full buffers plus a remainder, so the loop iterates.
        std::fs::write(&src, vec![3u8; BUF_SIZE * 2 + 17]).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let mut seen: Vec<Progress> = Vec::new();
        let o = write_image(&src, &dst, &mut |p| seen.push(p)).unwrap();
        let last = seen.last().expect("at least one progress report");
        assert_eq!(last.stage, Stage::Write);
        assert_eq!(last.bytes, last.total, "final report must show completion");
        assert_eq!(last.bytes, (BUF_SIZE * 2 + 17) as u64);
        assert!((last.percent() - 100.0).abs() < 0.001);

        // And verification reports against the same total.
        let mut vseen: Vec<Progress> = Vec::new();
        verify_written(&dst, &o.digest, last.total, &mut |p| vseen.push(p)).unwrap();
        assert_eq!(vseen.last().unwrap().stage, Stage::Verify);
        assert_eq!(vseen.last().unwrap().bytes, last.total);
    }

    // ---- compressed images ----
    //
    // Fixtures are real archives produced by the system compressors, so these
    // exercise actual decoding rather than just the plumbing.

    const PAYLOAD: &[u8] = b"SIRIUS-FLASH-COMPRESSION-TEST-PAYLOAD-0123456789";

    /// `gz` of PAYLOAD, produced by the system `gzip` tool.
    const GZ: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x08, 0xd4, 0x9c, 0xa7, 0x6a, 0x00, 0x03, 0x63, 0x66, 0x69, 0x78, 0x2e,
        0x62, 0x69, 0x6e, 0x00, 0x0b, 0xf6, 0x0c, 0xf2, 0x0c, 0x0d, 0xd6, 0x75, 0xf3, 0x71, 0x0c,
        0xf6, 0xd0, 0x75, 0xf6, 0xf7, 0x0d, 0x08, 0x72, 0x0d, 0x0e, 0xf6, 0xf4, 0xf7, 0xd3, 0x0d,
        0x71, 0x0d, 0x0e, 0xd1, 0x0d, 0x70, 0x8c, 0xf4, 0xf1, 0x77, 0x74, 0xd1, 0x35, 0x30, 0x34,
        0x32, 0x36, 0x31, 0x35, 0x33, 0xb7, 0xb0, 0x04, 0x00, 0xba, 0x2d, 0x30, 0xb2, 0x30, 0x00,
        0x00, 0x00,
    ];

    /// `xz` of PAYLOAD, produced by the system `xz` tool.
    const XZ: &[u8] = &[
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46, 0x04, 0xc0, 0x34,
        0x30, 0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6c, 0x13,
        0x5f, 0x61, 0x01, 0x00, 0x2f, 0x53, 0x49, 0x52, 0x49, 0x55, 0x53, 0x2d, 0x46, 0x4c, 0x41,
        0x53, 0x48, 0x2d, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53, 0x49, 0x4f, 0x4e, 0x2d,
        0x54, 0x45, 0x53, 0x54, 0x2d, 0x50, 0x41, 0x59, 0x4c, 0x4f, 0x41, 0x44, 0x2d, 0x30, 0x31,
        0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x00, 0xc0, 0x1a, 0x53, 0x26, 0x6d, 0x37,
        0x3f, 0xdf, 0x00, 0x01, 0x50, 0x30, 0xd3, 0xd8, 0xe4, 0xbc, 0x1f, 0xb6, 0xf3, 0x7d, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
    ];

    /// `zst` of PAYLOAD, produced by the system `zstd` tool.
    const ZST: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x30, 0x81, 0x01, 0x00, 0x53, 0x49, 0x52, 0x49, 0x55, 0x53,
        0x2d, 0x46, 0x4c, 0x41, 0x53, 0x48, 0x2d, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53,
        0x49, 0x4f, 0x4e, 0x2d, 0x54, 0x45, 0x53, 0x54, 0x2d, 0x50, 0x41, 0x59, 0x4c, 0x4f, 0x41,
        0x44, 0x2d, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0xd4, 0xf2, 0xfb,
        0xd4,
    ];

    /// `bz2` of PAYLOAD, produced by the system `bzip2` tool.
    const BZ2: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x23, 0xec, 0x01, 0x91, 0x00,
        0x00, 0x14, 0x9e, 0x00, 0x00, 0x02, 0x7f, 0xe0, 0x2f, 0x67, 0xde, 0x20, 0x20, 0x00, 0x22,
        0xa1, 0xea, 0x64, 0xf2, 0x8f, 0x28, 0xd0, 0xc6, 0x9a, 0x68, 0x51, 0xa1, 0xa0, 0x00, 0x00,
        0x12, 0x77, 0x6b, 0x46, 0x5c, 0x09, 0x73, 0xd1, 0xa7, 0x6b, 0x6b, 0x9f, 0xed, 0x30, 0xf7,
        0x40, 0x57, 0x28, 0x12, 0xb0, 0xe0, 0xc1, 0xd0, 0xf0, 0xf0, 0xd8, 0x65, 0x1c, 0x8b, 0xb9,
        0x22, 0x9c, 0x28, 0x48, 0x11, 0xf6, 0x00, 0xc8, 0x80,
    ];

    /// `lzma` of PAYLOAD, produced by the system `lzma` tool (64 MiB dictionary).
    const LZMA: &[u8] = &[
        0x5d, 0x00, 0x00, 0x00, 0x04, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x29,
        0x92, 0x46, 0x8a, 0x01, 0x1b, 0x6e, 0x7b, 0xec, 0x1f, 0x25, 0x45, 0xbc, 0xe9, 0xcf, 0x7c,
        0x2d, 0xcd, 0xe4, 0x2e, 0xdc, 0x4c, 0xf4, 0x1e, 0x36, 0x32, 0xb6, 0x5b, 0x67, 0x22, 0x83,
        0x2f, 0x68, 0x20, 0x11, 0x38, 0xd8, 0x07, 0x64, 0x4a, 0x81, 0xe6, 0x65, 0x71, 0x75, 0xa8,
        0xa1, 0xe8, 0x92, 0x5a, 0xbb, 0xff, 0xfc, 0xf3, 0x30, 0x00,
    ];

    /// A real `zip` from the system `zip` tool: three stored members, the
    /// largest of them (`1-image.img`, exactly PAYLOAD) in the middle rather
    /// than first.
    const ZIP: &[u8] = &[
        0x50, 0x4b, 0x03, 0x04, 0x0a, 0x00, 0x02, 0x00, 0x00, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x83,
        0x16, 0xdc, 0x8c, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00,
        0x30, 0x2d, 0x74, 0x69, 0x6e, 0x79, 0x2e, 0x74, 0x78, 0x74, 0x78, 0x50, 0x4b, 0x03, 0x04,
        0x0a, 0x00, 0x02, 0x00, 0x00, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0xba, 0x2d, 0x30, 0xb2, 0x30,
        0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x31, 0x2d, 0x69, 0x6d,
        0x61, 0x67, 0x65, 0x2e, 0x69, 0x6d, 0x67, 0x53, 0x49, 0x52, 0x49, 0x55, 0x53, 0x2d, 0x46,
        0x4c, 0x41, 0x53, 0x48, 0x2d, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53, 0x49, 0x4f,
        0x4e, 0x2d, 0x54, 0x45, 0x53, 0x54, 0x2d, 0x50, 0x41, 0x59, 0x4c, 0x4f, 0x41, 0x44, 0x2d,
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x50, 0x4b, 0x03, 0x04, 0x0a,
        0x00, 0x02, 0x00, 0x00, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x8c, 0xa6, 0x1b, 0x01, 0x05, 0x00,
        0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x32, 0x2d, 0x6e, 0x6f, 0x74,
        0x65, 0x73, 0x2e, 0x74, 0x78, 0x74, 0x6e, 0x6f, 0x74, 0x65, 0x73, 0x50, 0x4b, 0x01, 0x02,
        0x1e, 0x03, 0x0a, 0x00, 0x02, 0x00, 0x00, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x83, 0x16, 0xdc,
        0x8c, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x00, 0x00, 0x30, 0x2d, 0x74,
        0x69, 0x6e, 0x79, 0x2e, 0x74, 0x78, 0x74, 0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x0a, 0x00,
        0x02, 0x00, 0x00, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0xba, 0x2d, 0x30, 0xb2, 0x30, 0x00, 0x00,
        0x00, 0x30, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0xa4, 0x81, 0x29, 0x00, 0x00, 0x00, 0x31, 0x2d, 0x69, 0x6d, 0x61, 0x67, 0x65,
        0x2e, 0x69, 0x6d, 0x67, 0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x0a, 0x00, 0x02, 0x00, 0x00,
        0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x8c, 0xa6, 0x1b, 0x01, 0x05, 0x00, 0x00, 0x00, 0x05, 0x00,
        0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xa4,
        0x81, 0x82, 0x00, 0x00, 0x00, 0x32, 0x2d, 0x6e, 0x6f, 0x74, 0x65, 0x73, 0x2e, 0x74, 0x78,
        0x74, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x03, 0x00, 0xaa, 0x00,
        0x00, 0x00, 0xb0, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    /// A real `zip` in which the two ways to rank members disagree:
    /// `noisy.bin` is 64 incompressible bytes stored as 64, while
    /// `big-sparse.img` is 4096 zero bytes that deflate down to 20. Ranking by
    /// compressed size would pick the wrong one, and the winner is deflated,
    /// so this covers the deflate path and the ranking rule at once.
    const ZIP_DEFLATED: &[u8] = &[
        0x50, 0x4b, 0x03, 0x04, 0x0a, 0x00, 0x02, 0x00, 0x00, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x4f,
        0x2e, 0xd3, 0x72, 0x40, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00,
        0x6e, 0x6f, 0x69, 0x73, 0x79, 0x2e, 0x62, 0x69, 0x6e, 0x0d, 0xb4, 0x5b, 0x02, 0xa9, 0x50,
        0xf7, 0x9e, 0x45, 0xec, 0x93, 0x3a, 0xe1, 0x88, 0x2f, 0xd6, 0x7d, 0x24, 0xcb, 0x72, 0x19,
        0xc0, 0x67, 0x0e, 0xb5, 0x5c, 0x03, 0xaa, 0x51, 0xf8, 0x9f, 0x46, 0xed, 0x94, 0x3b, 0xe2,
        0x89, 0x30, 0xd7, 0x7e, 0x25, 0xcc, 0x73, 0x1a, 0xc1, 0x68, 0x0f, 0xb6, 0x5d, 0x04, 0xab,
        0x52, 0xf9, 0xa0, 0x47, 0xee, 0x95, 0x3c, 0xe3, 0x8a, 0x31, 0xd8, 0x7f, 0x26, 0x50, 0x4b,
        0x03, 0x04, 0x14, 0x00, 0x02, 0x00, 0x08, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x11, 0x00, 0x1c,
        0xc7, 0x14, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x62, 0x69,
        0x67, 0x2d, 0x73, 0x70, 0x61, 0x72, 0x73, 0x65, 0x2e, 0x69, 0x6d, 0x67, 0xed, 0xc1, 0x01,
        0x0d, 0x00, 0x00, 0x00, 0xc2, 0xa0, 0xf7, 0x4f, 0x6d, 0x0f, 0x07, 0x14, 0x00, 0x00, 0x00,
        0xf0, 0x6e, 0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x0a, 0x00, 0x02, 0x00, 0x00, 0x00, 0xf2,
        0x68, 0x2e, 0x5d, 0x4f, 0x2e, 0xd3, 0x72, 0x40, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
        0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa4, 0x81, 0x00,
        0x00, 0x00, 0x00, 0x6e, 0x6f, 0x69, 0x73, 0x79, 0x2e, 0x62, 0x69, 0x6e, 0x50, 0x4b, 0x01,
        0x02, 0x1e, 0x03, 0x14, 0x00, 0x02, 0x00, 0x08, 0x00, 0xf2, 0x68, 0x2e, 0x5d, 0x11, 0x00,
        0x1c, 0xc7, 0x14, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa4, 0x81, 0x67, 0x00, 0x00, 0x00, 0x62, 0x69,
        0x67, 0x2d, 0x73, 0x70, 0x61, 0x72, 0x73, 0x65, 0x2e, 0x69, 0x6d, 0x67, 0x50, 0x4b, 0x05,
        0x06, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x02, 0x00, 0x73, 0x00, 0x00, 0x00, 0xa7, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    /// PAYLOAD as a real Unix `compress` archive. Cross-checked against GNU
    /// `gzip`, which decodes `.Z`, so the fixture cannot quietly agree with a
    /// bug of our own. The decoder itself is exercised in `crate::lzw`.
    const DOT_Z: &[u8] = &[
        0x1f, 0x9d, 0x90, 0x53, 0x92, 0x48, 0x49, 0x52, 0x65, 0x4a, 0x0b, 0x23, 0x4c, 0x82, 0x4c,
        0x41, 0xd2, 0x62, 0xc8, 0x93, 0x26, 0x50, 0xa4, 0x14, 0x99, 0x12, 0xf0, 0x89, 0x93, 0x16,
        0x54, 0x26, 0x52, 0x69, 0x01, 0x25, 0x48, 0x16, 0x26, 0x4f, 0x82, 0x10, 0x69, 0x01, 0x23,
        0x86, 0x8c, 0x19, 0x34, 0x6a, 0xd8, 0xb8, 0x81, 0x23, 0x07,
    ];

    /// `zstd --long=27` of PAYLOAD: a 128 MiB window, well past the 100 MB
    /// default cap ruzstd began applying to the first frame in 0.9.
    const ZST_LONG_WINDOW: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x88, 0x81, 0x01, 0x00, 0x53, 0x49, 0x52, 0x49, 0x55, 0x53,
        0x2d, 0x46, 0x4c, 0x41, 0x53, 0x48, 0x2d, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53,
        0x49, 0x4f, 0x4e, 0x2d, 0x54, 0x45, 0x53, 0x54, 0x2d, 0x50, 0x41, 0x59, 0x4c, 0x4f, 0x41,
        0x44, 0x2d, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0xd4, 0xf2, 0xfb,
        0xd4,
    ];

    /// The second half of the concatenated fixtures below.
    const SECOND: &[u8] = b"-AND-A-SECOND-STREAM-CONCATENATED-ONTO-THE-FIRST";

    /// Two real `gzip` streams, concatenated — which is a valid archive
    /// that the reference tool unpacks in full.
    const CAT_GZ: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x0b, 0xf6, 0x0c, 0xf2, 0x0c,
        0x0d, 0xd6, 0x75, 0xf3, 0x71, 0x0c, 0xf6, 0xd0, 0x75, 0xf6, 0xf7, 0x0d, 0x08, 0x72, 0x0d,
        0x0e, 0xf6, 0xf4, 0xf7, 0xd3, 0x0d, 0x71, 0x0d, 0x0e, 0xd1, 0x0d, 0x70, 0x8c, 0xf4, 0xf1,
        0x77, 0x74, 0xd1, 0x35, 0x30, 0x34, 0x32, 0x36, 0x31, 0x35, 0x33, 0xb7, 0xb0, 0x04, 0x00,
        0xba, 0x2d, 0x30, 0xb2, 0x30, 0x00, 0x00, 0x00, 0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x02, 0x03, 0x0d, 0xc6, 0xc1, 0x09, 0x00, 0x30, 0x08, 0x03, 0xc0, 0x89, 0x32, 0x84,
        0xd4, 0x94, 0xf6, 0x51, 0x05, 0xcd, 0xfe, 0xb3, 0xd4, 0xc7, 0xc1, 0xc1, 0xc2, 0x61, 0x68,
        0xae, 0x9c, 0xb4, 0x8a, 0xf6, 0x30, 0x5f, 0x26, 0xc6, 0x70, 0x64, 0x28, 0xa1, 0x43, 0xec,
        0x5b, 0xad, 0x0f, 0x44, 0x94, 0x7c, 0xcc, 0x30, 0x00, 0x00, 0x00,
    ];

    /// Two real `xz` streams, concatenated — which is a valid archive
    /// that the reference tool unpacks in full.
    const CAT_XZ: &[u8] = &[
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46, 0x04, 0xc0, 0x34,
        0x30, 0x21, 0x01, 0x1c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa5, 0x83,
        0x7d, 0x76, 0x01, 0x00, 0x2f, 0x53, 0x49, 0x52, 0x49, 0x55, 0x53, 0x2d, 0x46, 0x4c, 0x41,
        0x53, 0x48, 0x2d, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53, 0x49, 0x4f, 0x4e, 0x2d,
        0x54, 0x45, 0x53, 0x54, 0x2d, 0x50, 0x41, 0x59, 0x4c, 0x4f, 0x41, 0x44, 0x2d, 0x30, 0x31,
        0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x00, 0xc0, 0x1a, 0x53, 0x26, 0x6d, 0x37,
        0x3f, 0xdf, 0x00, 0x01, 0x50, 0x30, 0xd3, 0xd8, 0xe4, 0xbc, 0x1f, 0xb6, 0xf3, 0x7d, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a, 0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04,
        0xe6, 0xd6, 0xb4, 0x46, 0x04, 0xc0, 0x36, 0x30, 0x21, 0x01, 0x1c, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x47, 0x13, 0x97, 0xe0, 0x00, 0x2f, 0x00, 0x2e, 0x5d,
        0x00, 0x16, 0x90, 0x45, 0xc4, 0x4e, 0x2a, 0x04, 0x07, 0x9e, 0x34, 0xd1, 0x24, 0x59, 0x7b,
        0x6e, 0x3a, 0xee, 0xf4, 0xa3, 0x7f, 0xb8, 0x24, 0xca, 0x25, 0xa5, 0x1f, 0x24, 0x32, 0x98,
        0xf4, 0xf2, 0x4c, 0xef, 0x49, 0xfa, 0xe6, 0x48, 0x46, 0x28, 0x12, 0xe8, 0xc2, 0x21, 0x02,
        0x25, 0x98, 0x00, 0x00, 0x00, 0xdb, 0xd8, 0xf6, 0xda, 0x87, 0x96, 0x98, 0x21, 0x00, 0x01,
        0x52, 0x30, 0x51, 0xba, 0xd2, 0x8e, 0x1f, 0xb6, 0xf3, 0x7d, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x04, 0x59, 0x5a,
    ];

    /// Two real `bzip2` streams, concatenated — which is a valid archive
    /// that the reference tool unpacks in full.
    const CAT_BZ2: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x23, 0xec, 0x01, 0x91, 0x00,
        0x00, 0x14, 0x9e, 0x00, 0x00, 0x02, 0x7f, 0xe0, 0x2f, 0x67, 0xde, 0x20, 0x20, 0x00, 0x22,
        0xa1, 0xea, 0x64, 0xf2, 0x8f, 0x28, 0xd0, 0xc6, 0x9a, 0x68, 0x51, 0xa1, 0xa0, 0x00, 0x00,
        0x12, 0x77, 0x6b, 0x46, 0x5c, 0x09, 0x73, 0xd1, 0xa7, 0x6b, 0x6b, 0x9f, 0xed, 0x30, 0xf7,
        0x40, 0x57, 0x28, 0x12, 0xb0, 0xe0, 0xc1, 0xd0, 0xf0, 0xf0, 0xd8, 0x65, 0x1c, 0x8b, 0xb9,
        0x22, 0x9c, 0x28, 0x48, 0x11, 0xf6, 0x00, 0xc8, 0x80, 0x42, 0x5a, 0x68, 0x39, 0x31, 0x41,
        0x59, 0x26, 0x53, 0x59, 0xf2, 0x17, 0x01, 0xf9, 0x00, 0x00, 0x00, 0x96, 0x00, 0x00, 0x02,
        0x2f, 0x63, 0x9c, 0x00, 0x20, 0x00, 0x22, 0x27, 0x92, 0x34, 0x66, 0x88, 0x40, 0x00, 0x07,
        0xc9, 0x87, 0xe0, 0x51, 0xb0, 0xbc, 0x0b, 0x90, 0x2a, 0xf1, 0x34, 0x0b, 0x5e, 0xa2, 0x1b,
        0x74, 0xd0, 0x70, 0xc8, 0x9e, 0xb1, 0x4f, 0xf1, 0x77, 0x24, 0x53, 0x85, 0x09, 0x0f, 0x21,
        0x70, 0x1f, 0x90,
    ];

    /// Two real `zstd` streams, concatenated — which is a valid archive
    /// that the reference tool unpacks in full.
    const CAT_ZST: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x30, 0x81, 0x01, 0x00, 0x53, 0x49, 0x52, 0x49, 0x55, 0x53,
        0x2d, 0x46, 0x4c, 0x41, 0x53, 0x48, 0x2d, 0x43, 0x4f, 0x4d, 0x50, 0x52, 0x45, 0x53, 0x53,
        0x49, 0x4f, 0x4e, 0x2d, 0x54, 0x45, 0x53, 0x54, 0x2d, 0x50, 0x41, 0x59, 0x4c, 0x4f, 0x41,
        0x44, 0x2d, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0xd4, 0xf2, 0xfb,
        0xd4, 0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x30, 0x35, 0x01, 0x00, 0x02, 0x83, 0x08, 0x0a, 0xb0,
        0x3d, 0x24, 0xbf, 0x86, 0x77, 0x67, 0x99, 0xc4, 0x41, 0x5f, 0x35, 0xf2, 0xd4, 0x9d, 0xc7,
        0xb4, 0xcc, 0x01, 0xe7, 0x10, 0x98, 0xe1, 0x83, 0xd1, 0xaf, 0x05, 0x33, 0x66, 0x0d, 0x16,
        0x04, 0x07, 0x00, 0x6d, 0x20, 0xd7, 0x73,
    ];

    /// A real `zip` holding a 4096-byte image next to a small *encrypted*
    /// note. The image is perfectly writable; only the sibling is locked.
    const ZIP_ENCRYPTED_SIBLING: &[u8] = &[
        0x50, 0x4b, 0x03, 0x04, 0x0a, 0x00, 0x09, 0x00, 0x00, 0x00, 0x88, 0x6b, 0x2e, 0x5d, 0x67,
        0x45, 0x26, 0xd7, 0x19, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0x09, 0x00, 0x1c, 0x00,
        0x6e, 0x6f, 0x74, 0x65, 0x73, 0x2e, 0x74, 0x78, 0x74, 0x55, 0x54, 0x09, 0x00, 0x03, 0x1f,
        0xb0, 0xa7, 0x6a, 0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0xe8, 0x03,
        0x00, 0x00, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x9c, 0x35, 0x0c, 0x7d, 0x26, 0x5e, 0x5b, 0x4a,
        0x20, 0x30, 0xae, 0xb1, 0x07, 0xc1, 0x80, 0xa9, 0x39, 0x1c, 0xef, 0xa8, 0xb8, 0xaa, 0x4e,
        0x7b, 0xd8, 0x50, 0x4b, 0x07, 0x08, 0x67, 0x45, 0x26, 0xd7, 0x19, 0x00, 0x00, 0x00, 0x0d,
        0x00, 0x00, 0x00, 0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x02, 0x00, 0x08, 0x00, 0x88, 0x6b,
        0x2e, 0x5d, 0x11, 0x00, 0x1c, 0xc7, 0x14, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x09,
        0x00, 0x1c, 0x00, 0x69, 0x6d, 0x61, 0x67, 0x65, 0x2e, 0x69, 0x6d, 0x67, 0x55, 0x54, 0x09,
        0x00, 0x03, 0x1f, 0xb0, 0xa7, 0x6a, 0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01,
        0x04, 0xe8, 0x03, 0x00, 0x00, 0x04, 0xe8, 0x03, 0x00, 0x00, 0xed, 0xc1, 0x01, 0x0d, 0x00,
        0x00, 0x00, 0xc2, 0xa0, 0xf7, 0x4f, 0x6d, 0x0f, 0x07, 0x14, 0x00, 0x00, 0x00, 0xf0, 0x6e,
        0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x0a, 0x00, 0x09, 0x00, 0x00, 0x00, 0x88, 0x6b, 0x2e,
        0x5d, 0x67, 0x45, 0x26, 0xd7, 0x19, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0x09, 0x00,
        0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x00,
        0x00, 0x6e, 0x6f, 0x74, 0x65, 0x73, 0x2e, 0x74, 0x78, 0x74, 0x55, 0x54, 0x05, 0x00, 0x03,
        0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x04,
        0xe8, 0x03, 0x00, 0x00, 0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x14, 0x00, 0x02, 0x00, 0x08,
        0x00, 0x88, 0x6b, 0x2e, 0x5d, 0x11, 0x00, 0x1c, 0xc7, 0x14, 0x00, 0x00, 0x00, 0x00, 0x10,
        0x00, 0x00, 0x09, 0x00, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa4,
        0x81, 0x6c, 0x00, 0x00, 0x00, 0x69, 0x6d, 0x61, 0x67, 0x65, 0x2e, 0x69, 0x6d, 0x67, 0x55,
        0x54, 0x05, 0x00, 0x03, 0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0xe8,
        0x03, 0x00, 0x00, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x02, 0x00, 0x9e, 0x00, 0x00, 0x00, 0xc3, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    /// A real `zip` whose largest member is itself encrypted, so there is
    /// nothing we can legitimately write.
    const ZIP_ENCRYPTED_LARGEST: &[u8] = &[
        0x50, 0x4b, 0x03, 0x04, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x6b, 0x2e, 0x5d, 0xc9,
        0x97, 0xb8, 0x50, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x09, 0x00, 0x1c, 0x00,
        0x73, 0x6d, 0x61, 0x6c, 0x6c, 0x2e, 0x74, 0x78, 0x74, 0x55, 0x54, 0x09, 0x00, 0x03, 0x1f,
        0xb0, 0xa7, 0x6a, 0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0xe8, 0x03,
        0x00, 0x00, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x74, 0x69, 0x6e, 0x79, 0x50, 0x4b, 0x03, 0x04,
        0x14, 0x00, 0x0b, 0x00, 0x08, 0x00, 0x88, 0x6b, 0x2e, 0x5d, 0x11, 0x00, 0x1c, 0xc7, 0x20,
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x0a, 0x00, 0x1c, 0x00, 0x73, 0x65, 0x63, 0x72,
        0x65, 0x74, 0x2e, 0x69, 0x6d, 0x67, 0x55, 0x54, 0x09, 0x00, 0x03, 0x1f, 0xb0, 0xa7, 0x6a,
        0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x04,
        0xe8, 0x03, 0x00, 0x00, 0x56, 0xf2, 0x6d, 0x7a, 0x64, 0xdd, 0x4b, 0x5f, 0xf2, 0x58, 0x44,
        0x72, 0x0c, 0x45, 0x90, 0x2c, 0x64, 0x7e, 0xea, 0xd9, 0x60, 0x75, 0x56, 0x77, 0xe3, 0xbe,
        0xc1, 0x4e, 0xe4, 0x1c, 0xa4, 0x60, 0x50, 0x4b, 0x07, 0x08, 0x11, 0x00, 0x1c, 0xc7, 0x20,
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x50, 0x4b, 0x01, 0x02, 0x1e, 0x03, 0x0a, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x88, 0x6b, 0x2e, 0x5d, 0xc9, 0x97, 0xb8, 0x50, 0x04, 0x00, 0x00,
        0x00, 0x04, 0x00, 0x00, 0x00, 0x09, 0x00, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x00, 0x00, 0x73, 0x6d, 0x61, 0x6c, 0x6c, 0x2e, 0x74,
        0x78, 0x74, 0x55, 0x54, 0x05, 0x00, 0x03, 0x1f, 0xb0, 0xa7, 0x6a, 0x75, 0x78, 0x0b, 0x00,
        0x01, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x50, 0x4b, 0x01, 0x02,
        0x1e, 0x03, 0x14, 0x00, 0x0b, 0x00, 0x08, 0x00, 0x88, 0x6b, 0x2e, 0x5d, 0x11, 0x00, 0x1c,
        0xc7, 0x20, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x0a, 0x00, 0x18, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa4, 0x81, 0x47, 0x00, 0x00, 0x00, 0x73, 0x65, 0x63,
        0x72, 0x65, 0x74, 0x2e, 0x69, 0x6d, 0x67, 0x55, 0x54, 0x05, 0x00, 0x03, 0x1f, 0xb0, 0xa7,
        0x6a, 0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0xe8, 0x03, 0x00, 0x00, 0x04, 0xe8, 0x03, 0x00,
        0x00, 0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x02, 0x00, 0x9f, 0x00,
        0x00, 0x00, 0xbb, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    fn roundtrip(tag: &str, blob: &[u8], expect: Compression) {
        let src = tmp(&format!("c-{tag}-src"));
        let dst = tmp(&format!("c-{tag}-dst"));
        std::fs::write(&src, blob).unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert_eq!(
            detect_compression(&src).unwrap(),
            expect,
            "{tag}: wrong format"
        );
        let o = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(o.compression, expect);
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            PAYLOAD,
            "{tag}: decompressed bytes do not match"
        );
        assert_eq!(o.bytes_written, PAYLOAD.len() as u64);
        // The digest must cover what landed on the device, not the archive.
        verify_written(&dst, &o.digest, o.bytes_written, &mut noop).unwrap();
    }

    #[test]
    fn decompresses_gzip() {
        roundtrip("gz", GZ, Compression::Gzip);
    }

    #[test]
    fn decompresses_xz() {
        roundtrip("xz", XZ, Compression::Xz);
    }

    #[test]
    fn decompresses_zstd() {
        roundtrip("zst", ZST, Compression::Zstd);
    }

    #[test]
    fn decompresses_bzip2() {
        roundtrip("bz2", BZ2, Compression::Bzip2);
    }

    #[test]
    fn decompresses_lzma() {
        roundtrip("lzma", LZMA, Compression::Lzma);
    }

    /// `gzip` of an empty file — a perfectly valid archive that decodes to
    /// nothing, produced by the system `gzip` tool.
    const EMPTY_GZ: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x03, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn a_container_that_decodes_to_nothing_is_an_error() {
        // The file itself is not empty, so the up-front size check passes and
        // only decoding reveals there is nothing to write.
        let src = tmp("emptydecode");
        let dst = tmp("emptydecodedst");
        std::fs::write(&src, EMPTY_GZ).unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert_eq!(detect_compression(&src).unwrap(), Compression::Gzip);
        let err = write_image(&src, &dst, &mut noop).unwrap_err();
        assert!(err.to_string().contains("zero bytes"), "got: {err}");
    }

    #[test]
    fn decompresses_dot_z() {
        roundtrip("dotz", DOT_Z, Compression::Lzw);
    }

    #[test]
    fn decompresses_zip_picking_the_largest_member() {
        roundtrip("zip", ZIP, Compression::Zip);
    }

    /// The member we write is the largest **uncompressed**, because that is
    /// what lands on the device. Ranking by compressed size would hand the user
    /// a 64-byte text file instead of the 4 KiB image beside it — disk images
    /// compress well, so the real payload is usually the *smallest* member on
    /// disk.
    #[test]
    fn the_largest_member_is_measured_uncompressed() {
        let src = tmp("zip-rank-src");
        let dst = tmp("zip-rank-dst");
        std::fs::write(&src, ZIP_DEFLATED).unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert_eq!(detect_compression(&src).unwrap(), Compression::Zip);
        let o = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(o.bytes_written, 4096, "picked the wrong member");
        assert_eq!(std::fs::read(&dst).unwrap(), vec![0u8; 4096]);
        verify_written(&dst, &o.digest, o.bytes_written, &mut noop).unwrap();
    }

    /// Progress for a zip counts the chosen member, not the whole archive —
    /// otherwise it would stop short of 100% by however much the other members
    /// and the central directory weigh.
    #[test]
    fn zip_progress_tracks_the_member_not_the_archive() {
        let src = tmp("zip-prog-src");
        let dst = tmp("zip-prog-dst");
        std::fs::write(&src, ZIP_DEFLATED).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let mut seen: Vec<Progress> = Vec::new();
        write_image(&src, &dst, &mut |p| seen.push(p)).unwrap();
        let last = seen.last().unwrap();
        assert_eq!(last.bytes, last.total, "must finish at 100%");
        assert_eq!(last.total, 20, "the deflated member is 20 bytes on disk");
        assert!(
            last.total < ZIP_DEFLATED.len() as u64,
            "the archive is larger than the member; the member is what we track"
        );
    }

    #[test]
    fn an_empty_zip_is_rejected_rather_than_written_raw() {
        // "PK\x05\x06" — a zip with no members at all. Writing the archive
        // itself to the device would produce an unbootable stick.
        let empty: &[u8] = &[
            0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let src = tmp("zip-empty-src");
        let dst = tmp("zip-empty-dst");
        std::fs::write(&src, empty).unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert_eq!(detect_compression(&src).unwrap(), Compression::Zip);
        let err = write_image(&src, &dst, &mut noop).unwrap_err();
        assert!(err.to_string().contains("empty zip"), "got: {err}");
    }

    /// ruzstd 0.9 began applying its 100 MB default window cap to the first
    /// frame as well as later ones, which would refuse every image written
    /// with `zstd --long`. We raise the cap rather than inherit that.
    #[test]
    fn a_large_zstd_window_is_still_accepted() {
        let src = tmp("zst-long-src");
        let dst = tmp("zst-long-dst");
        std::fs::write(&src, ZST_LONG_WINDOW).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let o = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), PAYLOAD);
        assert_eq!(o.compression, Compression::Zstd);
    }

    /// Concatenating two archives of a format yields a valid archive of that
    /// format, and every reference tool unpacks the lot. Ours used to stop
    /// after the first stream and report a clean end of file, so a two-stream
    /// image wrote only its first half — and since the digest is taken over
    /// what was actually written, read-back verification then *passed*. A
    /// half-written drive reported as verified is the worst outcome this code
    /// has, so each decoder is pinned here.
    #[test]
    fn every_stream_of_a_concatenated_archive_is_written() {
        let mut expected = PAYLOAD.to_vec();
        expected.extend_from_slice(SECOND);
        for (tag, blob, expect) in [
            ("gz", CAT_GZ, Compression::Gzip),
            ("xz", CAT_XZ, Compression::Xz),
            ("bz2", CAT_BZ2, Compression::Bzip2),
            ("zst", CAT_ZST, Compression::Zstd),
        ] {
            let src = tmp(&format!("cat-{tag}-src"));
            let dst = tmp(&format!("cat-{tag}-dst"));
            std::fs::write(&src, blob).unwrap();
            std::fs::write(&dst, b"").unwrap();
            assert_eq!(detect_compression(&src).unwrap(), expect, "{tag}");
            let o = write_image(&src, &dst, &mut noop).unwrap();
            assert_eq!(
                std::fs::read(&dst).unwrap(),
                expected,
                "{tag}: only the first stream was written"
            );
            assert_eq!(o.bytes_written, expected.len() as u64, "{tag}");
            verify_written(&dst, &o.digest, o.bytes_written, &mut noop).unwrap();
        }
    }

    /// The index scan must not build a decoder for every entry. It used to,
    /// so one locked note beside the image aborted the whole archive with
    /// "Password required to decrypt file" — an archive we can read perfectly
    /// well, refused over a member we never intended to touch.
    #[test]
    fn a_locked_sibling_does_not_block_the_image() {
        let src = tmp("zip-encsib-src");
        let dst = tmp("zip-encsib-dst");
        std::fs::write(&src, ZIP_ENCRYPTED_SIBLING).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let o = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(o.bytes_written, 4096);
        assert_eq!(std::fs::read(&dst).unwrap(), vec![0u8; 4096]);
    }

    /// The flip side: reading the index raw means the crate no longer refuses
    /// encrypted members for us, so an encrypted *winner* must be caught here.
    /// Left unchecked it would be streamed to the device as ciphertext.
    #[test]
    fn an_encrypted_image_is_refused_not_written_as_ciphertext() {
        let src = tmp("zip-enclrg-src");
        let dst = tmp("zip-enclrg-dst");
        std::fs::write(&src, ZIP_ENCRYPTED_LARGEST).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let err = write_image(&src, &dst, &mut noop).unwrap_err();
        assert!(err.to_string().contains("encrypted"), "got: {err}");
        assert!(
            std::fs::read(&dst).unwrap().is_empty(),
            "nothing may reach the device"
        );
    }

    /// A truncated archive must be caught while reading the index, not by the
    /// decoder partway through the write with the drive already overwritten.
    #[test]
    fn a_truncated_zip_is_caught_before_writing() {
        let src = tmp("zip-trunc-src");
        let dst = tmp("zip-trunc-dst");
        // Keep the central directory (so it still parses) but drop payload
        // bytes out of the middle by shrinking the file's declared extent.
        let mut blob = ZIP_DEFLATED.to_vec();
        let cut = blob.len() - 4;
        blob.truncate(cut);
        std::fs::write(&src, &blob).unwrap();
        std::fs::write(&dst, b"").unwrap();
        // Either the index fails to parse or the extent check fires; both are
        // before any byte reaches the device, which is the point.
        assert!(write_image(&src, &dst, &mut noop).is_err());
        assert!(std::fs::read(&dst).unwrap().is_empty());
    }

    // ---- fixed VHD ----

    /// The payload inside `fixtures/fixed.vhd`, regenerated rather than stored
    /// a second time.
    fn vhd_pattern() -> Vec<u8> {
        (0..34816).map(|i| ((i * 167 + 13) % 256) as u8).collect()
    }

    #[test]
    fn writes_a_fixed_vhd_without_its_footer() {
        // A fixed VHD is a raw disk image with 512 bytes of metadata glued to
        // the end. Those 512 bytes are not part of the disk and must not reach
        // the device.
        let src = tmp("vhd-src");
        let dst = tmp("vhd-dst");
        std::fs::write(&src, include_bytes!("../fixtures/fixed.vhd")).unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert_eq!(detect_compression(&src).unwrap(), Compression::VhdFixed);
        let o = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(o.bytes_written, 34816, "the footer must be trimmed");
        assert_eq!(std::fs::read(&dst).unwrap(), vhd_pattern());
        verify_written(&dst, &o.digest, o.bytes_written, &mut noop).unwrap();
    }

    #[test]
    fn vhd_progress_ends_at_the_payload_not_the_file() {
        // Counting the footer would park the bar at 99.95% forever.
        let src = tmp("vhd-prog-src");
        let dst = tmp("vhd-prog-dst");
        std::fs::write(&src, include_bytes!("../fixtures/fixed.vhd")).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let mut seen: Vec<Progress> = Vec::new();
        write_image(&src, &dst, &mut |p| seen.push(p)).unwrap();
        let last = seen.last().unwrap();
        assert_eq!(last.bytes, last.total);
        assert_eq!(last.total, 34816);
        assert!((last.percent() - 100.0).abs() < 0.001);
    }

    /// A dynamic VHD stores its data in scattered blocks behind a block
    /// allocation table, so writing it verbatim produces a drive full of
    /// metadata. It is refused during detection — before a target has even been
    /// chosen, let alone wiped.
    #[test]
    fn a_dynamic_vhd_is_refused_at_detection() {
        let src = tmp("vhd-dyn-src");
        std::fs::write(&src, include_bytes!("../fixtures/dynamic.vhd")).unwrap();
        let err = detect_compression(&src).unwrap_err();
        assert!(err.to_string().contains("dynamic VHD"), "got: {err}");
    }

    #[test]
    fn the_footer_checksum_is_what_rules_out_a_lookalike() {
        // Every other field can be forged by accident; the checksum is what
        // makes a false positive on a raw image implausible.
        let real = include_bytes!("../fixtures/fixed.vhd");
        let mut tail = [0u8; 512];
        tail.copy_from_slice(&real[real.len() - 512..]);
        assert!(VhdFooter::parse_tail(&tail, real.len() as u64).is_some());

        // Flip one bit anywhere outside the checksum field and it stops parsing.
        for at in [0usize, 8, 60, 100, 300, 511] {
            let mut broken = tail;
            broken[at] ^= 0x01;
            assert!(
                VhdFooter::parse_tail(&broken, real.len() as u64).is_none(),
                "a footer with byte {at} corrupted must not be accepted"
            );
        }

        // A file that is nothing but a footer is not an image.
        assert!(VhdFooter::parse_tail(&tail, 512).is_none());
    }

    /// Images written before Virtual PC 2004 carry a 511-byte footer. Derived
    /// from the real one by dropping its final reserved zero, which is exactly
    /// what those images lack — and which leaves the checksum unchanged.
    #[test]
    fn a_legacy_511_byte_footer_is_recognised() {
        let real = include_bytes!("../fixtures/fixed.vhd");
        let genuine = &real[real.len() - 512..];
        assert_eq!(genuine[511], 0, "the dropped byte must be reserved padding");

        let mut tail = [0u8; 512];
        tail[1..512].copy_from_slice(&genuine[..511]);
        tail[0] = 0x5a; // the last byte of payload, whatever it happens to be
        let footer = VhdFooter::parse_tail(&tail, 35327).expect("511-byte footer");
        assert_eq!(footer.footer_len, 511);
        assert_eq!(footer.disk_type, VhdFooter::FIXED);
    }

    /// Detection order: a magic number wins outright, and the checksum-verified
    /// footer is consulted before the magic-less LZMA guess.
    #[test]
    fn a_compressed_vhd_is_treated_as_compressed() {
        // gzip's magic must win over any trailer the compressed bytes happen to
        // end with, or we would try to trim a footer off an archive.
        assert_eq!(Compression::sniff(&[0x1f, 0x8b, 0x08]), Compression::Gzip);
        assert!(Compression::Gzip.has_magic());
        assert!(!Compression::None.has_magic());
        assert!(!Compression::Lzma.has_magic());
        assert!(Compression::VhdFixed.is_verbatim());
        assert!(Compression::None.is_verbatim());
        assert!(!Compression::Gzip.is_verbatim());
    }

    #[test]
    fn raw_images_are_left_alone() {
        roundtrip("raw", PAYLOAD, Compression::None);
    }

    #[test]
    fn format_is_sniffed_from_content_not_extension() {
        // An image served as ".iso" while actually being gzip must still work.
        assert_eq!(Compression::sniff(GZ), Compression::Gzip);
        assert_eq!(Compression::sniff(XZ), Compression::Xz);
        assert_eq!(Compression::sniff(ZST), Compression::Zstd);
        assert_eq!(Compression::sniff(BZ2), Compression::Bzip2);
        assert_eq!(Compression::sniff(LZMA), Compression::Lzma);
        assert_eq!(Compression::sniff(ZIP), Compression::Zip);
        assert_eq!(Compression::sniff(DOT_Z), Compression::Lzw);
        assert_eq!(Compression::sniff(b"CD001 plain iso"), Compression::None);
        assert_eq!(Compression::sniff(b""), Compression::None);
    }

    /// LZMA-alone has no magic number, so this is the test that stands between
    /// a user's ISO and a wiped drive. Each row is the real leading bytes of a
    /// format somebody could plausibly hand us; none may sniff as LZMA.
    ///
    /// These matter more than usual because liblzma does not reject a bad
    /// header — it decodes a zero-filled one to an empty stream and returns
    /// success.
    #[test]
    fn lzma_detection_never_fires_on_a_real_disk_image() {
        // Leading 13+ bytes, captured from real images built with mkfs.vfat,
        // mkfs.ext4, genisoimage and fdisk, plus documented on-disk headers.
        let not_lzma: &[(&str, &[u8])] = &[
            ("iso9660 system area", &[0u8; 16]),
            ("ext4", &[0u8; 16]),
            ("mbr, zero boot code", &[0u8; 16]),
            ("gpt protective mbr", &[0u8; 16]),
            (
                "fat32",
                &[
                    0xeb, 0x58, 0x90, 0x6d, 0x6b, 0x66, 0x73, 0x2e, 0x66, 0x61, 0x74, 0x00, 0x02,
                    0x01, 0x20, 0x00,
                ],
            ),
            (
                "ntfs",
                &[
                    0xeb, 0x52, 0x90, 0x4e, 0x54, 0x46, 0x53, 0x20, 0x20, 0x20, 0x20, 0x00, 0x02,
                    0x08, 0x00, 0x00,
                ],
            ),
            (
                "exfat",
                &[
                    0xeb, 0x76, 0x90, 0x45, 0x58, 0x46, 0x41, 0x54, 0x20, 0x20, 0x20, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            ),
            (
                "squashfs",
                &[
                    0x68, 0x73, 0x71, 0x73, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc8,
                    0xb4, 0x00, 0x00,
                ],
            ),
            (
                "vmdk",
                &[
                    0x4b, 0x44, 0x4d, 0x56, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            ),
            (
                "qcow2",
                &[
                    0x51, 0x46, 0x49, 0xfb, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            ),
            (
                "apple dmg trailer",
                &[
                    0x6b, 0x6f, 0x6c, 0x79, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x02, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            ),
            (
                "hfs+",
                &[
                    0x48, 0x2b, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            ),
            (
                "elf",
                &[
                    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
            ),
            ("erased nor flash", &[0xffu8; 16]),
        ];
        for (name, head) in not_lzma {
            assert_ne!(
                Compression::sniff(head),
                Compression::Lzma,
                "{name} must never be mistaken for an lzma image"
            );
        }

        // ...while every dictionary size a real encoder emits is accepted.
        // Verified against `lzma -0` through `lzma -9` and `xz --format=lzma`.
        for shift in 12..=30 {
            let mut head = [0xffu8; 13];
            head[0] = 0x5d;
            head[1..5].copy_from_slice(&(1u32 << shift).to_le_bytes());
            assert_eq!(
                Compression::sniff(&head),
                Compression::Lzma,
                "a 2^{shift}-byte dictionary is a legitimate lzma image"
            );
        }
    }

    #[test]
    fn a_truncated_lzma_image_fails_loudly() {
        // Header only, no payload. This must be an error, never a silent
        // zero-byte "success".
        let src = tmp("trunclzma");
        let dst = tmp("trunclzmadst");
        std::fs::write(&src, &LZMA[..13]).unwrap();
        std::fs::write(&dst, b"").unwrap();
        assert_eq!(detect_compression(&src).unwrap(), Compression::Lzma);
        assert!(write_image(&src, &dst, &mut noop).is_err());
    }

    #[test]
    fn compressed_progress_tracks_the_archive_not_the_output() {
        // Output size is unknown until the last byte, so progress is reported
        // against how much of the archive has been consumed.
        let src = tmp("c-prog-src");
        let dst = tmp("c-prog-dst");
        std::fs::write(&src, GZ).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let mut seen: Vec<Progress> = Vec::new();
        write_image(&src, &dst, &mut |p| seen.push(p)).unwrap();
        let last = seen.last().unwrap();
        assert_eq!(
            last.total,
            GZ.len() as u64,
            "total should be the archive size"
        );
        assert_eq!(last.bytes, GZ.len() as u64, "should end fully consumed");
    }

    // ---- copy_tree ----

    fn tree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("sirius-copytree-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dst) = (base.join("src"), base.join("dst"));
        std::fs::create_dir_all(src.join("sources")).unwrap();
        std::fs::create_dir_all(src.join("efi/boot")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("setup.exe"), vec![1u8; 100]).unwrap();
        std::fs::write(src.join("sources/install.wim"), vec![2u8; 500]).unwrap();
        std::fs::write(src.join("sources/boot.wim"), vec![3u8; 50]).unwrap();
        std::fs::write(src.join("efi/boot/bootx64.efi"), vec![4u8; 25]).unwrap();
        (src, dst)
    }

    #[test]
    fn copies_a_nested_tree_including_empty_dirs() {
        let (src, dst) = tree("all");
        let copied = copy_tree(&src, &dst, &[], &mut noop).unwrap();
        assert_eq!(copied, 100 + 500 + 50 + 25);
        assert_eq!(
            std::fs::read(dst.join("sources/install.wim"))
                .unwrap()
                .len(),
            500
        );
        assert_eq!(
            std::fs::read(dst.join("efi/boot/bootx64.efi"))
                .unwrap()
                .len(),
            25
        );
        assert!(dst.join("empty").is_dir(), "empty directories must survive");
    }

    #[test]
    fn exclusion_skips_the_named_file_only() {
        let (src, dst) = tree("excl");
        let copied = copy_tree(&src, &dst, &["sources/install.wim".into()], &mut noop).unwrap();
        assert_eq!(copied, 100 + 50 + 25, "excluded bytes must not be counted");
        assert!(!dst.join("sources/install.wim").exists());
        // Its siblings and directory must still be there.
        assert!(dst.join("sources/boot.wim").exists());
    }

    #[test]
    fn exclusion_is_case_insensitive() {
        // ISO9660 often uppercases what UDF stores in lowercase, so a
        // case-sensitive match would copy a 7 GB image we meant to split.
        let (src, dst) = tree("case");
        copy_tree(&src, &dst, &["SOURCES/INSTALL.WIM".into()], &mut noop).unwrap();
        assert!(!dst.join("sources/install.wim").exists());
    }

    #[test]
    fn copy_progress_ends_at_the_measured_total() {
        let (src, dst) = tree("prog");
        let mut seen: Vec<Progress> = Vec::new();
        copy_tree(&src, &dst, &[], &mut |p| seen.push(p)).unwrap();
        let last = seen.last().expect("a final progress report");
        assert_eq!(last.stage, Stage::Copy);
        assert_eq!(last.bytes, last.total);
        assert_eq!(last.total, 675);
    }

    #[test]
    fn progress_reports_percent_and_eta() {
        let p = Progress {
            stage: Stage::Write,
            bytes: 50,
            total: 100,
            bytes_per_sec: 10,
        };
        assert!((p.percent() - 50.0).abs() < f64::EPSILON);
        assert_eq!(p.eta_secs(), Some(5));
        let unknown = Progress {
            stage: Stage::Verify,
            bytes: 0,
            total: 100,
            bytes_per_sec: 0,
        };
        assert_eq!(unknown.eta_secs(), None);
    }
}
