"use strict";

/* ------------------------------------------------------------------ *
 * Settings
 * ------------------------------------------------------------------ */

const DEFAULTS = {
  enabled: true,
  minSize: 1024 * 1024, // 1 MiB
  blockedSites: [],
  blockedExtensions: [],
  askBeforeDownload: false,
  sniffMedia: true,
  videoButton: true,
  captureImages: true,
  captureBlobs: true,
  handoffTimeoutMs: 1500,
};

let cfg = { ...DEFAULTS };

async function loadSettings() {
  const stored = await browser.storage.local.get("settings");
  cfg = { ...DEFAULTS, ...(stored.settings || {}) };
  return cfg;
}

browser.storage.onChanged.addListener((changes, area) => {
  if (area === "local" && changes.settings) {
    cfg = { ...DEFAULTS, ...(changes.settings.newValue || {}) };
    updateBadge();
  }
});

/* ------------------------------------------------------------------ *
 * Shared state
 * ------------------------------------------------------------------ */

const state = {
  /** URLs the user explicitly handed back to Firefox; consumed once. */
  bypass: new Set(),
};

/** requestId -> request context, populated at send time, read at response time. */
const requests = new Map();
const REQUEST_TTL_MS = 60_000;

/** Guards against the webRequest net and the downloads net both firing. */
const recentlyCaptured = new Map(); // url -> timestamp
const CAPTURE_DEDUPE_MS = 15_000;

/** tabId -> Map<url, mediaInfo> discovered by the sniffer. */
const tabMedia = new Map();

function markCaptured(url) {
  recentlyCaptured.set(url, Date.now());
}

function wasRecentlyCaptured(url) {
  const t = recentlyCaptured.get(url);
  if (t === undefined) return false;
  if (Date.now() - t > CAPTURE_DEDUPE_MS) {
    recentlyCaptured.delete(url);
    return false;
  }
  return true;
}

/** Periodic sweep; cheap and keeps the maps from growing without bound. */
setInterval(() => {
  const now = Date.now();
  for (const [id, r] of requests)
    if (now - r.at > REQUEST_TTL_MS) requests.delete(id);
  for (const [url, t] of recentlyCaptured)
    if (now - t > CAPTURE_DEDUPE_MS) recentlyCaptured.delete(url);
}, 30_000);

/* ------------------------------------------------------------------ *
 * Header forwarding
 * ------------------------------------------------------------------ */

/**
 * Headers that must not be replayed by the downloader. Hop-by-hop headers are
 * connection-scoped, and Range/Accept-Encoding/Host must be set by aria2 itself
 * — forwarding Range in particular would truncate every segmented download.
 */
const STRIP_HEADERS = new Set([
  "host","connection","keep-alive","proxy-authorization","proxy-connection",
  "te","trailer","transfer-encoding","upgrade","content-length","range",
  "if-range","accept-encoding","if-modified-since","if-none-match",
  "sec-fetch-dest","sec-fetch-mode","sec-fetch-site","sec-fetch-user",
  "upgrade-insecure-requests","priority",
]);

function forwardableHeaders(list) {
  const out = [];
  for (const h of list || []) {
    if (STRIP_HEADERS.has(h.name.toLowerCase())) continue;
    if (h.value === undefined) continue;
    out.push({ name: h.name, value: h.value });
  }
  return out;
}

/* ------------------------------------------------------------------ *
 * Net 1 — webRequest
 * ------------------------------------------------------------------ */

browser.webRequest.onBeforeSendHeaders.addListener(
  (details) => {
    requests.set(details.requestId, {
      method: details.method,
      url: details.url,
      type: details.type,
      tabId: details.tabId,
      cookieStoreId: details.cookieStoreId,
      documentUrl: details.documentUrl || details.originUrl || "",
      headers: forwardableHeaders(details.requestHeaders),
      at: Date.now(),
    });
    return {};
  },
  // Not "blocking": this listener only records headers, and making every
  // navigation wait on the event page would cost latency for no gain.
  { urls: ["<all_urls>"], types: [...CAPTURABLE_TYPES] },
  ["requestHeaders"]
);

