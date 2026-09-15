//! Sirius Flash — core logic: safe device discovery, ISO detection, and flashing.
//!
//! Safety principle: writes are only ever addressed via the stable
//! `/dev/disk/by-id` path, gated on removable + size checks. Kernel names
//! (`sdb`, `nvme0n1`) are treated as unstable and never trusted for targeting.

pub mod blockio;
pub mod format;
pub mod lzw;
pub use blockio::{hex, Progress, Stage};

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

// Device discovery and flashing are Linux-only for now, so these are too —
// without the gate they are unused imports on macOS/Windows, which CI's
// `clippy -D warnings` treats as errors.
#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
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
    /// Logical sector size as the device reports it — 512 almost everywhere,
    /// 4096 on 4Kn media.
    ///
    /// Not cosmetic. A cluster is counted in sectors, so `mkfs.fat -s` means
    /// "this many sectors", and assuming 512 on a 4Kn drive formats with
    /// clusters eight times the size the plan chose. That can drive the
    /// cluster count below FAT32's 65,525 minimum, at which point firmware
    /// refuses to read the volume and the stick does not boot.
    pub sector_size: u32,
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

/// Windows 11 install-experience tweaks.
///
/// Applied by writing an `autounattend.xml` to the root of the USB, which
/// Windows Setup detects and honours automatically — no WIM patching needed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsTweaks {
    /// Bypass the TPM 2.0 requirement.
    pub bypass_tpm: bool,
    /// Bypass the Secure Boot requirement.
    pub bypass_secure_boot: bool,
    /// Bypass the 4 GB minimum-RAM check.
    pub bypass_ram: bool,
    /// Bypass the supported-CPU check (matters on pre-8th-gen Intel).
    pub bypass_cpu: bool,
    /// Bypass the system-disk size/type check.
    pub bypass_storage: bool,
    /// Skip the Microsoft-account requirement in OOBE (offline install).
    pub skip_ms_account: bool,
    /// Create this local administrator account (also skips the MSA screens).
    pub local_account: Option<String>,
    /// Password for `local_account` (`None`/empty = blank, change at first logon).
    pub local_password: Option<String>,
    /// Turn off telemetry / data collection and auto-accept the EULA.
    pub disable_data_collection: bool,
    /// Prevent automatic BitLocker device encryption.
    pub disable_bitlocker: bool,
    /// Disable OneDrive, Copilot, Teams, ads, News and Fast Startup.
    pub qol_tweaks: bool,
    /// Preset locale, e.g. `en-US` — skips the region/keyboard screens.
    pub region: Option<String>,
    /// Preset time zone, e.g. `India Standard Time`.
    pub timezone: Option<String>,
}

/// Account names Windows reserves and will refuse to create.
const RESERVED_ACCOUNT_NAMES: &[&str] = &[
    "administrator",
    "guest",
    "defaultaccount",
    "wdagutilityaccount",
    "helpassistant",
    "krbtgt",
    "local",
    "none",
    "system",
];

impl WindowsTweaks {
    /// Every Windows 11 hardware requirement bypassed (the "install anywhere" preset).
    pub fn all_hardware_bypasses() -> Self {
        Self {
            bypass_tpm: true,
            bypass_secure_boot: true,
            bypass_ram: true,
            bypass_cpu: true,
            bypass_storage: true,
            ..Self::default()
        }
    }

