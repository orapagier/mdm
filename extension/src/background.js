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

/* ------------------------------------------------------------------ *
 * Keeping the sniffer's record across a stopped service worker
 *
 * On Chromium the background is a service worker, and a service worker is
 * stopped once it has been idle for thirty seconds. Every map above goes with
 * it. For most of them that is fine and even tidy — a claim on a URL is worth
 * five seconds, a half-remembered request rather less — but the sniffer's
 * record is not that kind of state. It is the answer to "what is this page
 * playing", asked minutes or hours after the answer was learned.
 *
 * `storage.session` is memory the browser keeps rather than memory this script
 * keeps: it lives as long as the browser session, is never written to disk, and
 * is there again when the worker comes back. Which makes the mirror below the
 * difference between a Download button that knows what the page is playing and
 * one that has to say it found nothing.
 * ------------------------------------------------------------------ */

const MEDIA_KEY = "tabMedia";

/** Available on Chromium and on Firefox 115 and later; absent is survivable. */
const SESSION = browser.storage && browser.storage.session;

/**
 * Hydration, awaited by everything that reads the record.
 *
 * Resolved rather than rejected on every failure. A sniffer that starts empty
 * is exactly what the old behaviour was, and no worse; a sniffer that never
 * starts because a storage read threw would be.
 */
const mediaReady = (async () => {
  if (!SESSION) return;
  let stored;
  try {
    stored = (await SESSION.get(MEDIA_KEY))[MEDIA_KEY];
  } catch (e) {
    console.warn("[mdm] could not read back what the page was playing:", e.message);
    return;
  }
  for (const [tabId, items] of Object.entries(stored || {})) {
    // Only for tabs this worker has not already heard from: an event that woke
    // the worker can easily be answered before this read comes back, and what
    // just happened is newer than what was written.
    if (tabMedia.has(Number(tabId))) continue;
    tabMedia.set(Number(tabId), new Map((items || []).map((m) => [m.url, m])));
  }
})();

/**
 * Write the record back, at most every couple of seconds.
 *
 * Debounced because the thing being recorded is a stream: segments arrive
 * every few seconds per track, and a storage write per segment would be the
 * most expensive thing this extension does. Two seconds is far inside the
 * thirty a worker is given before it is stopped.
 */
let persistTimer = null;
function persistMedia() {
  if (!SESSION || persistTimer) return;
  persistTimer = setTimeout(async () => {
    persistTimer = null;
    const out = {};
    for (const [tabId, items] of tabMedia) out[tabId] = [...items.values()];
    try {
      await SESSION.set({ [MEDIA_KEY]: out });
    } catch (e) {
      console.warn("[mdm] could not save what the page is playing:", e.message);
    }
  }, 2000);
}

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
    return Native.request(
      { type: "download", job },
      handoffDeadline(details.url) || cfg.handoffTimeoutMs
    )
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
 * Net 0 — before the browser asks
 *
 * Every other net in this file watches a response and then asks the server for
 * the same file again. Some file hosts will not answer twice: the address is
 * good for one request, and the browser has already spent it by the time
 * anything here can act. Those downloads were handed back to the browser and
 * the host recorded, which is honest but is not what a download manager is
 * for.
 *
 * This is the one net that acts *before* the request goes out, so MDM makes
 * the request the browser was about to. Only on the hosts the app has been
 * caught out by, because it is the expensive net: it cannot know whether an
 * address is a download until something asks, so the request is held while
 * MDM asks, and a page comes straight back to the browser that was holding it.
 *
 * Firefox only, and structurally so: holding a request open is exactly what
 * Manifest V3 took away from Chromium, and there is nothing to hold one with.
 * ------------------------------------------------------------------ */

/**
 * Hosts the app has found serving links that answer once.
 *
 * The app's list, not ours — it is the app that discovers them, by watching a
 * capture fail — and it is read rather than written here.
 */
