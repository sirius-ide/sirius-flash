//! Sirius Flash CLI — list removable USBs and write ISOs to them safely.

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use sirius_flash_core as core;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "sirius-flash",
    version,
    about = "Cross-platform bootable USB creator (Sirius Flash)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List removable USB devices (safe to target)
    List,
    /// Write an ISO to a USB device — ERASES all data on it
    Write {
        /// Path to the .iso file
        #[arg(long)]
        iso: PathBuf,
        /// Target device: its /dev/disk/by-id path (from `list`)
        #[arg(long)]
        device: String,
        /// Treat the ISO as this kind (default: auto-detect)
        #[arg(long, value_enum, default_value = "auto")]
        kind: KindArg,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Skip reading the device back to confirm it matches the image
        #[arg(long)]
        no_verify: bool,
        #[command(flatten)]
        tweaks: TweakArgs,
        #[command(flatten)]
        format: FormatArgs,
    },
    /// Print the autounattend.xml the given tweaks would produce (writes nothing)
    Unattend {
        #[command(flatten)]
        tweaks: TweakArgs,
    },
    /// Show which format options are legal for a device (writes nothing)
    FormatOptions {
        /// Target device: its /dev/disk/by-id path (from `list`)
        #[arg(long, conflicts_with = "size_gb")]
        device: Option<String>,
        /// Ask about a hypothetical drive of this many GB instead of a real one
        #[arg(long, value_name = "GB")]
        size_gb: Option<u64>,
        /// Logical sector size for a hypothetical drive. 512 everywhere except
        /// 4Kn media, where the smaller cluster sizes disappear. Ignored with
        /// --device, which reports its own.
        #[arg(long, default_value = "512", value_parser = parse_sector_size)]
        sector_size: u32,
    },
}

/// How the target should be partitioned and formatted. Only meaningful for a
/// Windows ISO, which is the path that creates a filesystem; a raw image brings
/// its own partition table and is written byte for byte.
#[derive(clap::Args, Clone)]
struct FormatArgs {
    /// Volume label (FAT32 keeps 11 characters and uppercases them)
    #[arg(long, value_name = "LABEL", help_heading = "Format options")]
    label: Option<String>,
    /// Cluster size, e.g. 4096, 4K or 32K (default: best for the drive size)
    #[arg(long, value_name = "SIZE", help_heading = "Format options")]
    cluster_size: Option<String>,
    /// Read every sector while formatting and mark the unreadable ones bad.
    /// Finds a dying stick; does NOT detect a fake-capacity counterfeit.
    #[arg(long, help_heading = "Format options")]
    full_format: bool,
    /// How to lay out Windows media. Default: ntfs-uefi-ntfs when the install
    /// image is too big for FAT32, fat32-split otherwise — the same rule Rufus
    /// uses. fat32-split boots on any firmware; ntfs-uefi-ntfs is faster but
    /// needs the third-party UEFI CA trusted by the target machine.
    #[arg(long, value_enum, help_heading = "Format options")]
    layout: Option<LayoutArg>,
    /// Firmware the media must boot on. uefi writes GPT; bios writes MBR with
    /// Sirius Flash's own boot code, and always the fat32-split layout, since
    /// FAT32 is the one filesystem it has a boot record for.
    #[arg(
        long,
        value_enum,
        default_value = "uefi",
        help_heading = "Format options"
    )]
    target: TargetArg,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum TargetArg {
    /// UEFI firmware: GPT, no boot code needed.
    Uefi,
    /// Legacy BIOS, or UEFI in CSM mode: MBR, our MBR and FAT32 boot record.
    Bios,
}

#[derive(ValueEnum, Clone, Copy)]
enum LayoutArg {
    /// One FAT32 partition; splits an oversized install image into .swm chunks.
    Fat32Split,
    /// NTFS plus the UEFI:NTFS loader partition; keeps the image whole.
    NtfsUefiNtfs,
}

