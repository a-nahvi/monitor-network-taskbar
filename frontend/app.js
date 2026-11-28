"use strict";

const byId = (id) => document.getElementById(id);
const invoke = window.__TAURI__?.core?.invoke;
const colorScheme = window.matchMedia("(prefers-color-scheme: dark)");
const nav = document.querySelector(".navigation");
const tabs = [...nav.querySelectorAll("[role=tab]")];
const form = byId("settings-form");
const workspace = byId("workspace");
const controls = {
  language: byId("language"),
  display: { graph: form.elements.layout, visible: byId("widget-visible"), width: byId("graph-width"),
    font: byId("text-size"), theme: byId("text-color"), locked: byId("position-locked") },
  network: { adapter: byId("adapter"), bits: byId("speed-units"), interval: byId("interval") },
  colors: { upColor: byId("upload-color"), downColor: byId("download-color") },
  startup: byId("start-windows"),
};
const defaultSettings = { schema: 1, interval: 1000, font: 12, offset: 0, locked: true,
  visible: true, theme: 0, bits: false, graph: false, width: 88, customUp: false, customDown: false,
  upColor: 0xffab45, downColor: 0x42c2ff, adapter: "", language: "english" };
const translatedText = document.querySelectorAll("[data-i18n]");
const translatedLabels = document.querySelectorAll("[data-i18n-label]");
const statuses = new Map();

let state;
let settings;
let savedSettings;
let savedStartup;
let activeLanguage;
let activeTab = "display";
let availableVersion = null;
let settingsBusy = false;
let updateBusy = false;

function translate(key, value) {
  const message = locales[activeLanguage ?? "english"][key];
  return typeof message === "function" ? message(value) : message;
}

function applyLanguage(language) {
  if (activeLanguage === language) return;

  activeLanguage = language;
  const locale = locales[language];
  document.documentElement.lang = locale.lang;
  document.documentElement.dir = locale.dir;
  document.title = locale.windowTitle;

  for (const element of translatedText) element.textContent = translate(element.dataset.i18n);
  for (const element of translatedLabels) {
    element.setAttribute("aria-label", translate(element.dataset.i18nLabel));
  }
  for (const id of statuses.keys()) renderStatus(id);

  if (state) {
    byId("sidebar-version").textContent = translate("version", state.version);
    setAdapterChoices();
  }
}

function showTab(name, focus = false) {
  activeTab = name;
  for (const tab of tabs) {
    const selected = tab.dataset.tab === name;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
    byId(tab.getAttribute("aria-controls")).hidden = !selected;
    if (selected && focus) tab.focus();
  }
  byId("scroll-area").scrollTop = 0;
  byId("restore-defaults").style.visibility = name === "updates" ? "hidden" : "";
  if (name === "display" && settings) renderPreview();
}

for (const tab of tabs) tab.addEventListener("click", () => showTab(tab.dataset.tab));
nav.addEventListener("keydown", (event) => {
  const index = tabs.indexOf(event.target);
  if (index < 0) return;
  const horizontal = nav.getAttribute("aria-orientation") === "horizontal";
  const rtl = document.documentElement.dir === "rtl";
  let next;
  switch (event.key) {
    case "ArrowDown":
      if (horizontal) return;
      next = index + 1;
      break;
    case "ArrowUp":
      if (horizontal) return;
      next = index - 1;
      break;
    case "ArrowRight":
      if (!horizontal) return;
      next = index + (rtl ? -1 : 1);
      break;
    case "ArrowLeft":
      if (!horizontal) return;
      next = index + (rtl ? 1 : -1);
      break;
    case "Home": next = 0; break;
    case "End": next = tabs.length - 1; break;
    default: return;
  }
  event.preventDefault();
  showTab(tabs[(next + tabs.length) % tabs.length].dataset.tab, true);
});

function renderStatus(id) {
  const { key, tone, value, detail } = statuses.get(id);
  const target = byId(id);
  target.textContent = activeLanguage === "english" && detail ? detail : (key ? translate(key, value) : "");
  target.dataset.tone = tone;
}

function setStatus(id, key, tone = "", value, detail) {
  statuses.set(id, { key, tone, value, detail });
  renderStatus(id);
}

