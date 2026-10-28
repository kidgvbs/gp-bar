import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";

type Phase = "Disconnected" | "Connecting" | "Connected" | "Disconnecting" | "Error";

interface VpnState {
  phase: Phase;
  portal: string | null;
  gateway: string | null;
  connected_since_epoch_secs: number | null;
  pid: number | null;
  // While Connected: a fresh connect would go through a named gateway other
  // than the live one. Backend-owned (see apply_reconnect_armed in
  // src-tauri/src/vpn.rs) — this side only renders it.
  reconnect_armed: boolean;
  last_error: string | null;
}

// Mirrors src-tauri/src/config.rs `Config` field-for-field (frontend and
// backend ship in one bundle: no tolerances for missing fields).
interface PortalsConfig {
  portals: string[];
  selected: string | null;
  reuse_cookie: boolean;
  gpclient_path: string | null;
  // Gateway contract with the backend: the known gateway names (harvested
  // from connect logs), the user's favorite (used when the picker selection
  // is "Automatic"), and the explicit picker choice (null = "Automatic").
  gateways: string[];
  preferred_gateway: string | null;
  selected_gateway: string | null;
  // Reverse display map: gateway FQDN → picker name (see config.rs).
  gateway_hosts: Record<string, string>;
  // null = auto: native glass backdrop where available, CSS vibrancy below.
  liquid_glass: boolean | null;
}