let singleUseHosts = [];

/** Whether the blocking listener below is currently registered. */
let preempting = false;

/**
 * Take the app's list, and put the listener up or down to match.
 *
 * A blocking listener on every request is not a small thing to add — it makes
 * the browser wait on this script before it opens a socket — and for anyone
 * who has never met a host like this there would be nothing behind the wait.
 * So it exists only while there is a host to use it on, which for most people
 * is never.
 */
function learnSingleUse(hosts) {
  singleUseHosts = Array.isArray(hosts) ? hosts : [];
  publishSingleUse();
  const wanted = CAN_BLOCK && singleUseHosts.length > 0;
  if (wanted === preempting) return;
  if (wanted) {
    browser.webRequest.onBeforeRequest.addListener(
      holdForMdm,
      { urls: ["<all_urls>"], types: FILTER_TYPES },
      ["blocking"]
    );
  } else {
    browser.webRequest.onBeforeRequest.removeListener(holdForMdm);
  }
  preempting = wanted;
  // The click net's rule names these hosts, so a list that has emptied leaves
  // a rule that can never match and would sit there until its timer ran out.
  if (!singleUseHosts.length) disarmRequestPreempt();
}

/**
 * Hand the list to the content scripts, for the browser that cannot hold a
 * request.
 *
 * src/content/preempt.js catches the *click* instead, which is the only way in
 * front of a request Manifest V3 left standing — and it has to decide whether
 * to cancel that click synchronously, so it cannot ask us and must already
 * know. Storage rather than a message for exactly that reason.
 *
 * Written on Firefox too, where nothing reads it: the two builds share this
 * file, and a branch here would be a branch that only ever runs on one of them
 * and so only ever gets tested on one of them.
 */
let publishedSingleUse = "";
function publishSingleUse() {
  const next = singleUseHosts.join("\n");
  // Compared before writing, because this runs on every ping and every
  // navigation, and a storage write fires storage.onChanged in every content
  // script in every frame in every tab.
  if (next === publishedSingleUse) return;
  publishedSingleUse = next;
  browser.storage.local.set({ singleUseHosts }).catch(() => {});
}

/**
 * How long to let MDM think about a hand-off.
 *
 * Ordinarily a hand-off is bookkeeping: the app writes a row and answers, and
 * anything slower than a second or two is the app not being there. A capture
 * from a single-use host is not that. The app no longer declines those on
 * sight — it opens the connection and decides from what comes back, and where
 * a file comes back that same connection *is* the download. Which means the
 * answer now waits on a server, and the old deadline expired while the app was
 * still holding a perfectly good response to a file it had been asked for.
 *
 * The ceiling is Chromium's, not ours: `onDeterminingFilename` holds a
 * download for fifteen seconds, `ensureNative` may spend two of them, and the
 * app gives up on the server at six. Ten leaves room at both ends.
 */
function handoffDeadline(url) {
  const host = hostOf(url);
  const listed = host && singleUseHosts.some((h) => hostMatches(host, h));
  return listed ? 10_000 : 0;
}

/** How often to ask for it again, at most. */
const SINGLE_USE_REFRESH_MS = 30_000;
let singleUseAskedAt = 0;

/**
 * Has the app answered this session?
 *
 * The gate on asking it anything the user did not ask for. A message to the
 * native host is not free when MDM is closed: the host answers by *starting*
 * it, and waits up to fifteen seconds for the socket while every message
 * behind it queues. That is the right trade for a download somebody clicked
 * on, and quite the wrong one for a page load — a browser session spent with
 * MDM deliberately closed would keep dragging it back, a page at a time.
 *
 * Set by the pong the port opens with, and cleared when the host says the app
 * has gone. So the refresh below rides on a connection that already exists,
 * and never creates one.
 */
let appAnswered = false;

