"use strict";

/* Hosts that spend a link the first time it is used.
 *
 * The decisions only — whether the blocking listener is wanted, how long to
 * wait for a hand-off, whether it is time to ask the app again. Putting the
 * listener up and taking it down is background.js's, because that is the part
 * that needs a browser.
 */

/**
 * How often to ask the app for its list, at most.
 *
 * The app learns a host the moment a capture from it fails, and the download
 * after that one is the first that can be saved — so the gap here is how long
 * a user keeps losing downloads after the first one taught us.
 */
const SINGLE_USE_REFRESH_MS = 5_000;

/**
 * How long to let MDM think about a hand-off from a listed host.
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
const SINGLE_USE_HANDOFF_MS = 10_000;

/** The hand-off deadline for this URL, or 0 to use the ordinary one. */
function handoffDeadlineFor(url, hosts) {
  const host = hostOf(url);
  const listed = host && (hosts || []).some((h) => hostMatches(host, h));
  return listed ? SINGLE_USE_HANDOFF_MS : 0;
}

/**
 * Should the blocking listener be up?
 *
 * A blocking listener on every request is not a small thing to add — it makes
 * the browser wait on this script before it opens a socket — and for anyone
 * who has never met a host like this there would be nothing behind the wait.
 * So it exists only while there is a host to use it on, which for most people
 * is never.
 */
function wantsPreempt(hosts, canBlock) {
  return !!canBlock && Array.isArray(hosts) && hosts.length > 0;
}

/**
 * Is it time to ask the app for its list again?
 *
 * Asked on navigation, which is the event just before somebody presses a
 * download button, and throttled because navigation is not rare.
 */
function singleUseRefreshDue(now, askedAt, force = false, intervalMs = SINGLE_USE_REFRESH_MS) {
  return !!force || now - askedAt >= intervalMs;
}