// get_network_health(): stale=true when the VPN is DOWN but the active
// network service still has manual DNS servers — a crashed gpclient skipped
// the vpnc-script teardown.
interface NetworkHealth {
  stale: boolean;
  service: string | null;
  servers: string[];
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
const gatewayInput = document.getElementById("gateway-input") as HTMLInputElement;
const gatewayAddBtn = document.getElementById("gateway-add")!;
const portalDone = document.getElementById("portal-done")!;
const gatewaySelect = document.getElementById("gateway-select") as HTMLSelectElement;
const gatewayPrefer = document.getElementById("gateway-prefer") as HTMLButtonElement;
const gatewayRemove = document.getElementById("gateway-remove") as HTMLButtonElement;
const dnsBanner = document.getElementById("dns-banner")!;
const dnsBannerText = document.getElementById("dns-banner-text")!;
const dnsRepair = document.getElementById("dns-repair") as HTMLButtonElement;
const dnsResult = document.getElementById("dns-result")!;
const cmdError = document.getElementById("cmd-error")!;
const cookieCache = document.getElementById("cookie-cache") as HTMLInputElement;
const actionBtn = document.getElementById("action-btn") as HTMLButtonElement;
const logEl = document.getElementById("log")!;
const copyLog = document.getElementById("copy-log")!;
const versionsEl = document.getElementById("versions")!;
const logBlock = document.getElementById("log-block")!;
const logToggle = document.getElementById("log-toggle")!;

// -- activity log disclosure -----------------------------------------------------
// The panel window is a fixed-size popover: the full size comes from
// tauri.conf.json (captured here at startup), and with the log collapsed the
// window shrinks to the natural height of the remaining content.

const win = getCurrentWindow();
const [winScale, winInner] = await Promise.all([win.scaleFactor(), win.innerSize()]);
const fullWidth = winInner.width / winScale;
const fullHeight = winInner.height / winScale;

// Panel backdrop: on macOS 26+ the backend installs a native NSGlassEffectView
// behind the webview (see src-tauri/src/glass.rs); the body then drops its own
// CSS blur/tint (kept stronger otherwise, see panel.css) so the glass shows
// through.
if ((await invoke<string>("get_panel_effect")) === "liquid-glass") {
  document.body.classList.add("liquid-glass");
}

function collapsedHeight(): number {
  const app = document.getElementById("app")!;
  app.style.height = "auto";
  const height = app.getBoundingClientRect().height;
  app.style.height = "";
  return height;
}

// Refit the popover height whenever rows appear or disappear: with the log
// collapsed the window is content-sized and the body clips overflow, so the
// residual-DNS banner / repair outcome rows would be cut off without this.
function syncWindowSize() {
  const collapsed = logBlock.classList.contains("collapsed");
  win.setSize(new LogicalSize(fullWidth, Math.ceil(collapsed ? collapsedHeight() : fullHeight)));
}

logToggle.addEventListener("click", () => {
  const collapsed = logBlock.classList.toggle("collapsed");
  win.setSize(new LogicalSize(fullWidth, Math.ceil(collapsed ? collapsedHeight() : fullHeight)));
});

// The log starts collapsed: fit the window to the content right away.
win.setSize(new LogicalSize(fullWidth, Math.ceil(collapsedHeight())));

let currentPhase: Phase = "Disconnected";
let connectedSince: number | null = null;
let logLines: string[] = [];

// Gateway picker: mirror of the last-rendered config fields, kept so the
// star / remove controls can update without re-reading the config.
let preferredGateway: string | null = null;
// Signature of the last gateway <select> render: lets `config-updated`
// events skip no-op rebuilds (an open dropdown must not be disturbed).
let gatewaySig = "";
// Gateway whose remove button is armed, and the inline-confirmation timeout.
let removeArmedFor: string | null = null;
let removeConfirmTimer: ReturnType<typeof setTimeout> | undefined;

// Residual-DNS banner: last known network health + the timer that
// auto-hides the repair outcome line.
let networkHealth: NetworkHealth | null = null;
let dnsResultTimer: ReturnType<typeof setTimeout> | undefined;

// -- inline command errors -----------------------------------------------------
// A rejected `invoke` (invalid portal name, connect refused mid-teardown…)
// must be visible in the panel: a native alert() would steal focus and
// dismiss the popover. One shared red line, auto-hidden after 6 s.
let cmdErrorTimer: ReturnType<typeof setTimeout> | undefined;

function showCmdError(message: string) {
  cmdError.textContent = message;
  cmdError.classList.remove("hidden");
  clearTimeout(cmdErrorTimer);
  cmdErrorTimer = setTimeout(() => {
    cmdError.classList.add("hidden");
    syncWindowSize();
  }, 6000);
  syncWindowSize();
}

function clearCmdError() {
  clearTimeout(cmdErrorTimer);
  cmdError.classList.add("hidden");
}

// -- config persistence ---------------------------------------------------------
// get_config_read_error(): the startup config file could not be READ
// (permissions, I/O). Saves are then no-ops for the whole session — the
// backend must not overwrite a file it could not read — so the user has
// to know their changes are not sticking. Shown once, in the shared error
// row, the first time the panel is actually looked at: the panel window
// is created hidden at startup, so showing it at load would expire
// unseen behind the 6 s auto-hide.
const configReadError = await invoke<string | null>("get_config_read_error");
let configErrorShown = false;
if (configReadError !== null) {
  await win.onFocusChanged(({ payload: focused }) => {
    if (focused && !configErrorShown) {
      configErrorShown = true;
      showCmdError(`Config could not be read (${configReadError}) — changes are NOT saved`);
    }
  });
}

// -- action button in-flight guard ----------------------------------------------
// The action button changes meaning with the phase: a fast double click would
// send `connect` and then — once the Connecting event arrives — `cancel`.
// While an invoke is in flight the button stays disabled; render() keeps the
// guard into account so a vpn-state event cannot re-enable it mid-flight.
let opInFlight = false;
// Last rendered state: re-applied when an invoke settles, so the button is
// re-enabled only after the state render that follows the invoke.
let lastState: VpnState | null = null;

function setActionDisabled(disabled: boolean) {
  actionBtn.disabled = opInFlight || disabled;
}

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
  lastState = state;
  connectedSince = state.connected_since_epoch_secs;
  statusDot.className = "status-dot " + state.phase.toLowerCase();
  stateBlock.dataset.phase = state.phase;

