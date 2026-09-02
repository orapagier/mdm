"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/* `$`, `bytes`, `rate`, `duration` and `eta` come from format.js. */

/* ------------------------------------------------------------------ *
 * State
 * ------------------------------------------------------------------ */

let snapshot = { downloads: [], globalSpeed: 0, active: 0, queued: 0 };
let filter = { kind: "all", value: null };
let settings = null;

function visible(d) {
  if (filter.kind === "all") return true;
  if (filter.kind === "category") return d.category === filter.value;
  return d.status === filter.kind;
}

/* ------------------------------------------------------------------ *
 * Rendering
 *
 * Rows are reconciled by id rather than rebuilt: a full innerHTML swap every
 * 700 ms would drop text selection and make the scroll position jump.
 * ------------------------------------------------------------------ */

const rowCache = new Map(); // id -> {tr, refs}

function render() {
  const tbody = $("rows");
  const items = snapshot.downloads.filter(visible);

  $("empty").hidden = items.length > 0;
  $("stat-speed").textContent = rate(snapshot.globalSpeed);
  $("c-active").textContent = snapshot.active || "";
  $("c-queued").textContent = snapshot.queued || "";

  const seen = new Set();
  let previous = null;

  for (const d of items) {
    seen.add(d.id);
    let entry = rowCache.get(d.id);
    if (!entry) {
      entry = buildRow(d);
      rowCache.set(d.id, entry);
    }
    updateRow(entry, d);

    // Keep DOM order matching the sorted data without reinserting every row.
    const expected = previous ? previous.nextSibling : tbody.firstChild;
    if (entry.tr !== expected) {
      tbody.insertBefore(entry.tr, expected);
    }
    previous = entry.tr;
  }

  for (const [id, entry] of rowCache) {
    if (!seen.has(id)) {
      entry.tr.remove();
      rowCache.delete(id);
    }
  }

  renderCategories();
}

function buildRow(d) {
  const tr = document.createElement("tr");
  tr.innerHTML = `
    <td><div class="name"><span class="file"></span><span class="sub"></span></div></td>
    <td class="num size"></td>
    <td><div class="bar"><i></i></div><div class="pct"></div></td>
    <td class="num speed"></td>
    <td class="num eta"></td>
    <td><div class="actions"></div></td>`;

  const refs = {
    file: tr.querySelector(".file"),
    sub: tr.querySelector(".sub"),
    size: tr.querySelector(".size"),
    bar: tr.querySelector(".bar"),
    fill: tr.querySelector(".bar > i"),
    pct: tr.querySelector(".pct"),
    speed: tr.querySelector(".speed"),
    eta: tr.querySelector(".eta"),
    actions: tr.querySelector(".actions"),
    lastSignature: "",
  };
  return { tr, refs };
}

function updateRow({ refs }, d) {
  refs.file.textContent = d.filename;
  refs.file.title = d.url;

  const parts = [d.category];
  if (d.status === "failed" && d.error) parts.push(d.error);
  else if (d.status === "complete") parts.push(d.directory);
  else if (d.connections > 0) parts.push(`${d.connections} connections`);
  else parts.push(d.status);
  const sub = parts.join(" · ");
  refs.sub.textContent = sub;
  // The digest is too long for the row, but belongs somewhere it can be read
  // and compared against a project's published checksum.
  refs.sub.title = d.sha256 ? sub + "\n\nSHA-256: " + d.sha256 : sub;
  refs.sub.className = "sub " + (d.status === "failed" ? "status-failed" : "");

  refs.size.textContent = bytes(d.totalBytes);

  const pct = d.totalBytes > 0 ? (d.completedBytes / d.totalBytes) : 0;
  refs.fill.style.width = `${Math.min(100, pct * 100).toFixed(1)}%`;
  refs.bar.className =
    "bar" +
    (d.status === "complete" ? " done" : d.status === "failed" ? " err" :
     d.status === "active" ? "" : " idle");
  refs.pct.textContent =
    d.status === "complete" ? "Done"
    : d.totalBytes > 0 ? `${(pct * 100).toFixed(1)}% of ${bytes(d.totalBytes)}`
    : bytes(d.completedBytes);

  refs.speed.textContent = d.status === "active" ? rate(d.downloadSpeed) : "—";
  refs.eta.textContent = eta(d);

  // Buttons only change when the status does, so skip the churn otherwise.
  // The digest lands after completion, so it has to take part in the key or
  // the copy button would never appear.
  const signature = d.status + (d.sha256 ? ":hashed" : "");
  if (refs.lastSignature !== signature) {
    refs.lastSignature = signature;
    refs.actions.replaceChildren(...actionsFor(d));
  }
}