/**
 * Catch up with the app's list.
 *
 * The app learns a host the moment a capture from it fails, and the download
 * after that one is the first that can be saved — so a list fetched only at
 * startup would be a version behind for the whole session. Asked on
 * navigation, which is the event just before somebody presses a download
 * button, and throttled because navigation is not rare.
 */
async function refreshSingleUse(force = false) {
  if (!appAnswered) return;
  const now = Date.now();
  if (!force && now - singleUseAskedAt < SINGLE_USE_REFRESH_MS) return;
  singleUseAskedAt = now;
  try {
    const reply = await Native.request({ type: "ping" }, 2000);
    if (reply && Array.isArray(reply.singleUseHosts)) learnSingleUse(reply.singleUseHosts);
  } catch (e) {
    // The app is not there. Nothing to pre-empt for, and the ordinary nets
    // fail open the same way.
  }
}

/**
 * Is MDM running — and, while we are asking, what does it know?
 *
 * The same message either way, because the app answers a ping with its state
 * and there is no sense in sending two. This is what the popup asks with.
 */
async function askApp() {
  singleUseAskedAt = Date.now();
  try {
    const reply = await Native.request({ type: "ping" }, 1500);
    if (reply && Array.isArray(reply.singleUseHosts)) learnSingleUse(reply.singleUseHosts);
    appAnswered = !!(reply && reply.ok);
    return appAnswered;
  } catch {
    return false;
  }
}

/* ------------------------------------------------------------------ *
 * Net 0b — the request net, for the browser that cannot hold a request
 *
 * The click net above catches a link. It cannot catch anything else, and on
 * the hosts this exists for the download button routinely is not a link: a
 * single-page app renders a <button>, asks its own API where the file is, and
 * navigates by assigning to `location`. There is no click on an anchor to
 * cancel, so the click net sees nothing, the browser makes the request, and
 * the one answer the address had is spent before MDM is told the download
 * exists. That is exactly the case reported against filekeeper.net in Brave.
 *
 * `location` cannot be patched — it is unforgeable, by specification — so
 * there is no way to catch that navigation in the page. What is left is
 * declarativeNetRequest, which can redirect a request *before* it is sent. A
 * redirect is a hold, provided something lets go again, and pages/handoff.js
 * is what lets go.
 *
 * Armed by a click rather than left standing, and that is the whole of the
 * cost control. A rule matching every navigation to these hosts would send
 * ordinary browsing through the handoff page too — a flash and a round trip on
 * every page of the site. So the click net arms it when it sees a click it
 * cannot itself handle, which is the moment just before a scripted download
 * navigates and is otherwise nothing like ordinary browsing.
 * ------------------------------------------------------------------ */

/** Whether this build has the API at all. Chromium only in practice. */
const CAN_REDIRECT = !!(
  browser.declarativeNetRequest && browser.declarativeNetRequest.updateSessionRules
);

/**
 * One rule, replaced rather than accumulated.
 *
 * Session rules rather than dynamic ones: these describe a click that has just
 * happened, they are meaningless a minute later, and none of them has any
 * business surviving a browser restart on disk.
 */
const PREEMPT_RULE_ID = 9001;

/**
 * How long an arming lasts.
 *
 * Long enough for a download button that asks its own API where the file is
 * before navigating — a couple of round trips — and short enough that a click
 * which turned out not to be a download leaves nothing behind. Whichever comes
 * first, the rule is also removed the instant the handoff page loads.
 */
const ARM_MS = 12_000;

let disarmTimer = null;

/**
 * Put the rule up: the next navigation to a single-use host is MDM's.
 *
 * The condition carries every listed host rather than the one clicked, because
 * the address a download button navigates to is routinely on a different
 * subdomain from the page the button was on, and the list already matches by
 * suffix. `\1` is the whole address, handed to the handoff page in the
 * fragment — the query would have worked for most links and then quietly
 * truncated the first one whose token contained an `&`.
 */