impl FormatArgs {
    /// The scheme and firmware target. GPT cannot boot a BIOS, so the target
    /// decides both.
    fn scheme_and_target(&self) -> (core::format::PartitionScheme, core::format::TargetSystem) {
        use core::format::{PartitionScheme, TargetSystem};
        match self.target {
            TargetArg::Uefi => (PartitionScheme::Gpt, TargetSystem::Uefi),
            TargetArg::Bios => (PartitionScheme::Mbr, TargetSystem::Bios),
        }
    }

    /// The format flags actually given, by name.
    fn given(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.label.is_some() {
            v.push("--label");
        }
        if self.cluster_size.is_some() {
            v.push("--cluster-size");
        }
        if self.full_format {
            v.push("--full-format");
        }
        if self.layout.is_some() {
            v.push("--layout");
        }
        if self.target != TargetArg::Uefi {
            v.push("--target");
        }
        v
    }

    fn layout(&self) -> Option<core::format::WindowsLayout> {
        self.layout.map(|l| match l {
            LayoutArg::Fat32Split => core::format::WindowsLayout::Fat32Split,
            LayoutArg::NtfsUefiNtfs => core::format::WindowsLayout::NtfsUefiNtfs,
        })
    }
}

/// The gate, asked before the YES prompt: the same `validate` and `buildable`
/// the core runs before it touches the drive, so "this would not boot" comes
/// while the user can still change their mind rather than after.
fn check_buildable(
    format: &FormatArgs,
    layout: core::format::WindowsLayout,
    size_bytes: u64,
    sector_size: u32,
) -> Result<()> {
    let plan = format
        .to_request_for(layout)?
        .validate(core::format::Volume::new(
            core::windows_volume_size(size_bytes),
            sector_size,
        ))?;
    plan.buildable().map_err(|why| {
        anyhow!(
            "cannot make bootable {} + {} + {} media: {why}",
            plan.scheme().as_str().to_uppercase(),
            plan.target().as_str(),
            plan.filesystem()
        )
    })
}

/// Refuse an option that does nothing for this kind of image, rather than
/// dropping it. Invariant 13: the user's check is that the tool did what they
/// asked, so an option accepted and then ignored is worse than one refused.
/// A raw image brings its own partition table and boot code, so the format
/// options and the Windows tweaks have nothing to act on; Windows media is
/// checked file by file, so there is no image read-back for --no-verify to skip.
fn refuse_misplaced_options(
    kind: &core::IsoKind,
    format: &FormatArgs,
    tweaks: &core::WindowsTweaks,
    no_verify: bool,
) -> Result<()> {
    match kind {
        core::IsoKind::Other => {
            let mut misplaced = format.given();
            if !tweaks.is_noop() {
                misplaced.push("the Windows 11 tweaks");
            }
            if !misplaced.is_empty() {
                bail!(
                    "{} only apply to Windows installer media, and this image is being written \
                     raw, with its own partition table and boot code. Nothing has been written; \
                     drop them, or pass --kind windows if this is a Windows ISO.",
                    misplaced.join(", ")
                );
            }
        }
        core::IsoKind::Windows => {
            if no_verify {
                bail!(
                    "--no-verify skips the read-back of a raw image; Windows installer media is \
                     checked file by file instead, so there is nothing for it to skip. Nothing \
                     has been written."
                );
            }
        }
    }
    Ok(())
}

/// A sector size has to be a power of two of at least 512, and zero would
/// underflow the cluster mask rather than being rejected later.
fn parse_sector_size(s: &str) -> Result<u32, String> {
    let n: u32 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if n < 512 || !n.is_power_of_two() {
        return Err(format!(
            "{n} is not a usable sector size; it must be a power of two of at least 512"
        ));
    }
    Ok(n)
}