browser.webRequest.onHeadersReceived.addListener(
  (details) => {
    const req = requests.get(details.requestId) || {
      method: details.method,
      url: details.url,
      type: details.type,
      tabId: details.tabId,
      headers: [],
      at: Date.now(),
    };

    const headers = headerMap(details.responseHeaders);
    const verdict = classify(
      req,
      { statusCode: details.statusCode, headers, url: details.url },
      cfg,
      state
    );
    if (!verdict.capture) return {};

    const url = details.url;
    if (wasRecentlyCaptured(url)) return {};

    // Fail open: if the daemon is not reachable, let Firefox download it
    // normally rather than stalling or losing the file.
    if (!Native.isAvailable()) {
      Native.connect();
      return {};
    }

    const job = buildJob(req, details, headers, verdict.reason);

    // Firefox — unlike Chrome — lets a blocking listener return a Promise, so
    // the request is held open until the daemon confirms it took the job.
    // Only then is it cancelled, which makes double-downloads impossible.
    return Native.request({ type: "download", job }, cfg.handoffTimeoutMs)
      .then((reply) => {
        if (reply && reply.accepted) {
          markCaptured(url);
          return { cancel: true };
        }
        return {};
      })
      .catch((e) => {
        console.warn("[mdm] handoff failed, leaving to Firefox:", e.message);
        return {};
      });
  },
  { urls: ["<all_urls>"], types: [...CAPTURABLE_TYPES] },
  ["responseHeaders", "blocking"]
);

function cleanup(details) {
  requests.delete(details.requestId);
}
browser.webRequest.onCompleted.addListener(cleanup, { urls: ["<all_urls>"] });
browser.webRequest.onErrorOccurred.addListener(cleanup, { urls: ["<all_urls>"] });

function buildJob(req, details, headers, reason) {
  const filename = deriveFilename(details.url, headers);
  return {
    url: details.url,
    // Other servers holding the same bytes, if this one said so. Only the
    // webRequest net sees response headers, so only it can find them.
    mirrors: mirrorsOf(headers, details.url),
    filename,
    size: sizeOf(headers),
    mime: mimeOf(headers),
    headers: req.headers,
    referrer: req.documentUrl || "",
    cookieStoreId: req.cookieStoreId || "",
    tabId: req.tabId ?? -1,
    reason,
    source: "webRequest",
  };
}

/* ------------------------------------------------------------------ *
 * Net 2 — downloads API backstop
 *
 * Catches whatever slipped past net 1: "Save Link As", downloads started by
 * page script, and responses whose headers gave no usable signal until Firefox
 * had already classified them.
 * ------------------------------------------------------------------ */

/**
 * Bring the native port up, and give it a moment to answer.
 *
 * `isAvailable()` is false for the whole window between the browser starting
 * and the host's first connection, and again after a host dies while its
 * reconnect is pending. Downloads that landed in either window were dropped on
 * the floor — the one thing this listener exists to prevent. Waiting costs
 * nothing that matters: the browser has not begun transferring yet.
 */
async function ensureNative(timeoutMs = 2000) {
  if (Native.isAvailable()) return true;
  Native.connect();
  const deadline = Date.now() + timeoutMs;
  while (!Native.isAvailable() && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 50));
  }
  return Native.isAvailable();
}