    /// True when nothing is enabled, so no answer file needs writing.
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }

    /// Reject account names Windows reserves; strip characters it forbids.
    pub fn validated_account(&self) -> Result<Option<String>> {
        let Some(raw) = self.local_account.as_deref() else {
            return Ok(None);
        };
        let name: String = raw
            .chars()
            .filter(|c| !r#"/\[]:;|=,+*?<>"@"#.contains(*c))
            .collect();
        let name = name.trim().to_string();
        if name.is_empty() {
            bail!("local account name is empty after removing characters Windows forbids");
        }
        if RESERVED_ACCOUNT_NAMES.contains(&name.to_lowercase().as_str()) {
            bail!("`{name}` is a reserved Windows account name — pick another");
        }
        Ok(Some(name))
    }

    /// Human-readable list of what will be applied (for the progress log).
    pub fn summary(&self) -> Vec<String> {
        let mut s = Vec::new();
        let hw: Vec<&str> = [
            (self.bypass_tpm, "TPM 2.0"),
            (self.bypass_secure_boot, "Secure Boot"),
            (self.bypass_ram, "RAM"),
            (self.bypass_cpu, "CPU"),
            (self.bypass_storage, "storage"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect();
        if !hw.is_empty() {
            s.push(format!("bypass {} check(s)", hw.join(" / ")));
        }
        if self.skip_ms_account {
            s.push("skip Microsoft account".into());
        }
        if let Some(a) = &self.local_account {
            s.push(format!("create local admin `{a}`"));
        }
        if self.disable_data_collection {
            s.push("disable data collection".into());
        }
        if self.disable_bitlocker {
            s.push("prevent BitLocker auto-encryption".into());
        }
        if self.qol_tweaks {
            s.push("QoL: no OneDrive / Copilot / Teams / ads".into());
        }
        if let Some(r) = &self.region {
            s.push(format!("locale {r}"));
        }
        if let Some(t) = &self.timezone {
            s.push(format!("time zone {t}"));
        }
        s
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Encode a password the way `unattend.xml` expects: the literal string
/// "Password" is appended, then the whole thing is UTF-16LE + Base64.
/// An empty password therefore yields the well-known `UABhAHMAcwB3AG8AcgBkAA==`.
fn encode_unattend_password(pw: &str) -> String {
    let bytes: Vec<u8> = pw
        .encode_utf16()
        .chain("Password".encode_utf16())
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64(&bytes)
}

const COMPONENT_ATTRS: &str = concat!(
    "processorArchitecture=\"amd64\" language=\"neutral\" ",
    "xmlns:wcm=\"http://schemas.microsoft.com/WMIConfig/2002/State\" ",
    "xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" ",
    "publicKeyToken=\"31bf3856ad364e35\" versionScope=\"nonSxS\""
);

fn component(name: &str, inner: Vec<String>) -> Vec<String> {
    let mut lines = vec![format!("    <component name=\"{name}\" {COMPONENT_ATTRS}>")];
    lines.extend(inner);
    lines.push("    </component>".into());
    lines
}

/// Ordered `<RunSynchronousCommand>` entries (used in windowsPE/specialize).
fn run_synchronous(cmds: &[String]) -> Vec<String> {
    let mut lines = vec!["      <RunSynchronous>".to_string()];
    for (i, c) in cmds.iter().enumerate() {
        lines.push("        <RunSynchronousCommand wcm:action=\"add\">".into());
        lines.push(format!("          <Order>{}</Order>", i + 1));
        lines.push(format!("          <Path>{}</Path>", xml_escape(c)));
        lines.push("        </RunSynchronousCommand>".into());
    }
    lines.push("      </RunSynchronous>".into());
    lines
}

/// Build the `autounattend.xml` that applies `t` during Windows Setup.
pub fn generate_autounattend(t: &WindowsTweaks) -> Result<String> {
    let account = t.validated_account()?;
    let mut out = vec![
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>".to_string(),
        "<!-- Generated by Sirius Flash — https://github.com/sirius-ide/sirius-flash -->"
            .to_string(),
        "<unattend xmlns=\"urn:schemas-microsoft-com:unattend\">".to_string(),
    ];

    // ---- windowsPE: LabConfig keys are read by Setup's compatibility gate ----
    let bypasses: Vec<String> = [
        (t.bypass_tpm, "BypassTPMCheck"),
        (t.bypass_secure_boot, "BypassSecureBootCheck"),
        (t.bypass_ram, "BypassRAMCheck"),
        (t.bypass_cpu, "BypassCPUCheck"),
        (t.bypass_storage, "BypassStorageCheck"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, k)| format!(r"reg add HKLM\SYSTEM\Setup\LabConfig /v {k} /t REG_DWORD /d 1 /f"))
    .collect();
    if !bypasses.is_empty() {
        let mut inner = vec![
            // WinPE refuses to proceed without a ProductKey element — even an empty one.
            "      <UserData>".to_string(),
            "        <AcceptEula>true</AcceptEula>".to_string(),
            "        <ProductKey>".to_string(),
            "          <Key />".to_string(),
            "        </ProductKey>".to_string(),
            "      </UserData>".to_string(),
        ];
        inner.extend(run_synchronous(&bypasses));
        out.push("  <settings pass=\"windowsPE\">".into());
        out.extend(component("Microsoft-Windows-Setup", inner));
        out.push("  </settings>".into());
    }

    // ---- specialize: applied to the installed image before OOBE ----
    let mut sp: Vec<String> = Vec::new();
    if t.skip_ms_account || account.is_some() {
        sp.push(r#"reg add "HKLM\Software\Microsoft\Windows\CurrentVersion\OOBE" /v BypassNRO /t REG_DWORD /d 1 /f"#.into());
    }
    if t.disable_data_collection {
        sp.push(r#"reg add "HKLM\Software\Policies\Microsoft\Windows\DataCollection" /v AllowTelemetry /t REG_DWORD /d 0 /f"#.into());
    }
    if t.disable_bitlocker {
        sp.push(r#"reg add "HKLM\System\CurrentControlSet\Control\BitLocker" /v PreventDeviceEncryption /t REG_DWORD /d 1 /f"#.into());
    }
    if t.qol_tweaks {
        sp.extend([
            r#"reg add "HKLM\Software\Policies\Microsoft\Windows\OneDrive" /v DisableFileSyncNGSC /t REG_DWORD /d 1 /f"#.to_string(),
            r#"reg add "HKLM\Software\Policies\Microsoft\Windows\WindowsCopilot" /v TurnOffWindowsCopilot /t REG_DWORD /d 1 /f"#.to_string(),
            r#"reg add "HKLM\Software\Policies\Microsoft\Windows\CloudContent" /v DisableWindowsConsumerFeatures /t REG_DWORD /d 1 /f"#.to_string(),
            r#"reg add "HKLM\Software\Microsoft\Windows\CurrentVersion\Communications" /v ConfigureChatAutoInstall /t REG_DWORD /d 0 /f"#.to_string(),
            r#"reg add "HKLM\Software\Policies\Microsoft\Dsh" /v AllowNewsAndInterests /t REG_DWORD /d 0 /f"#.to_string(),
            r#"reg add "HKLM\Software\Policies\Microsoft\Edge" /v HideFirstRunExperience /t REG_DWORD /d 1 /f"#.to_string(),
            r#"reg add "HKLM\System\CurrentControlSet\Control\Session Manager\Power" /v HiberbootEnabled /t REG_DWORD /d 0 /f"#.to_string(),
        ]);
    }
    if !sp.is_empty() {
        out.push("  <settings pass=\"specialize\">".into());
        out.extend(component(
            "Microsoft-Windows-Deployment",
            run_synchronous(&sp),
        ));
        out.push("  </settings>".into());
    }

    // ---- oobeSystem: the out-of-box experience the user actually sees ----
    let mut shell: Vec<String> = Vec::new();
    if t.disable_data_collection {
        shell.extend([
            "      <OOBE>".to_string(),
            "        <HideEULAPage>true</HideEULAPage>".to_string(),
            // Microsoft's euphemism for "allow data collection".
            "        <ProtectYourPC>3</ProtectYourPC>".to_string(),
            "        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>".to_string(),
            "        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>".to_string(),
            "      </OOBE>".to_string(),
        ]);
    }
    if let Some(tz) = &t.timezone {
        shell.push(format!("      <TimeZone>{}</TimeZone>", xml_escape(tz)));
    }
    // First-logon commands must all live in ONE section — Windows rejects duplicates.
    let mut first_logon: Vec<String> = Vec::new();
    if let Some(name) = &account {
        let n = xml_escape(name);
        let pw = encode_unattend_password(t.local_password.as_deref().unwrap_or(""));
        shell.extend([
            "      <UserAccounts>".to_string(),
            "        <LocalAccounts>".to_string(),
            "          <LocalAccount wcm:action=\"add\">".to_string(),
            format!("            <Name>{n}</Name>"),
            format!("            <DisplayName>{n}</DisplayName>"),
            "            <Group>Administrators</Group>".to_string(),
            "            <Password>".to_string(),
            format!("              <Value>{pw}</Value>"),
            "              <PlainText>false</PlainText>".to_string(),
            "            </Password>".to_string(),
            "          </LocalAccount>".to_string(),
            "        </LocalAccounts>".to_string(),
            "      </UserAccounts>".to_string(),
        ]);
        if t.local_password.as_deref().unwrap_or("").is_empty() {
            // Blank password: force a change at first logon, and stop it expiring.
            first_logon.push(format!(r#"net user "{name}" /logonpasswordchg:yes"#));
            first_logon.push("net accounts /maxpwage:unlimited".to_string());
        }
    }
    if t.qol_tweaks {
        // HKCU keys only exist once a user is logged on.
        first_logon.extend([
            r#"reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced" /v ShowCopilotButton /t REG_DWORD /d 0 /f"#.to_string(),
            r#"reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Search" /v SearchboxTaskbarMode /t REG_DWORD /d 1 /f"#.to_string(),
            r#"reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Search" /v BingSearchEnabled /t REG_DWORD /d 0 /f"#.to_string(),
            r#"reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager" /v SystemPaneSuggestionsEnabled /t REG_DWORD /d 0 /f"#.to_string(),
        ]);
    }
    if !first_logon.is_empty() {
        shell.push("      <FirstLogonCommands>".into());
        for (i, c) in first_logon.iter().enumerate() {
            shell.push("        <SynchronousCommand wcm:action=\"add\">".into());
            shell.push(format!("          <Order>{}</Order>", i + 1));
            shell.push(format!(
                "          <CommandLine>{}</CommandLine>",
                xml_escape(c)
            ));
            shell.push("        </SynchronousCommand>".into());
        }
        shell.push("      </FirstLogonCommands>".into());
    }

    let mut oobe_components: Vec<String> = Vec::new();
    if !shell.is_empty() {
        oobe_components.extend(component("Microsoft-Windows-Shell-Setup", shell));
    }
    if let Some(r) = &t.region {
        let r = xml_escape(r);
        oobe_components.extend(component(
            "Microsoft-Windows-International-Core",
            vec![
                format!("      <InputLocale>{r}</InputLocale>"),
                format!("      <SystemLocale>{r}</SystemLocale>"),
                format!("      <UILanguage>{r}</UILanguage>"),
                format!("      <UserLocale>{r}</UserLocale>"),
            ],
        ));
    }
    if !oobe_components.is_empty() {
        out.push("  <settings pass=\"oobeSystem\">".into());
        out.extend(oobe_components);
        out.push("  </settings>".into());
    }

    out.push("</unattend>".into());
    Ok(out.join("\n") + "\n")
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
        // `size` is in 512-byte units whatever the logical block size is, so
        // this stays x512 even on 4Kn media.
        let size_bytes = read_u64(&sys.join("size")).unwrap_or(0) * 512;
        let sector_size = read_u64(&sys.join("queue/logical_block_size")).unwrap_or(512) as u32;
        let model = fs::read_to_string(sys.join("device/model"))
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(UsbDevice {
            by_id,
            dev,
            model,
            size_bytes,
            removable,
            sector_size,
        });
    }
    out.sort_by(|a, b| a.by_id.cmp(&b.by_id));
    Ok(out)
}

/// True if an archive listing names the Windows installer image.
#[cfg(target_os = "linux")]
fn lists_windows_image(list: &str) -> bool {
    let l = list.to_lowercase();
    l.contains("sources/install.wim")
        || l.contains("sources\\install.wim")
        || l.contains("sources/install.esd")
        || l.contains("sources\\install.esd")
}

/// Detect whether an ISO is a Windows installer.
///
/// The reliable marker is `sources/install.{wim,esd}`, but two gotchas make
/// this non-trivial and both, if mishandled, produce a broken USB:
///  - Modern Win11 ISOs are **UDF**; their ISO9660 layer is only a stub
///    (a lone `README.TXT`). `bsdtar` reads ISO9660 and so never sees the real
///    files, so we must also consult a UDF-aware lister (`7z`) and `blkid`.
///  - We must never conclude "Other" from a tool that saw only the stub — a
///    UDF Windows ISO mistaken for "Other" would be `dd`'d directly, and its
///    oversized `install.wim` (over 4 GB) cannot live on the FAT32 partition
///    UEFI needs, giving a non-booting USB. So a UDF volume is treated as
///    Windows even when no lister could read the tree; if that guess is ever
///    wrong the Windows path fails loudly (no install image) rather than
///    writing a bad one.
#[cfg(target_os = "linux")]
pub fn detect_iso_kind(iso: &Path) -> Result<IsoKind> {
    let mut read_ok = false;

    // 1a. bsdtar — fast ISO9660 reader (Linux ISOs, older Windows ISOs).
    if let Ok(o) = Command::new("bsdtar").arg("-tf").arg(iso).output() {
        if o.status.success() {
            read_ok = true;
            if lists_windows_image(&String::from_utf8_lossy(&o.stdout)) {
                return Ok(IsoKind::Windows);
            }
        }
    }
    // 1b. 7z — UDF-aware; sees the real files of modern Win11 ISOs, whose
    //     ISO9660 layer (all that bsdtar reads) is only a stub.
    if let Ok(o) = Command::new("7z").arg("l").arg(iso).output() {
        if o.status.success() {
            read_ok = true;
            if lists_windows_image(&String::from_utf8_lossy(&o.stdout)) {
                return Ok(IsoKind::Windows);
            }
        }
    }

    // 2. blkid — a bootable UDF volume is Windows install media in practice
    //    (Linux media are ISO9660/hybrid, reported as iso9660). Trust this even
    //    when no lister saw the tree (e.g. `7z` absent), per the note above.
    if let Ok(o) = Command::new("blkid")
        .args(["-o", "export"])
        .arg(iso)
        .output()
    {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some(v) = line.strip_prefix("TYPE=") {
                    if v.eq_ignore_ascii_case("udf") {
                        return Ok(IsoKind::Windows);
                    }
                }
            }
        }
    }

    // 3. A reader saw the contents and found no Windows marker -> Other (Linux/etc).
    if read_ok {
        return Ok(IsoKind::Other);
    }

    // 4. Nothing could read the ISO -> last-resort filename heuristic.
    let n = iso
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    Ok(if n.contains("win") {
        IsoKind::Windows
    } else {
        IsoKind::Other
    })
}

#[cfg(target_os = "linux")]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        // Helper tools are run in a predictable UTF-8 locale rather than
        // whatever we inherited. Under pkexec the environment is reset, and in
        // the bare `C` locale `mkfs.exfat` refuses any non-ASCII volume label
        // with "invalid character sequence in current locale" — after `parted`
        // has already run. Which labels work should not depend on how the tool
        // was launched. (FAT32 is a separate matter: it converts through CP850
        // in every locale, which is why its labels are folded to ASCII.)
        .env("LC_ALL", "C.UTF-8")
        .status()
        .with_context(|| format!("failed to spawn `{cmd}`"))?;
    if !status.success() {
        bail!("`{cmd}` exited with {status}");
    }
    Ok(())
}

/// Pick the install image a Windows ISO ships, with its size.
///
/// Media carries either `install.wim` or `install.esd` — `detect_iso_kind`
/// accepts both, so the flashing path must handle both. Hardcoding `.wim` here
/// meant an `.esd` ISO wiped the drive and only then failed.
#[cfg(target_os = "linux")]
fn pick_install_image(sources_dir: &Path) -> Option<(String, u64)> {
    ["install.wim", "install.esd"].iter().find_map(|name| {
        fs::metadata(sources_dir.join(name))
            .ok()
            .map(|m| ((*name).to_string(), m.len()))
    })
}

/// Flash a Windows installer ISO (GPT + FAT32, splitting the install image if
/// it exceeds FAT32's 4 GiB file limit). Requires root; erases the device.
///
/// `tweaks`, when present, is written to the USB as `autounattend.xml` — this is
/// what bypasses the Windows 11 TPM / Secure Boot / RAM checks and the
/// Microsoft-account requirement.
/// The removable-media bootloader names UEFI firmware looks for, one per
/// architecture.
///
/// UEFI §3.5.1.1: with no boot entry, firmware appends
/// `\EFI\BOOT\BOOT{machine type short-name}.EFI`. Assuming `bootx64.efi`
/// silently excludes every ARM64 Windows ISO, which carries `bootaa64.efi`.
#[cfg(target_os = "linux")]
const EFI_BOOT_NAMES: [&str; 6] = [
    "bootx64.efi",
    "bootaa64.efi",
    "bootia32.efi",
    "bootarm.efi",
    "bootia64.efi",
    "bootriscv64.efi",
];

/// Find the removable-media bootloader in a mounted tree, whatever its
/// architecture and whatever case the filesystem reports.
///
/// ISO9660 and UDF disagree about case, and FAT does not preserve it, so both
/// the directory walk and the name match are case-insensitive.
#[cfg(target_os = "linux")]
fn find_efi_bootloader(root: &Path) -> Option<PathBuf> {
    let efi = child_ignoring_case(root, "efi")?;
    let boot = child_ignoring_case(&efi, "boot")?;
    EFI_BOOT_NAMES
        .iter()
        .find_map(|n| child_ignoring_case(&boot, n))
}

/// One directory entry matching `name` without regard to case.
#[cfg(target_os = "linux")]
fn child_ignoring_case(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        (e.file_name().to_string_lossy().to_lowercase() == name.to_lowercase()).then(|| e.path())
    })
}

/// The UEFI:NTFS partition image, vendored from Rufus.
///
/// Embedded rather than built, because it cannot be built: its value is the
/// Microsoft Secure Boot signature on the binaries inside, and only Microsoft
/// can produce that. See `crates/core/assets/README.md` for the provenance and
/// the licences of its three upstreams.
pub const UEFI_NTFS_IMAGE: &[u8] = include_bytes!("../assets/uefi-ntfs.img");

/// What `UEFI_NTFS_IMAGE` must hash to.
///
/// Checked before the image is written, not merely at build time: this blob is
/// the one thing we ship that we cannot rebuild or audit line by line, and it
/// goes onto a user's drive verbatim.
pub const UEFI_NTFS_SHA256: &str =
    "72683fa1250eeea772d3399277b434d4e55ba8dd0dc926e52d817e701fc2eb9e";

/// Lay out the partitions for a layout, and return the device paths.
///
/// `sfdisk` rather than `parted` for the two-partition case, because it is the
/// only one of the two that can set what this layout actually needs: an exact
/// type GUID, a partition name, and GPT attribute bit 63.
///
/// The type GUID is Microsoft **basic data**, not ESP, and that is deliberate.
/// Rufus's comment on the same decision (`src/drive.c:2477-2486`) explains why:
/// a GPT drive declaring two ESPs makes the Windows installer fail at "Copying
/// Windows Files". Bit 63 is "no drive letter", which keeps the 1 MiB helper
/// partition from appearing as a drive in Windows.
///
/// The helper partition goes last, as Rufus places it. The loader finds its
/// target by reading each volume's filesystem magic rather than by partition
/// number, so the order is about keeping Windows Setup happy, not about boot.
#[cfg(target_os = "linux")]
fn partition_for_layout(
    dev: &str,
    device_size: u64,
    sector_size: u64,
    layout: format::WindowsLayout,
) -> Result<()> {
    const MIB: u64 = 1024 * 1024;
    let total = device_size / sector_size;
    // GPT keeps its backup header and entry array in the last 33 sectors.
    let last_usable = total.saturating_sub(34);
    let first = MIB / sector_size;
    const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

    let script = if layout.needs_uefi_ntfs_partition() {
        let helper = UEFI_NTFS_IMAGE.len() as u64 / sector_size;
        if last_usable <= first + helper {
            bail!("this drive is too small to hold both a data partition and the loader");
        }
        // Align the helper partition down to a mebibyte so the data partition
        // ends on a boundary too.
        let helper_start = (last_usable - helper + 1) / (MIB / sector_size) * (MIB / sector_size);
        format!(
            "label: gpt\n\
             start={first}, size={}, type={BASIC_DATA}, name=\"Main Data Partition\"\n\
             start={helper_start}, size={helper}, type={BASIC_DATA}, name=\"UEFI:NTFS\", attrs=\"63\"\n",
            helper_start - first
        )
    } else {
        format!(
            "label: gpt\n\
             start={first}, size={}, type={BASIC_DATA}, name=\"Main Data Partition\"\n",
            last_usable - first + 1
        )
    };

    let mut child = Command::new("sfdisk")
        .arg("--quiet")
        .arg(dev)
        .env("LC_ALL", "C.UTF-8")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn `sfdisk`")?;
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(script.as_bytes())
        .context("writing the partition script to sfdisk")?;
    let status = child.wait().context("waiting for sfdisk")?;
    if !status.success() {
        bail!("`sfdisk` exited with {status} while partitioning {dev}");
    }
    Ok(())
}

/// Exposed so the layout can be exercised against a plain file, which is how
/// it is tested without a real drive.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn partition_for_layout_for_test(
    dev: &str,
    device_size: u64,
    sector_size: u64,
    layout: format::WindowsLayout,
) -> Result<()> {
    partition_for_layout(dev, device_size, sector_size, layout)
}

/// Write the vendored UEFI:NTFS image onto its partition.
///
/// Checked against its pinned hash first. This blob is the one thing we ship
/// that we cannot rebuild, and it goes to the drive verbatim, so it is verified
/// every time rather than trusted because it was right at build time.
#[cfg(target_os = "linux")]
fn write_uefi_ntfs(part: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let digest = blockio::hex(&Sha256::digest(UEFI_NTFS_IMAGE));
    if digest != UEFI_NTFS_SHA256 {
        bail!(
            "the built-in UEFI:NTFS image hashes to {digest}, not {UEFI_NTFS_SHA256} — \
             refusing to write a loader we cannot vouch for"
        );
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .open(part)
        .with_context(|| format!("opening {part} to write the UEFI:NTFS loader"))?;
    out.write_all(UEFI_NTFS_IMAGE)
        .with_context(|| format!("writing the UEFI:NTFS loader to {part}"))?;
    out.flush().context("flushing the loader partition")?;
    out.sync_all().context("syncing the loader partition")?;
    Ok(())
}

/// Is this external tool on PATH?
///
/// Searched by hand rather than by running it: plenty of these have no
/// `--version`, and spawning a formatter to ask whether it exists is a poor
/// idea on a path whose whole job is not to touch anything yet.
#[cfg(target_os = "linux")]
fn tool_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() && {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&candidate)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
    })
}