/// "32K" / "4096" -> bytes.
fn parse_cluster_size(s: &str) -> Result<u32> {
    let t = s.trim();
    let (digits, mult) = match t.chars().last() {
        Some('K') | Some('k') => (&t[..t.len() - 1], 1024),
        Some('M') | Some('m') => (&t[..t.len() - 1], 1024 * 1024),
        _ => (t, 1),
    };
    let n: u32 = digits
        .parse()
        .map_err(|_| anyhow!("{s:?} is not a cluster size; try 4096, 4K or 32K"))?;
    n.checked_mul(mult)
        .ok_or_else(|| anyhow!("{s:?} is too large to be a cluster size"))
}

impl FormatArgs {
    /// The request the core will validate once the layout is settled: the
    /// flags, with the filesystem the layout decides.
    fn to_request_for(
        &self,
        layout: core::format::WindowsLayout,
    ) -> Result<core::format::FormatRequest> {
        let mut r = match self.to_request()? {
            Some(r) => r,
            None => {
                let (scheme, target) = self.scheme_and_target();
                core::format::FormatRequest {
                    scheme,
                    target,
                    filesystem: core::format::FileSystem::Fat32,
                    uefi_ntfs_helper: false,
                    cluster_size: None,
                    label: "WIN11USB".into(),
                    quick: true,
                }
            }
        };
        r.filesystem = layout.filesystem();
        r.uefi_ntfs_helper = layout.needs_uefi_ntfs_partition();
        Ok(r)
    }

    /// Turn the flags into a format *request*, or `None` to let the core pick
    /// its defaults.
    ///
    /// Deliberately not validated here. The filesystem depends on the layout,
    /// the layout depends on the size of the install image, and that is not
    /// known until the core has probed the ISO. Validating now could only
    /// assert FAT32 — about media the core might be about to make NTFS.
    fn to_request(&self) -> Result<Option<core::format::FormatRequest>> {
        if self.label.is_none()
            && self.cluster_size.is_none()
            && !self.full_format
            && self.target == TargetArg::Uefi
        {
            return Ok(None);
        }
        let cluster_size = match &self.cluster_size {
            Some(c) => Some(parse_cluster_size(c)?),
            None => None,
        };
        let (scheme, target) = self.scheme_and_target();
        Ok(Some(core::format::FormatRequest {
            scheme,
            target,
            // Both are set from the resolved layout inside the core.
            filesystem: core::format::FileSystem::Fat32,
            uefi_ntfs_helper: false,
            cluster_size,
            label: self.label.clone().unwrap_or_else(|| "WIN11USB".into()),
            quick: !self.full_format,
        }))
    }
}