browser.downloads.onCreated.addListener(async (item) => {
  if (!cfg.enabled) return;
  if (wasRecentlyCaptured(item.url)) return;
  if (state.bypass.has(item.url)) {
    state.bypass.delete(item.url);
    return;
  }
  if (!(await ensureNative())) return;

  // A blob has no server to re-fetch it from; the page is the only source.
  if (/^blob:/i.test(item.url)) return captureBlob(item);
  // A data: URL cannot be re-fetched either — but it does not need to be. It
  // *is* the bytes, spelled out in the URL, so it is handed over the way a
  // blob's are rather than left behind as unfetchable.
  if (/^data:/i.test(item.url)) return captureDataUrl(item);
  if (!/^https?:\/\//i.test(item.url)) return;

  const host = hostOf(item.url);
  if (host && cfg.blockedSites.some((s) => hostMatches(host, s))) return;

  const filename = sanitizeFilename(
    (item.filename || "").split("/").pop() || filenameFromUrl(item.url)
  );
  const ext = extensionOf(filename);
  if (cfg.blockedExtensions.includes(ext)) return;
  if (looksLikeImage(item.mime || "", ext) && !cfg.captureImages) return;

  // No size floor here, deliberately, and none by file type either.
  //
  // That threshold belongs to the *automatic* net, where a small response is
  // far more likely to be an API reply than a file and guessing wrong costs
  // the user a download they never asked for. Nothing reaches this listener by
  // guesswork: the browser has already decided every one of these is a
  // download. A 40 KB photo saved out of a chat is exactly as much a download
  // as a 4 GB image, and skipping it only meant MDM captured some of what the
  // browser downloaded rather than all of it.

  const job = {
    url: item.url,
    filename: filename || "download",
    size: item.fileSize > 0 ? item.fileSize : (item.totalBytes > 0 ? item.totalBytes : -1),
    mime: item.mime || "",
    headers: await headersForUrl(item.url, item.referrer, item.cookieStoreId),
    referrer: item.referrer || "",
    cookieStoreId: item.cookieStoreId || "",
    tabId: -1,
    reason: "downloads.onCreated",
    source: "downloads",
  };

  try {
    const reply = await Native.request({ type: "download", job }, 4000);
    if (!reply || !reply.accepted) return;
    markCaptured(item.url);
    await takeOverDownload(item.id);
  } catch (e) {
    console.warn("[mdm] backstop handoff failed:", e.message);
  }
});

/**
 * Leave Firefox with no trace of a download MDM has taken over.
 *
 * Cancel first, so a transfer still running lets go of its partial file. But a
 * small file — and every blob — can be finished before the hand-off completes,
 * and a cancelled-too-late download leaves a second copy on disk under
 * Firefox's own name, which is exactly the duplicate this is here to prevent:
 * hence removeFile as well, which only bites when it did finish.
 */
async function takeOverDownload(id) {
  await browser.downloads.cancel(id).catch(() => {});
  await browser.downloads.removeFile(id).catch(() => {});
  await browser.downloads.erase({ id }).catch(() => {});
}

/* ------------------------------------------------------------------ *
 * Net 3 — downloads the page built in memory
 *
 * Chat and gallery sites increasingly fetch a file with script, wrap it in a
 * Blob and click an <a download> at it. What reaches the downloads API is then
 * `blob:https://site/<uuid>`, a handle that means nothing outside the page
 * that made it — no downloader can re-fetch it, which is why these were the
 * one class of download that always fell back to Firefox. So MDM asks the page
 * itself for the bytes and hands those over instead of a URL.
 * ------------------------------------------------------------------ */

/**
 * The most a blob may weigh before it is left to Firefox.
 *
 * The bytes travel base64-encoded through native messaging, so this is roughly
 * a third again in flight. Blob downloads are photos, exports and generated
 * documents; anything genuinely large is served by a server, and a server can
 * be fetched from properly.
 */
const MAX_BLOB_BYTES = 24 * 1024 * 1024;

async function captureBlob(item) {
  if (!cfg.captureBlobs) return;

  // "blob:https://site/uuid" — the origin inside is the page that owns it, and
  // the only context that can read it back.
  const origin = originOfBlob(item.url);
  if (!origin) return;
  const host = hostOf(origin);
  if (host && cfg.blockedSites.some((s) => hostMatches(host, s))) return;

  let filename = sanitizeFilename((item.filename || "").split("/").pop());
  const ext = extensionOf(filename);
  if (ext && cfg.blockedExtensions.includes(ext)) return;
  if (looksLikeImage(item.mime || "", ext) && !cfg.captureImages) return;

  const blob = await readBlobFromPage(item.url, origin);
  if (!blob) return;

  // Firefox has not always settled on a target path by the time the download
  // is announced, and a blob URL has no path to fall back on — so the type the
  // page put on the blob is the last thing left to name it by.
  filename = filename || "download" + extensionForMime(blob.mime || item.mime);
  if (cfg.blockedExtensions.includes(extensionOf(filename))) return;

  // The bytes are in hand, so the browser's copy is now the duplicate.
  markCaptured(item.url);
  await takeOverDownload(item.id);

  const job = {
    url: item.url,
    filename,
    size: blob.size,
    mime: blob.mime || item.mime || "",
    data: blob.data,
    headers: [],
    referrer: origin,
    cookieStoreId: "",
    tabId: -1,
    reason: "blob download",
    source: "blob",
  };

  try {
    // A generous timeout: the app has megabytes to decode and write, and
    // failing here would throw away bytes nothing can fetch again.
    const reply = await Native.request({ type: "download", job }, 20000);
    if (!reply || !reply.accepted) {
      notifyPlain("MDM could not save " + job.filename);
    }
  } catch (e) {
    console.warn("[mdm] blob handoff failed:", e.message);
    notifyPlain("MDM could not save " + job.filename + ": " + e.message);
  }
}

/**
 * A download the page spelled out in the URL itself.
 *
 * `data:` is what a page reaches for when it has something to hand over and no
 * server behind it — a canvas export, a generated document, a small attachment
 * decoded in script. There is no request to re-issue, so as with a blob it is
 * the bytes that are handed over rather than an address.
 */
async function captureDataUrl(item) {
  if (!cfg.captureBlobs) return;

  const payload = decodeDataUrl(item.url);
  if (!payload) return;
  if (payload.size > MAX_BLOB_BYTES) return;

  const mime = payload.mime || item.mime || "";
  let filename = sanitizeFilename((item.filename || "").split("/").pop());
  filename = filename || "download" + extensionForMime(mime);
  if (cfg.blockedExtensions.includes(extensionOf(filename))) return;
  if (looksLikeImage(mime, extensionOf(filename)) && !cfg.captureImages) return;

  // The bytes are in hand, so the browser's copy is now the duplicate.
  markCaptured(item.url);
  await takeOverDownload(item.id);

  const job = {
    // Not the data: URL itself — that *is* the file, and megabytes of base64
    // would be written into the database and shown as the download's address.
    // The browser's own id for this download is short, unique, and enough to
    // keep two saves of different bytes from being read as a repeat of one.
    url: `data:${mime || "application/octet-stream"};download=${item.id}`,
    filename,
    size: payload.size,
    mime,
    data: payload.data,
    headers: [],
    referrer: item.referrer || "",
    cookieStoreId: "",
    tabId: -1,
    reason: "data url download",
    source: "data",
  };

  try {
    const reply = await Native.request({ type: "download", job }, 20000);
    if (!reply || !reply.accepted) notifyPlain("MDM could not save " + filename);
  } catch (e) {
    console.warn("[mdm] data url handoff failed:", e.message);
    notifyPlain("MDM could not save " + filename + ": " + e.message);
  }
}

/** Enough of a mapping to name a file the browser did not name. */
const MIME_EXTENSION = {
  "image/jpeg": ".jpg", "image/png": ".png", "image/gif": ".gif",
  "image/webp": ".webp", "image/avif": ".avif", "image/bmp": ".bmp",
  "image/tiff": ".tif", "image/heic": ".heic", "image/svg+xml": ".svg",
  "application/pdf": ".pdf", "application/zip": ".zip", "application/json": ".json",
  "text/plain": ".txt", "text/csv": ".csv", "text/html": ".html",
  "video/mp4": ".mp4", "video/webm": ".webm", "video/quicktime": ".mov",
  "audio/mpeg": ".mp3", "audio/ogg": ".ogg", "audio/wav": ".wav",
};

function extensionForMime(mime) {
  return MIME_EXTENSION[(mime || "").split(";", 1)[0].trim().toLowerCase()] || "";
}

function originOfBlob(url) {
  try {
    return new URL(url.replace(/^blob:/i, "")).origin;
  } catch {
    return "";
  }
}

/**
 * Ask the document that owns the blob to read it back for us.
 *
 * Only a document of the blob's own origin can resolve the handle, so the
 * search is over *frames*, not tabs. That distinction is the whole point: a
 * chat, a mail client or an embedded viewer runs its interface in a frame, and
 * a blob it creates belongs to that frame's origin while the tab around it
 * still reads as the site it is embedded in. Matching tabs by address bar
 * looked straight past those documents — so a photo saved from a conversation
 * rendered in a frame fell through to the browser while the same photo from
 * one rendered in the page was captured.
 *
 * Frames that do not hold the blob answer nothing, so the reply comes from
 * whichever one does; the active tab goes first because that is where a
 * download nearly always starts.
 */
async function readBlobFromPage(url, origin) {
  let tabs = [];
  try {
    tabs = await browser.tabs.query({});
  } catch (e) {
    console.warn("[mdm] tab lookup failed:", e.message);
    return null;
  }
  tabs.sort((a, b) => Number(b.active) - Number(a.active));

  // Collected before any of them is asked, so the budget below is spent on
  // documents actually on the origin rather than on whichever tabs came first.
  const targets = [];
  for (const tab of tabs) {
    for (const frameId of await framesOn(tab, origin)) {
      targets.push({ tabId: tab.id, frameId });
    }
    if (targets.length >= MAX_FRAMES_ASKED) break;
  }

  let refusal = null;
  for (const { tabId, frameId } of targets.slice(0, MAX_FRAMES_ASKED)) {
    try {
      const reply = await browser.tabs.sendMessage(
        tabId,
        { type: "mdm-read-blob", url, limit: MAX_BLOB_BYTES },
        { frameId }
      );
      if (reply && reply.ok) return reply;
      // A frame on the right origin that does not hold this blob fails its
      // fetch, which is not an answer about the blob — keep asking.
      if (reply && reply.error) refusal = reply.error;
    } catch {
      /* no content script in that frame */
    }
  }
  if (refusal) console.warn("[mdm] blob could not be read:", refusal);
  return null;
}

/**
 * A ceiling, because every open tab is now considered and an ad-heavy page can
 * carry dozens of frames on its own.
 */
const MAX_FRAMES_ASKED = 24;

/**
 * Every frame of a tab that shares the blob's origin.
 *
 * Asked one at a time rather than broadcast to the whole tab: a page can hold
 * several same-origin frames, only one of them made the blob, and a broadcast
 * answers with whichever replies first — which is the frame that failed
 * fastest, not the one holding the bytes.
 */
async function framesOn(tab, origin) {
  try {
    const frames = await browser.webNavigation.getAllFrames({ tabId: tab.id });
    return frames.filter((f) => originOfUrl(f.url) === origin).map((f) => f.frameId);
  } catch {
    // Without a frame list there is only the top document to guess at, and
    // guessing is only worth it for a tab that is on the origin itself —
    // every other tab would spend a slot in the budget to be told nothing.
    return originOfUrl(tab.url) === origin ? [0] : [];
  }
}

function originOfUrl(url) {
  try {
    return new URL(url).origin;
  } catch {
    return "";
  }
}

/**
 * The downloads API gives no request headers, so rebuild the essentials from
 * the cookie jar. Container tabs keep separate jars, hence storeId.
 */
async function headersForUrl(url, referrer, storeId) {
  const headers = [];
  try {
    const query = { url };
    if (storeId) query.storeId = storeId;
    const cookies = await browser.cookies.getAll(query);
    if (cookies.length) {
      headers.push({
        name: "Cookie",
        value: cookies.map((c) => `${c.name}=${c.value}`).join("; "),
      });
    }
  } catch (e) {
    console.warn("[mdm] cookie lookup failed:", e.message);
  }
  if (referrer) headers.push({ name: "Referer", value: referrer });
  headers.push({ name: "User-Agent", value: navigator.userAgent });
  return headers;
}

/* ------------------------------------------------------------------ *
 * Media sniffer (non-blocking)
 * ------------------------------------------------------------------ */

const STREAM_HINT = /\.(m3u8|mpd)(\?|$)/i;

browser.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (!cfg.sniffMedia || details.tabId < 0) return;
    const headers = headerMap(details.responseHeaders);
    const mime = mimeOf(headers);
    const isStream =
      STREAM_HINT.test(details.url) ||
      mime === "application/vnd.apple.mpegurl" ||
      mime === "application/x-mpegurl" ||
      mime === "application/dash+xml";
    const isMedia = mime.startsWith("video/") || mime.startsWith("audio/");
    if (!isStream && !isMedia) return;

    let m = tabMedia.get(details.tabId);
    if (!m) tabMedia.set(details.tabId, (m = new Map()));
    if (m.has(details.url)) return;
    if (m.size > 50) return; // fragmented streams emit endlessly
    m.set(details.url, {
      url: details.url,
      mime,
      size: sizeOf(headers),
      kind: isStream ? "stream" : "media",
      at: Date.now(),
    });
    updateBadge();
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "main_frame", "sub_frame"] },
  ["responseHeaders"]
);

