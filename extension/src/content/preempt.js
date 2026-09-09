"use strict";

/* Net 0, for a browser that cannot hold a request.
 *
 * A file host that spends its links has exactly one answer to give, and
 * whoever asks first gets it. Firefox lets the extension hold the request open
 * while MDM goes and asks, so MDM is first and the browser's copy is cancelled
 * — see `holdForMdm` in src/background.js. Manifest V3 took that away, and
 * with it the only way Chromium had of getting in front of a request.
 *
 * But a download does not start with a request. It starts with a click, and a
 * click is still cancellable on every browser there is. So this catches the
 * click instead: the link is handed to MDM and the browser is never told to
 * navigate, which means the browser never spends the one answer.
 *
 * What it costs, and why it is not what Firefox uses. A held request can be
 * released — the app says "that was a page, not a file" and the browser's own
 * request, still open and still unsent, goes ahead. A cancelled click cannot
 * be released; the navigation has to be made again from here. For a page that
 * is harmless, because a page is by definition an address that answers twice —
 * it is only the *file* links on these hosts that answer once. But it is one
 * extra request, so where the browser can hold the real one it does, and this
 * stays out of the way.
 *
 * Scope is the whole of the safety here. It runs on the hosts MDM has been
 * caught out by and nowhere else, and MDM discovers those by watching a
 * capture fail rather than by pattern-matching an address.
 */

/* Firefox's net 0 is better and does the same job; see above. Asked of the
 * manifest rather than of a user-agent string, because what actually differs
 * is the permission this build was granted. */
const CAN_BLOCK = (browser.runtime.getManifest().permissions || []).includes(
  "webRequestBlocking"
);

/** Hosts MDM has found serving links that answer once. Empty for most people. */
let hosts = [];

/* Read from storage rather than asked for over a message, because the decision
 * below has to be made synchronously: `preventDefault` is only honoured while
 * the click handler is still on the stack, and a round trip to the background
 * would return long after the browser had begun navigating. The background
 * publishes the list; this only ever reads it. */
function adopt(list) {
  hosts = Array.isArray(list) ? list.filter((h) => typeof h === "string" && h) : [];
}

if (!CAN_BLOCK) {
  browser.storage.local.get("singleUseHosts").then(
    (stored) => adopt(stored && stored.singleUseHosts),
    () => {}
  );
  browser.storage.onChanged.addListener((changes, area) => {
    if (area === "local" && changes.singleUseHosts) adopt(changes.singleUseHosts.newValue);
  });
}

/** "example.com" matches example.com and any subdomain of it. */
function hostMatches(host, pattern) {
  const p = String(pattern || "").trim().toLowerCase().replace(/^\*\./, "");
  if (!p) return false;
  return host === p || host.endsWith("." + p);
}

function onSingleUseHost(url) {
  if (!hosts.length) return false;
  let host;
  try {
    const u = new URL(url, location.href);
    if (u.protocol !== "http:" && u.protocol !== "https:") return false;
    host = u.hostname.toLowerCase();
  } catch {
    return false;
  }
  return hosts.some((h) => hostMatches(host, h));
}

/**
 * The link this click is going to follow, or nothing.
 *
 * `composedPath` first so a link inside a shadow root is found: file hosts
 * wrap their download button in a component often enough that `closest` alone
 * missed it, and a missed link is the download this file exists to save.
 */
function linkFor(event) {
  const path = typeof event.composedPath === "function" ? event.composedPath() : [];
  for (const node of path) {
    if (node && node.nodeType === 1 && node.tagName === "A" && node.href) return node;
  }
  const target = event.target;
  return target && target.nodeType === 1 ? target.closest?.("a[href]") : null;
}

/** Would this click have opened a new tab or window rather than navigating? */
function wantsNewTab(event, link) {
  return (
    event.button === 1 ||
    event.ctrlKey ||
    event.metaKey ||
    event.shiftKey ||
    (link.target && link.target !== "_self" && link.target !== "_top" && link.target !== "_parent")
  );
}

/**
 * Send the browser where it was going after all.
 *
 * Reached when MDM did not take the link — it was a page, or the app is not
 * running, or it did not answer in time. Every one of those has to end here,
 * because the click was cancelled and nothing else will do it.
 */
