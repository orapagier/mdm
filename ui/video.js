"use strict";

/**
 * The standalone download window.
 *
 * Two views in one window: pick a quality, then watch it download. The main
 * app is never raised — a grab from the browser should feel like IDM's little
 * download box, not like launching a program.
 *
 * `$`, `bytes`, `rate`, `duration` and `eta` come from format.js.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

let settings = null;

/* ------------------------------------------------------------------ *
 * Chrome
 * ------------------------------------------------------------------ */

let toastTimer = null;
function toast(message) {
  const el = $("toast");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 2600);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e));
    return null;
  }
}

/** Set an element's text and hide it entirely when there is nothing to say. */
function say(id, text, className) {
  const el = $(id);
  el.textContent = text || "";
  if (className !== undefined) el.className = className;
  el.hidden = !text;
}

function closeWindow() {
  getCurrentWindow().close();
}

/**
 * Ask for a taller window when what is laid out does not fit.
 *
 * How much of a window's height the desktop keeps for its own title bar is not
 * something the backend can know — it differs by desktop and theme, and here
 * it comes out of the size the backend asked for. So nothing guesses: the body
 * reports its own shortfall in its own coordinates and the window grows by
 * exactly that. Self-limiting, since a window that fits has no shortfall left
 * to report.
 */
async function fitWindow() {
  const body = document.querySelector(".pane-body");
  // A window manager applies a resize when it feels like it, and the answer
  // may not be the size that was asked for, so measure again afterwards and
  // keep asking until there is nothing left to ask for. Bounded, because a
  // desktop that refuses to grow the window must not be argued with for ever.
  for (let attempt = 0; attempt < 5; attempt++) {
    await new Promise(requestAnimationFrame);
    const short = body.scrollHeight - body.clientHeight;
    if (short <= 1) return;
    await invoke("fit_window", { grow: short }).catch(() => {});
    await new Promise((done) => setTimeout(done, 120));
  }
}

/* ------------------------------------------------------------------ *
 * Picking a format
 * ------------------------------------------------------------------ */

let info = null;
let kind = "both";
let chosen = null;
/** Other URLs this grab could resolve through, from the extension. */
let candidates = [];
/**
 * A media file to fetch outright, set only when no page would resolve.
 * It bypasses yt-dlp: there is nothing left to extract, and on a machine
 * without yt-dlp at all it is the only way the grab can still work.
 */
let direct = null;
/** Bumped on every reset, so a probe that lands late cannot paint over the
 *  page the window has since been pointed at. */
let probeSeq = 0;

/**
 * Which tabs a format belongs under.
 *
 * A video-only format is still a "video + audio" choice: picking one pairs it
 * with the best audio track and yt-dlp muxes the two. DASH sites — which is
 * now most of them, YouTube and Facebook included — serve *only* separate
 * streams, so grouping by what a format literally contains would leave the one
 * tab people actually want permanently empty.
 */
function tabsFor(f) {
  const hasVideo = f.vcodec !== "none";
  const hasAudio = f.acodec !== "none";
  if (hasVideo && hasAudio) return ["both"];
  if (hasVideo) return ["both", "video"];
  return ["audio"];
}

/** Is this row one that has to borrow an audio track from elsewhere? */
function needsAudio(f) {
  return kind === "both" && f.acodec === "none";
}

function heightOf(f) {
  const m = /(\d+)\s*[x×]\s*(\d+)/.exec(f.resolution || "");
  return m ? Number(m[2]) : 0;
}

function codecName(c) {
  if (!c || c === "none") return "";
  // yt-dlp reports full profile strings like "avc1.640028"; the family is the
  // only part worth showing.
  return c.split(".")[0].replace("mp4a", "aac").replace("avc1", "h264");
}

function describe(f) {
  const bits = [];
  if (f.ext) bits.push(f.ext);
  const v = codecName(f.vcodec);
  const a = codecName(f.acodec);
  if (v) bits.push(v);
  if (a) bits.push(a);
  // Say so rather than leave a silent-looking row in the video+audio tab.
  else if (needsAudio(f)) bits.push("+ best audio");
  if (f.note) bits.push(f.note);
  return bits.join(" · ");
}

function label(f) {
  if (f.vcodec === "none") {
    return f.tbr ? `${Math.round(f.tbr)} kbps` : "audio";
  }
  const h = heightOf(f);
  return h ? `${h}p` : f.resolution || f.formatId;
}