function actionsFor(d) {
  const make = (label, title, fn) => {
    const b = document.createElement("button");
    b.textContent = label;
    b.title = title;
    b.addEventListener("click", fn);
    return b;
  };
  const out = [];

  if (d.status === "active" || d.status === "queued") {
    out.push(make("❚❚", "Pause", () => call("pause", { id: d.id })));
  } else if (d.status === "paused") {
    out.push(make("▶", "Resume", () => call("resume", { id: d.id })));
  } else if (d.status === "failed") {
    out.push(make("↻", "Retry", () => call("retry", { id: d.id })));
  } else if (d.status === "complete") {
    out.push(make("↗", "Open file", () =>
      call("open_path", { path: `${d.directory}/${d.filename}`, reveal: false })));
    out.push(make("🗀", "Open folder", () =>
      call("open_path", { path: `${d.directory}/${d.filename}`, reveal: true })));
    if (d.sha256) {
      out.push(make("#", "Copy SHA-256", async () => {
        try {
          await navigator.clipboard.writeText(d.sha256);
          toast("SHA-256 copied");
        } catch {
          // Clipboard access can be refused; showing it still lets the user
          // read the digest off the screen.
          toast(d.sha256);
        }
      }));
    }
  }

  out.push(make("✕", "Remove", async () => {
    const unfinished = d.status !== "complete";
    // Only offer to delete bytes when there are bytes worth deleting.
    const deleteFile =
      unfinished && d.completedBytes > 0
        ? confirm(`Remove "${d.filename}" and delete the partial file?`)
        : false;
    if (!unfinished && !confirm(`Remove "${d.filename}" from the list?`)) return;
    await call("remove", { id: d.id, deleteFile });
  }));

  return out;
}

function renderCategories() {
  const counts = new Map();
  for (const d of snapshot.downloads) {
    counts.set(d.category, (counts.get(d.category) || 0) + 1);
  }
  const host = $("nav-categories");
  const wanted = [...counts.keys()].sort();
  if (host.dataset.keys === wanted.join(",")) {
    for (const btn of host.children) {
      btn.querySelector(".count").textContent = counts.get(btn.dataset.value) || "";
    }
    return;
  }
  host.dataset.keys = wanted.join(",");
  host.replaceChildren(
    ...wanted.map((name) => {
      const b = document.createElement("button");
      b.className = "nav";
      b.dataset.filter = "category";
      b.dataset.value = name;
      b.innerHTML = `<span></span><span class="count"></span>`;
      b.firstChild.textContent = name;
      b.lastChild.textContent = counts.get(name) || "";
      b.addEventListener("click", () => selectFilter(b, "category", name));
      return b;
    })
  );
}

function selectFilter(button, kind, value) {
  document.querySelectorAll(".nav.active").forEach((n) => n.classList.remove("active"));
  button.classList.add("active");
  filter = { kind, value };
  render();
}

/* ------------------------------------------------------------------ *
 * Backend calls
 * ------------------------------------------------------------------ */

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e));
    throw e;
  }
}

let toastTimer = null;
function toast(message) {
  const el = $("toast");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 4200);
}

async function refresh() {
  snapshot = await invoke("get_snapshot");
  render();
}

/* ------------------------------------------------------------------ *
 * Dialogs
 * ------------------------------------------------------------------ */

function wireAddDialog() {
  const dlg = $("dlg-add");
  $("btn-add").addEventListener("click", async () => {
    // Pre-fill from the clipboard, the way IDM offers a caught URL.
    const clip = await invoke("read_clipboard_url").catch(() => null);
    $("add-url").value = clip || "";
    $("add-dir").value = "";
    $("add-ytdlp").checked = false;
    dlg.showModal();
  });

  $("add-browse").addEventListener("click", async () => {
    const dir = await call("pick_directory", { start: settings?.downloadDir });
    if (dir) $("add-dir").value = dir;
  });

  dlg.addEventListener("close", async () => {
    if (dlg.returnValue !== "ok") return;
    const url = $("add-url").value.trim();
    if (!url) return;
    await call("add_download", {
      url,
      directory: $("add-dir").value.trim() || null,
      useYtdlp: $("add-ytdlp").checked,
      formatId: null,
      filename: null,
    });
    toast("Queued");
  });
}

