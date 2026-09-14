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
        /// Logical sector size. 512 everywhere except 4Kn media, where the
        /// smaller cluster sizes disappear.
        #[arg(long, default_value = "512")]
        sector_size: u32,
    },
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
                if plan.needs_boot_code() {
                    println!(
                        "        not yet: BIOS media needs an MBR bootstrap and a partition \
                         boot record, which this build does not write"
                    );
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
                    core::flash_windows_iso(&d, &iso, Some(&tw), &mut print_progress)?;
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
