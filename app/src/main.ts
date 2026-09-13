import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

interface Device { by_id: string; dev: string; model: string; size_gib: number; removable: boolean; }

let isoPath = "";
let isoKind = "auto";
let deviceById = "";
let flashing = false;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const isoName = $("isoName");
const isoMeta = $("isoMeta");
const isoKindEl = $("isoKind");
const optMode = $("optMode");
const optFs = $("optFs");
const devSelect = $<HTMLSelectElement>("devSelect");
const flashBtn = $<HTMLButtonElement>("flashBtn");
const progPanel = $("progPanel");
const progFill = $("progFill");
const logEl = $("log");

async function refreshDevices() {
  devSelect.innerHTML = `<option value="">Scanning…</option>`;
  try {
    const devs = await invoke<Device[]>("list_devices");
    if (!devs.length) {
      devSelect.innerHTML = `<option value="">No removable USB found — plug one in and rescan</option>`;
    } else {
      devSelect.innerHTML = devs
        .map((d) => {
          const dev = d.dev.split("/").pop();
          const label = `${d.model || "USB drive"} — ${d.size_gib.toFixed(1)} GiB  (${dev})`;
          return `<option value="${d.by_id}">${label}</option>`;
        })
        .join("");
    }
    deviceById = devSelect.value;
  } catch (e) {
    devSelect.innerHTML = `<option value="">error: ${e}</option>`;
  }
  updateFlash();
}

devSelect.addEventListener("change", () => {
  deviceById = devSelect.value;
  updateFlash();
});
$("refreshBtn").addEventListener("click", refreshDevices);

$("browseBtn").addEventListener("click", async () => {
  const sel = await open({ multiple: false, filters: [{ name: "Disk image", extensions: ["iso", "img"] }] });
  if (!sel || Array.isArray(sel)) return;
  isoPath = sel;
  isoName.textContent = isoPath.split("/").pop() || isoPath;
  isoMeta.hidden = false;
  isoKindEl.textContent = "detecting…";
  isoKindEl.className = "chip";
  try {
    isoKind = await invoke<string>("detect_iso", { path: isoPath });
    const win = isoKind === "windows";
    isoKindEl.textContent = win ? "Windows installer" : "Linux / other ISO";
    isoKindEl.className = "chip " + (win ? "chip-win" : "chip-lin");
    optMode.textContent = win ? "Windows" : "Linux / direct";
    optFs.textContent = win ? "FAT32 + WIM split" : "Direct image write";
  } catch (e) {
    isoKindEl.textContent = String(e);
  }
  updateFlash();
});

function updateFlash() {
  flashBtn.disabled = flashing || !isoPath || !deviceById;
}

flashBtn.addEventListener("click", async () => {
  if (!isoPath || !deviceById) return;
  const name = devSelect.selectedOptions[0]?.textContent || "the selected drive";
  if (!confirm(`This will PERMANENTLY ERASE:\n\n${name}\n\nEverything on it will be lost. Continue?`)) return;
  flashing = true;
  flashBtn.disabled = true;
  flashBtn.textContent = "FLASHING…";
  progPanel.hidden = false;
  logEl.textContent = "";
  setProgress(null);
  try {
    await invoke("flash", { device: deviceById, iso: isoPath, kind: isoKind });
  } catch (e) {
    appendLog("ERROR: " + e);
    finish(false);
  }
});

function appendLog(s: string) {
  logEl.textContent += s + "\n";
  logEl.scrollTop = logEl.scrollHeight;
  const m = s.match(/(\d{1,3})\s?%/);
  if (m) setProgress(Math.min(100, parseInt(m[1], 10)));
}
function setProgress(p: number | null) {
  if (p === null) {
    progFill.classList.add("indet");
    progFill.style.width = "25%";
  } else {
    progFill.classList.remove("indet");
    progFill.style.width = p + "%";
  }
}
function finish(ok: boolean) {
  flashing = false;
  flashBtn.textContent = ok ? "DONE ✓" : "FLASH";
  if (ok) setProgress(100);
  else progFill.classList.remove("indet");
  updateFlash();
}

listen<string>("flash-log", (e) => appendLog(e.payload));
listen("flash-done", () => {
  appendLog("✓ Completed — safe to remove the USB.");
  finish(true);
});
listen<string>("flash-error", (e) => {
  appendLog("✗ " + e.payload);
  finish(false);
});

refreshDevices();