function setError(id, key, error) {
  console.error(error);
  const detail = error instanceof Error ? error.message : (typeof error === "string" ? error : "");
  setStatus(id, key, "error", undefined, detail);
}

function toHex(value) {
  return `#${value.toString(16).padStart(6, "0")}`;
}

function setAdapterChoices() {
  const select = controls.network.adapter;
  select.replaceChildren(new Option(translate("automatic"), ""));
  const previous = settings.adapter;
  let hadSelection = !previous;
  for (const adapter of state.adapters) {
    const oldSelection = adapter.id.toLowerCase() === previous.toLowerCase();
    if (oldSelection) hadSelection = true;
    const label = adapter.connected ? adapter.name : translate("disconnectedAdapter", adapter.name);
    select.add(new Option(label, oldSelection ? previous : adapter.id));
  }
  if (!hadSelection) select.add(new Option(translate("unavailableAdapter"), previous));
  select.value = previous;
}

function writeControls() {
  const { display, network, colors } = controls;
  controls.language.value = settings.language;
  display.graph.value = settings.graph ? "graph" : "numbers";
  display.visible.checked = settings.visible;
  display.width.value = settings.width;
  display.font.value = settings.font;
  display.theme.value = settings.theme;
  display.locked.checked = settings.locked;
  network.bits.value = settings.bits ? "bits" : "bytes";
  network.interval.value = settings.interval;
  network.adapter.value = settings.adapter;
  colors.upColor.value = toHex(settings.upColor);
  colors.downColor.value = toHex(settings.downColor);
  updateControls();
}

function hasChanges() {
  return Object.keys(savedSettings).some((key) => savedSettings[key] !== settings[key]) ||
    (state.startupAllowed && savedStartup !== controls.startup.checked);
}

function updateControls() {
  applyLanguage(settings.language);
  const dirty = hasChanges();
  byId("graph-width-row").hidden = !settings.graph;
  byId("text-size-row").hidden = settings.graph;
  byId("width-value").textContent = translate("pixels", settings.width);
  byId("reset-position").disabled = settings.offset === 0;
  byId("reset-colors").disabled = !settings.customUp && !settings.customDown;
  byId("save-settings").disabled = settingsBusy || !dirty;
  byId("cancel-settings").disabled = settingsBusy;
  byId("cancel-settings").textContent = translate(dirty ? "cancel" : "close");
  updateReleaseControls();
  if (activeTab === "display") renderPreview();
}

form.addEventListener("input", (event) => {
  if (!state) return;
  const input = event.target;
  const { display, network, colors } = controls;
  Object.assign(settings, {
    language: controls.language.value,
    graph: display.graph.value === "graph",
    visible: display.visible.checked,
    width: Number(display.width.value),
    font: Number(display.font.value),
    theme: Number(display.theme.value),
    locked: display.locked.checked,
    bits: network.bits.value === "bits",
    interval: Number(network.interval.value),
    adapter: network.adapter.value,
  });
  if (input === colors.upColor) {
    settings.upColor = Number.parseInt(input.value.slice(1), 16);
    settings.customUp = true;
  } else if (input === colors.downColor) {
    settings.downColor = Number.parseInt(input.value.slice(1), 16);
    settings.customDown = true;
  }
  setStatus("settings-status", "");
  updateControls();
});

byId("reset-colors").addEventListener("click", () => {
  for (const field of ["customUp", "customDown", "upColor", "downColor"]) settings[field] = state.defaults[field];
  setStatus("settings-status", "");
  writeControls();
});
byId("reset-position").addEventListener("click", () => {
  settings.offset = 0;
  setStatus("settings-status", "");
  updateControls();
});
byId("restore-defaults").addEventListener("click", () => {
  settings = { ...state.defaults, schema: settings.schema, language: settings.language };
  setStatus("settings-status", "defaultsRestored");
  writeControls();
});

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!settings || settingsBusy || !hasChanges()) return;
  settingsBusy = true;
  const draft = { ...settings };
  const startupEnabled = controls.startup.checked;
  setStatus("settings-status", "savingChanges");
  updateControls();
  try {
    if (invoke) await invoke("save_settings", { settings: draft, startupEnabled });
    savedSettings = draft;
    savedStartup = startupEnabled;
    setStatus("settings-status", invoke ? "changesSaved" : "previewChangesSaved", "success");
    if (availableVersion && !updateBusy && !hasChanges()) {
      setStatus("update-status", "updateAvailable", "success", availableVersion);
    }
  } catch (error) {
    setError("settings-status", "saveFailed", error);
  } finally {
    settingsBusy = false;
    updateControls();
  }
});