  switch (state.phase) {
    case "Connected":
      stateLabel.textContent = "Connected";
      stateDetail.textContent = state.portal + (state.gateway ? ` · ${state.gateway}` : "");
      // The backend owns the armed flag (reconnect_armed): it survives
      // state refreshes (timer tick, log events) until the switch fires
      // or the pick matches the tunnel again.
      if (state.reconnect_armed) {
        actionBtn.textContent = "Reconnect";
      } else {
        actionBtn.textContent = "Disconnect";
      }
      setActionDisabled(false);
      break;
    case "Connecting":
      stateLabel.textContent = "Connecting…";
      stateDetail.textContent = state.portal ?? "";
      actionBtn.textContent = "Cancel";
      setActionDisabled(false);
      break;
    case "Disconnecting":
      stateLabel.textContent = "Disconnecting…";
      stateDetail.textContent = state.portal ?? "";
      actionBtn.textContent = "Disconnecting…";
      setActionDisabled(true);
      break;
    case "Error":
      stateLabel.textContent = "Error";
      stateDetail.textContent = state.last_error ?? "Something went wrong";
      actionBtn.textContent = "Connect";
      setActionDisabled(false);
      break;
    default:
      stateLabel.textContent = "Disconnected";
      stateDetail.textContent = state.portal ? `${state.portal} ready` : "Add a portal to get started";
      actionBtn.textContent = "Connect";
      setActionDisabled(state.portal === null);
      break;
  }
  tickDuration();
  updateActionReconnect(state.reconnect_armed);
  // Both the gateway row (frozen while a connection is active) and the
  // residual-DNS banner (only meaningful while the VPN is down) track the
  // phase on every state event.
  updateGatewayControls();
  updateDnsBanner();
}

function updateActionReconnect(armed: boolean) {
  actionBtn.classList.toggle("reconnect", armed);
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
  renderGateways(config);

  portalList.innerHTML = "";
  for (const p of config.portals) {
    const li = document.createElement("li");
    li.textContent = p;
    const rm = document.createElement("button");
    rm.className = "icon-btn";
    rm.textContent = "−";
    rm.title = "Remove";
    rm.onclick = async () => {
      clearCmdError();
      try {
        await invoke("remove_portal", { portal: p });
        await refreshPortals();
      } catch (error) {
        showCmdError(String(error));
        return;
      }
      syncWindowSize();
    };
    li.appendChild(rm);
    portalList.appendChild(li);
  }
}

// -- gateway picker ------------------------------------------------------------
// Mirrors the portal picker: options rebuilt from the config, with
// "Automatic" standing for `selected_gateway === null` (gateway: null on
// connect). The preferred gateway is marked "★" in its own option, and the
// semantics — "Automatic" resolves to the preferred gateway when one is
// set, otherwise gpclient's --auto-gateway; an explicit choice always wins —
// live in the select's tooltip.

function renderGateways(config: PortalsConfig) {
  const gateways = config.gateways;
  const preferred = config.preferred_gateway;
  const selected = config.selected_gateway;

  // Skip no-op rebuilds: `config-updated` can fire mid-connect (gateway
  // harvested from the log) and rebuilding identical options would close an
  // open dropdown and could lose the current selection.
  const sig = JSON.stringify([gateways, preferred, selected]);
  if (sig === gatewaySig) return;
  gatewaySig = sig;
  preferredGateway = preferred;

  gatewaySelect.innerHTML = "";
  const auto = document.createElement("option");
  auto.value = "";
  auto.textContent = "Automatic";
  // A selection naming a gateway that is no longer listed falls back to
  // "Automatic" (the backend zeroes it on removal anyway).
  auto.selected = selected === null || !gateways.includes(selected);
  gatewaySelect.appendChild(auto);
  for (const g of gateways) {
    const opt = document.createElement("option");
    opt.value = g;
    opt.textContent = g + (g === preferred ? " ★" : "");
    opt.selected = g === selected;
    gatewaySelect.appendChild(opt);
  }
  updateGatewayControls();
}

