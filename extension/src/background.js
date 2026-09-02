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

browser.downloads.onCreated.addListener(async (item) => {
  if (!cfg.enabled) return;
  if (!/^https?:\/\//i.test(item.url)) return; // blob:/data: cannot be re-fetched
  if (wasRecentlyCaptured(item.url)) return;
  if (state.bypass.has(item.url)) {
    state.bypass.delete(item.url);
    return;
  }
  if (!Native.isAvailable()) return;

  const host = hostOf(item.url);
  if (host && cfg.blockedSites.some((s) => hostMatches(host, s))) return;

  const filename = sanitizeFilename(
    (item.filename || "").split("/").pop() || filenameFromUrl(item.url)
  );
  const ext = extensionOf(filename);
  if (cfg.blockedExtensions.includes(ext)) return;
  if (item.fileSize > 0 && item.fileSize < cfg.minSize) return;

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
    // Cancel first, then erase, so the partial file is released before the
    // history row disappears.
    await browser.downloads.cancel(item.id).catch(() => {});
    await browser.downloads.erase({ id: item.id }).catch(() => {});
  } catch (e) {
    console.warn("[mdm] backstop handoff failed:", e.message);
  }
});

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
  }
});

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
async function grabVideo(msg) {
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
  if (msg.videoSrc && !isStreamingSite(host)) {
    await sendSimple(msg.videoSrc, { pageUrl }, { url: pageUrl });
    return { ok: true, mode: "direct" };
  }

  // Await the app's answer rather than firing and forgetting: a post only
  // proves the port is open, so an app that rejected the message (an old
  // build, a closed window) would still have been reported as success.
  try {
    const reply = await Native.request(
      { type: "videoPage", url: pageUrl, title: msg.title || "" },
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

browser.runtime.onMessage.addListener(async (msg) => {
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
      return grabVideo(msg);
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
