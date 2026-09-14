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
const progStat = $("progStat");
const wueCard = $("wueCard");
const twUser = $<HTMLInputElement>("twUser");
const twUserName = $<HTMLInputElement>("twUserName");
const xmlPreview = $("xmlPreview");

/** The Windows User Experience options, shaped for the Rust `TweaksDto`. */
function currentTweaks() {
  const on = (id: string) => $<HTMLInputElement>(id).checked;
  const hw = on("twBypassHw");
  const extra = on("twBypassExtra");
  const account = twUser.checked ? twUserName.value.trim() : "";
  return {
    bypassTpm: hw,
    bypassSecureBoot: hw,
    bypassRam: hw,
    bypassCpu: extra,
    bypassStorage: extra,
    skipMsAccount: on("twSkipMsa"),
    localAccount: account || null,
    localPassword: null,
    disableDataCollection: on("twData"),
    disableBitlocker: on("twBitlocker"),
    qol: on("twQol"),
    region: null,
    timezone: null,
  };
}

/** Short human summary of the enabled tweaks, for the confirm dialog. */
function tweakSummary(): string[] {
  const t = currentTweaks();
  const out: string[] = [];
  if (t.bypassTpm) out.push("Bypass TPM 2.0 / Secure Boot / RAM checks");
  if (t.bypassCpu) out.push("Bypass CPU / disk checks");
  if (t.skipMsAccount) out.push("No Microsoft account required");
  if (t.disableDataCollection) out.push("Data collection disabled");
  if (t.disableBitlocker) out.push("BitLocker auto-encryption prevented");
  if (t.qol) out.push("OneDrive / Copilot / Teams removed");
  if (t.localAccount) out.push(`Local account: ${t.localAccount}`);
  return out;
}

twUser.addEventListener("change", () => {
  twUserName.disabled = !twUser.checked;
  if (twUser.checked) twUserName.focus();
});

$("previewBtn").addEventListener("click", async () => {
  if (!xmlPreview.hidden) {
    xmlPreview.hidden = true;
    return;
  }
  try {
    xmlPreview.textContent = await invoke<string>("preview_unattend", { tweaks: currentTweaks() });
  } catch (e) {
    xmlPreview.textContent = "Error: " + e;
  }
  xmlPreview.hidden = false;
});

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
  const sel = await open({
    multiple: false,
    filters: [
      { name: "Disk image", extensions: ["iso", "img", "raw", "gz", "xz", "zst", "bz2", "wic"] },
    ],
  });
  if (!sel || Array.isArray(sel)) return;
  isoPath = sel;
  isoName.textContent = isoPath.split("/").pop() || isoPath;
  isoMeta.hidden = false;
  isoKindEl.textContent = "detecting…";
  isoKindEl.className = "chip";
  try {
    const info = await invoke<{ kind: string; compression: string }>("detect_iso", {
      path: isoPath,
    });
    isoKind = info.kind;
    const win = isoKind === "windows";
    const packed = info.compression !== "raw";
    isoKindEl.textContent = win
      ? "Windows installer"
      : packed
        ? `Compressed image (${info.compression})`
        : "Linux / other ISO";
    isoKindEl.className = "chip " + (win ? "chip-win" : "chip-lin");
    optMode.textContent = win ? "Windows" : "Linux / direct";
    optFs.textContent = win
      ? "FAT32 + WIM split"
      : packed
        ? "Decompress + write"
        : "Direct image write";
    // The Windows tweaks only apply to a Windows installer.
    wueCard.hidden = !win;
    xmlPreview.hidden = true;
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
  const win = isoKind === "windows";
  const extras = win ? tweakSummary() : [];
  const extraText = extras.length ? `\n\nWindows tweaks:\n• ${extras.join("\n• ")}` : "";
  if (!confirm(`This will PERMANENTLY ERASE:\n\n${name}\n\nEverything on it will be lost.${extraText}\n\nContinue?`)) return;
  flashing = true;
  flashBtn.disabled = true;
  flashBtn.textContent = "FLASHING…";
  progPanel.hidden = false;
  logEl.textContent = "";
  setProgress(null);
  try {
    await invoke("flash", {
      device: deviceById,
      iso: isoPath,
      kind: isoKind,
      tweaks: win ? currentTweaks() : null,
    });
  } catch (e) {
    appendLog("ERROR: " + e);
    finish(false);
  }
});

// e.g. "write 27% · 1.2 GiB / 4.4 GiB · 45.0 MiB/s · ETA 1m10s"
const PROGRESS_RE = /^(write|verify|copy)\s+(\d{1,3})%\s+·\s+(.+?)\s*$/;
const STAGE_LABEL: Record<string, string> = {
  write: "Writing",
  verify: "Verifying",
  copy: "Copying",
};

function appendLog(s: string) {
  const m = s.match(PROGRESS_RE);
  if (m) {
    // A live status line: replace it in place rather than flooding the log
    // with one entry every 250 ms.
    progStat.textContent = `${STAGE_LABEL[m[1]] ?? m[1]} — ${m[3]}`;
    setProgress(Math.min(100, parseInt(m[2], 10)));
    return;
  }
  logEl.textContent += s + "\n";
  logEl.scrollTop = logEl.scrollHeight;
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
