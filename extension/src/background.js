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

/**
 * Resolves once `cfg` holds the user's settings rather than the defaults.
 *
 * Started here, at the top of the script, and awaited by every handler that
 * reads `cfg`. Chromium stops the service worker after about thirty seconds
 * idle and starts it again for the next event, so this file is re-run far more
 * often than Firefox's event page re-runs it — and a handler that fires in the
 * gap before the read completes would see `enabled: true`, an empty blocklist
 * and every other default. Capturing a download the user had switched off is
 * not a race worth taking: the wait is one storage read, once per worker.
 */
let settingsLoaded = false;
const settingsReady = loadSettings().then((loaded) => {
  settingsLoaded = true;
  return loaded;
});

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

/**
 * Guards against two nets both firing for one download.
 *
 * A note left by whichever net got there first, for the other to find a moment
 * later: same URL, same click. It says nothing about the *file* — only that
 * this particular hand-off has already been made.
 *
 * Which is why a claim is taken by its reader rather than left to expire, and
 * why the window is seconds rather than tens of them. The two nets fire within
 * milliseconds of each other, so a note that outlives that gap is not
 * protecting anything — it is lying in wait for the user to ask for the same
 * file a second time, and answering "already captured" about a download that
 * has not happened yet. Left standing for fifteen seconds it did exactly that:
 * the first click went to MDM, the second read MDM's own note and went to the
 * browser, and the two appeared to take turns.
 */
const recentlyCaptured = new Map(); // url -> timestamp
const CAPTURE_DEDUPE_MS = 5_000;

/** tabId -> Map<url, mediaInfo> discovered by the sniffer. */
const tabMedia = new Map();

function markCaptured(url) {
  recentlyCaptured.set(url, Date.now());
}

/** Take the claim on this URL, if there is one. True if there was. */
function claimCapture(url) {
  const t = recentlyCaptured.get(url);
  if (t === undefined) return false;
  recentlyCaptured.delete(url);
  return Date.now() - t <= CAPTURE_DEDUPE_MS;
}

/** Periodic sweep; cheap and keeps the maps from growing without bound.
 *
 * On Chromium the service worker is stopped when idle and every one of these
 * maps goes with it, which does the same job more thoroughly — so a sweep that
 * never fires there costs nothing. */
setInterval(() => {
  const now = Date.now();
  for (const [id, r] of requests)
    if (now - r.at > REQUEST_TTL_MS) requests.delete(id);
  for (const [url, t] of recentlyCaptured)
    if (now - t > CAPTURE_DEDUPE_MS) recentlyCaptured.delete(url);
  for (const [url, seen] of seenResponses)
    if (now - seen.at > RESPONSE_TTL_MS) seenResponses.delete(url);
}, 30_000);

/* ------------------------------------------------------------------ *
 * Header forwarding
 * ------------------------------------------------------------------ */

/**
 * Headers that must not be replayed by the downloader. Hop-by-hop headers are
 * connection-scoped, and Range/Accept-Encoding/Host must be set by the fetcher
 * itself — forwarding Range in particular would truncate every segmented
 * download.
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
 *
 * Firefox lets an extension hold a response open while it decides what to do
 * with it. Chromium removed that in Manifest V3: `webRequestBlocking` is now
 * only for force-installed enterprise extensions, so on Chrome and Edge this
 * net can watch but not intercept, and net 2 does the catching.
 *
 * Watching is still worth doing there. Response headers are the only place a
 * download's true size, type and mirrors are stated, and the downloads API
 * reports none of them — so what this sees is remembered for net 2 to use,
 * and a Chromium capture ends up describing the file as well as a Firefox one
 * does.
 * ------------------------------------------------------------------ */

/** Does this browser let a listener hold a response open and cancel it? */
const CAN_BLOCK = (browser.runtime.getManifest().permissions || []).includes(
  "webRequestBlocking"
);

/**
 * Chromium hides `Cookie`, `Referer` and friends from webRequest unless the
 * listener asks for "extraHeaders"; Firefox has no such option and rejects the
 * value outright. Asked for only where it exists.
 */
