//! Streaming image writes with progress, hashing and read-back verification.
//!
//! This replaces shelling out to `dd`, which cost us three things: no usable
//! progress (its records are carriage-return terminated), no digest, and no
//! portability — `oflag=sync status=progress` is GNU coreutils only and does
//! not exist on macOS.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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
}

impl Compression {
    pub fn as_str(self) -> &'static str {
        match self {
            Compression::None => "raw",
            Compression::Gzip => "gzip",
            Compression::Xz => "xz",
            Compression::Zstd => "zstd",
            Compression::Bzip2 => "bzip2",
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
        } else {
            Compression::None
        }
    }
}

/// Detect how `path` is compressed.
pub fn detect_compression(path: &Path) -> Result<Compression> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 8];
    let n = f
        .read(&mut magic)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Compression::sniff(&magic[..n]))
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

/// Open an image for reading, transparently decompressing it.
///
/// Returns the reader, the compressed size on disk, the detected compression,
/// and a counter tracking consumption of the underlying file.
pub fn open_image(path: &Path) -> Result<(Box<dyn Read>, u64, Compression, ByteCounter)> {
    let compression = detect_compression(path)?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata()?.len();
    if size == 0 {
        bail!("image {} is empty", path.display());
    }
    let counter = ByteCounter::default();
    let counted = Counted {
        inner: file,
        counter: counter.clone(),
    };
    let reader: Box<dyn Read> = match compression {
        Compression::None => Box::new(counted),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(counted)),
        Compression::Xz => Box::new(liblzma::read::XzDecoder::new(counted)),
        Compression::Bzip2 => Box::new(bzip2_rs::DecoderReader::new(counted)),
        Compression::Zstd => Box::new(
            ruzstd::StreamingDecoder::new(counted)
                .map_err(|e| anyhow::anyhow!("not a readable zstd image: {e}"))?,
        ),
    };
    Ok((reader, size, compression, counter))
}

/// What a write actually produced.
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
    // Uncompressed input produces exactly as many bytes as it consumes, so the
    // output count is already the right progress signal.
    let track = if compression == Compression::None {
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
        assert_eq!(Compression::sniff(b"CD001 plain iso"), Compression::None);
        assert_eq!(Compression::sniff(b""), Compression::None);
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