browser.tabs.onRemoved.addListener((tabId) => tabMedia.delete(tabId));
browser.tabs.onUpdated.addListener((tabId, change) => {
  if (change.url) {
    tabMedia.delete(tabId);
    updateBadge();
  }
});

/* ------------------------------------------------------------------ *
 * Badge & notifications
 *
 * A handed-over download is announced by MDM's own window, which shows where
 * it is going and how it is getting on. A toast saying only that it left the
 * browser added nothing, and answered to a second setting the app's own
 * "Desktop notifications" switch had no say over.
 * ------------------------------------------------------------------ */

async function updateBadge() {
  try {
    const tabs = await browser.tabs.query({ active: true, currentWindow: true });
    const tabId = tabs[0]?.id;
    const n = tabId !== undefined ? (tabMedia.get(tabId)?.size ?? 0) : 0;
    await browser.action.setBadgeText({ text: n > 0 ? String(n) : "" });
    await browser.action.setBadgeBackgroundColor({ color: "#2f6feb" });
  } catch {
    /* action API unavailable during startup */
  }
}
browser.tabs.onActivated.addListener(updateBadge);

/* ------------------------------------------------------------------ *
 * Context menus
 * ------------------------------------------------------------------ */

const MENUS = [
  { id: "mdm-link", title: "Download with MDM", contexts: ["link"] },
  { id: "mdm-media", title: "Download this media with MDM", contexts: ["video", "audio", "image"] },
  { id: "mdm-page-links", title: "Download all links on this page…", contexts: ["page"] },
  { id: "mdm-page-media", title: "Grab media from this page…", contexts: ["page"] },
  { id: "mdm-page-images", title: "Grab images from this page…", contexts: ["page", "image"] },
  { id: "mdm-selection", title: "Download selected links…", contexts: ["selection"] },
];

