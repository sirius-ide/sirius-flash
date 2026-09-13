//! Sirius Flash CLI — list removable USBs and write ISOs to them safely.

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use sirius_flash_core as core;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sirius-flash", version, about = "Cross-platform bootable USB creator (Sirius Flash)")]
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
    },
}

#[derive(ValueEnum, Clone)]
enum KindArg {
    Auto,
    Windows,
    Linux,
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
        Cmd::Write { iso, device, kind, yes } => {
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

            println!("Target : {} ({:.1} GiB, {})", d.dev.display(), d.size_gib(), d.model);
            println!("ISO    : {}", iso.display());
            println!("Kind   : {k:?}");
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
                core::IsoKind::Windows => core::flash_windows_iso(&d, &iso)?,
                core::IsoKind::Other => core::flash_linux_iso(&d, &iso)?,
            }
            println!("Done. Safe to remove the USB.");
        }
    }
    Ok(())
}