/// Print every legal combination for a drive, and say which of them this
/// build can actually make bootable.
fn show_format_options(size_bytes: u64, sector_size: u32) -> Result<()> {
    use core::format::*;
    let volume = Volume::new(size_bytes, sector_size);
    println!(
        "Drive: {:.1} GiB, {} byte sectors\n",
        size_bytes as f64 / 1024.0_f64.powi(3),
        sector_size
    );
    let mut any = false;
    for scheme in [PartitionScheme::Mbr, PartitionScheme::Gpt] {
        for target in target_systems_for(scheme) {
            let filesystems = filesystems_for(scheme, *target, volume);
            if filesystems.is_empty() {
                continue;
            }
            for fs in filesystems {
                let sizes = cluster_sizes(fs, volume);
                let default = default_cluster_size(fs, volume);
                let request = FormatRequest {
                    scheme,
                    target: *target,
                    filesystem: fs,
                    cluster_size: None,
                    label: String::new(),
                    quick: true,
                    uefi_ntfs_helper: false,
                };
                // Everything advertised must validate; if it does not, that is a
                // bug in the model rather than something to print.
                let plan = request.validate(volume)?;
                any = true;
                println!(
                    "{:<4} + {:<13} + {:<6}  clusters: {}",
                    scheme.as_str().to_uppercase(),
                    target.as_str(),
                    fs.as_str(),
                    sizes
                        .iter()
                        .map(|c| {
                            let mark = if Some(*c) == default { "*" } else { "" };
                            format!("{}{mark}", human(*c))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if let Err(why) = plan.buildable() {
                    println!("        not yet: {why}");
                }
                for w in plan.warnings() {
                    println!("        note: {w}");
                }
            }
        }
    }
    if !any {
        bail!("no filesystem can be created on a drive this size");
    }
    println!("\n* = default. A row marked \"not yet\" would format, and would not boot.");
    Ok(())
}

fn human(n: u32) -> String {
    if n >= 1 << 20 {
        format!("{}M", n >> 20)
    } else if n >= 1 << 10 {
        format!("{}K", n >> 10)
    } else {
        format!("{n}B")
    }
}

#[derive(ValueEnum, Clone)]
enum KindArg {
    Auto,
    Windows,
    Linux,
}

/// Windows 11 install-experience tweaks (ignored for non-Windows ISOs).
#[derive(clap::Args, Clone)]
struct TweakArgs {
    /// Bypass every Windows 11 hardware check (TPM, Secure Boot, RAM, CPU, storage)
    #[arg(long, help_heading = "Windows 11 tweaks")]
    bypass_all: bool,
    /// Bypass the TPM 2.0 requirement
    #[arg(long, help_heading = "Windows 11 tweaks")]
    bypass_tpm: bool,
    /// Bypass the Secure Boot requirement
    #[arg(long, help_heading = "Windows 11 tweaks")]
    bypass_secure_boot: bool,
    /// Bypass the 4 GB minimum-RAM check
    #[arg(long, help_heading = "Windows 11 tweaks")]
    bypass_ram: bool,
    /// Bypass the supported-CPU check
    #[arg(long, help_heading = "Windows 11 tweaks")]
    bypass_cpu: bool,
    /// Bypass the system-disk check
    #[arg(long, help_heading = "Windows 11 tweaks")]
    bypass_storage: bool,
    /// Skip the Microsoft-account requirement (offline install)
    #[arg(long, help_heading = "Windows 11 tweaks")]
    skip_ms_account: bool,
    /// Create this local administrator account
    #[arg(long, value_name = "NAME", help_heading = "Windows 11 tweaks")]
    local_account: Option<String>,
    /// Password for --local-account (omit for a blank one)
    #[arg(long, value_name = "PW", help_heading = "Windows 11 tweaks")]
    local_password: Option<String>,
    /// Disable telemetry / data collection and auto-accept the EULA
    #[arg(long, help_heading = "Windows 11 tweaks")]
    disable_data_collection: bool,
    /// Prevent automatic BitLocker device encryption
    #[arg(long, help_heading = "Windows 11 tweaks")]
    disable_bitlocker: bool,
    /// Disable OneDrive, Copilot, Teams, ads, News and Fast Startup
    #[arg(long, help_heading = "Windows 11 tweaks")]
    qol: bool,
    /// Preset locale, e.g. en-US
    #[arg(long, value_name = "LOCALE", help_heading = "Windows 11 tweaks")]
    region: Option<String>,
    /// Preset time zone, e.g. "India Standard Time"
    #[arg(long, value_name = "TZ", help_heading = "Windows 11 tweaks")]
    timezone: Option<String>,
}

impl TweakArgs {
    fn to_tweaks(&self) -> core::WindowsTweaks {
        core::WindowsTweaks {
            bypass_tpm: self.bypass_tpm || self.bypass_all,
            bypass_secure_boot: self.bypass_secure_boot || self.bypass_all,
            bypass_ram: self.bypass_ram || self.bypass_all,
            bypass_cpu: self.bypass_cpu || self.bypass_all,
            bypass_storage: self.bypass_storage || self.bypass_all,
            skip_ms_account: self.skip_ms_account,
            local_account: self.local_account.clone(),
            local_password: self.local_password.clone(),
            disable_data_collection: self.disable_data_collection,
            disable_bitlocker: self.disable_bitlocker,
            qol_tweaks: self.qol,
            region: self.region.clone(),
            timezone: self.timezone.clone(),
        }
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn human_secs(s: u64) -> String {
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// One carriage-returned status line, so a terminal overwrites in place and
/// the GUI (which splits on \r) gets one event per update.
fn print_progress(p: core::Progress) {
    let eta = p.eta_secs().map(human_secs).unwrap_or_else(|| "--".into());
    print!(
        "\r{} {:.0}% · {} / {} · {}/s · ETA {}    ",
        p.stage.as_str(),
        p.percent(),
        human_bytes(p.bytes),
        human_bytes(p.total),
        human_bytes(p.bytes_per_sec),
        eta
    );
    let _ = std::io::stdout().flush();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => {
            let devs = core::list_removable_devices()?;
            if devs.is_empty() {
                println!("No removable USB devices found.");
                return Ok(());
            }
            for d in devs {
                println!(
                    "{}\n    {:<24} {:>7.1} GiB  removable={}",
                    d.by_id.display(),
                    d.model,
                    d.size_gib(),
                    d.removable
                );
            }
        }
        Cmd::Unattend { tweaks } => {
            print!("{}", core::generate_autounattend(&tweaks.to_tweaks())?);
        }
        Cmd::FormatOptions {
            device,
            size_gb,
            sector_size,
        } => {
            let size_bytes = match (device, size_gb) {
                (Some(device), _) => {
                    core::list_removable_devices()?
                        .into_iter()
                        .find(|d| {
                            d.by_id.to_string_lossy() == device || d.dev.to_string_lossy() == device
                        })
                        .ok_or_else(|| {
                            anyhow!("{device} is not one of the removable devices `list` reports")
                        })?
                        .size_bytes
                }
                (None, Some(gb)) => gb * 1024 * 1024 * 1024,
                (None, None) => bail!("give either --device or --size-gb"),
            };
            show_format_options(size_bytes, sector_size)?;
        }
        Cmd::Write {
            iso,
            device,
            kind,
            yes,
            no_verify,
            tweaks,
            format,
        } => {
            if !iso.exists() {
                bail!("ISO not found: {}", iso.display());
            }
            let d = core::list_removable_devices()?
                .into_iter()
                .find(|d| d.by_id.to_string_lossy() == device || d.dev.to_string_lossy() == device)
                .ok_or_else(|| anyhow!("device not found among removable USBs: {device}"))?;
            core::assert_safe_target(&d)?;

            let k = match kind {
                KindArg::Auto => core::detect_iso_kind(&iso)?,
                KindArg::Windows => core::IsoKind::Windows,
                KindArg::Linux => core::IsoKind::Other,
            };

            let tw = tweaks.to_tweaks();
            refuse_misplaced_options(&k, &format, &tw, no_verify)?;
            println!(
                "Target : {} ({:.1} GiB, {})",
                d.dev.display(),
                d.size_gib(),
                d.model
            );
            println!("ISO    : {}", iso.display());
            println!("Kind   : {k:?}");
            if k == core::IsoKind::Windows && !tw.is_noop() {
                // Fail now, not after the drive has already been wiped.
                core::generate_autounattend(&tw)?;
                println!("Tweaks : {}", tw.summary().join(", "));
            }
            println!("This will PERMANENTLY ERASE the target device.");

            // Work out the layout now, so anything it requires of the target
            // machine is said BEFORE the user commits. Printing it afterwards
            // would make the warning worthless, which is exactly the trap Rufus
            // falls into by not printing one at all.
            let mut chosen_layout = format.layout();
            if k == core::IsoKind::Windows {
                let (_, target) = format.scheme_and_target();
                if chosen_layout.is_none() {
                    chosen_layout = if target.needs_bios_boot_code() {
                        // One layout has a boot record; the size cannot change that.
                        Some(core::format::WindowsLayout::default_for_target(0, target))
                    } else {
                        match core::windows_install_image_size(&iso) {
                            Ok(size) => Some(core::format::WindowsLayout::default_for_target(
                                size, target,
                            )),
                            // Not fatal here: the flasher probes the ISO properly
                            // and will fail there with a better message.
                            Err(e) => {
                                eprintln!("warning: could not inspect the image yet: {e}");
                                None
                            }
                        }
                    };
                }
                if let Some(l) = chosen_layout {
                    check_buildable(&format, l, d.size_bytes, d.sector_size)?;
                    println!("Layout : {}", l.as_str());
                    if let Some(caveat) = l.caveat() {
                        println!();
                        println!("{caveat}");
                        println!();
                    }
                }
                if target.needs_bios_boot_code() {
                    println!("Boot   : BIOS (MBR, with Sirius Flash's own boot code)");
                    if let Some(caveat) = tw.bios_caveat() {
                        println!();
                        println!(
                            "{caveat} (--bypass-secure-boot and --bypass-tpm, or --bypass-all.)"
                        );
                        println!();
                    }
                }
            }

            if !yes {
                print!("Type YES to proceed: ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                if line.trim() != "YES" {
                    bail!("aborted");
                }
            }

            match k {
                core::IsoKind::Windows => {
                    // Unvalidated on purpose: the core resolves the layout
                    // once it has probed the ISO, and the layout decides the
                    // filesystem.
                    let request = format.to_request()?;
                    core::flash_windows_iso(
                        &d,
                        &iso,
                        Some(&tw),
                        request.as_ref(),
                        // The resolved one, so the core cannot decide
                        // differently from what the user just agreed to.
                        chosen_layout,
                        &mut print_progress,
                    )?;
                    println!();
                }
                core::IsoKind::Other => {
                    core::flash_linux_iso(&d, &iso, !no_verify, &mut print_progress)?;
                    println!();
                }
            }
            println!("Done. Safe to remove the USB.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_sizes_parse_with_and_without_a_suffix() {
        assert_eq!(parse_cluster_size("4096").unwrap(), 4096);
        assert_eq!(parse_cluster_size("4K").unwrap(), 4096);
        assert_eq!(parse_cluster_size("32k").unwrap(), 32768);
        assert_eq!(parse_cluster_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_cluster_size(" 8K ").unwrap(), 8192);
    }

    #[test]
    fn a_cluster_size_that_is_not_a_size_is_rejected() {
        for bad in ["", "K", "4G", "-1", "4.5K", "lots"] {
            assert!(
                parse_cluster_size(bad).is_err(),
                "{bad:?} should not parse as a cluster size"
            );
        }
    }

    /// No format flags means no plan, so the core keeps its own defaults rather
    /// than having them restated — and wrongly — in two places.
    #[test]
    fn absent_format_flags_leave_the_core_to_decide() {
        let args = FormatArgs {
            label: None,
            cluster_size: None,
            full_format: false,
            layout: None,
            target: TargetArg::Uefi,
        };
        assert!(args.to_request().unwrap().is_none());
    }

    #[test]
    fn format_flags_become_a_request_the_core_will_validate() {
        let args = FormatArgs {
            label: Some("my install".into()),
            cluster_size: Some("32K".into()),
            full_format: true,
            layout: None,
            target: TargetArg::Uefi,
        };
        let request = args.to_request().unwrap().expect("flags were given");
        assert_eq!(request.cluster_size, Some(32768));
        assert!(!request.quick, "--full-format asks for the slow one");
        // The label is sanitised when the request becomes a plan, which cannot
        // happen until the layout has chosen a filesystem.
        assert_eq!(request.label, "my install");
    }

    /// `--target bios` alone is a request: MBR, BIOS. Without it, and with no
    /// other format flags, the core keeps its defaults.
    #[test]
    fn a_bios_target_is_a_request_for_mbr() {
        let args = FormatArgs {
            label: None,
            cluster_size: None,
            full_format: false,
            layout: None,
            target: TargetArg::Bios,
        };
        let r = args
            .to_request()
            .unwrap()
            .expect("--target bios asks for something");
        assert_eq!(r.scheme, core::format::PartitionScheme::Mbr);
        assert_eq!(r.target, core::format::TargetSystem::Bios);
    }

    /// The check `main` runs before the prompt: BIOS with the NTFS layout is
    /// refused, naming the combination, and BIOS with FAT32 is not; so is a
    /// drive whose sector size could not be read.
    #[test]
    fn the_pre_prompt_check_refuses_what_would_not_boot() {
        use sirius_flash_core::format::WindowsLayout;
        const GIB: u64 = 1024 * 1024 * 1024;
        let bios = FormatArgs {
            target: TargetArg::Bios,
            ..no_format_flags()
        };
        let err = check_buildable(&bios, WindowsLayout::NtfsUefiNtfs, 32 * GIB, 512).unwrap_err();
        assert!(err.to_string().contains("MBR + bios + ntfs"), "got: {err}");
        check_buildable(&bios, WindowsLayout::Fat32Split, 32 * GIB, 512)
            .expect("BIOS + FAT32 is what bootcode installs");
        let err = check_buildable(&bios, WindowsLayout::Fat32Split, 32 * GIB, 4096).unwrap_err();
        assert!(err.to_string().contains("512-byte"), "4Kn: {err}");
        assert!(
            check_buildable(&bios, WindowsLayout::Fat32Split, 32 * GIB, 0).is_err(),
            "an unreadable sector size is refused, not assumed"
        );
        check_buildable(&no_format_flags(), WindowsLayout::Fat32Split, 32 * GIB, 512)
            .expect("the UEFI path is unchanged");
    }

    fn no_format_flags() -> FormatArgs {
        FormatArgs {
            label: None,
            cluster_size: None,
            full_format: false,
            layout: None,
            target: TargetArg::Uefi,
        }
    }

    /// An option that cannot act on this kind of image is refused by name,
    /// before anything is written, and the options that do apply are not.
    #[test]
    fn options_that_do_not_apply_to_the_image_are_refused_by_name() {
        use sirius_flash_core::IsoKind::{Other, Windows};
        let none = core::WindowsTweaks::default();
        let bios = FormatArgs {
            target: TargetArg::Bios,
            ..no_format_flags()
        };
        let err = refuse_misplaced_options(&Other, &bios, &none, false).unwrap_err();
        assert!(err.to_string().contains("--target"), "got: {err}");
        let labelled = FormatArgs {
            label: Some("X".into()),
            ..no_format_flags()
        };
        let err = refuse_misplaced_options(&Other, &labelled, &none, false).unwrap_err();
        assert!(err.to_string().contains("--label"), "got: {err}");
        let tpm = core::WindowsTweaks {
            bypass_tpm: true,
            ..Default::default()
        };
        let err = refuse_misplaced_options(&Other, &no_format_flags(), &tpm, false).unwrap_err();
        assert!(err.to_string().contains("tweaks"), "got: {err}");
        let err = refuse_misplaced_options(&Windows, &no_format_flags(), &none, true).unwrap_err();
        assert!(err.to_string().contains("--no-verify"), "got: {err}");

        // …and what does apply is left alone.
        refuse_misplaced_options(&Other, &no_format_flags(), &none, true)
            .expect("--no-verify is for raw images");
        refuse_misplaced_options(&Windows, &bios, &tpm, false)
            .expect("format options and tweaks are for Windows media");
    }

    /// A cluster size the drive cannot take is still refused before anything is
    /// written — but by the core, once the layout has settled the filesystem,
    /// because which sizes are legal depends on which filesystem it is.
    #[test]
    fn an_impossible_cluster_size_is_carried_through_to_validation() {
        let args = FormatArgs {
            label: None,
            cluster_size: Some("512".into()),
            full_format: false,
            layout: None,
            target: TargetArg::Uefi,
        };
        let request = args.to_request().unwrap().expect("a flag was given");
        assert_eq!(request.cluster_size, Some(512));
        let err = request
            .validate(core::format::Volume::new(32 * 1024 * 1024 * 1024, 512))
            .unwrap_err();
        assert!(err.to_string().contains("16 KB"), "got: {err}");
    }
}
