import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Phase = "Disconnected" | "Connecting" | "Connected" | "Disconnecting" | "Error";

interface VpnState {
  phase: Phase;
  portal: string | null;
  gateway: string | null;
  connected_since_epoch_secs: number | null;
  pid: number | null;
  last_error: string | null;
}

interface PortalsConfig {
  portals: string[];
  selected: string | null;
  reuse_cookie: boolean;
  gpclient_path: string;
}

interface LogEvent {
  level: string;
  target: string;
  message: string;
}

const stateBlock = document.getElementById("status-block")!;
const statusDot = document.getElementById("status-dot")!;
const stateLabel = document.getElementById("state-label")!;
const stateDetail = document.getElementById("state-detail")!;
const durationEl = document.getElementById("duration")!;
const portalSelect = document.getElementById("portal-select") as HTMLSelectElement;
const portalEdit = document.getElementById("portal-edit")!;
const portalEditor = document.getElementById("portal-editor")!;
const portalList = document.getElementById("portal-list")!;
const portalInput = document.getElementById("portal-input") as HTMLInputElement;
const portalAdd = document.getElementById("portal-add")!;
const portalDone = document.getElementById("portal-done")!;
const cookieCache = document.getElementById("cookie-cache") as HTMLInputElement;
const actionBtn = document.getElementById("action-btn") as HTMLButtonElement;
const logEl = document.getElementById("log")!;
const copyLog = document.getElementById("copy-log")!;
const versionsEl = document.getElementById("versions")!;

let currentPhase: Phase = "Disconnected";
let connectedSince: number | null = null;
let logLines: string[] = [];

function appendLog(event: LogEvent) {
  const line = `${event.level.padEnd(5)} ${event.message}`;
  logLines.push(line);
  if (logLines.length > 400) logLines.shift();
  logEl.textContent = logLines.join("\n");
  logEl.scrollTop = logEl.scrollHeight;
}

function formatDuration(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m`;
  if (m > 0) return `${m}m ${String(s).padStart(2, "0")}s`;
  return `${s}s`;
}

function tickDuration() {
  if (currentPhase === "Connected" && connectedSince !== null) {
    durationEl.textContent = formatDuration(Math.floor(Date.now() / 1000 - connectedSince));
  } else {
    durationEl.textContent = "";
  }
}

function render(state: VpnState) {
  currentPhase = state.phase;
  connectedSince = state.connected_since_epoch_secs;
  statusDot.className = "status-dot " + state.phase.toLowerCase();
  stateBlock.dataset.phase = state.phase;

  switch (state.phase) {
    case "Connected":
      stateLabel.textContent = "Connected";
      stateDetail.textContent = state.portal + (state.gateway ? ` · ${state.gateway}` : "");
      actionBtn.textContent = "Disconnect";
      actionBtn.disabled = false;
      break;
    case "Connecting":
      stateLabel.textContent = "Connecting…";
      stateDetail.textContent = state.portal ?? "";
      actionBtn.textContent = "Cancel";
      actionBtn.disabled = false;
      break;
    case "Disconnecting":
      stateLabel.textContent = "Disconnecting…";
      stateDetail.textContent = state.portal ?? "";
      actionBtn.textContent = "Disconnecting…";
      actionBtn.disabled = true;
      break;
    case "Error":
      stateLabel.textContent = "Error";
      stateDetail.textContent = state.last_error ?? "Something went wrong";
      actionBtn.textContent = "Connect";
      actionBtn.disabled = false;
      break;
    default:
      stateLabel.textContent = "Disconnected";
      stateDetail.textContent = state.portal ? `${state.portal} ready` : "Add a portal to get started";
      actionBtn.textContent = "Connect";
      actionBtn.disabled = state.portal === null;
      break;
  }
  tickDuration();
}

function renderPortals(config: PortalsConfig) {
  portalSelect.innerHTML = "";
  for (const p of config.portals) {
    const opt = document.createElement("option");
    opt.value = p;
    opt.textContent = p;
    opt.selected = p === config.selected;
    portalSelect.appendChild(opt);
  }
  cookieCache.checked = config.reuse_cookie;

  portalList.innerHTML = "";
  for (const p of config.portals) {
    const li = document.createElement("li");
    li.textContent = p;
    const rm = document.createElement("button");
    rm.className = "icon-btn";
    rm.textContent = "−";
    rm.title = "Remove";
    rm.onclick = async () => {
      await invoke("remove_portal", { portal: p });
      await refreshPortals();
    };
    li.appendChild(rm);
    portalList.appendChild(li);
  }
}

async function refreshPortals() {
  const config = await invoke<PortalsConfig>("get_config");
  renderPortals(config);
  const state = await invoke<VpnState>("get_state");
  render(state);
}

actionBtn.addEventListener("click", async () => {
  if (currentPhase === "Connected" || currentPhase === "Disconnecting") {
    await invoke("disconnect");
  } else if (currentPhase === "Connecting") {
    await invoke("cancel");
  } else {
    await invoke("connect", { portal: portalSelect.value, reuseCookie: cookieCache.checked });
  }
});

portalSelect.addEventListener("change", async () => {
  await invoke("select_portal", { portal: portalSelect.value });
});

cookieCache.addEventListener("change", async () => {
  await invoke("set_reuse_cookie", { value: cookieCache.checked });
});

portalEdit.addEventListener("click", () => {
  portalEditor.classList.toggle("hidden");
});

portalAdd.addEventListener("click", async () => {
  const value = portalInput.value.trim();
  if (!value) return;
  await invoke("add_portal", { portal: value });
  portalInput.value = "";
  await refreshPortals();
});

portalInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") portalAdd.click();
});

portalDone.addEventListener("click", () => {
  portalEditor.classList.add("hidden");
});

copyLog.addEventListener("click", async () => {
  await navigator.clipboard.writeText(logLines.join("\n"));
  copyLog.textContent = "✓";
  setTimeout(() => (copyLog.textContent = "⧉"), 1200);
});

document.getElementById("quit-btn")!.addEventListener("click", () => {
  invoke("quit");
});

await listen<VpnState>("vpn-state", (e) => render(e.payload));
await listen<LogEvent>("vpn-log", (e) => appendLog(e.payload));
await listen("vpn-log-cleared", () => {
  logLines = [];
  logEl.textContent = "";
});

await refreshPortals();

const versions = await invoke<{ app_version: string; gpclient_version: string }>("get_versions");
versionsEl.textContent = `GP Bar ${versions.app_version} · gpclient ${versions.gpclient_version}`;

setInterval(tickDuration, 1000);