function goAnyway(href, newTab) {
  if (newTab) {
    // Very likely to be refused, and that is the point of the fallback. By the
    // time this runs the answer from MDM has been awaited, so the click is no
    // longer a user gesture as far as the browser is concerned and the popup
    // blocker takes the window. A blocked `open` returns null rather than
    // throwing, so it can be noticed — and a link opened in this tab instead
    // is a changed intention, where doing nothing at all is a broken page.
    const opened = window.open(href, "_blank", "noopener");
    if (opened) return;
  }
  window.location.href = href;
}

/**
 * How long a click may sit still.
 *
 * Set from the other end of the chain rather than from what feels tolerable.
 * The app answers a pre-emption within eight seconds — it holds its own
 * request to that, precisely so a server that will not answer cannot hold up a
 * click — and the background gives up at ten. Sitting between the two means
 * the answer acted on here is always the app's real one.
 *
 * A shorter deadline was tried and is worse than it looks. Giving up early
 * does not cancel anything: the app is still fetching, and it is now racing a
 * navigation this file has already started. Whichever of them reaches the
 * server first spends the link, so the other gets the landing page — and which
 * one that is varies by a few hundred milliseconds. The download that is lost
 * is lost intermittently, which is the hardest kind to report.
 */
const CLICK_TIMEOUT_MS = 9000;

/**
 * Ask the background to put the request net up for a few seconds.
 *
 * For every click this file cannot itself act on. A download button on these
 * hosts is routinely not a link — a single-page app renders a <button>, asks
 * its own API where the file is, and then assigns to `location`, which cannot
 * be patched and so cannot be caught here. What can be caught is the request
 * that assignment makes, and src/background.js holds the only instrument that
 * catches it. This is the signal that one is about to happen.
 *
 * Fire and forget, deliberately: it is on the click path, and a click must not
 * wait on a round trip to decide whether to be a click.
 */
function armRequestNet() {
  try {
    browser.runtime.sendMessage({ type: "armPreempt" }).catch(() => {});
  } catch {
    /* the background is gone; the browser gets the download, as ever */
  }
}

/** Is the document this click happened in on a host that spends its links? */
function onSingleUsePage() {
  return onSingleUseHost(location.href);
}

async function handle(event) {
  if (CAN_BLOCK) return;
  if (event.defaultPrevented) return;
  if (event.button !== 0 && event.button !== 1) return;
  if (event.altKey) return; // the modifier that means "save it", not "open it"

  const link = linkFor(event);
  const href = link ? link.href : "";

  // Everything this file cannot cancel itself, on a page where a download may
  // be about to be started by script. The two cases are a click on no link at
  // all — a button — and a click on a link pointing somewhere ordinary, which
  // on these sites is how the download page itself is reached and is followed
  // moments later by the download.
  if (!href || !onSingleUseHost(href)) {
    if (onSingleUsePage()) armRequestNet();
    return;
  }

  const newTab = wantsNewTab(event, link);
  event.preventDefault();

  let took = false;
  try {
    const reply = await Promise.race([
      browser.runtime.sendMessage({ type: "preemptClick", url: href, pageUrl: location.href }),
      new Promise((resolve) => setTimeout(() => resolve(null), CLICK_TIMEOUT_MS)),
    ]);
    took = !!(reply && reply.accepted);
  } catch (e) {
    // A background script that is gone, or an extension being reloaded. The
    // click still has to go somewhere.
  }
  if (!took) goAnyway(href, newTab);
}

/* Capture phase, so a page that cancels its own clicks does not get there
 * first — on a file host the download button routinely is a script that reads
 * the href and navigates by hand. `auxclick` is the middle button, which is a
 * download opened in a new tab and spends the link exactly as a left click
 * does. */
if (!CAN_BLOCK) {
  addEventListener("click", handle, true);
  addEventListener("auxclick", handle, true);
  /* A form is the other way these hosts start a download, and the classic one:
   * the download page posts `op=download2` and the server answers with a
   * redirect to the address the file is actually on. Nothing here can cancel
   * that — a POST cannot be replayed out of process — but the *redirect* is a
   * GET, and a GET is what the request net is waiting for. So the submit is
   * left alone and the net goes up behind it. */
  addEventListener(
    "submit",
    () => {
      if (onSingleUsePage()) armRequestNet();
    },
    true
  );
}