/**
 * The fallback covers a format that turns out to carry sound already, or a
 * page with no separate audio stream to pair with.
 */
function formatExpression(f) {
  return needsAudio(f) ? `${f.formatId}+bestaudio/${f.formatId}` : f.formatId;
}

/**
 * What the name has to say about the pick.
 *
 * Two picks from one page are two different files, and the extension does not
 * always tell them apart: opus audio and an AV1+opus mux of the same video are
 * both `.webm`. Landing them on one name is how an audio-only download ends up
 * reporting the video that is already saved.
 */
function kindSuffix() {
  return kind === "audio" ? "_audio" : kind === "video" ? "_video" : "";
}

/**
 * What the whole job will weigh, muxing aside.
 *
 * yt-dlp fetches the video stream and then the audio stream, so what the
 * downloader can report mid-job is only the stream in flight: a bar scaled to
 * that rescales downwards — 99% back to 67% — the moment the audio starts. The
 * picker already knows both sizes, so it says up front what the job comes to.
 */
function expectedSize(f) {
  if (!f.filesize) return null;
  if (!needsAudio(f)) return f.filesize;
  // yt-dlp pairs the pick with `bestaudio`, which is the top of the audio tab
  // in all but the odd case; being a little under is what the engine's floor
  // is there for.
  const audio = info.formats
    .filter((x) => tabsFor(x).includes("audio"))
    .sort((a, b) => (b.tbr || 0) - (a.tbr || 0))[0];
  return f.filesize + (audio?.filesize || 0);
}

function suggestedName() {
  if (!info) return "";
  // Mirrors yt-dlp's --restrict-filenames so what is shown is what lands.
  const stem = (info.title || "video")
    .replace(/[^\w\s.-]/g, "")
    .trim()
    .replace(/\s+/g, "_")
    .slice(0, 120);
  return stem + kindSuffix();
}

/** Set once the user types a name of their own, so switching tabs stops
 *  rewriting the field underneath them. */
let nameEdited = false;

function drawFormats() {
  const host = $("vid-formats");
  if (!info) return host.replaceChildren();

  const rows = info.formats
    .filter((f) => tabsFor(f).includes(kind))
    .sort((a, b) => (heightOf(b) - heightOf(a)) || ((b.tbr || 0) - (a.tbr || 0)));

  if (!rows.length) return host.replaceChildren();

  chosen = rows[0];
  host.replaceChildren(
    ...rows.map((f, i) => {
      const row = document.createElement("label");
      row.className = "fmt";
      row.innerHTML =
        '<input type="radio" name="fmt">' +
        '<span class="res"></span><span class="kind"></span><span class="meta"></span>';
      row.querySelector(".res").textContent = label(f);
      row.querySelector(".kind").textContent = describe(f);
      row.querySelector(".meta").textContent = f.filesize ? bytes(f.filesize) : "";
      const radio = row.querySelector("input");
      radio.checked = i === 0;
      radio.addEventListener("change", () => (chosen = f));
      return row;
    })
  );
}

function setTabs() {
  const present = new Set(info ? info.formats.flatMap(tabsFor) : []);
  for (const tab of $("vid-tabs").children) {
    tab.disabled = !present.has(tab.dataset.kind);
  }
  // Land on a tab that actually has something in it.
  if (!present.has(kind)) {
    kind = ["both", "video", "audio"].find((k) => present.has(k)) || "both";
  }
  for (const tab of $("vid-tabs").children) {
    tab.classList.toggle("active", tab.dataset.kind === kind);
  }
}

/** Wipe every trace of the previous page. */
function reset() {
  probeSeq++;
  info = null;
  chosen = null;
  direct = null;
  kind = "both";
  say("vid-status", "");
  $("vid-head").hidden = true;
  $("vid-tabs").hidden = true;
  $("vid-save").hidden = true;
  $("vid-formats").replaceChildren();
  $("vid-title").textContent = "";
  $("vid-sub").textContent = "";
  $("vid-name").value = "";
  nameEdited = false;
  const thumb = $("vid-thumb");
  thumb.hidden = true;
  thumb.removeAttribute("src");
}

/**
 * Paths that name one piece of media rather than a listing of them.
 *
 * A home page, a feed or a profile is a page *full* of videos. yt-dlp reads
 * one as a playlist and answers with entries and no formats — a perfectly
 * successful extraction of the wrong thing — so anything that looks like a
 * single video's own address is worth asking about first.
 */