async function closeSettings() {
  if (!settings || settingsBusy) return;
  if (!invoke) {
    settings = { ...savedSettings };
    controls.startup.checked = savedStartup;
    setStatus("settings-status", "");
    writeControls();
    return;
  }
  try {
    await invoke("close_settings");
  } catch (error) {
    setError("settings-status", "closeFailed", error);
  }
}

byId("cancel-settings").addEventListener("click", closeSettings);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && event.target.type !== "color" && event.target.tagName !== "SELECT") closeSettings();
});

function updateReleaseControls() {
  const allowed = Boolean(state.updatesAllowed);
  byId("check-updates").disabled = !allowed || updateBusy;
  byId("install-update").hidden = !availableVersion;
  byId("install-update").disabled = !allowed || updateBusy || settingsBusy || hasChanges();
  byId("install-update").title = hasChanges() ? translate("saveBeforeInstalling") : "";
}

async function runUpdateAction(message, failed, callback) {
  if (!state?.updatesAllowed || updateBusy) return;
  updateBusy = true;
  setStatus("update-status", message);
  updateReleaseControls();
  try {
    await callback();
  } catch (error) {
    setError("update-status", failed, error);
  } finally {
    updateBusy = false;
    updateReleaseControls();
  }
}

byId("check-updates").addEventListener("click", () => {
  runUpdateAction("checkingUpdates", "checkFailed", async () => {
    availableVersion = null;
    const release = await invoke("check_updates");
    availableVersion = release.version;
    if (availableVersion) {
      setStatus("update-status", hasChanges() ? "updateAvailableSave" : "updateAvailable", "success", availableVersion);
    } else {
      setStatus("update-status", "upToDate", "success");
    }
  });
});
byId("install-update").addEventListener("click", () => {
  if (!availableVersion || hasChanges()) return;
  runUpdateAction("downloadingUpdate", "installFailed", async () => {
    await invoke("install_update");
    availableVersion = null;
    setStatus("update-status", "installerStarted", "success");
  });
});