// Enabled/disabled, star and tooltip state of the gateway row. Prefer and
// remove stay frozen while a connection is starting, up or tearing down — a
// config change mid-connection would be silently deferred and confusing.
// The select itself stays editable while Connected: picking a different
// gateway arms a Reconnect instead.
function updateGatewayControls() {
  const active =
    currentPhase === "Connecting" ||
    currentPhase === "Connected" ||
    currentPhase === "Disconnecting";
  const name = gatewaySelect.value; // "" = "Automatic"
  const auto = name === "";

  if (active) resetRemoveConfirm();

  gatewaySelect.disabled =
    currentPhase === "Connecting" || currentPhase === "Disconnecting";
  gatewayPrefer.disabled = active || auto;
  gatewayRemove.disabled = active || auto;
  gatewayPrefer.classList.toggle("active", preferredGateway !== null);

  gatewaySelect.title = auto
    ? `Gateway "Automatic": connects with the preferred${
        preferredGateway ? ` (${preferredGateway} ★)` : ""
      } when set, otherwise gpclient picks on its own. A specific gateway always wins.${
        currentPhase === "Connected" ? " Pick another gateway to Reconnect through it." : ""
      }`
    : `Always connect through gateway ${name}.${
        currentPhase === "Connected" ? " Pick another gateway to Reconnect through it." : ""
      }`;
  if (auto) {
    gatewayPrefer.title = preferredGateway
      ? `Preferred: ${preferredGateway}. To remove it, select it and click ★ again.`
      : `Pick a gateway, then ★ to make it the preferred one (used by "Automatic").`;
  } else if (name === preferredGateway) {
    gatewayPrefer.title = `${name} is the preferred gateway — click to remove.`;
  } else {
    gatewayPrefer.title = `Make ${name} the preferred gateway (used by "Automatic").`;
  }
  gatewayRemove.title = auto
    ? "Select a gateway to remove it from the list."
    : `Remove ${name} from the list.`;
}

// The remove confirmation is inline: a native confirm() steals focus and
// the popover dismisses on Focused(false) (see main.rs). First click turns
// the button into "Confirm?" for three seconds, the second click removes.
function resetRemoveConfirm() {
  removeArmedFor = null;
  clearTimeout(removeConfirmTimer);
  gatewayRemove.textContent = "×";
  gatewayRemove.classList.remove("confirm");
}

async function refreshPortals() {
  const config = await invoke<PortalsConfig>("get_config");
  renderPortals(config);
  const state = await invoke<VpnState>("get_state");
  render(state);
}

// -- residual-DNS banner -------------------------------------------------------
// A crashed gpclient (e.g. after an overnight standby) skips the vpnc-script
// teardown and leaves its manual DNS servers on the active network service.
// The amber banner offers the backend's root-helper repair — but only while
// the VPN is really down: during Connecting/Connected the VPN DNS is
// legitimate, so the phase gates the visibility.

function formatDnsServers(servers: string[]): string {
  // First two addresses + "+n": the banner must stay one readable line.
  const shown = servers.slice(0, 2).join(", ");
  const extra = servers.length - 2;
  return extra > 0 ? `${shown} +${extra}` : shown;
}

function updateDnsBanner() {
  const health = networkHealth;
  const show =
    health !== null &&
    health.stale &&
    (currentPhase === "Disconnected" || currentPhase === "Error");
  const wasHidden = dnsBanner.classList.contains("hidden");
  dnsBanner.classList.toggle("hidden", !show);
  if (show) {
    dnsBannerText.textContent =
      `VPN DNS settings left behind by a crash (${formatDnsServers(health.servers)}) — restore them?`;
    dnsBanner.title = health.service
      ? `Network service "${health.service}" still has manual DNS servers.`
      : "";
  }
  // Visibility flipped: the popover is content-sized with the log collapsed,
  // so refit the window or the extra row gets clipped.
  if (wasHidden === show) syncWindowSize();
}