/* Kept in step with the same table in the extension's video panel, which uses
 * it to *find* these links; here it only decides which to ask about first. */
const PERMALINK =
  /\/(?:watch|videos?|reels?|clips?|status|posts?|shorts|embed|p|v)\/|[?&]v=|\/(?:permalink|story|video)\.php|[?&]story_fbid=/i;

function looksSpecific(url) {
  try {
    const u = new URL(url);
    return PERMALINK.test(u.pathname + u.search);
  } catch {
    return false;
  }
}

/**
 * A manifest is a whole stream written down, and yt-dlp reads real formats out
 * of one. A plain media file has nothing to extract.
 */
const MANIFEST = /\.(?:m3u8|mpd)(?:[?#]|$)/i;

/**
 * Everything worth asking yt-dlp about, in order, without repeats.
 *
 * Pages only, plus the manifests among the media. That distinction matters:
 * these patterns are shapes of a *page* URL, and a CDN path is under no
 * obligation to avoid them — Facebook serves its video files from
 * `/o1/v/t2/...`, whose "/v/" made a raw .mp4 look like the most specific page
 * on offer. It was asked about first, failed the way a video file does when
 * something tries to extract a page from it, and that failure became the error
 * the window reported for the whole attempt. A media file is not a page and is
 * no longer treated as one; it is what `offerDirect` falls back to.
 *
 * Among pages, the typed URL leads — it is what the user asked for, or the
 * page the button was pressed on — but only against URLs of equal standing.
 * Press Download on a video in a feed and the page is the feed, while the
 * extension has read the post's own permalink out of the DOM; asking about the
 * feed first spends a slow extraction to learn what its shape already said.
 * Capped, because each one that fails costs another.
 */
function sources() {
  const pages = [
    $("vid-url").value.trim(),
    ...candidates.filter((c) => c.kind !== "media").map((c) => c.url),
  ];
  const streams = candidates
    .filter((c) => c.kind === "media" && MANIFEST.test(c.url))
    .map((c) => c.url);

  const seen = new Set();
  return [
    ...pages.filter(looksSpecific),
    ...pages.filter((u) => !looksSpecific(u)),
    ...streams,
  ]
    .filter((u) => /^https?:\/\//i.test(u) && !seen.has(u) && seen.add(u))
    .slice(0, 4);
}

async function probe(pageTitle) {
  if (!$("vid-url").value.trim()) return;
  reset();
  const seq = probeSeq;
  const urls = sources();

  let result = null;
  let failure = null;
  // A page that resolved to a *list* of videos rather than one. Remembered so
  // the failure can say so, since it is a different problem from a page that
  // could not be read at all.
  let listing = false;
  for (const [i, url] of urls.entries()) {
    // Name the page being read: extraction is slow enough that "which video is
    // this working on?" is a fair question. After the first, say which attempt
    // this is — the page URL was not the video and something else is being
    // tried, which is worth seeing rather than a status bar that just sits.
    say(
      "vid-status",
      i === 0
        ? pageTitle
          ? `Reading ${pageTitle}…`
          : "Reading page…"
        : `Nothing there. Trying another source (${i + 1} of ${urls.length})…`,
      "hint"
    );
    try {
      result = await invoke("probe_media", { url });
    } catch (e) {
      if (seq !== probeSeq) return;
      // The first failure is the one worth reporting: it is about the page the
      // user was actually on. The rest are guesses failing.
      failure ??= String(e);
      continue;
    }
    // The window was pointed at another page while this was in flight.
    if (seq !== probeSeq) return;
    // Answering is not the same as answering about a video. A feed or a home
    // page extracts cleanly into a playlist: entries, and no formats to choose
    // between. Taking that as the answer is what left the picker reading
    // "0 formats" for a video the next candidate could have resolved — so it
    // is not an answer, and the remaining sources still get their turn.
    if (!result.formats.length) {
      listing = true;
      result = null;
      continue;
    }
    // Show what actually answered, so a grab from a feed says which post it
    // resolved to rather than silently downloading something else.
    $("vid-url").value = url;
    break;
  }

  if (!result) {
    if (seq !== probeSeq) return;
    return offerDirect(
      failure ||
        (listing
          ? "Every page tried is a list of videos rather than one video."
          : "Nothing on that page could be read.")
    );
  }

  info = result;
  say("vid-status", "");
  $("vid-head").hidden = false;
  $("vid-title").textContent = info.title;
  $("vid-sub").textContent = [
    info.extractor,
    info.duration ? duration(Math.round(info.duration)) : null,
    `${info.formats.length} formats`,
  ].filter(Boolean).join(" · ");
  const thumb = $("vid-thumb");
  if (info.thumbnail) {
    thumb.src = info.thumbnail;
    thumb.hidden = false;
  } else {
    thumb.hidden = true;
  }

  setTabs();
  $("vid-tabs").hidden = false;
  drawFormats();
  $("vid-name").value = suggestedName();
  $("vid-save").hidden = false;
  fitWindow();
}

/**
 * Last resort: the file the page is using, fetched as a file.
 *
 * When no page in the chain resolves — an extractor that does not know the
 * site, or no yt-dlp on the machine at all — there may still be a plain media
 * URL among the candidates. There is no quality to choose, so the picker stays
 * empty and only the name and folder are offered.
 */
function offerDirect(reason) {
  const media = candidates.find(
    (c) => c.kind === "media" && /^https?:\/\//i.test(c.url)
  );
  if (!media) return say("vid-status", reason, "hint bad");

  direct = media.url;
  $("vid-url").value = direct;
  $("vid-name").value = nameFromUrl(direct);
  $("vid-save").hidden = false;
  say(
    "vid-status",
    `${reason} — the media file the page is using can still be downloaded as it is.`,
    "hint bad"
  );
  fitWindow();
}

/** Last path segment of a URL, for naming a file nothing else named. */
function nameFromUrl(url) {
  try {
    const seg = new URL(url).pathname.split("/").filter(Boolean).pop() || "";
    return decodeURIComponent(seg).replace(/[\\/]/g, "_") || "video";
  } catch {
    return "video";
  }
}

/* ------------------------------------------------------------------ *
 * Watching it download
 * ------------------------------------------------------------------ */

/** The row the strip is following, or null while nothing has been started. */
let watching = null;
/** A capture the browser handed over, waiting on Start. */
let pendingCapture = null;

async function start(paused) {
  // A capture already has a row — created paused so nothing is lost if this
  // window is closed — so starting it is a resume, not a new submission.
  if (pendingCapture !== null) {
    const id = pendingCapture;
    if (paused) return closeWindow(); // "Download Later" is what it already is
    pendingCapture = null;
    $("pick-actions").hidden = true;
    $("vid-save").hidden = true;
    watching = id;
    $("dl-strip").hidden = false;
    clearStrip();
    await call("start_capture", {
      id,
      directory: $("vid-dir").value.trim() || null,
      filename: $("vid-name").value.trim() || null,
    });
    const snap = await invoke("get_snapshot").catch(() => null);
    if (snap) render(snap);
    fitWindow();
    return;
  }

  const url = $("vid-url").value.trim();
  if (!url) return toast("No URL to download");

  const id = await call("add_download", {
    url,
    directory: $("vid-dir").value.trim() || null,
    // A direct media URL has nothing to extract; handing it to yt-dlp would
    // only put an extractor between aria2 and a file it can already fetch.
    useYtdlp: !direct,
    formatId: chosen ? formatExpression(chosen) : null,
    filename: $("vid-name").value.trim() || null,
    startPaused: paused,
    size: chosen ? expectedSize(chosen) : null,
  });
  if (id === null) return;

  watching = id;
  $("dl-strip").hidden = false;
  // Cancel meant "never mind"; now the picking is over and the button's job is
  // to get the window out of the way without touching the download.
  $("vid-close").textContent = "Close";
  clearStrip();

  // The snapshot arrives on its own schedule; ask for one now rather than
  // leaving the strip blank for most of a second.
  const snap = await invoke("get_snapshot").catch(() => null);
  if (snap) render(snap);
  fitWindow();
}

/** What to show in place of a speed when there is no speed to show. */
function stateWord(d) {
  switch (d.status) {
    case "paused": return "Paused";
    case "queued": return "Waiting";
    case "failed": return "Failed";
    case "complete": return "Done";
    // Active but nothing transferred yet: a streamed page has to be resolved
    // before a single byte can be asked for.
    default: return "Preparing";
  }
}

/** The strip's one reserved line. Never hidden, so the strip never changes
 *  height and the window never resizes to follow it. The line is ellipsised
 *  from the right, so anything that must survive being cut goes in `tip`. */
function note(text, tone, tip) {
  const el = $("dl-note");
  el.textContent = text;
  el.title = tip || text;
  el.className = tone ? `hint ${tone}` : "hint";
}

/**
 * Blank the strip for a download that has not reported yet.
 *
 * Without this the strip goes on describing the *previous* download — a full
 * bar, "Done", the folder it was saved to — for the moment before the first
 * snapshot arrives, which reads as the new pick having finished instantly.
 */
function clearStrip() {
  $("dl-bar").firstElementChild.style.width = "0%";
  $("dl-bar").className = "bar idle";
  $("dl-pct").textContent = "…";
  $("dl-bytes").textContent = "";
  $("dl-speed").textContent = "Preparing";
  $("dl-conns").textContent = "";
  $("dl-eta").textContent = "";
  $("dl-pause").textContent = "Pause";
  $("dl-pause").hidden = false;
  $("dl-cancel").hidden = false;
  $("dl-open").hidden = true;
  $("dl-folder").hidden = true;
  note("");
}

function render(snapshot) {
  if (watching === null) return;
  const d = (snapshot.downloads || []).find((x) => x.id === watching);
  if (!d) return;

  const pct = d.totalBytes > 0 ? d.completedBytes / d.totalBytes : 0;
  $("dl-bar").firstElementChild.style.width =
    `${Math.min(100, pct * 100).toFixed(1)}%`;
  $("dl-bar").className =
    "bar" +
    (d.status === "complete" ? " done" : d.status === "failed" ? " err" :
     d.status === "active" ? "" : " idle");

  // Always a percentage: the state word has its own slot, and saying "Done"
  // in both of them just reads as a stutter.
  $("dl-pct").textContent = d.totalBytes > 0 ? `${(pct * 100).toFixed(1)}%` : "…";
  $("dl-bytes").textContent =
    d.totalBytes > 0 ? `${bytes(d.completedBytes)} of ${bytes(d.totalBytes)}`
                     : bytes(d.completedBytes);
  // The state word sits where the speed would be, because that is the slot
  // whose meaning it replaces. Anything that changes the *number* of lines in
  // the strip would resize the window under the user's cursor, so nothing does.
  const moving = d.status === "active" && d.completedBytes > 0;
  $("dl-speed").textContent = moving ? rate(d.downloadSpeed) : stateWord(d);
  $("dl-conns").textContent =
    moving && d.connections > 0 ? `${d.connections} conn` : "";
  // Bare "1m 28s" next to a byte count reads as a duration, not a countdown.
  const remaining = eta(d);
  $("dl-eta").textContent = remaining === "—" ? "" : `${remaining} left`;

  // The reserved line, which only the two things too long for the stats row
  // ever use. It keeps its height whether or not it has anything to say.
  if (d.status === "failed") note(d.error || "Download failed.", "bad");
  // The name, not just the folder: a second copy of something already saved is
  // numbered, and this is where that becomes visible.
  else if (d.status === "complete") {
    note(`Saved as ${d.filename}`, "ok", `${d.directory}/${d.filename}`);
  } else note("");

  const done = d.status === "complete";
  const finished = done || d.status === "failed";
  $("dl-pause").textContent = d.status === "paused" ? "Resume" : "Pause";
  $("dl-pause").hidden = finished;
  $("dl-cancel").hidden = finished;
  $("dl-open").hidden = !done;
  $("dl-folder").hidden = !done;
  $("dl-open").dataset.path = `${d.directory}/${d.filename}`;
  $("dl-folder").dataset.path = `${d.directory}/${d.filename}`;
  $("dl-pause").dataset.status = d.status;
}

/* ------------------------------------------------------------------ *
 * Wiring
 * ------------------------------------------------------------------ */

/** A URL the user typed stands alone; the last page's readings are not it. */
function probeTyped() {
  candidates = [];
  probe();
}

$("vid-probe").addEventListener("click", probeTyped);
$("vid-url").addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    probeTyped();
  }
});