function wireBatchDialog() {
  const dlg = $("dlg-batch");
  let links = [];

  function draw() {
    const needle = $("batch-filter").value.trim().toLowerCase();
    const shown = needle
      ? links.filter((l) => l.url.toLowerCase().includes(needle))
      : links;
    $("batch-list").replaceChildren(
      ...shown.map((l) => {
        const row = document.createElement("label");
        row.className = "batch-item";
        row.innerHTML = `<input type="checkbox"><span class="url"></span>`;
        row.querySelector(".url").textContent = l.url;
        row.querySelector(".url").title = l.text || l.url;
        row.querySelector("input").checked = l.selected !== false;
        row.querySelector("input").addEventListener("change", (e) => {
          l.selected = e.target.checked;
        });
        return row;
      })
    );
  }

  listen("mdm://batch", (event) => {
    links = (event.payload.links || []).map((l) => ({ ...l, selected: true }));
    $("batch-title").textContent =
      `${links.length} links on ${event.payload.title || event.payload.pageUrl}`;
    $("batch-filter").value = "";
    draw();
    if (!dlg.open) dlg.showModal();
  });

  listen("mdm://media", (event) => {
    links = (event.payload.items || []).map((m) => ({
      url: m.url,
      // The note is what tells two hundred JPEGs apart; the MIME only says
      // they are all JPEGs.
      text: [m.kind, m.note || m.mime].filter(Boolean).join(" · "),
      selected: true,
    }));
    const kinds = new Set((event.payload.items || []).map((m) => m.kind));
    const what = kinds.size === 1 && kinds.has("image") ? "images" : "media";
    $("batch-title").textContent =
      `${links.length} ${what} on ${event.payload.title || "page"}`;
    $("batch-filter").value = "";
    draw();
    if (!dlg.open) dlg.showModal();
  });

  $("batch-filter").addEventListener("input", draw);
  $("batch-all").addEventListener("click", () => {
    links.forEach((l) => (l.selected = true));
    draw();
  });
  $("batch-none").addEventListener("click", () => {
    links.forEach((l) => (l.selected = false));
    draw();
  });

  dlg.addEventListener("close", async () => {
    if (dlg.returnValue !== "ok") return;
    const urls = links.filter((l) => l.selected !== false).map((l) => l.url);
    if (!urls.length) return;
    const failures = await call("add_many", { urls, directory: null });
    toast(
      failures.length
        ? `Queued ${urls.length - failures.length}, ${failures.length} failed`
        : `Queued ${urls.length}`
    );
  });
}

/* Monday-first, matching Queue::days where 0 = Monday. */
const DAY_NAMES = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

function minutesToTime(m) {
  if (m === null || m === undefined) return "";
  return String(Math.floor(m / 60)).padStart(2, "0") + ":" +
         String(m % 60).padStart(2, "0");
}

function timeToMinutes(value) {
  const m = /^(\d{1,2}):(\d{2})$/.exec((value || "").trim());
  if (!m) return null;
  const mins = Number(m[1]) * 60 + Number(m[2]);
  return mins >= 0 && mins < 1440 ? mins : null;
}

