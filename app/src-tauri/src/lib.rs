//! Sirius Flash GUI backend — Tauri commands over sirius-flash-core.

use serde::{Deserialize, Serialize};
use sirius_flash_core as core;
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use tauri::{AppHandle, Emitter};

#[derive(Serialize, Clone)]
struct DeviceDto {
    by_id: String,
    dev: String,
    model: String,
    size_gib: f64,
    removable: bool,
}

#[tauri::command]
fn list_devices() -> Result<Vec<DeviceDto>, String> {
    core::list_removable_devices()
        .map_err(|e| e.to_string())
        .map(|ds| {
            ds.into_iter()
                .map(|d| {
                    let size_gib = d.size_gib();
                    DeviceDto {
                        by_id: d.by_id.to_string_lossy().into_owned(),
                        dev: d.dev.to_string_lossy().into_owned(),
                        model: d.model,
                        size_gib,
                        removable: d.removable,
                    }
                })
                .collect()
        })
}

#[derive(Serialize)]
struct ImageInfo {
    kind: String,
    /// "raw", "gzip", "xz", "zstd" or "bzip2".
    compression: String,
}

#[tauri::command]
fn detect_iso(path: String) -> Result<ImageInfo, String> {
    let p = Path::new(&path);
    let compression = core::blockio::detect_compression(p).map_err(|e| e.to_string())?;
    // A compressed image is a raw disk image, not a mountable installer, so
    // there is nothing to inspect inside it.
    let kind = if compression == core::blockio::Compression::None {
        match core::detect_iso_kind(p).map_err(|e| e.to_string())? {
            core::IsoKind::Windows => "windows",
            core::IsoKind::Other => "other",
        }
    } else {
        "other"
    };
    Ok(ImageInfo {
        kind: kind.to_string(),
        compression: compression.as_str().to_string(),
    })
}

/// Windows User Experience options chosen in the GUI.
#[derive(Deserialize, Default, Clone)]
#[serde(default, rename_all = "camelCase")]
struct TweaksDto {
    bypass_tpm: bool,
    bypass_secure_boot: bool,
    bypass_ram: bool,
    bypass_cpu: bool,
    bypass_storage: bool,
    skip_ms_account: bool,
    local_account: Option<String>,
    local_password: Option<String>,
    disable_data_collection: bool,
    disable_bitlocker: bool,
    qol: bool,
    region: Option<String>,
    timezone: Option<String>,
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

impl TweaksDto {
    fn to_core(&self) -> core::WindowsTweaks {
        core::WindowsTweaks {
            bypass_tpm: self.bypass_tpm,
            bypass_secure_boot: self.bypass_secure_boot,
            bypass_ram: self.bypass_ram,
            bypass_cpu: self.bypass_cpu,
            bypass_storage: self.bypass_storage,
            skip_ms_account: self.skip_ms_account,
            local_account: non_empty(&self.local_account).map(str::to_string),
            local_password: non_empty(&self.local_password).map(str::to_string),
            disable_data_collection: self.disable_data_collection,
            disable_bitlocker: self.disable_bitlocker,
            qol_tweaks: self.qol,
            region: non_empty(&self.region).map(str::to_string),
            timezone: non_empty(&self.timezone).map(str::to_string),
        }
    }

    /// The CLI flags equivalent to these options.
    fn to_args(&self) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        for (on, flag) in [
            (self.bypass_tpm, "--bypass-tpm"),
            (self.bypass_secure_boot, "--bypass-secure-boot"),
            (self.bypass_ram, "--bypass-ram"),
            (self.bypass_cpu, "--bypass-cpu"),
            (self.bypass_storage, "--bypass-storage"),
            (self.skip_ms_account, "--skip-ms-account"),
            (self.disable_data_collection, "--disable-data-collection"),
            (self.disable_bitlocker, "--disable-bitlocker"),
            (self.qol, "--qol"),
        ] {
            if on {
                a.push(flag.to_string());
            }
        }
        for (val, flag) in [
            (non_empty(&self.local_account), "--local-account"),
            (non_empty(&self.local_password), "--local-password"),
            (non_empty(&self.region), "--region"),
            (non_empty(&self.timezone), "--timezone"),
        ] {
            if let Some(v) = val {
                a.push(flag.to_string());
                a.push(v.to_string());
            }
        }
        a
    }
}