$("vid-tabs").addEventListener("click", (e) => {
  const tab = e.target.closest(".tab");
  if (!tab || tab.disabled) return;
  kind = tab.dataset.kind;
  setTabs();
  drawFormats();
  if (!nameEdited) $("vid-name").value = suggestedName();
});

$("vid-name").addEventListener("input", () => (nameEdited = true));

$("vid-browse").addEventListener("click", async () => {
  const dir = await call("pick_directory", { start: settings?.downloadDir });
  if (dir) $("vid-dir").value = dir;
});

$("vid-close").addEventListener("click", async () => {
  // A capture the user declined should not be left sitting paused for ever.
  if (pendingCapture !== null) {
    const id = pendingCapture;
    pendingCapture = null;
    await call("remove", { id, deleteFile: true });
  }
  closeWindow();
});
$("vid-start").addEventListener("click", () => start(false));
$("vid-later").addEventListener("click", () => start(true));

$("dl-pause").addEventListener("click", (e) => {
  const paused = e.currentTarget.dataset.status === "paused";
  call(paused ? "resume" : "pause", { id: watching });
});
$("dl-cancel").addEventListener("click", async () => {
  const id = watching;
  // Stop following it first: the row disappears from the next snapshot, and a
  // strip still pointed at it would freeze on its last numbers.
  watching = null;
  $("dl-strip").hidden = true;
  await call("remove", { id, deleteFile: true });
  // With no picker to fall back to there is nothing left in the window.
  if ($("pick-actions").hidden) closeWindow();
});
$("dl-close").addEventListener("click", closeWindow);
$("dl-open").addEventListener("click", (e) =>
  call("open_path", { path: e.currentTarget.dataset.path, reveal: false }));
