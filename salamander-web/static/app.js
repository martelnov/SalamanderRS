"use strict";

// SalamanderRS Web — dual-pane browser, F2/F5-F8, ZIP and streaming transfers.
// No user-provided HTML is interpolated.
// A separate session-local path is kept for each root in each panel.
const panes = [
  { root: "data", path: "", history: { data: "", models: "" }, selected: 0, entries: [], marked: new Set(), parent: null, revision: 0 },
  { root: "models", path: "", history: { data: "", models: "" }, selected: 0, entries: [], marked: new Set(), parent: null, revision: 0 },
];
let activePane = 0;
let showHidden = false;
let writesEnabled = false;
let operationInProgress = false;
const el = (id) => document.getElementById(id);
let conflictDialogReply = null;
let activeJobConflictKey = null;
let editorState = null;
let editorSaving = false;
const pathLabel = (path) => `/${path}`;
const joinPath = (a, b) => a ? `${a}/${b}` : b;

function humanSize(value) {
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value < 1024) return `${value} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let n = value;
  let unit = -1;
  do { n /= 1024; unit += 1; } while (n >= 1024 && unit < units.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${units[unit]}`;
}
function dateLabel(seconds) {
  return seconds == null ? "—" : new Date(seconds * 1000).toLocaleString("cs-CZ", {year:"numeric", month:"2-digit", day:"2-digit", hour:"2-digit", minute:"2-digit"});
}
function message(text) {
  const node = el("notification");
  node.textContent = text;
  node.classList.add("visible");
  window.clearTimeout(message.timer);
  message.timer = window.setTimeout(() => node.classList.remove("visible"), 5000);
}

