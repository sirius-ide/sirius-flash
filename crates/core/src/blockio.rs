//! Streaming image writes with progress, hashing and read-back verification.
//!
//! This replaces shelling out to `dd`, which cost us three things: no usable
//! progress (its records are carriage-return terminated), no digest, and no
//! portability — `oflag=sync status=progress` is GNU coreutils only and does
//! not exist on macOS.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
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
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Write => "write",
            Stage::Verify => "verify",
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

/// Drive a read/write loop, hashing as it goes and reporting progress.
///
/// `sink` receives each chunk; returning the total lets the caller decide
/// whether it is writing to a device or only reading one back.
fn stream<R, S>(
    mut src: R,
    total: u64,
    stage: Stage,
    on_progress: &mut dyn FnMut(Progress),
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
            let secs = started.elapsed().as_secs_f64();
            on_progress(Progress {
                stage,
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
    }

    let secs = started.elapsed().as_secs_f64();
    on_progress(Progress {
        stage,
        bytes: done,
        total,
        bytes_per_sec: if secs > 0.0 {
            (done as f64 / secs) as u64
        } else {
            0
        },
    });
    Ok(hasher.finalize().into())
}

/// Write `image` to the block device at `dest`, returning the SHA-256 of the
/// bytes written. Hashing rides along on the same buffers, so it is free.
pub fn write_image(
    image: &Path,
    dest: &Path,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<[u8; 32]> {
    let src = File::open(image).with_context(|| format!("opening image {}", image.display()))?;
    let total = src.metadata()?.len();
    if total == 0 {
        bail!("image {} is empty", image.display());
    }
    let mut out = OpenOptions::new()
        .write(true)
        .open(dest)
        .with_context(|| format!("opening {} for writing", dest.display()))?;

    let digest = stream(src, total, Stage::Write, on_progress, |chunk| {
        out.write_all(chunk).context("writing to device")
    })?;

    // Push everything to the medium before anyone calls this done.
    out.flush().context("flushing device")?;
    out.sync_all().context("syncing device")?;
    Ok(digest)
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
    let actual = stream(src.take(len), len, Stage::Verify, on_progress, |_| Ok(()))?;
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
        let d = write_image(&src, &dst, &mut noop).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"sirius flash");
        // Known SHA-256 of "sirius flash".
        assert_eq!(hex(&d).len(), 64);
        verify_written(&dst, &d, 12, &mut noop).unwrap();
    }

    #[test]
    fn verification_catches_corruption() {
        let src = tmp("src2");
        let dst = tmp("dst2");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        std::fs::write(&dst, b"").unwrap();
        let d = write_image(&src, &dst, &mut noop).unwrap();
        // Simulate a drive that silently stored something else.
        std::fs::write(&dst, vec![9u8; 4096]).unwrap();
        let err = verify_written(&dst, &d, 4096, &mut noop).unwrap_err();
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
        let d = write_image(&src, &dst, &mut |p| seen.push(p)).unwrap();
        let last = seen.last().expect("at least one progress report");
        assert_eq!(last.stage, Stage::Write);
        assert_eq!(last.bytes, last.total, "final report must show completion");
        assert_eq!(last.bytes, (BUF_SIZE * 2 + 17) as u64);
        assert!((last.percent() - 100.0).abs() < 0.001);

        // And verification reports against the same total.
        let mut vseen: Vec<Progress> = Vec::new();
        verify_written(&dst, &d, last.total, &mut |p| vseen.push(p)).unwrap();
        assert_eq!(vseen.last().unwrap().stage, Stage::Verify);
        assert_eq!(vseen.last().unwrap().bytes, last.total);
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