function showDnsResult(message: string, isError: boolean) {
  dnsResult.textContent = message;
  dnsResult.classList.toggle("error", isError);
  dnsResult.classList.remove("hidden");
  clearTimeout(dnsResultTimer);
  dnsResultTimer = setTimeout(() => {
    dnsResult.classList.add("hidden");
    syncWindowSize();
  }, 6000);
  syncWindowSize();
}

async function refreshNetworkHealth() {
  try {
    networkHealth = await invoke<NetworkHealth>("get_network_health");
  } catch (error) {
    // Advisory only: the banner depends on it, a failing health probe
    // (e.g. scutil error) just keeps it hidden.
    console.warn("get_network_health failed:", error);
    networkHealth = null;
  }
  updateDnsBanner();
}

actionBtn.addEventListener("click", async () => {
  // In-flight guard: a second click (double click, or one landing while the
  // phase event is still in flight) must not re-enter — it would read the new
  // phase and send `cancel` right after `connect`.
  if (opInFlight) return;
  opInFlight = true;
  setActionDisabled(true);
  clearCmdError();
  try {
    if (currentPhase === "Connected" && lastState?.reconnect_armed) {
      // Gateway switch: the backend owns the whole chain now — it tears
      // the tunnel down, waits out its own Disconnecting observation,
      // and connects through the target the armed flag was computed
      // against (nothing to forward from here).
      await invoke("reconnect");
    } else if (currentPhase === "Connected" || currentPhase === "Disconnecting") {
      await invoke("disconnect");
    } else if (currentPhase === "Connecting") {
      await invoke("cancel");
    } else {
      // Third argument per the gateway contract: null ("Automatic") lets the
      // backend resolve the preferred gateway, a name forces it.
      await invoke("connect", {
        portal: portalSelect.value,
        reuseCookie: cookieCache.checked,
        gateway: gatewaySelect.value || null,
      });
    }
  } catch (error) {
    showCmdError(String(error));
  } finally {
    // Re-enable only after the state render that follows the invoke: if the
    // backend already emitted a vpn-state event it re-rendered with the
    // guard held; re-apply the last rendered state either way.
    opInFlight = false;
    if (lastState) render(lastState);
  }
});

portalSelect.addEventListener("change", async () => {
  clearCmdError();
  try {
    await invoke("select_portal", { portal: portalSelect.value });
  } catch (error) {
    showCmdError(String(error));
  }
});

gatewaySelect.addEventListener("change", async () => {
  resetRemoveConfirm();
  // Empty value ("Automatic") = null: no explicit gateway selection.
  clearCmdError();
  try {
    await invoke("select_gateway", { gateway: gatewaySelect.value || null });
  } catch (error) {
    showCmdError(String(error));
    return;
  }
  // While Connected, a pick that differs from the live tunnel arms the
  // amber Reconnect — backend-side now: select_gateway re-notes the
  // reconnect target and the next vpn-state event carries the armed
  // flag to this button and the tray menu together. No recompute here,
  // so the two can no longer drift.
  updateGatewayControls();
});

gatewayPrefer.addEventListener("click", async () => {
  const name = gatewaySelect.value;
  if (!name) return; // "Automatic": the star is disabled there
  // Toggle: starring the already-preferred gateway clears it — that is the
  // documented way to remove a preferred gateway.
  clearCmdError();
  try {
    await invoke("set_preferred_gateway", {
      gateway: name === preferredGateway ? null : name,
    });
  } catch (error) {
    showCmdError(String(error));
    return;
  }
  await refreshPortals();
});

gatewayRemove.addEventListener("click", async () => {
  const name = gatewaySelect.value;
  if (!name) return;
  if (removeArmedFor !== name) {
    removeArmedFor = name;
    gatewayRemove.textContent = "Confirm?";
    gatewayRemove.classList.add("confirm");
    clearTimeout(removeConfirmTimer);
    removeConfirmTimer = setTimeout(resetRemoveConfirm, 3000);
    return;
  }
  resetRemoveConfirm();
  // If the removed gateway was the preferred and/or selected one the backend
  // zeroes those fields; the refreshed config re-renders the row.
  clearCmdError();
  try {
    await invoke("remove_gateway", { name });
  } catch (error) {
    showCmdError(String(error));
    return;
  }
  await refreshPortals();
});