function wireSettingsDialog() {
  const dlg = $("dlg-settings");
  const KB = 1024;
  let mainQueue = null;

  // Day chips are static; build them once.
  $("q-days").replaceChildren(
    ...DAY_NAMES.map((name, i) => {
      const label = document.createElement("label");
      label.innerHTML = '<input type="checkbox">';
      label.append(name);
      label.querySelector("input").dataset.day = String(i);
      return label;
    })
  );

  $("btn-settings").addEventListener("click", async () => {
    settings = await invoke("get_settings");
    $("s-dir").value = settings.downloadDir;
    $("s-categorize").checked = settings.categorize;
    $("s-connections").value = settings.connections;
    $("s-split").value = settings.split;
    $("s-minsplit").value = settings.minSplitSize;
    $("s-concurrent").value = settings.maxConcurrent;
    $("s-maxspeed").value = Math.round(settings.maxSpeed / KB);
    $("s-maxspeed-each").value = Math.round(settings.maxSpeedPerDownload / KB);
    $("s-retries").value = settings.retryLimit;
    $("s-format").value = settings.ytdlpFormat;
    $("s-cookies").value = settings.ytdlpCookiesFrom || "";
    $("s-ytargs").value = (settings.ytdlpExtraArgs || []).join(" ");
    $("s-checksum").checked = settings.checksum;
    $("s-notify").checked = settings.notify;
    $("s-clipboard").checked = settings.clipboardWatch;

    const queues = await invoke("get_queues").catch(() => []);
    mainQueue = queues.find((q) => q.name === "main") || {
      name: "main", enabled: true, startMinute: null, stopMinute: null,
      days: [], maxConcurrent: settings.maxConcurrent,
    };
    // A window is "on" only when both ends are set; `enabled` alone governs
    // whether the queue runs at all, which is a different thing.
    const windowed = mainQueue.startMinute !== null && mainQueue.stopMinute !== null;
    $("q-enabled").checked = windowed;
    $("q-start").value = windowed ? minutesToTime(mainQueue.startMinute) : "23:00";
    $("q-stop").value = windowed ? minutesToTime(mainQueue.stopMinute) : "06:00";
    for (const input of $("q-days").querySelectorAll("input")) {
      input.checked = mainQueue.days.includes(Number(input.dataset.day));
    }
    dlg.showModal();
  });

  $("s-browse").addEventListener("click", async () => {
    const dir = await call("pick_directory", { start: $("s-dir").value });
    if (dir) $("s-dir").value = dir;
  });

  dlg.addEventListener("close", async () => {
    if (dlg.returnValue !== "ok" || !settings) return;
    const next = {
      ...settings,
      downloadDir: $("s-dir").value.trim() || settings.downloadDir,
      categorize: $("s-categorize").checked,
      connections: clamp(+$("s-connections").value, 1, 16),
      split: clamp(+$("s-split").value, 1, 32),
      minSplitSize: $("s-minsplit").value.trim() || "1M",
      maxConcurrent: clamp(+$("s-concurrent").value, 1, 20),
      maxSpeed: Math.max(0, +$("s-maxspeed").value) * KB,
      maxSpeedPerDownload: Math.max(0, +$("s-maxspeed-each").value) * KB,
      retryLimit: clamp(+$("s-retries").value, 0, 20),
      ytdlpFormat: $("s-format").value.trim() || settings.ytdlpFormat,
      ytdlpCookiesFrom: $("s-cookies").value.trim(),
      // Naive split: these are flags, never paths with spaces.
      ytdlpExtraArgs: $("s-ytargs").value.trim().split(/\s+/).filter(Boolean),
      checksum: $("s-checksum").checked,
      notify: $("s-notify").checked,
      clipboardWatch: $("s-clipboard").checked,
    };
    await call("set_settings", { settings: next });
    settings = next;

    const days = [...$("q-days").querySelectorAll("input")]
      .filter((i) => i.checked)
      .map((i) => Number(i.dataset.day));
    const on = $("q-enabled").checked;
    const start = timeToMinutes($("q-start").value);
    const stop = timeToMinutes($("q-stop").value);
    await call("save_queue", {
      queue: {
        ...mainQueue,
        name: "main",
        enabled: true,
        // Clearing both ends is how the engine is told "no window".
        startMinute: on ? start : null,
        stopMinute: on ? stop : null,
        days,
        maxConcurrent: next.maxConcurrent,
      },
    });

    toast("Settings saved");
  });
}

function clamp(n, lo, hi) {
  if (!Number.isFinite(n)) return lo;
  return Math.min(hi, Math.max(lo, Math.round(n)));
}

/* ------------------------------------------------------------------ *
 * Startup
 * ------------------------------------------------------------------ */

async function init() {
  document.querySelectorAll(".nav[data-filter]").forEach((btn) => {
    if (btn.dataset.filter === "category") return;
    btn.addEventListener("click", () => selectFilter(btn, btn.dataset.filter, null));
  });

  // The picker lives in its own window, the way IDM keeps a download separate
  // from the library, so this only has to ask for that window.
  $("btn-video").addEventListener("click", () => call("open_video_window"));

  $("btn-pause-all").addEventListener("click", () => call("pause_all"));
  $("btn-resume-all").addEventListener("click", () => call("resume_all"));
  $("btn-clear").addEventListener("click", async () => {
    const n = await call("clear_finished");
    toast(n ? `Cleared ${n}` : "Nothing to clear");
    await refresh();
  });

  wireAddDialog();
  wireBatchDialog();
  wireSettingsDialog();

  await listen("mdm://snapshot", (event) => {
    snapshot = event.payload;
    render();
  });

  settings = await invoke("get_settings").catch(() => null);
  await refresh();
}

init().catch((e) => toast(`Startup failed: ${e}`));