const EXTRA_HEADERS = browser.webRequest.OnBeforeSendHeadersOptions?.EXTRA_HEADERS
  ? ["extraHeaders"]
  : [];

/**
 * The request types this browser will accept in a webRequest filter.
 *
 * `object_subrequest` is Firefox's alone, and Chromium does not merely ignore
 * an unknown type — it throws out the whole `addListener` call, which on a
 * service worker takes the entire extension down with it: no capture, no
 * native connection, and a popup stuck on "Checking…".
 *
 * Filtered against the browser's own enum rather than a hardcoded allowlist,
 * so a type either browser adds later needs no change here. `CAPTURABLE_TYPES`
 * itself is left whole: it is also what `classify` matches against, and the
 * classification is right on both browsers even where the filter cannot say so.
 */
const FILTER_TYPES = (() => {
  const wanted = [...CAPTURABLE_TYPES];
  const known = browser.webRequest.ResourceType;
  if (known) {
    const allowed = new Set(Object.values(known));
    return wanted.filter((type) => allowed.has(type));
  }
  // No enum to ask. Falling back to the whole set is what caused the crash in
  // the first place, so the fallback drops the one type that is Firefox's
  // alone — and only on the build that cannot block, which is the Chromium
  // one, because these two manifests are ours and only Firefox's asks for
  // `webRequestBlocking`.
  return CAN_BLOCK ? wanted : wanted.filter((type) => type !== "object_subrequest");
})();

/** url -> the response headers net 1 saw, for net 2 to describe the file with. */
const seenResponses = new Map();
const RESPONSE_TTL_MS = 60_000;

function rememberResponse(url, headers) {
  seenResponses.set(url, { headers, at: Date.now() });
}

function recallResponse(url) {
  const seen = seenResponses.get(url);
  if (!seen) return null;
  if (Date.now() - seen.at > RESPONSE_TTL_MS) {
    seenResponses.delete(url);
    return null;
  }
  return seen.headers;
}

browser.webRequest.onBeforeSendHeaders.addListener(
  (details) => {
    requests.set(details.requestId, {
      method: details.method,
      url: details.url,
      type: details.type,
      tabId: details.tabId,
      cookieStoreId: details.cookieStoreId,
      documentUrl: details.documentUrl || details.originUrl || details.initiator || "",
      headers: forwardableHeaders(details.requestHeaders),
      at: Date.now(),
    });
    return {};
  },
  // Not "blocking": this listener only records headers, and making every
  // navigation wait on the event page would cost latency for no gain.
  { urls: ["<all_urls>"], types: FILTER_TYPES },
  ["requestHeaders", ...EXTRA_HEADERS]
);