/// Fail now if anything this run will need is missing.
///
/// Every external tool the destructive phase uses has to be checked *before*
/// that phase, not when it is reached. `wimlib-imagex` was the worst of these:
/// it runs after the partitioning, the format and the whole file copy, so a
/// machine without it lost the drive's contents, waited through several
/// gigabytes of copying, and only then heard that a package was missing.
/// `mkfs.fat` had the same shape one step earlier.
///
/// Reports everything missing at once, because being told about them one
/// reinstall at a time is its own kind of unhelpful.
#[cfg(target_os = "linux")]
fn require_tools(tools: &[&str]) -> Result<()> {
    let missing: Vec<&str> = tools.iter().copied().filter(|t| !tool_exists(t)).collect();
    if !missing.is_empty() {
        bail!(
            "these tools are needed but not installed: {}. Nothing has been written to \
             the drive.",
            missing.join(", ")
        );
    }
    Ok(())
}

/// Where the single partition the Windows path creates begins.
///
/// `parted ... mkpart WIN11 fat32 1MiB 100%` — so the filesystem never spans
/// the whole drive, and a plan validated against the drive size is describing a
/// volume 1 MiB larger than the one that gets created. That matters at a
/// cluster-size band boundary, where it can pick a size the real volume does
/// not allow.
pub const WINDOWS_PARTITION_START: u64 = 1024 * 1024;