async function armRequestPreempt() {
  if (!CAN_REDIRECT || CAN_BLOCK || !singleUseHosts.length) return;
  const target =
    browser.runtime.getURL("pages/handoff.html") + "#\\1";
  try {
    await browser.declarativeNetRequest.updateSessionRules({
      removeRuleIds: [PREEMPT_RULE_ID],
      addRules: [
        {
          id: PREEMPT_RULE_ID,
          priority: 1,
          action: { type: "redirect", redirect: { regexSubstitution: target } },
          condition: {
            regexFilter: "^(.+)$",
            requestDomains: singleUseHosts.map((h) => h.replace(/^\*\./, "")),
            // A POST cannot be replayed out of process, and redirecting one
            // would lose the body it carries. The same rule `preemptable`
            // applies, applied a step earlier.
            requestMethods: ["get"],
            resourceTypes: ["main_frame", "sub_frame", "other"],
          },
        },
      ],
    });
  } catch (e) {
    console.warn("[mdm] could not arm the request net:", e.message);
    return;
  }
  if (disarmTimer) clearTimeout(disarmTimer);
  disarmTimer = setTimeout(() => {
    disarmTimer = null;
    disarmRequestPreempt();
  }, ARM_MS);
}

/** Take it down again. Safe to call when nothing is up. */
async function disarmRequestPreempt() {
  if (!CAN_REDIRECT) return;
  if (disarmTimer) {
    clearTimeout(disarmTimer);
    disarmTimer = null;
  }
  try {
    await browser.declarativeNetRequest.updateSessionRules({
      removeRuleIds: [PREEMPT_RULE_ID],
    });
  } catch (e) {
    console.warn("[mdm] could not take the request net down:", e.message);
  }
}

/** Long enough for the app to make a request and read its headers. */
const PREEMPT_TIMEOUT_MS = 10_000;

/**
 * Ask MDM to make this request instead of the browser.
 *
 * Returns what the blocking listener should do: cancel only where MDM says it
 * has the file. Everything else — a page, a refusal, a timeout, an app that is
 * not running — leaves the request exactly as it was, which is the same
 * failing open every other net here does.
 */
async function preempt(details) {
  const referrer = details.documentUrl || details.originUrl || "";
  const job = {
    url: details.url,
    // Deliberately unnamed. Nothing has asked the server yet, so the only name
    // available here is whatever the address happens to end in — a token, on
    // the hosts this net exists for — and the app is about to be handed a
    // Content-Disposition that actually says.
    filename: "",
    size: -1,
    mime: "",
    mirrors: [],
    headers: await headersForUrl(details.url, referrer, details.cookieStoreId),
    referrer,
    cookieStoreId: details.cookieStoreId || "",
    tabId: details.tabId ?? -1,
    reason: "single-use host",
    source: "preempt",
  };
  const reply = await Native.request({ type: "preempt", job }, PREEMPT_TIMEOUT_MS);
  if (!reply || !reply.accepted) return {};
  markCaptured(details.url);
  return { cancel: true };
}

/**
 * The blocking listener itself, named so it can be taken down again.
 *
 * Registered by `learnSingleUse` and only while there is something for it to
 * do; see the note there.
 */
