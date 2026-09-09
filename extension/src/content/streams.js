"use strict";

/* What the player fetched, remembered by the page rather than by us.
 *
 * The sniffer in src/background.js watches responses go past and files the
 * media ones under the tab they arrived in. That works and is not enough, for
 * two reasons that both bite on exactly the sites this matters on.
 *
 * The first is arithmetic. A stream's manifest is fetched *once*, before the
 * first frame of video; its segments are fetched every few seconds for as long
 * as anyone watches. The manifest is therefore always the oldest thing the
 * sniffer holds and the segments are always the newest, and any bound on how
 * much is remembered is a rule for throwing the manifest away. src/background.js
 * defends against this directly — see `makeRoom` — and it is still the wrong
 * shape of defence, because it is defending a record that should not have been
 * at risk.
 *
 * The second is that on Chromium the record does not survive. A Manifest V3
 * service worker is stopped when it has been idle for thirty seconds and every
 * map in it goes too. A film is two hours long. Press Download an hour in and
 * the background has been stopped and restarted many times over: what it holds
 * is whatever has arrived since the last restart, which is segments, and the
 * one address describing the whole video was recorded once at the beginning and
 * discarded with the first restart. That is the reported failure — a download
 * that completes, weighs a couple of megabytes and is six seconds of the film.
 *
 * The page has the answer to both, and has had it all along. Resource Timing is
 * a per-document record of every URL the document fetched, kept by the browser
 * for the life of the document. It does not care that the background was
 * stopped, it does not evict the oldest entry to make room, and it is populated
 * whether or not anything was watching at the time. So this reads it, keeps the
 * manifests out of it, and hands them over when asked.
 *
 * In every frame, deliberately: a streaming site serves its player in an iframe
 * from another origin, and the manifest is fetched by the player. The document
 * in the address bar has no record of it at all.
 */

/** `.m3u8` / `.m3u` / `.mpd`, wherever the query or a trailing path begins. */
const MANIFEST_PATH = /\.(?:m3u8|m3u|mpd)(?:[?#/]|$)/i;

/**
 * Addresses that name a manifest without ending in one.
 *
 * A CDN is under no obligation to give its playlist a file extension, and the
 * ones that hide it behind a token are the ones this whole file exists for.
 * These are the two shapes that are still unambiguous: a path segment that *is*
 * the word, and a query parameter naming the format. Anything looser than this
 * starts matching ordinary API calls, and a candidate list with an API call at
 * the top of it is worse than one with nothing.
 */
const MANIFEST_HINT = /(?:^|[/?&=])(?:master|playlist|index|manifest|hls|dash)(?:[-_.][^/?&]*)?\.(?:m3u8|m3u|mpd)|[?&](?:type|format|ext)=(?:m3u8|mpd|hls|dash)\b/i;

function looksLikeManifest(url) {
  if (!/^https?:\/\//i.test(url)) return false;
  return MANIFEST_PATH.test(url) || MANIFEST_HINT.test(url);
}

/**
 * The manifests this document has fetched, oldest first.
 *
 * A cap, because a page that cycles through videos would otherwise grow this
 * without bound for as long as it is open — but a generous one, since these are
 * strings and the whole point of the file is not throwing the useful one away.
 */
const MAX_KEPT = 40;
const found = [];

function remember(url) {
  if (!looksLikeManifest(url)) return;
  if (found.includes(url)) return;
  found.push(url);
  if (found.length > MAX_KEPT) found.shift();
}

/* A larger buffer than the default 250, because the browser stops recording
 * when it is full rather than evicting, and on a stream it fills with segments
 * in about half a minute. Raising it does not change what is kept — the
 * manifest is fetched first and would survive either way — but it keeps the
 * *observer* below receiving entries, which is what catches a manifest fetched
 * later: a quality change, or the next episode in a player that does not
 * reload. Wrapped because a page is allowed to have set its own, and a page
 * that objects is not a reason for the content script to stop. */
try {
  if (performance.setResourceTimingBufferSize) performance.setResourceTimingBufferSize(1000);
} catch {
  /* the page's own bookkeeping wins; the observer below still works */
}

/* `buffered: true` replays what was fetched before this ran, which matters
 * more than it looks: a content script is injected at document_start but a
 * document restored from the back/forward cache, or one this script was
 * re-injected into after an extension reload, has a timeline that starts long
 * before. */
try {
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) remember(entry.name);
  }).observe({ type: "resource", buffered: true });
} catch {
  /* no PerformanceObserver: the sweep below is then the whole of it */
}

/* And a sweep of whatever is already in the buffer, for the same reason the
 * observer asks for buffered entries: neither alone has been reliable across
 * every way a content script can come to be running in a document. */
try {
  for (const entry of performance.getEntriesByType("resource")) remember(entry.name);
} catch {
  /* nothing to sweep */
}

browser.runtime.onMessage.addListener((msg) => {
  if (!msg || msg.type !== "mdm-page-streams") return undefined;
  // Newest first: a player that has been through several videos has the one on
  // screen at the end of the list, and the caller ranks in the order given.
  return Promise.resolve({ ok: true, streams: [...found].reverse(), pageUrl: location.href });
});