$("dl-folder").addEventListener("click", (e) =>
  call("open_path", { path: e.currentTarget.dataset.path, reveal: true }));

listen("mdm://snapshot", (event) => render(event.payload));

/* ------------------------------------------------------------------ *
 * Startup
 * ------------------------------------------------------------------ */

/** Requests already acted on, so the same grab is not handled twice. */
let lastRequestId = 0;

/** Show the picker, or hide it for a download with nothing left to decide. */
function setKind(kind) {
  const file = kind === "file";
  // A capture has no page to fetch and no formats to choose between, so the
  // whole picking apparatus goes and its head takes the URL row's place.
  $("url-row").hidden = file;
  $("vid-formats").hidden = file;
  $("file-head").hidden = !file;
  $("vid-head").hidden = true;
  $("vid-tabs").hidden = true;
  $("vid-save").hidden = true;
  // A page can be fetched and started at any time, so the picker keeps its
  // buttons throughout. A capture only gets them once it has been offered.
  $("pick-actions").hidden = file;
  $("dl-close").hidden = !file;
}

async function handleRequest(request) {
  if (!request || request.id <= lastRequestId) return;
  lastRequestId = request.id;

  // A new request is a new question, so the strip for the last one goes: the
  // user asked about another download, not for a report on the previous one.
  watching = null;
  pendingCapture = null;
  $("dl-strip").hidden = true;
  $("vid-close").textContent = "Cancel";
  reset();
  $("vid-dir").value = "";
  setKind(request.kind);

  // A capture: recorded but not running. Offer it the way the picker offers a
  // video — a folder, a name, and a button that starts it.
  if (request.kind === "file") {
    $("file-name").textContent = request.title || request.url;
    $("file-name").title = request.title || request.url;
    say("file-sub", request.url, "hint");
    $("file-sub").title = request.url;
    pendingCapture = request.downloadId;
    $("vid-name").value = request.title || "";
    $("vid-dir").value = request.directory || "";
    $("vid-save").hidden = false;
    $("pick-actions").hidden = false;
    fitWindow();
    return;
  }

  candidates = request.candidates || [];
  const url = request.url || (await invoke("read_clipboard_url").catch(() => null)) || "";
  $("vid-url").value = url;
  if (url) await probe(request.title || "");
  else $("vid-url").focus();
}

(async () => {
  settings = await invoke("get_settings").catch(() => null);
  if (!(await invoke("ytdlp_available").catch(() => false))) {
    // The install command is asked for rather than assumed: apt, dnf and
    // pacman machines all end up here and only one of the three lines works.
    const how = await invoke("install_hint", { package: "yt-dlp" })
      .catch(() => "your package manager (the package is called yt-dlp)");
    say("vid-status", `yt-dlp is not installed — run: ${how}`, "hint bad");
  }
  // The window is usually opened *for* a request, which was set before the
  // page existed and so could not have been delivered as an event.
  await handleRequest(await invoke("take_pending_video").catch(() => null));
})();

listen("mdm://videoPage", (event) => handleRequest(event.payload));