function holdForMdm(details) {
  // A flag rather than an await, for the same reason net 1 uses one: this
  // listener is blocking, and every request on the page would wait behind a
  // storage read.
  if (!settingsLoaded) return {};

  const verdict = preemptable(
    { method: details.method, url: details.url, type: details.type },
    cfg,
    state,
    singleUseHosts
  );
  if (!verdict.capture) return {};
  if (claimCapture(details.url)) return {};

  return preempt(details).catch((e) => {
    console.warn("[mdm] could not take it before the browser:", e.message);
    return {};
  });
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

  const reply = await Native.request(
    { type: "download", job },
    handoffDeadline(item.url) || 4000
  );
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

/** How many media responses to remember per tab, newest kept. */
const MAX_SNIFFED = 50;

/**
 * Make room in a tab's media — the oldest first, but never a manifest while
 * there is a fragment to drop instead.
 *
 * Age is the wrong measure for a stream. The manifest is fetched once, before
 * the first frame, so it is always the oldest thing here; the pieces it lists
 * arrive every few seconds for as long as anyone watches. Fifty slots is about
 * five minutes of playback, after which the oldest-out rule had thrown away
 * the one entry that describes the whole video and kept fifty slices of it.
 * Press Download at six minutes in and the best thing left to offer was a
 * six-second fragment, which downloaded completely, weighed 1.6 MB and would
 * not open in anything.
 */
function makeRoom(m) {
  for (const [url, item] of m) {
    if (item.kind !== "stream") {
      m.delete(url);
      return;
    }
  }
  // Nothing but manifests: a page with more streams than slots, where the
  // oldest is the fairest thing to lose after all.
  m.delete(m.keys().next().value);
}

browser.webRequest.onHeadersReceived.addListener(
  async (details) => {
    await settingsReady;
    if (!cfg.sniffMedia || details.tabId < 0) return;
    // Before the map is read or written, so a worker that has just been
    // restarted adds to what the last one learned rather than to nothing.
    await mediaReady;
    const headers = headerMap(details.responseHeaders);
    const mime = mimeOf(headers);
    const isStream = isManifest(details.url, mime);
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
    while (m.size >= MAX_SNIFFED) makeRoom(m);
    m.set(details.url, {
      url: details.url,
      mime,
      size: sizeOf(headers),
      kind: isStream ? "stream" : "media",
      at: Date.now(),
    });
    persistMedia();
    updateBadge();
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other", "main_frame", "sub_frame"] },
  ["responseHeaders"]
);

browser.tabs.onRemoved.addListener((tabId) => {
  tabMedia.delete(tabId);
  persistMedia();
});

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
  persistMedia();
  updateBadge();
  // A page load is the moment before a download button is pressed, which
  // makes it the last useful moment to find out whether this site is one MDM
  // has to go first on. Throttled inside; see refreshSingleUse.
  refreshSingleUse();
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
    await mediaReady;
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
      await mediaReady;
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
  // Both the frame and the page around it, because either is the site the
  // exclusion was written about. Excluding a site whose player is served from
  // an iframe otherwise excluded only the frame's own host — a name the user
  // has never seen, on a page they had said to leave alone.
  const excluded = (h) => !!h && cfg.blockedSites.some((s) => hostMatches(h, s));
  if (excluded(host) || excluded(hostOf(msg.topUrl || ""))) {
    return { ok: false, error: "site excluded" };
  }
  // Brought up and waited for, the way every other capture path does it,
  // rather than asked of the port as it stands. `isAvailable()` is false for
  // the whole window between a port dropping and its reconnect landing — a
  // service worker Chromium has just restarted, an app restarted underneath
  // one — and the old check reported the app missing throughout it while
  // opening the port on its way out. The click that asked the question was
  // spent proving the port could be opened; only the one after it worked,
  // which is why the popup could say "connected" and this could say the app
  // was not there, in that order, about the same running app.
  if (!(await ensureNative())) {
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
      // Long enough to cover a cold start. The port opens optimistically, so
      // nothing above here has established that the app is running, and the
      // native host answers only once it has launched the app and waited for
      // its socket — up to fifteen seconds. A shorter deadline reports a
      // timeout about an app that is in the middle of starting, and the
      // window it opens arrives to a button that has already given up.
      20000
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
/**
 * Every manifest the *page* remembers fetching, best frame first.
 *
 * src/content/streams.js keeps these out of Resource Timing, which is the one
 * record of a stream that neither the background's memory limit nor Chromium
 * stopping the service worker can take away. See the note at the top of that
 * file for why both of those routinely do.
 *
 * Asked of every frame, because the player is usually in one of them and the
 * manifest was fetched there; ranked with the button's own frame first,
 * because a streaming page has advertising frames and they fetch streams too.
 * A frame that does not answer — no content script, a document that has gone
 * away — is skipped rather than waited on.
 */
async function pageStreams(tabId, preferFrameId) {
  if (tabId < 0) return [];
  let frames = [];
  try {
    frames = (await browser.webNavigation.getAllFrames({ tabId })) || [];
  } catch {
    frames = [{ frameId: 0 }];
  }
  const order = [...frames].sort((a, b) => {
    const rank = (f) => (f.frameId === preferFrameId ? 0 : f.frameId === 0 ? 1 : 2);
    return rank(a) - rank(b);
  });

  const out = [];
  const seen = new Set();
  for (const frame of order.slice(0, MAX_FRAMES_ASKED)) {
    let reply = null;
    try {
      reply = await browser.tabs.sendMessage(
        tabId,
        { type: "mdm-page-streams" },
        { frameId: frame.frameId }
      );
    } catch {
      continue; // no content script in that frame
    }
    for (const url of (reply && reply.streams) || []) {
      if (seen.has(url)) continue;
      seen.add(url);
      out.push(url);
    }
  }
  return out;
}

async function videoCandidates(msg, tabId) {
  await mediaReady;
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
      // A manifest is a manifest wherever it was found. Only the sniffer used
      // to say so, which left the one a page names in its own metadata ranked
      // below the fragments the player had fetched of it.
      stream: stream || (kind === "media" && isManifest(url, mime)),
      origin,
      videoId: facts.videoId,
      post: postOf(facts.videoId, origin),
      // The site's own word for it first; failing that, what the server
      // called the response. Only one site in the pair is honest at a time —
      // Facebook labels its audio track `video/mp4` and says `_audio` in the
      // URL, and an ordinary site does the reverse.
      audioOnly: facts.audioOnly || /^audio\//i.test(mime || ""),
      // Half of a DASH pair, or one slice of an HLS one. Complete as a
      // download and useless as a video either way: a fragment saved on its
      // own is a few seconds out of the middle of a film, with none of the
      // headers a player needs to make sense of it.
      partial: facts.partial || facts.audioOnly || isFragment(url, mime),
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

  // The page around the frame, on a site that serves its player in one.
  //
  // Everything above came out of the document the button was pressed in, and
  // in an embed that document is the player: its permalink readings are the
  // embed's own address, and the address bar's — the one a person would call
  // "the page", and the only one either a site-specific extractor or a
  // generic one following the iframe can do anything with — appears nowhere.
  // Offered after the frame's own readings rather than instead of them: a
  // YouTube video embedded in an article is best extracted from the embed,
  // and this is the answer for when that fails.
  add(msg.topUrl || "", "page");
  for (const c of found.filter((c) => c.kind === "media")) {
    add(c.url, "media", "", false, c.origin || "page");
  }

  // What the page itself remembers fetching. First among the media, because a
  // manifest read back out of Resource Timing is the only one that is still
  // there an hour into a film — see pageStreams, and the note in
  // src/content/streams.js.
  for (const url of await pageStreams(tabId, msg.frameId ?? 0)) {
    add(url, "media", "", true, "page-timing");
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
      // Chromium stops this worker after thirty seconds idle, and the popup is
      // usually what starts it again: the message arrives before the settings
      // read has come back, so answering straight away handed the popup the
      // defaults as the user's settings. Firefox keeps its event page resident
      // behind the open native port, which is why this only ever showed on
      // Chromium.
      await settingsReady;
      await mediaReady;
      return {
        cfg,
        // Asked of the app rather than of the port. A port is opened
        // optimistically and, on a worker this popup has only just started,
        // may not be open at all yet — neither says whether MDM is running.
        connected: await askApp(),
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
      return grabVideo(
        {
          ...msg,
          // The address the user is actually on, where that is not the one the
          // button was pressed in. The content script runs in every frame, so
          // a player served in an iframe reports the *embed* as its page — a
          // signed token on a host nobody has an extractor for — while the
          // page around it, the one in the address bar, is not offered at all.
          // Only the background script can see both. Empty for a click in the
          // top document, where it would be the page URL a second time.
          topUrl: sender.frameId ? sender.tab?.url || "" : "",
          // Which frame that was, so the manifests read back out of the page
          // can be ranked by whose player they belong to. A streaming page
          // carries advertising frames, and those fetch streams of their own.
          frameId: sender.frameId ?? 0,
        },
        sender.tab?.id ?? msg.tabId ?? -1
      );
    // The click net has met a click it cannot handle itself -- a button, or
    // anything else that will navigate by script -- so the request net goes up
    // for the few seconds in which that navigation happens. Nothing is decided
    // here; this only puts MDM in a position to decide.
    case "armPreempt":
      armRequestPreempt();
      return { ok: true };

    // The handoff page has loaded, so the rule that sent it there has done its
    // job. Taken down before that page asks anything, so the navigation it
    // makes afterwards -- whichever way the answer goes -- cannot be redirected
    // back to it.
    case "disarmPreempt":
      await disarmRequestPreempt();
      return { ok: true };

    // A request the browser was about to make and has not: the redirect got in
    // front of it, so this address is unspent and MDM's request is the first.
    // The same decision as the click net's, from the same evidence.
    case "preemptHeld":
    case "preemptClick": {
      // The click net's half of net 0 — src/content/preempt.js has already
      // cancelled the click, so this decides whether the browser is sent after
      // the link anyway. Everything but a plain "yes, MDM has it" is a no, and
      // a no is the click going through as if none of this had happened.
      await settingsReady;
      const details = {
        url: msg.url,
        method: "GET",
        // The click net only ever catches a navigation, and says so rather
        // than leaving `preemptable` to guess from an address.
        type: "main_frame",
        documentUrl: msg.pageUrl || sender.tab?.url || "",
        tabId: sender.tab?.id ?? -1,
        cookieStoreId: sender.tab?.cookieStoreId || "",
      };
      const verdict = preemptable(details, cfg, state, singleUseHosts);
      if (!verdict.capture) return { accepted: false, reason: verdict.reason };
      if (claimCapture(details.url)) return { accepted: false, reason: "already captured" };
      if (!(await ensureNative())) return { accepted: false, reason: "MDM is not running" };
      try {
        const taken = await preempt(details);
        return { accepted: !!taken.cancel };
      } catch (e) {
        console.warn("[mdm] could not take the click before the browser:", e.message);
        return { accepted: false, reason: e.message };
      }
    }
    case "openApp":
      Native.post({ type: "focus" });
      return { ok: true };
    default:
      return undefined;
  }
});

Native.onMessage((msg) => {
  if (!msg) return;
  if (msg.type === "hostState") {
    appAnswered = !!msg.connected;
    updateBadge();
  }
  // The answer to the `hello` the port opens with, which is the first chance
  // there is to learn which hosts MDM has to go first on.
  if (msg.type === "pong") {
    appAnswered = true;
    singleUseAskedAt = Date.now();
    if (Array.isArray(msg.singleUseHosts)) learnSingleUse(msg.singleUseHosts);
  }
});

/* ------------------------------------------------------------------ *
 * Startup
 * ------------------------------------------------------------------ */

// Opening the native port is what makes the app reachable, and on Chromium an
// open port is also what keeps this service worker resident instead of being
// stopped thirty seconds later. It reads none of the settings, so it does not
// wait for them — every moment it spends behind that read is a moment the
// extension cannot say the app is there.
Native.connect();

// The badge is the half that does wait: the settings read is already in flight
// from the top of the file.
settingsReady.then(updateBadge);