browser.webRequest.onHeadersReceived.addListener(
  (details) => {
    // Deliberately a flag rather than an `await`: this listener is *blocking*
    // on Firefox, and returning a promise from it would hold every response on
    // the browser for as long as the promise takes. Skipping the handful of
    // requests that arrive before the first storage read is finished costs at
    // most one capture, and net 2 catches that one anyway.
    if (!settingsLoaded) return {};
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
    if (claimCapture(url)) return {};

    // Everything above is the same judgement on either browser. Only the
    // acting on it differs.
    if (!CAN_BLOCK) {
      // Nothing can be cancelled here, so say nothing to the daemon yet —
      // acting now would download the file twice, once by each of us. Leave
      // what was learned where net 2 will find it when the browser announces
      // the same URL as a download a moment later.
      rememberResponse(url, headers);
      return {};
    }

    // Fail open: if the daemon is not reachable, let the browser download it
    // normally rather than stalling or losing the file.
    if (!Native.isAvailable()) {
      Native.connect();
      return {};
    }

    const job = buildJob(req, details, headers, verdict.reason);

    // Firefox lets a blocking listener return a Promise, so the request is
    // held open until the daemon confirms it took the job. Only then is it
    // cancelled, which makes double-downloads impossible.
    return Native.request({ type: "download", job }, cfg.handoffTimeoutMs)
      .then((reply) => {
        if (reply && reply.accepted) {
          markCaptured(url);
          return { cancel: true };
        }
        return {};
      })
      .catch((e) => {
        console.warn("[mdm] handoff failed, leaving it to the browser:", e.message);
        return {};
      });
  },
  { urls: ["<all_urls>"], types: FILTER_TYPES },
  CAN_BLOCK ? ["responseHeaders", "blocking"] : ["responseHeaders"]
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

/**
 * Does this browser hold a download open while extensions name it?
 *
 * `onDeterminingFilename` is Chromium's, and it is the one blocking hook
 * Manifest V3 left standing: a listener that returns true keeps the download
 * suspended for up to fifteen seconds while it decides, which is most of what
 * `webRequestBlocking` used to buy on Firefox.
 *
 * Where it matters is *when* it fires. Chromium settles a download's target in
 * order — generate a name, **notify extensions**, reserve the path, then
 * prompt the user — so a download cancelled from this listener never reaches
 * the "Save as" dialog. Caught in `onCreated` instead, one step later, the
 * browser has already put its own file picker on screen by the time the
 * hand-off finishes, and the user answers two dialogs for one click: the
 * browser's, and then MDM's.
 */
const HOLDS_FILENAME = !!browser.downloads.onDeterminingFilename;

/**
 * Decide a plain http(s) download and hand it to MDM.
 *
 * Everything the two nets below must agree on lives here; they differ only in
 * how the browser's own copy is kept still while this runs. Returns whether
 * MDM took the job, and the caller clears the browser's copy only if it did.
 */
async function offerWebDownload(item) {
  await settingsReady;
  if (!cfg.enabled) return false;
  // Another net's claim — the context menu, or net 1 where net 1 can act.
  // Never this one's own: see the markCaptured at the end of this function.
  if (claimCapture(item.url)) return false;
  if (state.bypass.has(item.url)) {
    state.bypass.delete(item.url);
    return false;
  }

  const host = hostOf(item.url);
  if (host && cfg.blockedSites.some((s) => hostMatches(host, s))) return false;

  const filename = sanitizeFilename(
    (item.filename || "").split("/").pop() || filenameFromUrl(item.url)
  );
  const ext = extensionOf(filename);
  if (cfg.blockedExtensions.includes(ext)) return false;
  if (looksLikeImage(item.mime || "", ext) && !cfg.captureImages) return false;

  // No size floor here, deliberately, and none by file type either.
  //
  // That threshold belongs to the *automatic* net, where a small response is
  // far more likely to be an API reply than a file and guessing wrong costs
  // the user a download they never asked for. Nothing reaches these listeners
  // by guesswork: the browser has already decided every one of these is a
  // download. A 40 KB photo saved out of a chat is exactly as much a download
  // as a 4 GB image, and skipping it only meant MDM captured some of what the
  // browser downloaded rather than all of it.

  if (!(await ensureNative())) return false;

  // What net 1 saw of the response, if it saw it. On Firefox this is usually
  // empty — net 1 captured the file itself and this never runs — but on
  // Chromium it is the only sight anyone gets of the response headers, and it
  // is what turns "some bytes, type unknown" into a properly described job.
  const seen = recallResponse(item.url);
  const size =
    item.fileSize > 0
      ? item.fileSize
      : item.totalBytes > 0
        ? item.totalBytes
        : seen
          ? sizeOf(seen)
          : -1;

  const job = {
    url: item.url,
    filename: filename || "download",
    size,
    mime: item.mime || (seen ? mimeOf(seen) : ""),
    mirrors: seen ? mirrorsOf(seen, item.url) : [],
    headers: await headersForUrl(item.url, item.referrer, item.cookieStoreId),
    referrer: item.referrer || "",
    cookieStoreId: item.cookieStoreId || "",
    tabId: -1,
    reason: "downloads API",
    source: "downloads",
  };

  const reply = await Native.request({ type: "download", job }, 4000);
  if (!reply || !reply.accepted) return false;

  // Claimed only where net 1 can act on it, because net 1 is its only reader:
  // on Firefox the response headers can arrive after the download item has
  // been created, so net 1 has still to be told this one is spoken for. Where
  // net 1 can only watch, nothing will ever read this claim except this same
  // line on the user's next download of the same file — which would then be
  // handed straight back to the browser.
  if (CAN_BLOCK) markCaptured(item.url);
  return true;
}

/* ------------------------------------------------------------------ *
 * Net 2a — while the browser is still holding the download
 * ------------------------------------------------------------------ */

if (HOLDS_FILENAME) {
  browser.downloads.onDeterminingFilename.addListener((item, suggest) => {
    // blob: and data: carry their bytes rather than an address, and are dealt
    // with in net 2b where the page that owns them can be read back. Returning
    // false leaves the browser's own naming of them untouched.
    if (!/^https?:\/\//i.test(item.url)) return false;

    (async () => {
      let taken = false;
      try {
        taken = await offerWebDownload(item);
      } catch (e) {
        console.warn("[mdm] handoff failed, leaving it to the browser:", e.message);
      }
      if (taken) await takeOverDownload(item.id);

      // Released either way, and last. Until this call the browser has neither
      // asked where to put the file nor written a byte of it to disk, so the
      // cancel above is the whole of its copy — there is no partial file to
      // clear up and no dialog to dismiss, which is the difference between
      // this net and the backstop below. On a download that was taken the item
      // is already erased and this throws; that is the ordinary ending here,
      // not a failure.
      try {
        suggest();
      } catch (e) {
        /* already cancelled and erased */
      }
    })();

    // "suggest will be called asynchronously" — the fifteen seconds Chromium
    // allows for that are well clear of ensureNative (2s) plus the hand-off
    // (4s), which is the longest this can take before it gives up.
    return true;
  });
}

/* ------------------------------------------------------------------ *
 * Net 2b — the downloads backstop
 * ------------------------------------------------------------------ */

browser.downloads.onCreated.addListener(async (item) => {
  // Whether this is a network transfer at all, decided without awaiting
  // anything. blob: and data: downloads are the page's own bytes, already in
  // memory; there is no second transfer to race with, and captureBlob and
  // captureDataUrl clear the browser's copy themselves.
  const isWeb = /^https?:\/\//i.test(item.url);

  // Where the browser is about to hold the download for us, that is where it
  // is caught: net 2a decides it before the file picker opens, and deciding it
  // here as well would be two hand-offs for one click. A web download that
  // never reaches that listener is left to the browser, which is the same
  // failing open every other path in this file does.
  if (isWeb && HOLDS_FILENAME) return;

  // Nothing above this line awaits, and that is the whole point.
  //
  // On a browser that will not hold the download, the transfer is already
  // running by the time this fires, so every await before the browser's copy
  // is stopped is bytes written to disk. `await settingsReady` used to come
  // first, and it is not a cheap await here: this event is usually what
  // *starts* the service worker, so it is a storage read on a cold worker,
  // after importScripts has pulled in five files. For a small file that is the
  // entire download, and the hand-off then arrives to find nothing left to
  // cancel -- which is the double download this is here to prevent.
  //
  // So the copy is stopped first and asked about afterwards. Paused rather
  // than cancelled because the decision has not been made yet and a pause is
  // free to undo; the finally below resumes every path that does not end with
  // MDM taking the download, so a daemon that is missing, busy or unwilling
  // leaves the browser to finish exactly as it would have.
  const held = isWeb ? await holdBrowserCopy(item.id) : false;
  let taken = false;

  try {
    await settingsReady;
    if (!cfg.enabled) return;
    if (claimCapture(item.url)) return;
    if (state.bypass.has(item.url)) {
      state.bypass.delete(item.url);
      return;
    }

    // A blob has no server to re-fetch it from; the page is the only source.
    if (/^blob:/i.test(item.url)) {
      if (!(await ensureNative())) return;
      return await captureBlob(item);
    }
    // A data: URL cannot be re-fetched either — but it does not need to be. It
    // *is* the bytes, spelled out in the URL, so it is handed over the way a
    // blob's are rather than left behind as unfetchable.
    if (/^data:/i.test(item.url)) {
      if (!(await ensureNative())) return;
      return await captureDataUrl(item);
    }
    if (!isWeb) return;

    if (!(await offerWebDownload(item))) return;
    taken = true;
    await takeOverDownload(item.id);
  } catch (e) {
    console.warn("[mdm] backstop handoff failed:", e.message);
  } finally {
    // Every way out of that block other than MDM having taken the download —
    // a return, a throw, a daemon that said no — arrives here. held is false
    // for anything that was never paused, and releasing that is a no-op.
    if (!taken) await releaseBrowserCopy(item.id, held);
  }
});

/**
 * Hold the browser's own transfer still while the hand-off is decided.
 *
 * Whether there is anything to release afterwards is the return value. A
 * download the browser will not pause — one that has already finished, most
 * often — is not a failure here: it only means that if MDM does take the job,
 * clearing the browser's copy falls to removeFile rather than to cancel.
 */
async function holdBrowserCopy(id) {
  try {
    await browser.downloads.pause(id);
    return true;
  } catch (e) {
    return false;
  }
}

/**
 * Let the browser get on with a download MDM did not take.
 *
 * Failing open is the rule everywhere in this file: a daemon that is missing,
 * busy or unwilling has to cost the user nothing. This is what makes pausing
 * before the answer is known a safe thing to do.
 */
async function releaseBrowserCopy(id, held) {
  if (!held) return;
  try {
    await browser.downloads.resume(id);
  } catch (e) {
    console.warn("[mdm] could not resume the browser's copy:", e.message);
  }
}

/**
 * Leave the browser with no trace of a download MDM has taken over.
 *
 * Cancel first, so a transfer still running — or held by holdBrowserCopy —
 * lets go of its partial file. But a small file, and every blob, can be
 * finished before the hand-off completes, and a cancelled-too-late download
 * leaves a second copy on disk under the browser's own name, which is exactly
 * the duplicate this is here to prevent: hence removeFile as well, which only
 * bites when it did finish.
 *
 * Each of those fails for an ordinary reason as well as an alarming one —
 * cancel on a download that already finished, removeFile on one that did not —
 * so neither failing is worth reporting by itself. What is worth reporting is
 * a copy that survived both, and that is checked rather than assumed. Every
 * failure here used to be swallowed, which is what let a duplicate sit on disk
 * with nothing anywhere to say why.
 */
async function takeOverDownload(id) {
  try {
    await browser.downloads.cancel(id);
  } catch (e) {
    // Expected when it had already finished; removeFile is what covers that.
  }
  try {
    await browser.downloads.removeFile(id);
  } catch (e) {
    // Expected when the cancel got there first: there is no file to remove.
  }

  let survived = false;
  try {
    const [left] = await browser.downloads.search({ id });
    survived = !!(left && left.exists);
  } catch (e) {
    // Not searchable — there is nothing further to be learned about it.
  }

  // Erased last: the entry has to still exist for the check above to see it.
  try {
    await browser.downloads.erase({ id });
  } catch (e) {
    console.warn("[mdm] could not erase the browser's download entry:", e.message);
  }

  if (survived) {
    console.warn(
      "[mdm] the browser's copy survived the hand-off and is still on disk — " +
        "MDM has downloaded it as well"
    );
  }
  return !survived;
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

/** How many media responses to remember per tab, newest kept. */
const MAX_SNIFFED = 50;

browser.webRequest.onHeadersReceived.addListener(
  async (details) => {
    await settingsReady;
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
    // Fragmented streams emit endlessly, so there has to be a ceiling — but it
    // drops the *oldest* rather than refusing the newest. Refusing went deaf:
    // scroll far enough down a feed and the fifty slots are full of videos
    // already gone by, the one on screen is never recorded, and the button has
    // nothing to offer for it.
    while (m.size >= MAX_SNIFFED) m.delete(m.keys().next().value);
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

/**
 * Forget a tab's media when the tab actually goes somewhere else.
 *
 * A real navigation replaces the document, and everything the old page was
 * playing is gone with it. A *rewritten address* is not that: an infinite feed
 * pushes the current post's URL into the address bar as you scroll, without
 * loading anything. Clearing on `tabs.onUpdated` could not tell the two apart,
 * so every scroll of TikTok's home feed wiped what the player had just
 * fetched, and pressing Download found nothing to offer — while /explore,
 * which does not rewrite the address, worked perfectly. `onCommitted` fires
 * only for a genuinely new document; the SPA case is `onHistoryStateUpdated`,
 * which is deliberately not listened for.
 */
browser.webNavigation.onCommitted.addListener(({ tabId, frameId }) => {
  if (frameId !== 0) return;
  tabMedia.delete(tabId);
  updateBadge();
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
    // Only the sniffer knows this; a context menu click carries no type.
    mime: info.mime || "",
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
  await settingsReady;
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
        // What the player element says it has loaded. The window compares a
        // resolved page against it, which is how a page that reads cleanly and
        // is about the post next door gets caught.
        seconds: Number(msg.seconds) || 0,
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
  // What the markup around the player said this post is. The content script
  // already reads it to find the post's permalink and its record in the page
  // state; here it is what tells one post's streams from the next one's.
  const ids = (msg.ids || []).map(String);
  /**
   * Which post a file belongs to, as far as anything here can tell.
   *
   * "this" is the post the button was pressed on, "other" is some neighbour
   * in the feed, and "" is nothing known — which is not the same as "other",
   * and is the answer on most sites and every file the site does not describe.
   * Carried on the candidate as well as used here: the window is where a file
   * is fallen back to when no page resolves, and it needs to know whether the
   * one it is about to offer is the video the user was looking at.
   *
   * Only facts about the file answer this — the element that has it open, or
   * the post the file's own address names. What the page *said* is a reading
   * of the page, which is the thing a feed misleads, so a file the markup
   * turned up stays unclaimed however well it reads.
   */
  const postOf = (videoId, origin) =>
    origin === "player"
      ? "this"
      : videoId && ids.length
        ? ids.includes(videoId)
          ? "this"
          : "other"
        : "";
  const add = (rawUrl, kind, mime = "", stream = false, origin = "") => {
    // A slice of a stream is not the video, however complete the download of
    // it looks afterwards; ask for the file the slice was cut from.
    const url = kind === "media" ? withoutByteRange(rawUrl || "") : rawUrl;
    if (!/^https?:\/\//i.test(url || "") || seen.has(url)) return;
    seen.add(url);
    const facts = kind === "media" ? urlFacts(url) : { videoId: "", audioOnly: false };
    out.push({
      url,
      kind,
      mime,
      stream,
      origin,
      videoId: facts.videoId,
      post: postOf(facts.videoId, origin),
      // The site's own word for it first; failing that, what the server
      // called the response. Only one site in the pair is honest at a time —
      // Facebook labels its audio track `video/mp4` and says `_audio` in the
      // URL, and an ordinary site does the reverse.
      audioOnly: facts.audioOnly || /^audio\//i.test(mime || ""),
      // Half of a DASH pair. Complete as a download and useless as a video.
      partial: facts.partial || facts.audioOnly,
      headers: [],
      referrer: "",
    });
  };

  const found = msg.candidates || [];
  // Pages before files, deliberately. A page is what yt-dlp turns into a
  // choice of qualities; the file a page declares is usually the one it can
  // spare — a preview, or the lowest rung of a ladder. So a file is what this
  // falls back to, never what it reaches for first.
  for (const c of found.filter((c) => c.kind !== "media")) add(c.url, "page");
  for (const c of found.filter((c) => c.kind === "media")) {
    add(c.url, "media", "", false, c.origin || "page");
  }

  // What the player has actually fetched in this tab. A manifest is the whole
  // stream and outranks a fragment, which is one slice of it.
  //
  // Newest first among the plain files, because arrival order is not
  // importance: what a video site fetches *first* is its own furniture. TikTok
  // opens a page by playing a two-second clip in a hidden element to find out
  // whether the browser can decode HEVC — oldest in the tab, so first in this
  // list, and duly downloaded twice in place of the video being watched. The
  // one being watched is the one fetched most recently, whatever the site.
  const sniffed = [...(tabMedia.get(tabId)?.values() ?? [])];
  for (const m of sniffed.filter((m) => m.kind === "stream")) {
    add(m.url, "media", m.mime, true, "tab");
  }
  for (const m of [...sniffed.filter((m) => m.kind !== "stream")].sort((a, b) => b.at - a.at)) {
    add(m.url, "media", m.mime, false, "tab");
  }

  // Files re-ranked by what each turns out to *be*, now that every one of them
  // has been looked at. Arrival order settles nothing on a DASH feed, where
  // several posts are playing or preloading at once and every stream is an
  // mp4 from the same host: the two files that were downloaded in place of the
  // video are the audio track of the right post, which plays as a black
  // screen, and a whole file belonging to the post above it.
  //
  // Ordered rather than filtered. A file the site has not described stays
  // exactly where it was, and when nothing here is knowable — no `efg`, no id
  // on the markup — every candidate scores alike and the list is untouched.
  // Which post it belongs to, where both the file and the markup said. With
  // nothing to compare against this is silent rather than negative: an id on
  // the file and none on the page is not evidence of disagreement.
  const belongs = (c) => (c.post === "this" ? 1 : c.post === "other" ? -1 : 0);
  // Identity first, completeness second — in that order, because they conflict
  // and the order settles which mistake gets made. A whole file of the post
  // above the one on screen downloads perfectly and is the wrong video, which
  // is the failure nothing can excuse; half of the right one is at least the
  // right video, and is now also the thing an address can be recovered from.
  const worth = (c) =>
    (c.stream ? 16 : 0) + 8 * belongs(c) + (c.partial ? 0 : 2) + (c.audioOnly ? 0 : 1);
  const files = out.filter((c) => c.kind === "media").sort((a, b) => worth(b) - worth(a));

  // The post's own address, recovered from the file the player pulled.
  //
  // This is the way out of a feed that does not depend on reading the DOM
  // correctly. Where a site writes the post id into the address of its media —
  // Facebook does, on every stream it serves — the file identifies its post
  // outright, and the page built from it extracts properly: every quality, and
  // the sound. Without it the grab fell through to saving the raw stream, and
  // a DASH stream is half a video however completely it downloads.
  //
  // Last among the pages, not first: the permalink the markup gives up is tied
  // to the element the button appeared on, while this is tied to a file the
  // tab fetched, and only the first of those knows what was clicked.
  const derived = [];
  for (const file of files) {
    const page = pageForStream(file.url);
    if (!page || seen.has(page)) continue;
    seen.add(page);
    derived.push({ url: page, kind: "page", mime: "", stream: false, origin: "stream",
                   videoId: file.videoId, post: file.post, audioOnly: false,
                   partial: false, headers: [], referrer: "" });
  }
  const ranked = [...out.filter((c) => c.kind !== "media"), ...derived, ...files];

  // A ceiling, because each one the app tries costs an extraction — but never
  // one that drops the two candidates that can still work when the readings of
  // the page have all failed. Those are the address recovered from the stream
  // and, failing even that, the file itself; a seventh permalink nobody will
  // reach is worth less than either.
  const kept = ranked.slice(0, 6);
  // Each takes a slot of its own, counting back from the end, so the second
  // does not land on top of the first.
  let slot = kept.length - 1;
  for (const must of [derived[0], files[0]]) {
    if (must && !kept.includes(must) && slot >= 0) kept[slot--] = must;
  }

  // A media candidate may be downloaded straight from the window, by MDM,
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
      return sendSimple(msg.url, { pageUrl: msg.referrer, mime: msg.mime }, { id: msg.tabId });
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

// The settings read is already in flight from the top of the file; this only
// waits for it so the badge is drawn from the real ones. Connecting the native
// port is the other half: it is what makes the app reachable, and on Chromium
// an open port is also what keeps this service worker resident instead of
// being stopped thirty seconds later.
settingsReady.then(() => {
  Native.connect();
  updateBadge();
});