/// Preview the answer file the current options would write (used by the GUI).
#[tauri::command]
fn preview_unattend(tweaks: TweaksDto) -> Result<String, String> {
    core::generate_autounattend(&tweaks.to_core()).map_err(|e| e.to_string())
}

/// Locate the sirius-flash CLI (does the privileged work under pkexec).
fn resolve_cli() -> Option<String> {
    if let Ok(p) = std::env::var("SIRIUS_FLASH_CLI") {
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    if let Ok(o) = Command::new("sh").arg("-c").arg("command -v sirius-flash").output() {
        if o.status.success() {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(p);
            }
        }
    }
    for c in [
        "target/release/sirius-flash",
        "target/debug/sirius-flash",
        "../target/release/sirius-flash",
        "../target/debug/sirius-flash",
    ] {
        if Path::new(c).exists() {
            if let Ok(abs) = std::fs::canonicalize(c) {
                return Some(abs.to_string_lossy().into_owned());
            }
        }
    }
    None
}

enum StdioStream {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

/// Forward a child stream to the UI, splitting on BOTH `\n` and `\r`.
///
/// Tools like `dd status=progress` terminate each progress record with a
/// carriage return, which `BufReader::lines()` never yields — so the log stayed
/// empty until the process exited and the UI looked frozen.
fn pump<R: Read>(r: R, app: &AppHandle) {
    let mut reader = BufReader::new(r);
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' || b == b'\r' {
                if !line.is_empty() {
                    let _ = app.emit("flash-log", String::from_utf8_lossy(&line).to_string());
                    line.clear();
                }
            } else {
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        let _ = app.emit("flash-log", String::from_utf8_lossy(&line).to_string());
    }
}

/// Flash an ISO to a device via `pkexec sirius-flash write` (graphical auth),
/// streaming its output to the frontend as `flash-log` / `flash-done` / `flash-error`.
#[tauri::command]
fn flash(
    app: AppHandle,
    device: String,
    iso: String,
    kind: String,
    tweaks: Option<TweaksDto>,
) -> Result<(), String> {
    let cli = resolve_cli()
        .ok_or("sirius-flash CLI not found (build it: `cargo build -p sirius-flash-cli`, or set SIRIUS_FLASH_CLI)")?;
    let kind_arg = match kind.as_str() {
        "windows" => "windows",
        "linux" => "linux",
        _ => "auto",
    };
    // Tweaks only apply to Windows installs; validate now so a bad account name
    // fails before the drive is wiped rather than after.
    let tweak_args = match (&tweaks, kind_arg) {
        (Some(t), "windows") => {
            let core_tweaks = t.to_core();
            if !core_tweaks.is_noop() {
                core::generate_autounattend(&core_tweaks).map_err(|e| e.to_string())?;
            }
            t.to_args()
        }
        _ => Vec::new(),
    };
    std::thread::spawn(move || {
        let mut args: Vec<String> = ["write", "--iso", &iso, "--device", &device, "--kind", kind_arg, "--yes"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.extend(tweak_args);
        let spawn = Command::new("pkexec")
            .arg(&cli)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawn {
            Ok(c) => c,
            Err(e) => {
                let _ = app.emit("flash-error", format!("could not start: {e}"));
                return;
            }
        };
        // Pump stdout and stderr concurrently. Draining one to EOF before
        // reading the other deadlocks the child once the unread pipe fills
        // (64 KiB — roughly 18 minutes of `dd status=progress` records).
        let mut pumps = Vec::new();
        for (stream, handle) in [
            (child.stdout.take().map(StdioStream::Out), app.clone()),
            (child.stderr.take().map(StdioStream::Err), app.clone()),
        ] {
            if let Some(s) = stream {
                pumps.push(std::thread::spawn(move || match s {
                    StdioStream::Out(r) => pump(r, &handle),
                    StdioStream::Err(r) => pump(r, &handle),
                }));
            }
        }
        for p in pumps {
            let _ = p.join();
        }
        match child.wait() {
            Ok(s) if s.success() => {
                let _ = app.emit("flash-done", true);
            }
            Ok(s) => {
                let _ = app.emit("flash-error", format!("flashing failed ({s})"));
            }
            Err(e) => {
                let _ = app.emit("flash-error", e.to_string());
            }
        }
    });
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            list_devices,
            detect_iso,
            flash,
            preview_unattend
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sirius Flash");
}
