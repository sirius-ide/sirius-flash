//! Sirius Flash — core logic: safe device discovery, ISO detection, and flashing.
//!
//! Safety principle: writes are only ever addressed via the stable
//! `/dev/disk/by-id` path, gated on removable + size checks. Kernel names
//! (`sdb`, `nvme0n1`) are treated as unstable and never trusted for targeting.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct UsbDevice {
    /// Stable /dev/disk/by-id path (the only thing we ever write to).
    pub by_id: PathBuf,
    /// Resolved kernel device (e.g. /dev/sdb) — for display only.
    pub dev: PathBuf,
    pub model: String,
    pub size_bytes: u64,
    pub removable: bool,
}

impl UsbDevice {
    pub fn size_gib(&self) -> f64 {
        self.size_bytes as f64 / 1024.0_f64.powi(3)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum IsoKind {
    Windows,
    Other,
}

/// Reject anything that is not a plausible removable USB target.
pub fn assert_safe_target(d: &UsbDevice) -> Result<()> {
    if !d.removable {
        bail!("refusing to write: {:?} is not a removable device", d.dev);
    }
    const MIN: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
    const MAX: u64 = 512 * 1024 * 1024 * 1024; // 512 GiB
    if d.size_bytes < MIN || d.size_bytes > MAX {
        bail!(
            "refusing to write: {:?} is {:.1} GiB, outside the safe USB window (2–512 GiB)",
            d.dev,
            d.size_gib()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_u64(p: &Path) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Enumerate removable USB block devices via /dev/disk/by-id (whole disks only).
#[cfg(target_os = "linux")]
pub fn list_removable_devices() -> Result<Vec<UsbDevice>> {
    let mut out = Vec::new();
    let by_id_dir = Path::new("/dev/disk/by-id");
    if !by_id_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(by_id_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("usb-") || name.contains("-part") {
            continue;
        }
        let by_id = entry.path();
        let dev = match fs::canonicalize(&by_id) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let base = match dev.file_name() {
            Some(b) => b.to_string_lossy().to_string(),
            None => continue,
        };
        let sys = Path::new("/sys/block").join(&base);
        let removable = read_u64(&sys.join("removable")).unwrap_or(0) == 1;
        let size_bytes = read_u64(&sys.join("size")).unwrap_or(0) * 512;
        let model = fs::read_to_string(sys.join("device/model"))
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(UsbDevice { by_id, dev, model, size_bytes, removable });
    }
    out.sort_by(|a, b| a.by_id.cmp(&b.by_id));
    Ok(out)
}

/// Detect whether an ISO is a Windows installer (looks for sources/install.{wim,esd}).
#[cfg(target_os = "linux")]
pub fn detect_iso_kind(iso: &Path) -> Result<IsoKind> {
    if let Ok(o) = Command::new("bsdtar").arg("-tf").arg(iso).output() {
        if o.status.success() {
            let list = String::from_utf8_lossy(&o.stdout).to_lowercase();
            if list.contains("sources/install.wim") || list.contains("sources/install.esd") {
                return Ok(IsoKind::Windows);
            }
            return Ok(IsoKind::Other);
        }
    }
    let n = iso
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    Ok(if n.contains("win") { IsoKind::Windows } else { IsoKind::Other })
}

#[cfg(target_os = "linux")]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `{cmd}`"))?;
    if !status.success() {
        bail!("`{cmd}` exited with {status}");
    }
    Ok(())
}

/// Flash a Windows installer ISO (GPT + FAT32 + split install.wim). Requires root; erases the device.
#[cfg(target_os = "linux")]
pub fn flash_windows_iso(d: &UsbDevice, iso: &Path) -> Result<()> {
    assert_safe_target(d)?;
    let dev = d.by_id.to_string_lossy().to_string();
    let part1 = format!("{dev}-part1");

    let _ = Command::new("bash")
        .arg("-c")
        .arg(format!("for p in {dev}-part*; do umount \"$p\" 2>/dev/null || true; done"))
        .status();

    run("parted", &["--script", &dev, "mklabel", "gpt", "mkpart", "WIN11", "fat32", "1MiB", "100%", "set", "1", "msftdata", "on"])?;
    run("udevadm", &["settle"])?;
    run("mkfs.fat", &["-F", "32", "-n", "WIN11USB", &part1])?;

    let iso_mnt = "/run/sirius-flash-iso";
    let usb_mnt = "/run/sirius-flash-usb";
    fs::create_dir_all(iso_mnt)?;
    fs::create_dir_all(usb_mnt)?;
    run("mount", &["-o", "loop,ro", &iso.to_string_lossy(), iso_mnt])?;

    let result = (|| -> Result<()> {
        run("mount", &[&part1, usb_mnt])?;
        run("rsync", &["-rt", "--no-perms", "--no-owner", "--no-group", "--exclude=sources/install.wim", &format!("{iso_mnt}/"), &format!("{usb_mnt}/")])?;
        run("wimlib-imagex", &["split", &format!("{iso_mnt}/sources/install.wim"), &format!("{usb_mnt}/sources/install.swm"), "3800"])?;
        if !Path::new(&format!("{usb_mnt}/efi/boot/bootx64.efi")).exists() {
            bail!("verification failed: efi/boot/bootx64.efi missing on USB");
        }
        if !Path::new(&format!("{usb_mnt}/sources/boot.wim")).exists() {
            bail!("verification failed: sources/boot.wim missing on USB");
        }
        run("sync", &[])?;
        Ok(())
    })();

    let _ = Command::new("umount").arg(usb_mnt).status();
    let _ = Command::new("umount").arg(iso_mnt).status();
    result
}

/// Write a Linux/other bootable ISO directly to the device. Requires root; erases the device.
#[cfg(target_os = "linux")]
pub fn flash_linux_iso(d: &UsbDevice, iso: &Path) -> Result<()> {
    assert_safe_target(d)?;
    let dev = d.by_id.to_string_lossy().to_string();
    let _ = Command::new("bash")
        .arg("-c")
        .arg(format!("for p in {dev}-part*; do umount \"$p\" 2>/dev/null || true; done"))
        .status();
    run("dd", &[&format!("if={}", iso.to_string_lossy()), &format!("of={dev}"), "bs=4M", "oflag=sync", "status=progress"])?;
    Ok(())
}

// ---- Non-Linux stubs so the crate compiles everywhere (implemented per-OS later) ----
#[cfg(not(target_os = "linux"))]
pub fn list_removable_devices() -> Result<Vec<UsbDevice>> {
    bail!("device enumeration not yet implemented on this OS")
}
#[cfg(not(target_os = "linux"))]
pub fn detect_iso_kind(_iso: &Path) -> Result<IsoKind> {
    bail!("ISO detection not yet implemented on this OS")
}
#[cfg(not(target_os = "linux"))]
pub fn flash_windows_iso(_d: &UsbDevice, _iso: &Path) -> Result<()> {
    bail!("Windows-ISO flashing not yet implemented on this OS")
}
#[cfg(not(target_os = "linux"))]
pub fn flash_linux_iso(_d: &UsbDevice, _iso: &Path) -> Result<()> {
    bail!("Linux-ISO flashing not yet implemented on this OS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_removable() {
        let d = UsbDevice { by_id: "/x".into(), dev: "/dev/sda".into(), model: "big".into(), size_bytes: 8_000_000_000_000, removable: false };
        assert!(assert_safe_target(&d).is_err());
    }
    #[test]
    fn rejects_oversized() {
        let d = UsbDevice { by_id: "/x".into(), dev: "/dev/sda".into(), model: "big".into(), size_bytes: 8_000_000_000_000, removable: true };
        assert!(assert_safe_target(&d).is_err());
    }
    #[test]
    fn accepts_normal_usb() {
        let d = UsbDevice { by_id: "/x".into(), dev: "/dev/sdb".into(), model: "DataTraveler".into(), size_bytes: 62_000_000_000, removable: true };
        assert!(assert_safe_target(&d).is_ok());
    }
}
