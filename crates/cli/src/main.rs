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
        #[command(flatten)]
        tweaks: TweakArgs,
    },
    /// Print the autounattend.xml the given tweaks would produce (writes nothing)
    Unattend {
        #[command(flatten)]
        tweaks: TweakArgs,
    },
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
        Cmd::Write {
            iso,
            device,
            kind,
            yes,
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
                core::IsoKind::Windows => core::flash_windows_iso(&d, &iso, Some(&tw))?,
                core::IsoKind::Other => core::flash_linux_iso(&d, &iso)?,
            }
            println!("Done. Safe to remove the USB.");
        }
    }
    Ok(())
}