dnsRepair.addEventListener("click", async () => {
  dnsRepair.disabled = true;
  dnsRepair.textContent = "…";
  try {
    // repair_network runs the root helper and returns an outcome message.
    const message = await invoke<string>("repair_network");
    showDnsResult(message || "DNS restored", false);
  } catch (error) {
    showDnsResult(String(error), true);
  } finally {
    dnsRepair.disabled = false;
    dnsRepair.textContent = "Restore";
    refreshNetworkHealth();
  }
});

cookieCache.addEventListener("change", async () => {
  clearCmdError();
  try {
    await invoke("set_reuse_cookie", { value: cookieCache.checked });
  } catch (error) {
    showCmdError(String(error));
  }
});

portalEdit.addEventListener("click", () => {
  portalEditor.classList.toggle("hidden");
  // The popover is content-sized while the log is collapsed: the editor row
  // changes the natural height, so refit the window or the editor pushes
  // "Activity" and the footer out of view.
  syncWindowSize();
});

portalAdd.addEventListener("click", async () => {
  const value = portalInput.value.trim();
  if (!value) return;
  clearCmdError();
  try {
    await invoke("add_portal", { portal: value });
  } catch (error) {
    // e.g. "https://vpn.example.com": add_portal rejects ':' and '/'.
    showCmdError(String(error));
    return;
  }
  portalInput.value = "";
  await refreshPortals();
  // The refreshed portal list may grow (new <li>): refit here too.
  syncWindowSize();
});

// Manual gateway add (see the placeholder note in index.html): the backend
// validates the name like every gateway path does.
gatewayAddBtn.addEventListener("click", async () => {
  const value = gatewayInput.value.trim();
  if (!value) return;
  clearCmdError();
  try {
    await invoke("add_gateway", { name: value });
    gatewayInput.value = "";
    await refreshPortals();
  } catch (error) {
    // Inline error: no native dialogs from the popover (they steal focus
    // and the panel dismisses on Focused(false)).
    gatewayInput.value = String(error);
    setTimeout(() => {
      if (gatewayInput.value === String(error)) gatewayInput.value = "";
    }, 3000);
  }
});

gatewayInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    gatewayAddBtn.click();
  }
});

portalInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") portalAdd.click();
});

portalDone.addEventListener("click", () => {
  portalEditor.classList.add("hidden");
  // Back to the compact height (see the portalEdit handler).
  syncWindowSize();
});

copyLog.addEventListener("click", async () => {
  await navigator.clipboard.writeText(logLines.join("\n"));
  copyLog.textContent = "✓";
  setTimeout(() => (copyLog.textContent = "⧉"), 1200);
});

document.getElementById("quit-btn")!.addEventListener("click", () => {
  invoke("quit");
});

await listen<VpnState>("vpn-state", (e) => {
  render(e.payload);
  // Every settling into a down phase re-checks for residual DNS: a crashed
  // worker leaves the manual DNS servers behind.
  if (e.payload.phase === "Disconnected" || e.payload.phase === "Error") {
    refreshNetworkHealth();
  }
});
await listen<LogEvent>("vpn-log", (e) => appendLog(e.payload));
await listen("vpn-log-cleared", () => {
  logLines = [];
  logEl.textContent = "";
});
// The backend harvests gateway names from the connect log while connecting
// and emits the whole new config; renderGateways re-syncs the picker without
// losing the current selection.
await listen<PortalsConfig>("config-updated", (e) => renderPortals(e.payload));

await refreshPortals();
// Residual-DNS banner: first health check at panel load.
await refreshNetworkHealth();

const versions = await invoke<{ app_version: string; gpclient_version: string }>("get_versions");
versionsEl.textContent = `GP Bar ${versions.app_version} · ${versions.gpclient_version}`;

setInterval(tickDuration, 1000);