browser.runtime.onInstalled.addListener(() => {
  browser.contextMenus.removeAll().then(() => {
    for (const m of MENUS) browser.contextMenus.create(m);
  });
});

browser.contextMenus.onClicked.addListener(async (info, tab) => {
  switch (info.menuItemId) {
    case "mdm-link":
      return sendSimple(info.linkUrl, info, tab);
    case "mdm-media":
      return sendSimple(info.srcUrl || info.linkUrl, info, tab);
    case "mdm-page-links":
      return grabFromPage(tab, "links");
    case "mdm-selection":
      return grabFromPage(tab, "selection");
    case "mdm-page-media": {
      const found = [...(tabMedia.get(tab.id)?.values() ?? [])];
      if (!found.length) return notifyPlain("No media detected on this page yet.");
      return Native.post({ type: "media", items: found, pageUrl: tab.url, title: tab.title });
    }
    case "mdm-page-images":
      return grabImages(tab);
  }
});

/**
 * Offer every picture on the page.
 *
 * Images are read out of the live DOM rather than off the sniffer: a page has
 * hundreds of them and recording each as it loads would drown the badge, while
 * the DOM already knows which ones the page actually put on screen — and how
 * big each turned out to be, which is what tells a photograph from an icon.
 */
async function grabImages(tab) {
  try {
    const results = await browser.scripting.executeScript({
      target: { tabId: tab.id, allFrames: true },
      func: collectImages,
      args: [MIN_IMAGE_PIXELS],
    });
    // One result per frame, and a gallery is as often in an iframe as not.
    // Two frames can hold the same picture, so dedupe across them.
    const seen = new Set();
    const items = results
      .flatMap((r) => r?.result ?? [])
      .filter((i) => !seen.has(i.url) && seen.add(i.url));
    if (!items.length) return notifyPlain("No images found on this page.");
    Native.post({ type: "media", items, pageUrl: tab.url, title: tab.title });
  } catch (e) {
    notifyPlain("Could not read the page: " + e.message);
  }
}