function renderRate(valueId, bytes, showBits) {
  const base = showBits ? 1000 : 1024;
  const units = showBits ? ["b/s", "Kb/s", "Mb/s", "Gb/s"] : ["B/s", "KB/s", "MB/s", "GB/s"];
  let value = showBits ? bytes * 8 : bytes;
  let unit = 0;
  while (value >= base && unit < units.length - 1) {
    value /= base;
    unit += 1;
  }
  if (Math.round(value) >= base && unit < units.length - 1) {
    value /= base;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  byId(valueId).textContent = value.toFixed(digits);
  byId(`${valueId}-unit`).textContent = units[unit];
}

const graphSamples = [
  [0.17, 0.16, 0.25, 0.14, 0.3, 0.25, 0.18, 0.2, 0.45, 0.22, 0.28, 0.17, 0.24, 0.18, 0.24, 0.33, 0.17],
  [0.37, 0.54, 0.4, 0.58, 0.72, 0.46, 0.5, 0.77, 0.58, 0.64, 0.88, 0.7, 0.86, 0.53, 0.66, 0.62, 0.78],
];

function renderGraph(canvas, upload, download, dark) {
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (width === 0 || height === 0) return;
  const scale = Math.min(window.devicePixelRatio || 1, 2);
  canvas.width = Math.round(width * scale);
  canvas.height = Math.round(height * scale);
  const context = canvas.getContext("2d");
  if (!context) return;
  context.scale(scale, scale);
  const top = 3;
  const bottom = height - 4;
  const left = 2;
  const right = width - 2;
  context.strokeStyle = dark ? "#8399b541" : "#55708b42";
  context.lineWidth = 1;
  context.beginPath();
  for (let line = 0; line < 3; line += 1) {
    const y = top + (bottom - top) * line / 2;
    context.moveTo(left, y);
    context.lineTo(right, y);
  }
  context.stroke();
  context.lineWidth = 1.65;
  context.lineJoin = "round";
  for (const [index, color] of [upload, download].entries()) {
    const samples = graphSamples[index];
    context.beginPath();
    context.strokeStyle = color;
    for (const [sample, value] of samples.entries()) {
      const x = left + (right - left) * sample / (samples.length - 1);
      const y = bottom - (bottom - top) * value;
      if (sample === 0) context.moveTo(x, y);
      else context.lineTo(x, y);
    }
    context.stroke();
  }
}

function renderPreview() {
  const { graph, visible, theme, font, width, bits, customUp, customDown, upColor, downColor } = settings;
  const dark = theme === 1 || (theme === 0 && colorScheme.matches);
  const track = byId("preview-track");
  const widget = byId("preview-widget");
  const canvas = byId("preview-graph");
  track.classList.toggle("is-dark", dark);
  widget.classList.toggle("is-off", !visible);
  widget.style.setProperty("--widget-width", `${visible ? (graph ? width + 14 : Math.max(101, font * 7 + 22)) : 0}px`);
  widget.style.setProperty("--widget-font", `${font}px`);
  widget.style.setProperty("--preview-upload", customUp ? toHex(upColor) : "inherit");
  widget.style.setProperty("--preview-download", customDown ? toHex(downColor) : "inherit");
  byId("preview-rates").hidden = !visible || graph;
  canvas.hidden = !visible || !graph;
  if (!visible) return;
  if (graph) {
    renderGraph(canvas, customUp ? toHex(upColor) : (dark ? "#ffab45" : "#ae460c"),
      customDown ? toHex(downColor) : (dark ? "#42c2ff" : "#0067a7"), dark);
  } else {
    renderRate("preview-up", 624 * 1024, bits);
    renderRate("preview-down", 8.4 * 1024 ** 2, bits);
  }
}

function refreshPreview() {
  if (activeTab === "display" && settings) renderPreview();
}

colorScheme.addEventListener("change", refreshPreview);
window.addEventListener("resize", refreshPreview, { passive: true });

const smallWindow = window.matchMedia("(max-width: 615px)");
function updateTabOrientation() {
  nav.setAttribute("aria-orientation", smallWindow.matches ? "horizontal" : "vertical");
}
smallWindow.addEventListener("change", updateTabOrientation);
updateTabOrientation();

const logo = byId("app-icon");
function showLogo() {
  if (!logo.naturalWidth) return;
  logo.hidden = false;
  logo.previousElementSibling.setAttribute("hidden", "");
}
logo.addEventListener("load", showLogo);
if (logo.complete) showLogo();

async function initialize() {
  workspace.inert = true;
  try {
    state = invoke ? await invoke("settings_state") : {
      settings: defaultSettings, defaults: defaultSettings, adapters: [], startupEnabled: false,
      startupAllowed: false, updatesAllowed: false, version: "2.3.0", preview: true,
    };
    settings = { ...state.settings, language: state.settings.language === "persian" ? "persian" : "english" };
    savedSettings = { ...settings };
    savedStartup = state.startupEnabled;
    controls.startup.checked = savedStartup;
    controls.startup.disabled = !state.startupAllowed;
    byId("installed-version").textContent = `NetBar ${state.version}`;
    writeControls();
    setStatus("update-status", state.updatesAllowed ? "" : "updatesUnavailable");
    workspace.inert = false;
  } catch (error) {
    applyLanguage(settings?.language ?? "english");
    workspace.inert = false;
    for (const control of form.elements) control.disabled = true;
    setError("settings-status", "loadFailed", error);
    byId("cancel-settings").disabled = false;
    byId("cancel-settings").textContent = translate("close");
    byId("cancel-settings").addEventListener("click", () => invoke?.("close_settings"), { once: true });
  } finally {
    workspace.setAttribute("aria-busy", "false");
  }
}

initialize();
