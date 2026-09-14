//! Sirius Flash — core logic: safe device discovery, ISO detection, and flashing.
//!
//! Safety principle: writes are only ever addressed via the stable
//! `/dev/disk/by-id` path, gated on removable + size checks. Kernel names
//! (`sdb`, `nvme0n1`) are treated as unstable and never trusted for targeting.

pub mod blockio;
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
        let size_bytes = read_u64(&sys.join("size")).unwrap_or(0) * 512;
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
#[cfg(target_os = "linux")]
pub fn flash_windows_iso(
    d: &UsbDevice,
    iso: &Path,
    tweaks: Option<&WindowsTweaks>,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    assert_safe_target(d)?;
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

    // ---- everything from here on is destructive ----
    let _ = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "for p in {dev}-part*; do umount \"$p\" 2>/dev/null || true; done"
        ))
        .status();

    let result = (|| -> Result<()> {
        run(
            "parted",
            &[
                "--script", &dev, "mklabel", "gpt", "mkpart", "WIN11", "fat32", "1MiB", "100%",
                "set", "1", "msftdata", "on",
            ],
        )?;
        run("udevadm", &["settle"])?;
        run("mkfs.fat", &["-F", "32", "-n", "WIN11USB", &part1])?;
        run("mount", &[&part1, usb_mnt])?;

        // FAT32 cannot store a file of 4 GiB or more, so an oversized install
        // image is split into .swm chunks; a smaller one is copied as-is.
        const FAT32_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;
        let split = img_size > FAT32_MAX_FILE;
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
pub fn flash_linux_iso(
    d: &UsbDevice,
    iso: &Path,
    verify: bool,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    assert_safe_target(d)?;
    let size = fs::metadata(iso)
        .with_context(|| format!("reading {}", iso.display()))?
        .len();
    let compression = blockio::detect_compression(iso)?;
    // A compressed image's real size is unknown until it is unpacked, so only
    // the raw case can be checked up front; the compressed case is caught by
    // the ENOSPC handling in write_image. Either way this must happen before
    // anything is wiped rather than partway through the write.
    if compression == blockio::Compression::None && size > d.size_bytes {
        bail!(
            "image is {:.1} GiB but {} holds only {:.1} GiB",
            size as f64 / 1024.0_f64.powi(3),
            d.dev.display(),
            d.size_gib()
        );
    }
    if compression != blockio::Compression::None {
        println!("decompressing {} image on the fly", compression.as_str());
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

    #[test]
    fn noop_tweaks_still_produce_a_valid_empty_document() {
        let xml = generate_autounattend(&WindowsTweaks::default()).unwrap();
        assert!(xml.starts_with("<?xml"));
        assert!(xml.trim_end().ends_with("</unattend>"));
    }
}