/** Below this an image is furniture — an avatar, an icon, a spacer. */
const MIN_IMAGE_PIXELS = 200 * 200;

/* Runs in the page. Kept dependency-free — it is serialised across. */
function collectImages(minPixels) {
  const seen = new Set();
  const out = [];
  const add = (url, note) => {
    if (!/^https?:\/\//i.test(url) || seen.has(url)) return;
    seen.add(url);
    out.push({ url, mime: "", size: -1, kind: "image", note: note || "" });
  };

  for (const img of document.querySelectorAll("img")) {
    // naturalWidth is the file's own size, not the box it was squeezed into,
    // so a thumbnail shown small but stored large is still worth offering.
    const w = img.naturalWidth;
    const h = img.naturalHeight;
    if (w && h && w * h < minPixels) continue;
    // currentSrc is what the browser actually chose out of a srcset.
    add(img.currentSrc || img.src, w && h ? `${w}×${h}` : "");
  }

  // Galleries link the full-size copy from the thumbnail; that is the one
  // worth having, and it is never in the DOM as an <img>.
  for (const a of document.querySelectorAll("a[href]")) {
    if (/\.(jpe?g|png|gif|webp|avif|bmp|tiff?|heic|jxl)(\?|#|$)/i.test(a.href))
      add(a.href, "linked");
  }

  return out;
}

async function sendSimple(url, info, tab) {
  if (!url || !/^https?:\/\//i.test(url)) return;
  const job = {
    url,
    filename: filenameFromUrl(url) || "download",
    size: -1,
    mime: "",
    headers: await headersForUrl(url, info.pageUrl || tab?.url, tab?.cookieStoreId),
    referrer: info.pageUrl || tab?.url || "",
    cookieStoreId: tab?.cookieStoreId || "",
    tabId: tab?.id ?? -1,
    reason: "context menu",
    source: "menu",
  };
  markCaptured(url);
  const ok = Native.post({ type: "download", job });
  if (!ok) notifyPlain("MDM is not running.");
}

/** Ask the page for its links; the picker itself lives in the app. */
async function grabFromPage(tab, mode) {
  try {
    const [result] = await browser.scripting.executeScript({
      target: { tabId: tab.id },
      func: collectLinks,
      args: [mode],
    });
    const links = result?.result ?? [];
    if (!links.length) return notifyPlain("No links found.");
    Native.post({
      type: "batch",
      links,
      pageUrl: tab.url,
      title: tab.title,
      referrer: tab.url,
    });
  } catch (e) {
    notifyPlain("Could not read the page: " + e.message);
  }
}

/* Runs in the page. Kept dependency-free — it is serialised across. */
function collectLinks(mode) {
  const root =
    mode === "selection" && window.getSelection().rangeCount
      ? window.getSelection().getRangeAt(0).cloneContents()
      : document;
  const seen = new Set();
  const out = [];
  for (const a of root.querySelectorAll("a[href]")) {
    const href = a.href;
    if (!/^https?:\/\//i.test(href) || seen.has(href)) continue;
    seen.add(href);
    out.push({ url: href, text: (a.textContent || "").trim().slice(0, 200) });
  }
  return out;
}

function notifyPlain(message) {
  browser.notifications
    .create({
      type: "basic",
      iconUrl: browser.runtime.getURL("icons/mdm-64.png"),
      title: "My Download Manager",
      message,
    })
    .catch(() => {});
}

/**
 * Handle the on-page download button.
 *
 * A streaming page is handed to yt-dlp as a *page* URL, because its media is
 * delivered as separate range-fetched video and audio streams behind expiring
 * signatures — grabbing the <video> src would yield a silent, truncated file.
 * A page serving a plain file gets that file downloaded directly.
 */
async function grabVideo(msg, tabId) {
  const pageUrl = msg.pageUrl || "";
  const host = hostOf(pageUrl);
  if (host && cfg.blockedSites.some((s) => hostMatches(host, s))) {
    return { ok: false, error: "site excluded" };
  }
  if (!Native.isAvailable()) {
    Native.connect();
    return { ok: false, error: "MDM is not running" };
  }

  // A direct file URL only counts when the page is not a known player; on a
  // streaming site a same-origin mp4 is usually an ad or a preview clip.
  //
  // `videoSrc` is only there once the player has loaded something, so fall
  // back to whatever file the page declares — a <source>, an og:video — which
  // is readable the moment the page is, played or not.
  const file =
    msg.videoSrc ||
    (msg.candidates || []).find((c) => c.kind === "media")?.url ||
    "";
  if (file && !isStreamingSite(host)) {
    await sendSimple(file, { pageUrl }, { url: pageUrl });
    return { ok: true, mode: "direct" };
  }

  // Await the app's answer rather than firing and forgetting: a post only
  // proves the port is open, so an app that rejected the message (an old
  // build, a closed window) would still have been reported as success.
  try {
    const reply = await Native.request(
      {
        type: "videoPage",
        url: pageUrl,
        title: msg.title || "",
        // Where else this video might be resolvable from. The page URL is
        // often not the video's own — a feed, a timeline, an infinite scroll —
        // and it is the one thing yt-dlp cannot work around.
        candidates: await videoCandidates(msg, tabId),
      },
      5000
    );
    return reply && reply.accepted
      ? { ok: true, mode: "ytdlp" }
      : { ok: false, error: (reply && reply.error) || "MDM rejected the request" };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

/**
 * Every URL that could stand for the video under the button, best first.
 *
 * A page is only sometimes the video: click Download on a feed and the address
 * bar still says the feed, which yt-dlp can make nothing of. The page itself
 * knows better — it has the post's own permalink in the DOM and the file in
 * its metadata — and the sniffer has watched whatever the player fetched. All
 * of it is offered, and the app takes the first that resolves. This is what
 * lets a video be grabbed without playing it first: none of it needs the
 * player to have started.
 */
async function videoCandidates(msg, tabId) {
  const out = [];
  const seen = new Set([msg.pageUrl || ""]);
  const add = (url, kind, mime = "") => {
    if (!/^https?:\/\//i.test(url || "") || seen.has(url)) return;
    seen.add(url);
    out.push({ url, kind, mime, headers: [], referrer: "" });
  };

  const found = msg.candidates || [];
  // Pages before files, deliberately. A page is what yt-dlp turns into a
  // choice of qualities; the file a page declares is usually the one it can
  // spare — a preview, or the lowest rung of a ladder. So a file is what this
  // falls back to, never what it reaches for first.
  for (const c of found.filter((c) => c.kind !== "media")) add(c.url, "page");
  for (const c of found.filter((c) => c.kind === "media")) add(c.url, "media");

  // What the player has actually fetched in this tab. A manifest is the whole
  // stream and outranks a fragment, which is one slice of it.
  const sniffed = [...(tabMedia.get(tabId)?.values() ?? [])];
  for (const m of sniffed.filter((m) => m.kind === "stream")) add(m.url, "media", m.mime);
  for (const m of sniffed.filter((m) => m.kind !== "stream")) add(m.url, "media", m.mime);

  // A ceiling, because each one the app tries costs an extraction.
  const kept = out.slice(0, 6);

  // A media candidate may be downloaded straight from the window, by aria2,
  // outside the browser — so it has to travel with what the browser would have
  // sent for it. Facebook signs its video links per session and answers a bare
  // request with 403, which arrived as a download that simply would not start.
  // Pages need none of this: yt-dlp is given the browser's cookie jar already.
  await Promise.all(
    kept
      .filter((c) => c.kind === "media")
      .map(async (c) => {
        c.referrer = msg.pageUrl || "";
        c.headers = await headersForUrl(c.url, c.referrer, "");
      })
  );
  return kept;
}

/**
 * Sites whose pages are players rather than files. Kept in step with the
 * engine's own list; when in doubt yt-dlp is asked, since it fails gracefully.
 */
const STREAMING_HOSTS = [
  "youtube.com", "youtu.be", "vimeo.com", "dailymotion.com", "twitch.tv",
  "twitter.com", "x.com", "reddit.com", "tiktok.com", "instagram.com",
  "facebook.com", "soundcloud.com", "bandcamp.com", "bilibili.com",
  "odysee.com", "rumble.com", "nebula.tv", "ted.com",
];

function isStreamingSite(host) {
  return !!host && STREAMING_HOSTS.some((h) => hostMatches(host, h));
}

/* ------------------------------------------------------------------ *
 * Popup / options messaging
 * ------------------------------------------------------------------ */

browser.runtime.onMessage.addListener(async (msg, sender) => {
  switch (msg.type) {
    case "getState":
      return {
        cfg,
        connected: Native.isAvailable(),
        media: [...(tabMedia.get(msg.tabId)?.values() ?? [])],
      };
    case "setSettings":
      await browser.storage.local.set({ settings: { ...cfg, ...msg.settings } });
      return { ok: true };
    case "bypassOnce":
      state.bypass.add(msg.url);
      return { ok: true };
    case "download":
      return sendSimple(msg.url, { pageUrl: msg.referrer }, { id: msg.tabId });
    case "grabVideo":
      return grabVideo(msg, sender.tab?.id ?? msg.tabId ?? -1);
    case "openApp":
      Native.post({ type: "focus" });
      return { ok: true };
    default:
      return undefined;
  }
});

Native.onMessage((msg) => {
  if (msg && msg.type === "hostState") updateBadge();
});

/* ------------------------------------------------------------------ *
 * Startup
 * ------------------------------------------------------------------ */

loadSettings().then(() => {
  Native.connect();
  updateBadge();
});