/// The size of the volume the Windows path will actually format.
pub fn windows_volume_size(device_size: u64) -> u64 {
    device_size.saturating_sub(WINDOWS_PARTITION_START)
}

/// What this build can actually turn into bootable Windows media.
///
/// The options model in [`format`] describes the whole of Rufus's panel; this
/// is the corner of it we can produce media for today. Anything outside it is
/// refused here rather than formatted into a drive that silently will not boot.
#[cfg(target_os = "linux")]
fn assert_buildable(plan: &format::FormatPlan) -> Result<()> {
    plan.buildable().map_err(|why| {
        anyhow::anyhow!(
            "this build cannot make bootable {} + {} + {} media: {why}",
            plan.scheme().as_str().to_uppercase(),
            plan.target().as_str(),
            plan.filesystem()
        )
    })
}

#[cfg(target_os = "linux")]
pub fn flash_windows_iso(
    d: &UsbDevice,
    iso: &Path,
    tweaks: Option<&WindowsTweaks>,
    format: Option<&format::FormatPlan>,
    layout: Option<format::WindowsLayout>,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    assert_safe_target(d)?;

    // Everything about the format is settled here, while the drive is still
    // intact — invariant 1. A plan we cannot build must fail now, not after
    // `parted` has run.
    let default_plan;
    let plan = match format {
        Some(p) => p,
        None => {
            default_plan = format::FormatRequest {
                scheme: format::PartitionScheme::Gpt,
                target: format::TargetSystem::Uefi,
                filesystem: format::FileSystem::Fat32,
                cluster_size: None,
                label: "WIN11USB".into(),
                quick: true,
            }
            .validate(format::Volume::new(
                windows_volume_size(d.size_bytes),
                d.sector_size,
            ))?;
            &default_plan
        }
    };
    // A plan is proof that a combination is valid — for the drive it was
    // checked against. It carries that drive's geometry, so a plan built
    // elsewhere would silently format with the wrong cluster arithmetic.
    let expected = format::Volume::new(windows_volume_size(d.size_bytes), d.sector_size);
    if plan.volume() != expected {
        bail!(
            "this format plan was checked against a {:.1} GiB volume with {} byte sectors, \
             but {} will produce {:.1} GiB with {} byte sectors — a plan is only proof for \
             the drive it was checked against",
            plan.volume().size_bytes as f64 / 1024.0_f64.powi(3),
            plan.volume().sector_size,
            d.dev.display(),
            expected.size_bytes as f64 / 1024.0_f64.powi(3),
            expected.sector_size
        );
    }
    assert_buildable(plan)?;
    for w in plan.warnings() {
        println!("note: {w}");
    }

    let dev = d.by_id.to_string_lossy().to_string();
    let part1 = format!("{dev}-part1");

    let iso_mnt = "/run/sirius-flash-iso";
    let usb_mnt = "/run/sirius-flash-usb";
    fs::create_dir_all(iso_mnt)?;
    fs::create_dir_all(usb_mnt)?;

    // Inspect the ISO BEFORE touching the drive. A bad or unexpected image must
    // fail while the USB is still intact — never after `parted` has wiped it.
    run("mount", &["-o", "loop,ro", &iso.to_string_lossy(), iso_mnt])?;
    let (install_img, img_size) = match pick_install_image(Path::new(&format!("{iso_mnt}/sources")))
    {
        Some(v) => v,
        None => {
            let _ = Command::new("umount").arg(iso_mnt).status();
            bail!("not a Windows installer: neither sources/install.wim nor sources/install.esd is present");
        }
    };

    // Which layout? Rufus's trigger is the size of the largest file, and we
    // follow it — but unlike Rufus we say what the choice costs, here, while
    // the drive is still intact and the user can still change their mind.
    let layout = layout.unwrap_or_else(|| format::WindowsLayout::default_for(img_size));
    println!("layout: {}", layout.as_str());
    if let Some(caveat) = layout.caveat() {
        println!("note: {caveat}");
    }

    // Which bootloader does this ISO actually carry? An ARM64 Windows ISO has
    // `bootaa64.efi`, not `bootx64.efi`. Finding that out after the copy — which
    // is where the check used to be — means the drive is already wiped and the
    // installer already written before we notice. Invariant 1.
    let boot_name = match find_efi_bootloader(Path::new(iso_mnt)) {
        Some(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default(),
        None => {
            let _ = Command::new("umount").arg(iso_mnt).status();
            bail!(
                "{} has no \\EFI\\BOOT bootloader, so UEFI firmware would have nothing to \
                 start — it cannot be made into bootable media this way",
                iso.display()
            );
        }
    };

    // Does it fit? The filesystem goes on partition 1, which starts 1 MiB in,
    // and FAT32's own tables cost a little more on top. Checking this after the
    // wipe — which is where it used to happen, implicitly, when the copy ran
    // out of room — leaves the user with an erased drive and a half-written
    // installer. Invariant 1.
    //
    // The contents are measured whole, including any install image that will be
    // split: splitting turns one oversized file into `.swm` chunks of much the
    // same total size, so it saves no space at all.
    let content_bytes = blockio::tree_size(Path::new(iso_mnt))?;
    // FAT32 metadata is roughly 0.1% of the volume; 1% is a safe margin that
    // still refuses only genuinely hopeless cases.
    let usable = (windows_volume_size(d.size_bytes) as f64 * 0.99) as u64;
    if content_bytes > usable {
        let _ = Command::new("umount").arg(iso_mnt).status();
        bail!(
            "this image needs {:.1} GiB but {} holds about {:.1} GiB once formatted",
            content_bytes as f64 / 1024.0_f64.powi(3),
            d.dev.display(),
            usable as f64 / 1024.0_f64.powi(3)
        );
    }

    // Everything the destructive phase will shell out to, checked while the
    // drive is still intact. The splitter is only required when the install
    // image actually exceeds what FAT32 can hold.
    let mut needed = vec!["sfdisk", "udevadm", "mount", "umount", "sync"];
    match layout {
        format::WindowsLayout::Fat32Split => {
            needed.push("mkfs.fat");
            if layout.needs_splitter(img_size) {
                needed.push("wimlib-imagex");
            }
        }
        format::WindowsLayout::NtfsUefiNtfs => needed.push("mkfs.ntfs"),
    }
    if let Err(e) = require_tools(&needed) {
        let _ = Command::new("umount").arg(iso_mnt).status();
        return Err(e);
    }

    // ---- everything from here on is destructive ----
    let _ = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "for p in {dev}-part*; do umount \"$p\" 2>/dev/null || true; done"
        ))
        .status();

    let part2 = format!("{dev}-part2");
    let result = (|| -> Result<()> {
        partition_for_layout(&dev, d.size_bytes, u64::from(d.sector_size), layout)?;
        run("udevadm", &["settle"])?;

        match layout {
            format::WindowsLayout::Fat32Split => {
                // `-s` is sectors per cluster, not bytes.
                let spc = (plan.cluster_size() / plan.volume().sector_size).to_string();
                let mut mkfs = vec!["-F", "32", "-s", &spc, "-n", plan.label()];
                if !plan.quick() {
                    // `-c` reads every sector and marks the unreadable ones bad.
                    // It is read-only — it writes no pattern and reads none back
                    // — so it finds a failing stick but NOT a fake-capacity
                    // counterfeit, whose unwritten sectors read back fine and
                    // whose writes wrap silently. Catching those needs a
                    // write-and-verify pass, which read-back verification after
                    // the image is written already does.
                    mkfs.push("-c");
                }
                mkfs.push(&part1);
                run("mkfs.fat", &mkfs)?;
            }
            format::WindowsLayout::NtfsUefiNtfs => {
                // -Q is a quick format; -F skips the "this looks mounted"
                // heuristics, which misfire on a device we have just
                // repartitioned.
                let mut mkfs = vec!["-Q", "-F", "-L", plan.label()];
                if !plan.quick() {
                    // ntfs-3g spells the surface scan differently to dosfstools.
                    mkfs.retain(|a| *a != "-Q");
                }
                mkfs.push(&part1);
                run("mkfs.ntfs", &mkfs)?;
                // The loader partition is written raw: it is already a FAT12
                // filesystem, so there is nothing to format.
                write_uefi_ntfs(&part2)?;
            }
        }
        run("mount", &[&part1, usb_mnt])?;

        // FAT32 cannot store a file of 4 GiB or more, so an oversized install
        // image is split into .swm chunks. NTFS has no such limit, which is the
        // whole reason that layout exists.
        const FAT32_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;
        let split = layout == format::WindowsLayout::Fat32Split && img_size > FAT32_MAX_FILE;
        // When the image is too big for FAT32 it is left out of the copy and
        // split into .swm chunks afterwards instead.
        let exclude: Vec<String> = if split {
            vec![format!("sources/{install_img}")]
        } else {
            Vec::new()
        };
        println!(
            "Copying ISO contents ({install_img}, {} MiB — {})",
            img_size / 1024 / 1024,
            if split {
                "will be split for FAT32"
            } else if layout == format::WindowsLayout::NtfsUefiNtfs {
                "copied whole onto NTFS"
            } else {
                "fits FAT32, copied whole"
            }
        );
        blockio::copy_tree(
            Path::new(iso_mnt),
            Path::new(usb_mnt),
            &exclude,
            on_progress,
        )?;
        if split {
            run(
                "wimlib-imagex",
                &[
                    "split",
                    &format!("{iso_mnt}/sources/{install_img}"),
                    &format!("{usb_mnt}/sources/install.swm"),
                    "3800",
                ],
            )?;
        }
        if let Some(t) = tweaks {
            if !t.is_noop() {
                let xml = generate_autounattend(t)?;
                fs::write(format!("{usb_mnt}/autounattend.xml"), xml)
                    .context("writing autounattend.xml to the USB")?;
                println!("Windows tweaks applied via autounattend.xml:");
                for line in t.summary() {
                    println!("  • {line}");
                }
            }
        }
        if find_efi_bootloader(Path::new(usb_mnt)).is_none() {
            bail!("verification failed: efi/boot/{boot_name} missing on USB");
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
pub fn flash_linux_iso(
    d: &UsbDevice,
    iso: &Path,
    verify: bool,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    assert_safe_target(d)?;
    let compression = blockio::detect_compression(iso)?;
    // How much actually lands on the device is only knowable up front for the
    // verbatim formats; a compressed image's real size is not in the container,
    // so that case is caught by the ENOSPC handling in write_image. Either way
    // this happens before anything is wiped rather than partway through.
    if let Some(lands) = blockio::payload_len(iso)? {
        if lands > d.size_bytes {
            bail!(
                "image is {:.1} GiB but {} holds only {:.1} GiB",
                lands as f64 / 1024.0_f64.powi(3),
                d.dev.display(),
                d.size_gib()
            );
        }
    }
    if compression != blockio::Compression::None {
        println!("writing {} image on the fly", compression.as_str());
    }
    let dev = d.by_id.to_string_lossy().to_string();
    let _ = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "for p in {dev}-part*; do umount \"$p\" 2>/dev/null || true; done"
        ))
        .status();

    let outcome = blockio::write_image(iso, Path::new(&dev), on_progress)?;
    println!("sha256 {}", hex(&outcome.digest));
    if verify {
        // Verify against what was actually written: for a compressed image
        // that is the decompressed length, not the size of the file on disk.
        blockio::verify_written(
            Path::new(&dev),
            &outcome.digest,
            outcome.bytes_written,
            on_progress,
        )?;
        println!("verified: the device reads back byte-for-byte identical");
    }
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
pub fn flash_windows_iso(
    _d: &UsbDevice,
    _iso: &Path,
    _tweaks: Option<&WindowsTweaks>,
    _format: Option<&format::FormatPlan>,
    _layout: Option<format::WindowsLayout>,
    _on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    bail!("Windows-ISO flashing not yet implemented on this OS")
}
#[cfg(not(target_os = "linux"))]
pub fn flash_linux_iso(
    _d: &UsbDevice,
    _iso: &Path,
    _verify: bool,
    _on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    bail!("Linux-ISO flashing not yet implemented on this OS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_removable() {
        let d = UsbDevice {
            by_id: "/x".into(),
            dev: "/dev/sda".into(),
            model: "big".into(),
            size_bytes: 8_000_000_000_000,
            removable: false,
            sector_size: 512,
        };
        assert!(assert_safe_target(&d).is_err());
    }
    #[test]
    fn rejects_oversized() {
        let d = UsbDevice {
            by_id: "/x".into(),
            dev: "/dev/sda".into(),
            model: "big".into(),
            size_bytes: 8_000_000_000_000,
            removable: true,
            sector_size: 512,
        };
        assert!(assert_safe_target(&d).is_err());
    }
    #[test]
    fn accepts_normal_usb() {
        let d = UsbDevice {
            by_id: "/x".into(),
            dev: "/dev/sdb".into(),
            model: "DataTraveler".into(),
            size_bytes: 62_000_000_000,
            removable: true,
            sector_size: 512,
        };
        assert!(assert_safe_target(&d).is_ok());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn windows_marker_matches_wim_esd_and_case() {
        assert!(lists_windows_image("Sources/Install.wim"));
        assert!(lists_windows_image("SOURCES\\INSTALL.ESD"));
        assert!(lists_windows_image(
            "boot/bootx64.efi\nsources/install.wim\n"
        ));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn windows_marker_rejects_linux_listing() {
        assert!(!lists_windows_image(
            "casper/vmlinuz\nboot/grub/grub.cfg\nEFI/BOOT/BOOTX64.EFI"
        ));
        assert!(!lists_windows_image("README.TXT\n."));
    }

    // ---- Windows User Experience / autounattend.xml ----

    #[test]
    fn empty_password_matches_the_known_windows_encoding() {
        // Windows appends the literal "Password", then UTF-16LE + Base64.
        assert_eq!(encode_unattend_password(""), "UABhAHMAcwB3AG8AcgBkAA==");
    }

    #[test]
    fn noop_tweaks_are_detected() {
        assert!(WindowsTweaks::default().is_noop());
        assert!(!WindowsTweaks::all_hardware_bypasses().is_noop());
    }

    #[test]
    fn bypasses_emit_labconfig_keys_and_a_product_key() {
        let xml = generate_autounattend(&WindowsTweaks::all_hardware_bypasses()).unwrap();
        for k in [
            "BypassTPMCheck",
            "BypassSecureBootCheck",
            "BypassRAMCheck",
            "BypassCPUCheck",
            "BypassStorageCheck",
        ] {
            assert!(xml.contains(k), "missing {k}");
        }
        // WinPE refuses to proceed without a ProductKey element.
        assert!(xml.contains("<ProductKey>"));
        assert!(xml.contains("pass=\"windowsPE\""));
    }

    #[test]
    fn local_account_skips_msa_and_is_an_admin() {
        let t = WindowsTweaks {
            local_account: Some("TestUser".into()),
            ..Default::default()
        };
        let xml = generate_autounattend(&t).unwrap();
        assert!(xml.contains("<Name>TestUser</Name>"));
        assert!(xml.contains("<Group>Administrators</Group>"));
        assert!(xml.contains("BypassNRO"));
        assert!(xml.contains("/logonpasswordchg:yes"));
    }

    #[test]
    fn reserved_account_names_are_rejected() {
        let t = WindowsTweaks {
            local_account: Some("Administrator".into()),
            ..Default::default()
        };
        assert!(generate_autounattend(&t).is_err());
    }

    #[test]
    fn account_names_are_stripped_of_forbidden_characters() {
        let t = WindowsTweaks {
            local_account: Some("Te:st|User".into()),
            ..Default::default()
        };
        assert!(generate_autounattend(&t)
            .unwrap()
            .contains("<Name>TestUser</Name>"));
    }

    #[test]
    fn xml_special_characters_are_escaped() {
        let t = WindowsTweaks {
            local_account: Some("A&B".into()),
            ..Default::default()
        };
        let xml = generate_autounattend(&t).unwrap();
        assert!(xml.contains("<Name>A&amp;B</Name>"));
        assert!(!xml.contains("<Name>A&B<"));
    }

    #[test]
    fn only_one_first_logon_section_is_emitted() {
        let t = WindowsTweaks {
            local_account: Some("TestUser".into()),
            qol_tweaks: true,
            ..Default::default()
        };
        let xml = generate_autounattend(&t).unwrap();
        // Windows rejects duplicate <FirstLogonCommands> blocks.
        assert_eq!(xml.matches("<FirstLogonCommands>").count(), 1);
    }

    // ---- install image selection (regression: .esd ISOs wiped the drive, then failed) ----

    #[cfg(target_os = "linux")]
    fn scratch_sources(tag: &str, files: &[(&str, usize)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("sirius-flash-test-{tag}"))
            .join("sources");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        fs::create_dir_all(&dir).unwrap();
        for (name, size) in files {
            fs::write(dir.join(name), vec![0u8; *size]).unwrap();
        }
        dir
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_install_wim() {
        let d = scratch_sources("wim", &[("install.wim", 8)]);
        assert_eq!(pick_install_image(&d), Some(("install.wim".into(), 8)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_install_esd_when_there_is_no_wim() {
        // The exact case that used to destroy the drive and then abort.
        let d = scratch_sources("esd", &[("install.esd", 5)]);
        assert_eq!(pick_install_image(&d), Some(("install.esd".into(), 5)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prefers_wim_when_both_are_present() {
        let d = scratch_sources("both", &[("install.wim", 1), ("install.esd", 2)]);
        assert_eq!(pick_install_image(&d).unwrap().0, "install.wim");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reports_nothing_when_no_install_image_exists() {
        // Must be detectable BEFORE partitioning, so the drive survives.
        let d = scratch_sources("none", &[("boot.wim", 3)]);
        assert_eq!(pick_install_image(&d), None);
    }

    // ---- what the flasher can actually build ----

    #[cfg(target_os = "linux")]
    fn plan(
        scheme: format::PartitionScheme,
        target: format::TargetSystem,
        fs: format::FileSystem,
    ) -> format::FormatPlan {
        format::FormatRequest {
            scheme,
            target,
            filesystem: fs,
            cluster_size: None,
            label: "SIRIUS".into(),
            quick: true,
        }
        .validate(format::Volume::new(32 * 1024 * 1024 * 1024, 512))
        .expect("the combination itself is legal")
    }

    /// The options model describes more than we can build, deliberately. Every
    /// gap has to be refused here, because each one would otherwise format
    /// cleanly and hand the user media that does not boot — with no error at
    /// any point to say why.
    #[cfg(target_os = "linux")]
    #[test]
    fn combinations_we_cannot_build_are_refused_by_name() {
        use format::*;
        // Legal, and the path that ships.
        assert_buildable(&plan(
            PartitionScheme::Gpt,
            TargetSystem::Uefi,
            FileSystem::Fat32,
        ))
        .expect("GPT + UEFI + FAT32 is what we already make");

        // BIOS needs boot code we do not write.
        let err = assert_buildable(&plan(
            PartitionScheme::Mbr,
            TargetSystem::Bios,
            FileSystem::Fat32,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("MBR bootstrap"), "got: {err}");
        assert!(
            err.to_string().contains("MBR + bios"),
            "the message must name the combination: {err}"
        );

        // MBR + UEFI is a real layout but is not boot-verified here.
        let err = assert_buildable(&plan(
            PartitionScheme::Mbr,
            TargetSystem::Uefi,
            FileSystem::Fat32,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("not yet boot-verified"),
            "got: {err}"
        );
    }

    /// The filesystem goes on a partition that starts 1 MiB in, so it is
    /// smaller than the drive — and at a cluster-size band boundary that
    /// difference changes the answer. A 32 GiB drive is past FAT32's 32 GB
    /// threshold while its partition is not, so validating against the drive
    /// would pick a cluster set the real volume does not allow.
    #[test]
    fn the_plan_describes_the_partition_not_the_drive() {
        use format::*;
        const GIB: u64 = 1024 * 1024 * 1024;
        let drive = 32 * GIB;
        let partition = windows_volume_size(drive);
        assert_eq!(partition, drive - WINDOWS_PARTITION_START);

        let by_drive = cluster_sizes(FileSystem::Fat32, Volume::new(drive, 512));
        let by_partition = cluster_sizes(FileSystem::Fat32, Volume::new(partition, 512));
        assert_ne!(
            by_drive, by_partition,
            "32 GiB is exactly where the two disagree; if this ever stops being \
             true the test has lost its point"
        );
        assert_eq!(by_drive, vec![16384, 32768, 65536]);
        assert!(by_partition.contains(&8192), "the partition allows smaller");
    }

    /// Every external tool the destructive phase uses must be checked before
    /// that phase runs. `wimlib-imagex` was invoked sixty lines past the point
    /// of no return, so a machine without it lost the drive, copied several
    /// gigabytes, and only then reported a missing package.
    #[cfg(target_os = "linux")]
    #[test]
    fn missing_tools_are_reported_together_and_before_anything_is_written() {
        // Something certain to exist, and two that cannot.
        assert!(require_tools(&["sh"]).is_ok());
        let err = require_tools(&["sh", "definitely-not-a-real-tool-xyz"]).unwrap_err();
        assert!(err.to_string().contains("definitely-not-a-real-tool-xyz"));
        assert!(
            err.to_string().contains("Nothing has been written"),
            "the user needs to know the drive is untouched: {err}"
        );
        // All of them at once, not one per attempt.
        let err = require_tools(&["no-such-tool-a", "no-such-tool-b"]).unwrap_err();
        assert!(err.to_string().contains("no-such-tool-a"));
        assert!(err.to_string().contains("no-such-tool-b"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tool_lookup_ignores_directories_and_unexecutable_files() {
        assert!(tool_exists("sh"));
        assert!(!tool_exists("this-is-not-on-path-at-all"));
        // A bare path separator must not be treated as a hit.
        assert!(!tool_exists(""));
    }

    /// The vendored UEFI:NTFS image is the one thing we ship that we cannot
    /// rebuild, and it is written to a user's drive verbatim. Pin it.
    #[test]
    fn the_vendored_uefi_ntfs_image_is_the_one_we_vetted() {
        use sha2::{Digest, Sha256};
        assert_eq!(
            UEFI_NTFS_IMAGE.len(),
            1024 * 1024,
            "Rufus builds this as exactly 2048 sectors"
        );
        let digest = Sha256::digest(UEFI_NTFS_IMAGE);
        assert_eq!(
            blockio::hex(&digest),
            UEFI_NTFS_SHA256,
            "the vendored image does not match the hash it was vetted under"
        );
        // A FAT filesystem, so firmware can read it at all.
        assert_eq!(
            &UEFI_NTFS_IMAGE[510..512],
            &[0x55, 0xaa],
            "a boot sector signature must be present"
        );
    }

    /// The layout the flasher picks when the caller does not choose, and the
    /// tools each needs. Getting the preflight list wrong is how a missing
    /// package becomes a wiped drive.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_layout_decides_which_tools_must_exist() {
        use format::WindowsLayout;
        const GIB: u64 = 1024 * 1024 * 1024;
        // A modern Windows 11 install.wim.
        assert_eq!(
            WindowsLayout::default_for(5 * GIB),
            WindowsLayout::NtfsUefiNtfs
        );
        // An older image with a small install.esd.
        assert_eq!(
            WindowsLayout::default_for(3 * GIB),
            WindowsLayout::Fat32Split
        );
        // Only the splitting layout needs the splitter, and only when there is
        // something to split.
        assert!(WindowsLayout::Fat32Split.needs_splitter(5 * GIB));
        assert!(!WindowsLayout::Fat32Split.needs_splitter(3 * GIB));
        assert!(!WindowsLayout::NtfsUefiNtfs.needs_splitter(5 * GIB));
        // Everything the preflight names must be a real tool on this machine,
        // or the check would fire spuriously on a working system.
        for t in ["sfdisk", "udevadm", "mount", "umount", "sync", "mkfs.fat"] {
            assert!(tool_exists(t), "{t} is expected on a Linux build host");
        }
    }

    #[test]
    fn noop_tweaks_still_produce_a_valid_empty_document() {
        let xml = generate_autounattend(&WindowsTweaks::default()).unwrap();
        assert!(xml.starts_with("<?xml"));
        assert!(xml.trim_end().ends_with("</unattend>"));
    }
}
