//! Sirius Flash GUI backend — Tauri commands over sirius-flash-core.

use serde::Serialize;
use sirius_flash_core as core;
use std::io::{BufRead, BufReader};
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

#[tauri::command]
fn detect_iso(path: String) -> Result<String, String> {
    let k = core::detect_iso_kind(Path::new(&path)).map_err(|e| e.to_string())?;
    Ok(match k {
        core::IsoKind::Windows => "windows".into(),
        core::IsoKind::Other => "other".into(),
    })
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

/// Flash an ISO to a device via `pkexec sirius-flash write` (graphical auth),
/// streaming its output to the frontend as `flash-log` / `flash-done` / `flash-error`.
#[tauri::command]
fn flash(app: AppHandle, device: String, iso: String, kind: String) -> Result<(), String> {
    let cli = resolve_cli()
        .ok_or("sirius-flash CLI not found (build it: `cargo build -p sirius-flash-cli`, or set SIRIUS_FLASH_CLI)")?;
    let kind_arg = match kind.as_str() {
        "windows" => "windows",
        "linux" => "linux",
        _ => "auto",
    };
    std::thread::spawn(move || {
        let spawn = Command::new("pkexec")
            .arg(&cli)
            .args(["write", "--iso", &iso, "--device", &device, "--kind", kind_arg, "--yes"])
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
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = app.emit("flash-log", line);
            }
        }
        if let Some(err) = child.stderr.take() {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = app.emit("flash-log", line);
            }
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
        .invoke_handler(tauri::generate_handler![list_devices, detect_iso, flash])
        .run(tauri::generate_context!())
        .expect("error while running Sirius Flash");
}
