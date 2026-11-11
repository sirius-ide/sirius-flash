//! Install the BIOS boot code on a disk image, exactly as the flasher installs
//! it on a drive: `bootcode::prepare` checks everything first, then the VBR
//! goes to the boot sector and its backup, then the MBR's 440 bytes, then all
//! three are read back.
//!
//! For `scripts/boottest.py`, so the selftest boots what the Rust installer
//! wrote rather than what the harness's own Python installer wrote. Refuses
//! anything that is not a regular file.
//!
//!     cargo run -p sirius-flash-core --example install_bios_boot -- disk.img
//!     cargo run -p sirius-flash-core --example install_bios_boot -- --bootmgr-max

use sirius_flash_core::bootcode;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let arg = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: install_bios_boot IMAGE | --bootmgr-max"))?;
    // The flasher's BOOTMGR size limit, so the harness can check it against
    // what fat32.asm actually loads.
    if arg == "--bootmgr-max" {
        println!("{}", bootcode::BOOTMGR_MAX);
        return Ok(());
    }
    let image = PathBuf::from(arg);
    let p = bootcode::install_on_image(&image)?;
    println!(
        "installed the MBR at LBA 0 and the VBR at LBA {} (partition sectors {:?}), read back",
        p.partition_start(),
        p.vbr_sectors()
    );
    Ok(())
}