function validNewFilename(name) {
  return typeof name === "string" && name.length > 0 && name !== "." && name !== ".."
    && !name.includes("/") && !name.includes("\0")
    && new TextEncoder().encode(name).length <= 255;
}
function duplicateNameSuggestion(name, isDirectory) {
  const dot = !isDirectory ? name.lastIndexOf(".") : -1;
  if (dot > 0) return `${name.slice(0, dot)} (kopie)${name.slice(dot)}`;
  return `${name} (kopie)`;
}
function conflictDisplaySize(size, isDirectory) {
  return isDirectory ? "Adresář" : (Number.isFinite(size) ? humanSize(size) : "Velikost neznámá");
}
// Only one modal can be open at once: the server serializes write operations.
function askConflict(details) {
  if (conflictDialogReply) return Promise.reject(new Error("Dialog konfliktu už je otevřen."));
  el("conflict-source").textContent = details.source;
  el("conflict-target").textContent = details.target;
  el("conflict-source-size").textContent = conflictDisplaySize(details.source_size, details.source_is_dir);
  el("conflict-target-size").textContent = conflictDisplaySize(details.target_size, details.target_is_dir);
  el("conflict-apply-all").checked = false;
  return new Promise((resolve) => {
    conflictDialogReply = resolve;
    el("conflict-dialog").showModal();
  });
}
function chooseConflict(action) {
  if (!conflictDialogReply) return;
  let decision;
  if (action === "rename") {
    const target = el("conflict-target").textContent.split("/").at(-1);
    const newName = window.prompt("Nový název v cílovém adresáři:",
      duplicateNameSuggestion(target, el("conflict-target-size").textContent === "Adresář"));
    if (newName === null) return;
    if (!validNewFilename(newName) || newName === target) {
      message("Zadej jiný platný název bez lomítka (max. 255 bajtů).");
      return;
    }
    decision = {action:"rename", new_name:newName};
  } else if (action === "overwrite" || action === "skip") {
    decision = {action: el("conflict-apply-all").checked ? `${action}_all` : action};
  } else {
    decision = {action:"abort"};
  }
  const resolve = conflictDialogReply;
  conflictDialogReply = null;
  el("conflict-dialog").close();
  resolve(decision);
}
function setActive(index) {
  activePane = index;
  panes.forEach((_, i) => el(`pane-${i}`).classList.toggle("active-pane", i === index));
}
function savePaneState() {
  try {
    sessionStorage.setItem("salamander-web-panes-v1", JSON.stringify(panes.map((p) => ({root:p.root, history:p.history}))));
  } catch (_) { /* private browsing */ }
}
function restorePaneState() {
  try {
    const saved = JSON.parse(sessionStorage.getItem("salamander-web-panes-v1") || "null");
    if (!Array.isArray(saved) || saved.length !== 2) return;
    saved.forEach((v, index) => {
      if (!["data", "models"].includes(v.root)) return;
      panes[index].root = v.root;
      for (const root of ["data", "models"]) {
        if (typeof v.history?.[root] === "string") panes[index].history[root] = v.history[root];
      }
      panes[index].path = panes[index].history[v.root];
    });
  } catch (_) { /* invalid data */ }
}
function col(text, className) {
  const span = document.createElement("span");
  span.className = className;
  span.textContent = text;
  return span;
}
function addRow(container, index, entry, isParent=false, rowIndex=-1) {
  const row = document.createElement("button");
  row.type = "button";
  row.className = "file-row";
  row.setAttribute("role", "option");
  const mark = col(isParent ? "" : "□", "file-mark");
  mark.setAttribute("aria-hidden", "true");
  const icon = col(isParent ? "↖" : entry.kind === "directory" ? "▣" : entry.kind === "symlink" ? "↗" : entry.name.toLowerCase().endsWith(".zip") ? "▥" : "▤", `file-icon ${isParent || entry.directory ? "folder" : entry.kind === "symlink" ? "link" : "file"}`);
  const name = col(isParent ? ".." : entry.name, "file-name-value");
  const label = document.createElement("span");
  label.className = "file-name";
  label.append(mark, icon, name);
  row.append(label, col(isParent || entry.directory ? "<DIR>" : humanSize(entry.size), "file-size"), col(isParent ? "" : dateLabel(entry.modified_unix), "file-date"));
  row.addEventListener("click", (event) => {
    setActive(index);
    const previous = panes[index].selected;
    if (rowIndex >= 0 && event.shiftKey) markRange(index, previous, rowIndex);
    else if (rowIndex >= 0 && (event.ctrlKey || event.metaKey)) toggleMark(index, rowIndex);
    selectRow(index, rowIndex);
  });
  row.addEventListener("dblclick", () => {
    setActive(index);
    if (isParent) goUp(index);
    else if (entry.directory) goInto(index, entry.name);
    else openViewerFor(index,entry.name);
  });
  row.dataset.rowIndex = String(rowIndex);
  container.append(row);
}
function renderStorage(index, storage, quota) {
  const disk = el(`disk-${index}`);
  const bar = el(`disk-bar-${index}`);
  bar.hidden = true;
  const quotaLabel = el(`quota-${index}`);
  quotaLabel.hidden = true;
  if (!storage) {
    disk.textContent = "Volné: nelze zjistit";
    return;
  }
  const free = storage.available_bytes;
  if (quota && quota.hard_bytes != null) {
    // This will become authoritative only after integration with platform quotas.
    const remaining = Math.max(0, quota.hard_bytes - quota.used_bytes);
    disk.textContent = `Volné pro zápis: ${humanSize(Math.min(free, remaining))}`;
    disk.title = "Menší ze zbývající hard kvóty a volných bloků na aktuálním svazku.";
    if (quota.soft_bytes != null && quota.used_bytes >= quota.soft_bytes) {
      quotaLabel.textContent = "Upozornění: dosažen soft limit kvóty";
      quotaLabel.hidden = false;
    }
  } else {
    disk.textContent = `Volné: nejvýše ${humanSize(free)} · kvóta nezjištěna`;
    disk.title = "Kvótu zatím neznáme. Skutečně dostupné místo může být menší.";
  }
}
function renderMarks(index) {
  const pane = panes[index];
  el(`files-${index}`).querySelectorAll(".file-row").forEach((row) => {
    const rowIndex = Number(row.dataset.rowIndex);
    const marked = rowIndex >= 0 && pane.marked.has(pane.entries[rowIndex]?.name);
    row.classList.toggle("marked", marked);
    const mark = row.querySelector(".file-mark");
    if (mark) mark.textContent = rowIndex < 0 ? "" : (marked ? "☑" : "□");
  });
  el(`selection-${index}`).textContent = pane.marked.size
    ? `${pane.marked.size} označeno`
    : pane.selected === -1 ? ".." : pane.entries[pane.selected]?.name ?? "—";
}
function toggleMark(index, selected) {
  const pane = panes[index];
  const entry = pane.entries[selected];
  if (!entry) return;
  if (pane.marked.has(entry.name)) pane.marked.delete(entry.name);
  else pane.marked.add(entry.name);
  renderMarks(index);
}
function markRange(index, from, to) {
  const pane = panes[index];
  const start = Math.max(0, Math.min(from, to));
  const stop = Math.min(pane.entries.length - 1, Math.max(from, to));
  for (let i = start; i <= stop; i++) pane.marked.add(pane.entries[i].name);
  renderMarks(index);
}
function clearMarks(index) {
  panes[index].marked.clear();
  renderMarks(index);
}
function renderPane(index) {
  const pane = panes[index];
  el(`root-${index}`).value = pane.root;
  el(`path-${index}`).value = pathLabel(pane.path);
  const container = el(`files-${index}`);
  container.replaceChildren();
  const cols = document.createElement("div");
  cols.className = "file-cols";
  cols.append(col("Název", ""), col("Velikost", "file-size"), col("Změněno", "file-date"));
  container.append(cols);
  if (pane.parent !== null) addRow(container, index, {kind:"directory", name:".."}, true);
  pane.entries.forEach((entry, rowIndex) => addRow(container, index, entry, false, rowIndex));
  if (!pane.entries.length && pane.parent === null) {
    const empty = document.createElement("p");
    empty.className = "empty";
    empty.textContent = "Tento adresář je prázdný.";
    container.append(empty);
  }
  el(`count-${index}`).textContent = `${pane.entries.length} položek`;
  const dirs = pane.entries.filter((e) => e.directory).length;
  el(`summary-${index}`).textContent = `${dirs} složek · ${pane.entries.length - dirs} souborů`;
  el(`up-${index}`).disabled = pane.parent === null;
  selectRow(index, pane.selected);
}
function selectRow(index, selected) {
  const pane = panes[index];
  const max = Math.max(0, pane.entries.length - 1);
  pane.selected = Math.min(Math.max(selected, pane.parent === null ? 0 : -1), max);
  const row = el(`files-${index}`).querySelectorAll(".file-row");
  let current;
  row.forEach((node) => {
    const active = Number(node.dataset.rowIndex) === pane.selected;
    node.classList.toggle("selected", active);
    node.setAttribute("aria-selected", String(active));
    if (active) current = node;
  });
  el(`selection-${index}`).textContent = pane.selected === -1 ? ".." : pane.entries[pane.selected]?.name ?? "—";
  if (current) current.scrollIntoView({block:"nearest"});
  renderMarks(index);
}
async function loadPane(index) {
  const pane = panes[index];
  const revision = ++pane.revision;
  el(`loading-${index}`).classList.add("visible");
  const requestedPath = pane.path;
  const args = new URLSearchParams({root:pane.root, path:requestedPath, hidden:String(showHidden)});
  try {
    const response = await fetch(`/api/list?${args}`, {cache:"no-store"});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
    if (revision !== pane.revision) return;
    pane.path = result.path;
    pane.history[pane.root] = result.path;
    pane.parent = result.parent;
    pane.entries = result.entries;
    pane.marked = new Set([...pane.marked].filter((name) => pane.entries.some((entry) => entry.name === name)));
    renderStorage(index, result.storage ?? null, result.quota ?? null);
    pane.selected = result.parent == null ? 0 : -1;
    renderPane(index);
    if (result.warnings?.length) message(result.warnings[0]);
    savePaneState();
  } catch (err) {
    if (revision !== pane.revision) return;
    const container = el(`files-${index}`);
    container.replaceChildren();
    const error = document.createElement("div");
    error.className = "error";
    error.textContent = err.message;
    container.append(error);
    el(`count-${index}`).textContent = "Chyba";
    el(`summary-${index}`).textContent = "Načítání se nezdařilo";
    renderStorage(index, null, null);
    message(err.message);
  } finally {
    if (revision === pane.revision) el(`loading-${index}`).classList.remove("visible");
  }
}
function goInto(index, name) {
  const pane = panes[index];
  pane.marked.clear();
  pane.path = joinPath(pane.path, name);
  loadPane(index);
}
function goUp(index) {
  const pane = panes[index];
  if (pane.parent === null) return;
  pane.marked.clear();
  pane.path = pane.parent;
  loadPane(index);
}
function openSelection(index) {
  const pane = panes[index];
  if (pane.selected === -1) goUp(index);
  else if (pane.entries[pane.selected]?.directory) goInto(index, pane.entries[pane.selected].name);
}
function changeRoot(index, root) {
  const pane = panes[index];
  pane.marked.clear();
  pane.history[pane.root] = pane.path;
  pane.root = root;
  pane.path = pane.history[root];
  setActive(index);
  loadPane(index);
}
function typedPath(index) {
  const pane = panes[index];
  pane.marked.clear();
  pane.path = el(`path-${index}`).value.replace(/^\/+/, "");
  setActive(index);
  loadPane(index);
}
function copyNames(pane) {
  if (pane.marked.size) return [...pane.marked];
  if (pane.selected < 0) return [];
  const entry = pane.entries[pane.selected];
  return entry ? [entry.name] : [];
}
function setActionButtons() {
  for (const id of ["copy-button", "move-button", "mkdir-button", "new-file-button", "rename-button", "delete-button", "zip-button", "unzip-button", "upload-button"]) {
    el(id).disabled = !writesEnabled || operationInProgress;
  }
  // Download and F4 viewing are read-only, so both remain available without write access.
  el("download-button").disabled = operationInProgress;
  el("editor-button").disabled = operationInProgress;
  el("viewer-button").disabled = operationInProgress;
}
function jobTitle(operation) {
  return ({copy:"Kopírování", move:"Přesouvání", "zip-create":"Vytváření ZIP", "zip-extract":"Rozbalování ZIP"})[operation] || "Práce se soubory";
}
function renderJob(status) {
  const bar = el("job-bar");
  bar.hidden = false;
  const progress = el("job-progress");
  const total = status.total_bytes;
  if (total > 0) progress.value = Math.min(100, status.done_bytes / total * 100);
  else if (status.total_files > 0) progress.value = Math.min(100, status.done_files / status.total_files * 100);
  else progress.removeAttribute("value"); // archive operations report current item, not a known total
  const title = jobTitle(status.operation);
  el("job-label").textContent = status.state === "waiting_conflict"
    ? `${title} · čeká na rozhodnutí o konfliktu…`
    : status.state === "preparing" ? `${title} · příprava…` : `${title}…`;
  el("job-count").textContent = status.total_files > 0
    ? `${status.done_files} / ${status.total_files} položek · ${humanSize(status.done_bytes)} / ${humanSize(status.total_bytes)}`
    : `${status.done_files} zpracovaných položek${status.current ? " · " + status.current.split("/").at(-1) : ""}`;
}
async function refreshBoth() {
  panes.forEach((pane) => pane.marked.clear());
  await Promise.all(panes.map((_, index) => loadPane(index)));
}
async function pollJob(id) {
  try {
    const response = await fetch(`/api/copy/status?id=${encodeURIComponent(id)}`, {cache:"no-store"});
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
    renderJob(data);
    if (["finished", "cancelled", "failed"].includes(data.state)) {
      try { sessionStorage.removeItem("salamander-web-running-job"); } catch (_) { /* private mode */ }
      activeJobConflictKey = null;
      operationInProgress = false;
      setActionButtons();
      const parts = [];
      if (data.skipped) parts.push(`${data.skipped} přeskočeno (cíl existuje)`);
      if (data.failed) parts.push(`${data.failed} chyb`);
      message(`${jobTitle(data.operation)}: ${data.state === "finished" ? "dokončeno" : "nedokončeno"}${parts.length ? " · " + parts.join(", ") : ""}${data.errors?.length ? " · " + data.errors[0] : ""}`);
      el("job-bar").hidden = true;
      await refreshBoth();
      return;
    }
    if (data.state === "waiting_conflict" && data.pending_conflict) {
      const conflict = data.pending_conflict;
      const key = `${id}:${conflict.id}`;
      if (activeJobConflictKey !== key) {
        activeJobConflictKey = key;
        askConflict(conflict)
          .then((decision) => requestJson("/api/copy/resolve", "resolve", {
            id, conflict_id: conflict.id, ...decision,
          }))
          .catch((error) => {
            message(`Rozhodnutí nebylo přijato: ${error.message}`);
            // Allow the next poll to offer the same still-pending conflict.
            if (activeJobConflictKey === key) activeJobConflictKey = null;
          });
      }
    }
    window.setTimeout(() => pollJob(id), 400);
  } catch (err) {
    // Do not release client-side interlock: the worker could still be running.
    message(`Stav operace nelze načíst: ${err.message}`);
    el("job-label").textContent = "Stav operace neznámý; obnovte stránku";
  }
}
async function requestJson(endpoint, action, payload) {
  const response = await fetch(endpoint, {
    method:"POST", cache:"no-store",
    headers:{"Content-Type":"application/json", "X-Salamander-Request":action},
    body: JSON.stringify(payload),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
  return result;
}
async function startJob(endpoint, action, payload) {
  if (!writesEnabled) { message("Zápis je vypnutý. Povol zápisy v omezených kořenech na serveru."); return; }
  if (operationInProgress) { message("Jiná operace už probíhá."); return; }
  operationInProgress = true;
  activeJobConflictKey = null;
  setActionButtons();
  try {
    const result = await requestJson(endpoint, action, payload);
    try { sessionStorage.setItem("salamander-web-running-job", String(result.id)); } catch (_) { /* private mode */ }
    el("job-bar").hidden = false;
    el("job-label").textContent = `${jobTitle(action)} · příprava…`;
    await pollJob(result.id);
  } catch (err) {
    operationInProgress = false;
    setActionButtons();
    el("job-bar").hidden = true;
    message(err.message);
  }
}
async function writeOnce(endpoint, action, payload, successMessage) {
  if (!writesEnabled) { message("Zápis je vypnutý."); return; }
  if (operationInProgress) { message("Jiná operace už probíhá."); return; }
  operationInProgress = true;
  setActionButtons();
  try {
    await requestJson(endpoint, action, payload);
    message(successMessage);
    await refreshBoth();
  } catch (err) {
    message(err.message);
    await Promise.all(panes.map((_, i) => loadPane(i)));
  } finally {
    operationInProgress = false;
    setActionButtons();
  }
}
function transferSelection(operation) {
  const source = panes[activePane];
  const destination = panes[1 - activePane];
  const names = copyNames(source);
  if (!names.length) { message("Nejdřív vyber nebo označ položky."); return; }
  if (names.length > 128) { message("Najednou lze označit nejvýše 128 položek."); return; }
  if (source.root === destination.root && source.path === destination.path) {
    message("Zdroj i cíl jsou ve stejném adresáři."); return;
  }
  const dest = `${destination.root.toUpperCase()}:/${destination.path}`;
  const verb = operation === "copy" ? "Kopírovat" : "Přesunout";
  if (!window.confirm(`${verb} ${names.length} položek do ${dest}?\nPři existujícím cíli se zeptám, zda jej přepsat, přeskočit či přejmenovat.`)) return;
  startJob(`/api/${operation}`, operation, {
    source_root: source.root, source_path: source.path, names,
    dest_root: destination.root, dest_path: destination.path,
  });
}
function createEmptyFile() {
  if (!writesEnabled || operationInProgress) { message("Zápis není povolen nebo právě probíhá jiná operace."); return; }
  const pane = panes[activePane];
  const name = window.prompt("Název nového prázdného souboru:", "novy.txt");
  if (name === null) return;
  if (!validNewFilename(name)) {
    message("Neplatný název souboru (max. 255 bajtů, bez lomítek).");
    return;
  }
  writeOnce("/api/file/create", "create-file",
    {root:pane.root, path:pane.path, name}, `Vytvořen prázdný soubor: ${name}`);
}
function mkdirCurrent() {
  const pane = panes[activePane];
  const name = window.prompt("Název nového adresáře:", "Nová složka");
  if (name === null) return;
  writeOnce("/api/mkdir", "mkdir", {root:pane.root, path:pane.path, name}, `Adresář ${name} vytvořen.`);
}
function renameCurrent() {
  const pane = panes[activePane];
  const names = copyNames(pane);
  if (names.length !== 1) { message("Pro přejmenování vyber jednu položku."); return; }
  const name = names[0];
  const newName = window.prompt("Nový název:", name);
  if (newName === null || newName === name) return;
  writeOnce("/api/rename", "rename", {root:pane.root, path:pane.path, name, new_name:newName}, `Přejmenováno na ${newName}.`);
}
function deleteCurrent() {
  const pane = panes[activePane];
  const names = copyNames(pane);
  if (!names.length) { message("Nejdřív vyber položky."); return; }
  if (names.length > 128) { message("Lze smazat nejvýše 128 označených položek."); return; }
  const preview = names.slice(0, 5).map((name) => `  • ${name}`).join("\n");
  if (!window.confirm(`TRVALE smazat ${names.length} položek v ${pane.root.toUpperCase()}:/${pane.path}?\n${preview}${names.length > 5 ? "\n  ..." : ""}\n\nTuto operaci NELZE vrátit.`)) return;
  writeOnce("/api/delete", "delete", {root:pane.root, path:pane.path, names, confirm:true}, `Smazáno: ${names.length} položek.`);
}
function createZip() {
  const source = panes[activePane];
  const destination = panes[1 - activePane];
  const names = copyNames(source);
  if (!names.length) { message("Vyber, co chceš zabalit do ZIP."); return; }
  if (names.length > 128) { message("Lze archivovat nejvýše 128 označených položek."); return; }
  const suggested = names.length === 1
    ? (names[0].replace(/\.[^.]+$/, "") || names[0]) + ".zip"
    : "archiv.zip";
  const archiveName = window.prompt(`Název ZIP v ${destination.root.toUpperCase()}:/${destination.path}:`, suggested);
  if (archiveName === null) return;
  startJob("/api/zip/create", "zip-create", {
    source_root:source.root, source_path:source.path, names,
    dest_root:destination.root, dest_path:destination.path, archive_name:archiveName,
  });
}
function extractZip() {
  const source = panes[activePane];
  const destination = panes[1 - activePane];
  const names = copyNames(source);
  if (names.length !== 1 || !names[0].toLowerCase().endsWith(".zip")) {
    message("Vyber jediný soubor .zip."); return;
  }
  const dest = `${destination.root.toUpperCase()}:/${destination.path}`;
  if (!window.confirm(`Rozbalit ${names[0]} do ${dest}?\nExistující soubory se nepřepisují.\nBezpečnostní testovací limit: 20 GiB, 20 000 položek.`)) return;
  startJob("/api/zip/extract", "zip-extract", {
    source_root:source.root, source_path:source.path, name:names[0],
    dest_root:destination.root, dest_path:destination.path,
  });
}
// A browser File is sent as a raw HTTP body (NOT FormData). XMLHttpRequest
// provides upload progress without buffering the whole model in JS memory.
async function uploadWithConflict(file, destination, position, count, doneBytes, totalBytes, policy) {
  let mode = "reject";
  let renamedTo = null;
  for (let attempt = 0; attempt < 8; attempt++) {
    try {
      await uploadOne(file, destination, position, count, doneBytes, totalBytes, mode, renamedTo);
      return "uploaded";
    } catch (error) {
      if (error.status !== 409 || /Jiná souborová operace/.test(error.message)) throw error;
      let decision;
      if (policy.value) {
        decision = {action:policy.value};
      } else {
        const filename = renamedTo || file.name;
        const targetPath = `${destination.root.toUpperCase()}:/${joinPath(destination.path, filename)}`;
        const pane = panes.find((p) => p.root === destination.root && p.path === destination.path);
        const existing = pane?.entries.find((item) => item.name === filename);
        decision = await askConflict({
          source:`PC:/${file.name}`, target:targetPath,
          source_size:file.size, source_is_dir:false,
          target_size:existing?.size ?? null, target_is_dir:existing?.directory === true,
        });
      }
      if (decision.action === "abort") throw new Error("Nahrávání bylo zrušeno uživatelem.");
      if (decision.action === "skip" || decision.action === "skip_all") {
        if (decision.action === "skip_all") policy.value = "skip";
        return "skipped";
      }
      if (decision.action === "overwrite" || decision.action === "overwrite_all") {
        if (decision.action === "overwrite_all") policy.value = "overwrite";
        mode = "overwrite";
        // If the user has already renamed and the new name also collides,
        // overwrite THAT alternate target, never the original by mistake.
      } else if (decision.action === "rename") {
        mode = "rename";
        renamedTo = decision.new_name;
      } else {
        throw new Error("Neznámé rozhodnutí o konfliktu.");
      }
    }
  }
  throw new Error(`${file.name}: příliš mnoho kolizí názvů.`);
}
async function uploadFiles(fileList) {
  const files = [...fileList];
  if (!files.length) return;
  if (!writesEnabled) { message("Zápis je vypnutý."); return; }
  if (operationInProgress) { message("Jiná operace už probíhá."); return; }
  // Freeze the destination: navigating while the upload runs cannot redirect it.
  const destination = {root: panes[activePane].root, path: panes[activePane].path};
  const totalBytes = files.reduce((sum, file) => sum + file.size, 0);
  let completedBytes = 0;
  let completedFiles = 0;
  let skippedFiles = 0;
  const policy = {value:null};
  operationInProgress = true;
  setActionButtons();
  el("job-bar").hidden = false;
  el("job-progress").removeAttribute("value");
  try {
    for (const [index, file] of files.entries()) {
      el("job-label").textContent = `Nahrávání ${index + 1}/${files.length}: ${file.name}`;
      el("job-count").textContent = `${humanSize(completedBytes)} / ${humanSize(totalBytes)}`;
      const outcome = await uploadWithConflict(file, destination, index + 1,
        files.length, completedBytes, totalBytes, policy);
      completedBytes += file.size;
      if (outcome === "uploaded") completedFiles++;
      else skippedFiles++;
    }
    message(`Nahráno ${completedFiles} souborů, přeskočeno ${skippedFiles} do ${destination.root.toUpperCase()}:/${destination.path}.`);
  } catch (error) {
    message(`Nahráno ${completedFiles}, přeskočeno ${skippedFiles} z ${files.length}. ${error.message}`);
  } finally {
    operationInProgress = false;
    setActionButtons();
    el("job-bar").hidden = true;
    el("upload-picker").value = "";
    await refreshBoth();
  }
}
async function downloadCurrent() {
  const pane = panes[activePane];
  const names = copyNames(pane);
  if (names.length !== 1) {
    message("Pro stažení vyber právě jeden soubor. Více souborů nejprve zabal do ZIP.");
    return;
  }
  const entry = pane.entries.find((item) => item.name === names[0]);
  if (!entry || entry.kind !== "file") { message("Stáhnout lze pouze běžný soubor."); return; }
  if (operationInProgress) return;
  const url=`/api/download?${new URLSearchParams({root:pane.root,path:pane.path,name:entry.name})}`;
  if (!window.showSaveFilePicker || !window.indexedDB || !window.isSecureContext) {
    downloadLink(url,entry.name);
    message("Prohlížeč používá běžné stahování; pokračování po přerušení závisí na jeho správci stahování.");
    return;
  }
  operationInProgress=true;setActionButtons();
  el("job-bar").hidden=false;el("job-progress").removeAttribute("value");
  try { await resumableDownload({root:pane.root,path:pane.path},entry,url); }
  catch (err) { if (err.name!=="AbortError") message(`Stahování přerušeno: ${err.message} Vyber znovu Stáhnout pro navázání.`); }
  finally {operationInProgress=false;setActionButtons();el("job-bar").hidden=true;}
}

function lineEndingMode(text) {
  const hasCrLf = text.includes("\r\n");
  const hasBareLf = /(^|[^\r])\n/.test(text);
  const hasBareCr = /\r(?!\n)/.test(text);
  if (hasBareCr || (hasCrLf && hasBareLf)) return "mixed";
  return hasCrLf ? "crlf" : "lf";
}
async function openEditor() {
  if (operationInProgress || editorState || el("editor-dialog").open) return;
  const pane = panes[activePane];
  const names = copyNames(pane);
  if (names.length !== 1 || pane.entries.find((entry) => entry.name === names[0])?.kind !== "file") {
    message("Pro F4 vyber jeden běžný textový soubor (UTF-8, maximálně 1 MiB).");
    return;
  }
  const query = new URLSearchParams({root:pane.root, path:pane.path, name:names[0]});
  try {
    const response = await fetch(`/api/editor/open?${query}`, {cache:"no-store"});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
    const lineEndings = lineEndingMode(result.text);
    const textDisplay = result.text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
    const onlyView = result.read_only || lineEndings === "mixed";
    editorState = {root:pane.root, path:pane.path, name:names[0], original:result.text,
      displayOriginal:textDisplay, lineEndings};
    el("editor-heading").textContent = onlyView ? "Prohlížeč textu (pouze čtení)" : "Textový editor";
    el("editor-file").textContent = `${pane.root.toUpperCase()}:/${joinPath(pane.path, names[0])}`;
    el("editor-text").value = textDisplay;
    el("editor-text").readOnly = onlyView;
    el("editor-save").disabled = onlyView;
    if (lineEndings === "mixed") message("Smíšené CR/LF konce řádků: editor umožní jen čtení, aby je nezměnil.");
    editorBytesChanged();
    el("editor-dialog").showModal();
    el("editor-text").focus();
  } catch (err) { message(`Editor: ${err.message}`); }
}
function editorBytesChanged() {
  const value = el("editor-text").value;
  const bytes = new TextEncoder().encode(value).length;
  el("editor-info").textContent = `${humanSize(bytes)} / 1 MiB · UTF-8${editorState && value !== editorState.displayOriginal ? " · neuložené změny" : ""}`;
  if (editorState && !el("editor-text").readOnly) {
    el("editor-save").disabled = editorSaving || bytes > 1024 * 1024;
  }
}
async function saveEditor() {
  if (!editorState || el("editor-text").readOnly || editorSaving) return;
  const content = el("editor-text").value;
  if (content === editorState.displayOriginal) { message("Soubor není změněn."); return; }
  if (new TextEncoder().encode(content).length > 1024 * 1024) {
    message("Text je příliš velký (maximálně 1 MiB)."); return;
  }
  const storedText = editorState.lineEndings === "crlf" ? content.replace(/\n/g, "\r\n") : content;
  if (new TextEncoder().encode(storedText).length > 1024 * 1024) {
    message("Text s původními konci řádků přesahuje 1 MiB."); return;
  }
  editorSaving = true;
  el("editor-save").disabled = true;
  try {
    await requestJson("/api/editor/save", "edit", {
      root:editorState.root, path:editorState.path, name:editorState.name,
      original:editorState.original, text:storedText,
    });
    editorState.original = storedText;
    editorState.displayOriginal = content;
    message(`Uloženo: ${editorState.name}`);
    await loadPane(activePane);
  } catch (err) {
    message(`Uložení selhalo: ${err.message}`);
  } finally {
    editorSaving = false;
    editorBytesChanged();
  }
}
function closeEditor() {
  if (editorSaving) return;
  if (editorState && el("editor-text").value !== editorState.displayOriginal &&
      !window.confirm("Zahodit neuložené změny?")) return;
  el("editor-dialog").close();
  editorState = null;
}

async function loadCapabilities() {
  try {
    const response = await fetch("/api/health", {cache:"no-store"});
    const result = await response.json();
    writesEnabled = response.ok && result.writes_enabled === true;
    el("mode-label").textContent = result.unrestricted ? "⚠ Neomezený režim správce" :
      (writesEnabled ? "Landlock · zápisy povoleny" : "Pouze čtení");
    el("mode-badge").classList.toggle("unrestricted", Boolean(result.unrestricted));
    if (result.unrestricted) {
      const rootsResponse = await fetch("/api/roots", {cache:"no-store"});
      if (rootsResponse.ok) {
        const roots = (await rootsResponse.json()).roots || [];
        for (const index of [0, 1]) {
          for (const option of el(`root-${index}`).options) {
            const known = roots.find((root) => root.id === option.value);
            if (known) option.textContent = known.label;
          }
        }
      }
    }
    setActionButtons();
  } catch (_) {
    writesEnabled = false;
    setActionButtons();
  }
}
function onKeyboard(event) {
  if (el("preview-dialog").open) {
    if (event.key === "ArrowLeft") { event.preventDefault(); viewerNext(-1); }
    if (event.key === "ArrowRight") { event.preventDefault(); viewerNext(1); }
    if (event.key === "Escape") { event.preventDefault(); closeViewer(); }
    return;
  }
  if (el("editor-dialog").open) {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
      event.preventDefault(); saveEditor();
    }
    return;
  }
  if (el("conflict-dialog").open) return;
  if (event.ctrlKey && event.key.toLowerCase() === "r") {
    event.preventDefault();
    panes.forEach((_,i) => loadPane(i));
    return;
  }
  if (event.ctrlKey && event.key.toLowerCase() === "h") {
    event.preventDefault();
    showHidden = !showHidden;
    el("hidden").checked = showHidden;
    panes.forEach((_,i) => loadPane(i));
    return;
  }
  if (event.ctrlKey && event.key.toLowerCase() === "l") {
    event.preventDefault();
    el(`path-${activePane}`).focus();
    el(`path-${activePane}`).select();
    return;
  }
  if (event.key === "Tab" && !event.ctrlKey && !event.altKey && !event.shiftKey) {
    event.preventDefault();
    setActive(1 - activePane);
    return;
  }
  const target = event.target;
  if (target instanceof HTMLInputElement || target instanceof HTMLSelectElement) return;
  if (event.key === "F2") { event.preventDefault(); renameCurrent(); return; }
  if (event.key === "F3") { event.preventDefault(); openViewer(); return; }
  if (event.key === "F4" && event.shiftKey) { event.preventDefault(); createEmptyFile(); return; }
  if (event.key === "F4") { event.preventDefault(); openEditor(); return; }
  if (event.key === "F5") { event.preventDefault(); transferSelection("copy"); return; }
  if (event.key === "F6") { event.preventDefault(); transferSelection("move"); return; }
  if (event.key === "F7") { event.preventDefault(); mkdirCurrent(); return; }
  if (event.key === "F8") { event.preventDefault(); deleteCurrent(); return; }
  if (event.key === " " || event.key === "Spacebar") {
    event.preventDefault();
    const selected = panes[activePane].selected;
    toggleMark(activePane, selected);
    if (selected >= 0 && selected < panes[activePane].entries.length - 1) selectRow(activePane, selected + 1);
    return;
  }
  if (event.key === "Escape") { event.preventDefault(); clearMarks(activePane); return; }
  if (event.key === "Backspace") { event.preventDefault(); goUp(activePane); }
  if (event.key === "Enter") { event.preventDefault(); openSelection(activePane); }
  if (event.key === "ArrowDown") { event.preventDefault(); selectRow(activePane, panes[activePane].selected + 1); }
  if (event.key === "ArrowUp") { event.preventDefault(); selectRow(activePane, panes[activePane].selected - 1); }
}

restorePaneState();
for (const index of [0,1]) {
  el(`pane-${index}`).addEventListener("mousedown", () => setActive(index));
  el(`root-${index}`).addEventListener("change", (event) => changeRoot(index, event.target.value));
  el(`up-${index}`).addEventListener("click", () => {setActive(index); goUp(index);});
  el(`path-${index}`).addEventListener("keydown", (event) => {
    if (event.key === "Enter") {event.preventDefault(); typedPath(index); el(`path-${index}`).blur();}
    if (event.key === "Escape") {event.preventDefault(); el(`path-${index}`).value = pathLabel(panes[index].path); el(`path-${index}`).blur();}
  });
  loadPane(index);
}
el("refresh").addEventListener("click", () => panes.forEach((_,i) => loadPane(i)));
el("hidden").addEventListener("change", (event) => {showHidden = event.target.checked; panes.forEach((_,i) => loadPane(i));});
document.addEventListener("keydown", onKeyboard);
el("copy-button").addEventListener("click", () => transferSelection("copy"));
el("move-button").addEventListener("click", () => transferSelection("move"));
el("mkdir-button").addEventListener("click", mkdirCurrent);
el("new-file-button").addEventListener("click", createEmptyFile);
el("rename-button").addEventListener("click", renameCurrent);
el("editor-button").addEventListener("click", openEditor);
el("viewer-button").addEventListener("click", openViewer);
el("delete-button").addEventListener("click", deleteCurrent);
el("zip-button").addEventListener("click", createZip);
el("unzip-button").addEventListener("click", extractZip);
el("upload-button").addEventListener("click", () => {
  if (!operationInProgress && writesEnabled) el("upload-picker").click();
});
el("upload-picker").addEventListener("change", (event) => uploadFiles(event.target.files));
el("download-button").addEventListener("click", downloadCurrent);
el("editor-save").addEventListener("click", saveEditor);
el("editor-cancel").addEventListener("click", closeEditor);
el("editor-close").addEventListener("click", closeEditor);
el("editor-text").addEventListener("input", editorBytesChanged);
el("editor-dialog").addEventListener("cancel", (event) => {
  event.preventDefault(); closeEditor();
});
for (const button of document.querySelectorAll("[data-conflict-action]")) {
  button.addEventListener("click", () => chooseConflict(button.dataset.conflictAction));
}
el("conflict-dialog").addEventListener("cancel", (event) => {
  event.preventDefault();
  chooseConflict("abort");
});
loadCapabilities().then(() => {
  try {
    const stored = Number(sessionStorage.getItem("salamander-web-running-job"));
    if (Number.isSafeInteger(stored) && stored > 0 && !operationInProgress) {
      operationInProgress = true;
      setActionButtons();
      pollJob(stored);
    }
  } catch (_) { /* private mode */ }
});

// v0.10: resumable uploads and downloads. The server is the source of truth
// for committed upload bytes; localStorage stores only the random session ID.
const UPLOAD_CHUNK_SIZE = 4 * 1024 * 1024;
const DOWNLOAD_CHECKPOINT = 256 * 1024 * 1024;
let viewerState = null;
let viewerNativeZoom = false;

async function fingerprintFile(file) {
  if (!crypto.subtle || !window.isSecureContext) {
    throw new Error("Obnovitelné nahrávání vyžaduje zabezpečený kontext (localhost nebo HTTPS).");
  }
  const piece = 128 * 1024;
  const prefix = new TextEncoder().encode(`${file.name}|${file.size}|${file.lastModified}|`);
  const a = await file.slice(0, piece).arrayBuffer();
  const b = await file.slice(Math.max(0, file.size - piece)).arrayBuffer();
  const bytes = new Uint8Array(prefix.byteLength + a.byteLength + b.byteLength);
  bytes.set(prefix);
  bytes.set(new Uint8Array(a), prefix.byteLength);
  bytes.set(new Uint8Array(b), prefix.byteLength + a.byteLength);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map(v => v.toString(16).padStart(2, "0")).join("");
}
function transferJsonUrl(path, args) { return `${path}?${new URLSearchParams(args)}`; }
function sessionStorageKey(destination, file, fingerprint) {
  return `salamander-upload-v1:${JSON.stringify([destination.root,destination.path,file.name,file.size,file.lastModified,fingerprint])}`;
}
function getStoredUpload(key) {
  try { return localStorage.getItem(key); } catch (_) { return null; }
}
function storeUpload(key, id) {
  try { localStorage.setItem(key, id); } catch (_) { /* private browsing: within-tab resume still possible */ }
}
function forgetUpload(key) { try { localStorage.removeItem(key); } catch (_) {} }
async function postResume(path, action, data) {
  const response = await fetch(path, {method:"POST",cache:"no-store",headers:{
    "Content-Type":"application/json","X-Salamander-Request":action,
  },body:JSON.stringify(data)});
  const body = await response.json().catch(() => ({}));
  if (!response.ok) { const error = new Error(body.error || `HTTP ${response.status}`);error.status=response.status;throw error; }
  return body;
}
function uploadProgress(file, position, count, done, total) {
  el("job-label").textContent = `Nahrávání ${position}/${count}: ${file.name}`;
  el("job-count").textContent = `${humanSize(done)} / ${humanSize(total)}`;
  if (total > 0) el("job-progress").value = Math.min(100, 100*done/total);
  else el("job-progress").removeAttribute("value");
}
async function uploadOne(file, destination, position, count, completedBytes, allBytes, mode="reject", newName=null) {
  const fingerprint = await fingerprintFile(file);
  const key = sessionStorageKey(destination,file,fingerprint);
  let id = getStoredUpload(key);
  let state = null;
  if (id) {
    const response = await fetch(transferJsonUrl("/api/resumable/status",{
      root:destination.root,path:destination.path,id,
    }), {cache:"no-store"});
    if (response.ok) {
      const data = await response.json();
      if (data.name===file.name && data.expected_size===file.size &&
          data.last_modified===file.lastModified && data.fingerprint===fingerprint) {
        state = data;
        message(`Pokračuji v přerušeném nahrávání ${file.name} od ${humanSize(data.committed)}.`);
      }
    }
    if (!state) {forgetUpload(key);id=null;}
  }
  if (!state) {
    state = await postResume("/api/resumable/start","resume-start",{
      root:destination.root,path:destination.path,name:file.name,
      expected_size:file.size,last_modified:file.lastModified,fingerprint,
      mode,new_name:newName,
    });
    id = state.id;
    storeUpload(key,id);
  }
  let offset = state.committed;
  while (offset < file.size) {
    uploadProgress(file, position, count, completedBytes + offset, allBytes);
    const slice = file.slice(offset,Math.min(file.size,offset+UPLOAD_CHUNK_SIZE));
    const query = {root:destination.root,path:destination.path,id,offset};
    const response = await fetch(transferJsonUrl("/api/resumable/chunk",query),{
      method:"POST",cache:"no-store",headers:{
        "Content-Type":"application/octet-stream","X-Salamander-Request":"resume-chunk",
      },body:slice,
    });
    const result = await response.json().catch(() => ({}));
    if (!response.ok) {
      if (response.status === 409) {
        // A request may have reached the server even if its reply was lost.
        const check = await fetch(transferJsonUrl("/api/resumable/status",{
          root:destination.root,path:destination.path,id,
        }),{cache:"no-store"});
        if (check.ok) {
          const current = await check.json();
          if (current.committed > offset && current.committed <= file.size) {
            offset = current.committed;continue;
          }
        }
      }
      const error = new Error(`${file.name}: ${result.error || `HTTP ${response.status}`}`);
      error.status = response.status === 409 ? 0 : response.status; // checkpoint error, not a target-name conflict
      throw error;
    }
    if (!Number.isSafeInteger(result.offset) || result.offset !== offset + slice.size) {
      throw new Error("Server potvrdil nesprávnou délku bloku; přenos lze obnovit opětovným výběrem souboru.");
    }
    offset = result.offset;
  }
  const result = await postResume("/api/resumable/finish","resume-finish",{
    root:destination.root,path:destination.path,id,
  });
  if (!result.ok) throw new Error("Server nedokončil nahrávání.");
  forgetUpload(key);
  uploadProgress(file,position,count,completedBytes+file.size,allBytes);
}

function downloadLink(url,name) {
  const link=document.createElement("a");link.href=url;link.download=name;
  document.body.append(link);link.click();link.remove();
}
function transferDatabase() {
  return new Promise((resolve,reject) => {
    const request=indexedDB.open("salamander-web-transfers",1);
    request.onupgradeneeded=()=>request.result.createObjectStore("downloads",{keyPath:"key"});
    request.onsuccess=()=>resolve(request.result);
    request.onerror=()=>reject(request.error);
  });
}
async function transferDb(action,key,value) {
  const db=await transferDatabase();
  return new Promise((resolve,reject)=>{
    const tx=db.transaction("downloads",action==="get"?"readonly":"readwrite");
    const store=tx.objectStore("downloads");
    const r=action==="get"?store.get(key):action==="put"?store.put(value):store.delete(key);
    r.onsuccess=()=>resolve(r.result);
    r.onerror=()=>reject(r.error);
    tx.oncomplete=()=>db.close();
    tx.onerror=()=>db.close();
  });
}
async function resumableDownload(pane, entry, url) {
  // showSaveFilePicker + IndexedDB allow reconnecting a previously selected
  // destination after a page reload, subject to the browser's file permission.
  const head=await fetch(url,{method:"HEAD",cache:"no-store"});
  if (!head.ok) throw new Error(`Server nevrátil informace o souboru: HTTP ${head.status}`);
  const etag=head.headers.get("etag");
  const size=Number(head.headers.get("content-length"));
  if (!etag || !Number.isSafeInteger(size) || size<0) {
    throw new Error("Server neposkytl bezpečnou verzi a velikost stahovaného souboru.");
  }
  const key=JSON.stringify([pane.root,pane.path,entry.name]);
  let old=await transferDb("get",key).catch(()=>null);
  let handle=null;let offset=0;
  if (old && old.etag===etag && old.size===size && old.handle) {
    try {
      handle=old.handle;
      let perm=await handle.queryPermission({mode:"readwrite"});
      if (perm!=="granted") perm=await handle.requestPermission({mode:"readwrite"});
      if (perm!=="granted") handle=null;
      if (handle) {
        const onDisk=await handle.getFile();
        if (onDisk.size === old.bytes && onDisk.size <= size) offset=onDisk.size;
        else {handle=null;offset=0;}
      }
    } catch (_) {handle=null;}
  }
  if (!handle) {
    handle=await showSaveFilePicker({suggestedName:entry.name});
    offset=0;
  }
  await transferDb("put",key,{key,etag,size,handle,bytes:offset});
  el("job-bar").hidden=false;
  while (offset < size) {
    const end=Math.min(size-1, offset+DOWNLOAD_CHECKPOINT-1);
    const response=await fetch(url,{cache:"no-store",headers:{
      "Range":`bytes=${offset}-${end}`,"If-Range":etag,
    }});
    if (response.status!==206 || response.headers.get("etag")!==etag ||
        !response.headers.get("content-range")?.startsWith(`bytes ${offset}-${end}/${size}`)) {
      throw new Error("Soubor se změnil nebo server nepodporuje bezpečné navázání. Dosavadní část nebyla přepsána.");
    }
    // Checkpoint on disk at most every 256 MiB; never load a large file in RAM.
    const writer=await handle.createWritable({keepExistingData:offset>0});
    try {
      if (offset) await writer.seek(offset);
      const reader=response.body.getReader();
      let written=0;
      while (true) {
        const {done,value}=await reader.read();
        if (done) break;
        if (written+value.byteLength > end-offset+1) throw new Error("Server poslal více dat, než bylo požadováno.");
        await writer.write(value);written+=value.byteLength;
        el("job-progress").value=100*(offset+written)/size;
        el("job-label").textContent=`Stahování: ${entry.name}`;
        el("job-count").textContent=`${humanSize(offset+written)} / ${humanSize(size)}`;
      }
      if (written!==end-offset+1) throw new Error("Přenos se předčasně přerušil.");
      await writer.close();
      offset += written;
      await transferDb("put",key,{key,etag,size,handle,bytes:offset});
    } catch (err) {
      await writer.abort().catch(()=>{});
      throw err;
    }
  }
  if (size===0) { const writer=await handle.createWritable();await writer.close(); }
  await transferDb("delete",key);
  message(`Staženo: ${entry.name}`);
}

function previewParams(fields) {
  const summary=[];
  let prompt="",seed="",workflow="";
  const find=(name)=>fields.find(f=>f.key.toLowerCase()===name)?.text;
  const legacy=find("parameters") || find("usercomment");
  if (legacy) {
    const stepsPos=legacy.search(/(?:^|\n)Steps:\s*/);
    const begin=stepsPos<0 ? legacy : legacy.slice(0,stepsPos);
    const neg=begin.indexOf("Negative prompt:");
    prompt=(neg<0 ? begin : begin.slice(0,neg)).trim();
    const negative=neg<0?"":begin.slice(neg+"Negative prompt:".length).trim();
    if (prompt) summary.push(["Positive prompt",prompt]);
    if (negative) summary.push(["Negative prompt",negative]);
    if (stepsPos>=0) {
      const list=legacy.slice(stepsPos).replace(/^\s+/,"").split(/,\s+(?=[A-Za-z][A-Za-z _-]*:)/);
      for (const part of list) {
        const colon=part.indexOf(":");
        if (colon>0) {
          const key=part.slice(0,colon).trim(),value=part.slice(colon+1).trim();
          if (key && value) {summary.push([key,value]);if (key.toLowerCase()==="seed") seed=value;}
        }
      }
    }
  }
  workflow=find("workflow")||"";
  const comfy=find("prompt");
  if (comfy) {
    try {
      const graph=JSON.parse(comfy);
      const items=Object.entries(graph);
      for (const [id,node] of items) {
        const inputs=node && typeof node==="object" ? node.inputs : null;
        if (!inputs || typeof inputs!=="object") continue;
        const type=String(node.class_type||"Uzel");
        if (typeof inputs.text==="string" && type.toLowerCase().includes("textencode")) {
          summary.push([`Prompt (${type} #${id})`,inputs.text]);
          if (!prompt) prompt=inputs.text;
        }
        for (const key of ["seed","noise_seed","steps","cfg","sampler_name","scheduler","ckpt_name","unet_name","model_name"]) {
          if (["number","string"].includes(typeof inputs[key])) {
            summary.push([`${type} #${id} · ${key}`,String(inputs[key])]);
            if (!seed && (key==="seed"||key==="noise_seed")) seed=String(inputs[key]);
          }
        }
      }
    } catch (_) { /* preserve raw metadata even if the Comfy prompt is non-JSON */ }
  }
  if (workflow) {
    try { workflow=JSON.stringify(JSON.parse(workflow),null,2); } catch (_) { /* preserve source */ }
  }
  if (!summary.length && fields.length) summary.push(["Metadata","Soubor obsahuje metadata, ale nejsou rozpoznané jednoduché generační parametry. Viz původní metadata níže."]);
  return {summary,prompt,seed,workflow};
}
function metaRow(title,value) {
  const div=document.createElement("div");div.className="preview-meta-row";
  const name=document.createElement("strong");name.textContent=title;
  const text=document.createElement("pre");text.textContent=value;
  div.append(name,text);return div;
}
function showMetadata(info) {
  const fields=info.metadata || [];
  const parsed=previewParams(fields);
  viewerState.parsed=parsed;
  const rows=el("preview-summary"); rows.replaceChildren();
  for (const [key,value] of parsed.summary) rows.append(metaRow(key,value));
  if (!fields.length) rows.append(metaRow("Metadata","V souboru nebyla nalezena podporovaná vložená metadata."));
  const raw=el("preview-raw"); raw.replaceChildren();
  for (const field of fields) {
    const div=document.createElement("div");div.className="preview-raw-entry";
    const head=document.createElement("strong");head.textContent=`${field.source} · ${field.key}`;
    const text=document.createElement("pre");
    text.textContent=field.text ?? `Binární metadata (${Math.round(field.raw_base64.length*3/4)} B), původní bajty v exportu JSON.`;
    div.append(head,text);raw.append(div);
  }
  if (!fields.length) {const p=document.createElement("p");p.textContent="Žádná podporovaná metadata.";raw.append(p);}
  const warnings=(info.warnings||[]).join(" ");
  el("preview-meta-info").textContent=`${fields.length} položek · ${humanSize(info.size)}${warnings?` · ${warnings}`:""}`;
  el("preview-copy-prompt").disabled=!parsed.prompt;
  el("preview-copy-seed").disabled=!parsed.seed;
  el("preview-copy-workflow").disabled=!parsed.workflow;
  el("preview-export-workflow").disabled=!parsed.workflow;
  el("preview-export-all").disabled=!fields.length;
}
function previewImageName(name) {return /\.(png|jpe?g|webp|gif)$/i.test(name);}
async function openViewerFor(index,name) {
  const pane=panes[index];
  const entry=pane.entries.find(e=>e.name===name);
  if (!entry || entry.kind!=="file") {message("F3 vyžaduje běžný soubor.");return;}
  const details={index,root:pane.root,path:pane.path,name};
  if (!el("preview-dialog").open) el("preview-dialog").showModal();
  viewerState={...details,parsed:null,info:null};
  viewerNativeZoom=false;
  el("preview-image").removeAttribute("src");
  el("preview-image").hidden=true;
  el("preview-image").classList.remove("preview-native");
  el("preview-text").hidden=true;
  el("preview-empty").hidden=false;
  el("preview-empty").textContent="Načítám náhled…";
  el("preview-summary").replaceChildren();
  el("preview-raw").replaceChildren();
  el("preview-file").textContent=`${pane.root.toUpperCase()}:/${joinPath(pane.path,name)}`;
  el("preview-status").textContent="F3 – pouze čtení";
  const query=new URLSearchParams({root:pane.root,path:pane.path,name});
  try {
    const response=await fetch(`/api/preview/info?${query}`,{cache:"no-store"});
    const info=await response.json();
    if (!response.ok) throw new Error(info.error||`HTTP ${response.status}`);
    if (!viewerState || viewerState.name!==name || viewerState.path!==pane.path) return;
    viewerState.info=info;
    el("preview-empty").hidden=true;
    if (info.kind==="image") {
      const img=el("preview-image");
      img.src=`/api/preview/image?${query}`;
      img.hidden=false;
    } else {
      const pre=el("preview-text");pre.textContent=info.text||"";pre.hidden=false;
      if (info.truncated) el("preview-status").textContent="Zobrazeny první 2 MiB textu. Originál je beze změn.";
    }
    showMetadata(info);
    el("preview-prev").disabled=info.kind!=="image";
    el("preview-next").disabled=info.kind!=="image";
    el("preview-zoom").disabled=info.kind!=="image";
  } catch (err) {
    el("preview-empty").hidden=false;
    el("preview-empty").textContent=err.message;
    el("preview-meta-info").textContent="Náhled nelze otevřít.";
  }
}
function openViewer() {
  if (operationInProgress || el("editor-dialog").open) return;
  const pane=panes[activePane],names=copyNames(pane);
  if (names.length!==1) {message("Pro F3 vyber právě jeden soubor.");return;}
  openViewerFor(activePane,names[0]);
}
function viewerNext(direction) {
  if (!viewerState || !viewerState.info || viewerState.info.kind!=="image") return;
  const {index,name}=viewerState;
  const files=panes[index].entries.filter(e=>e.kind==="file" && previewImageName(e.name));
  const current=files.findIndex(e=>e.name===name);
  if (current<0 || files.length<2) return;
  const next=files[(current+direction+files.length)%files.length];
  openViewerFor(index,next.name);
}
function closeViewer() {
  if (!el("preview-dialog").open) return;
  el("preview-dialog").close();
  el("preview-image").removeAttribute("src");
  viewerState=null;
}
async function copyPreview(kind) {
  const text=viewerState?.parsed?.[kind];
  if (!text) return;
  try {await navigator.clipboard.writeText(text);message("Zkopírováno do schránky.");}
  catch (_) {message("Schránka není dostupná; použij zabezpečený kontext localhost/HTTPS.");}
}
function exportPreview(kind) {
  if (!viewerState?.info) return;
  const {name,parsed,info}=viewerState;
  const data=kind==="workflow" ? parsed?.workflow : JSON.stringify({
    filename:name, metadata:info.metadata, warnings:info.warnings,
  },null,2);
  if (!data) return;
  const blob=new Blob([data],{type:"application/json;charset=utf-8"});
  const link=document.createElement("a");
  const url=URL.createObjectURL(blob);
  link.href=url;
  link.download=kind==="workflow"?`${name}.workflow.json`:`${name}.metadata.json`;
  document.body.append(link);link.click();link.remove();
  setTimeout(()=>URL.revokeObjectURL(url),30000);
}

el("preview-close").addEventListener("click",closeViewer);
el("preview-prev").addEventListener("click",()=>viewerNext(-1));
el("preview-next").addEventListener("click",()=>viewerNext(1));
el("preview-zoom").addEventListener("click",()=>{
  viewerNativeZoom=!viewerNativeZoom;
  el("preview-image").classList.toggle("preview-native",viewerNativeZoom);
});
el("preview-copy-prompt").addEventListener("click",()=>copyPreview("prompt"));
el("preview-copy-seed").addEventListener("click",()=>copyPreview("seed"));
el("preview-copy-workflow").addEventListener("click",()=>copyPreview("workflow"));
el("preview-export-workflow").addEventListener("click",()=>exportPreview("workflow"));
el("preview-export-all").addEventListener("click",()=>exportPreview("all"));
el("preview-dialog").addEventListener("close",()=>{
  el("preview-image").removeAttribute("src");viewerState=null;
});
