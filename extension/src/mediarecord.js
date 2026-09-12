"use strict";

/* What a tab is playing, and what to forget when the record is full.
 *
 * Browser-free for the same reason as job.js: which entry a full record gives
 * up is the whole difference between offering a video and offering a
 * six-second slice of one.
 */

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

/**
 * Record one sniffed URL against a tab, dropping what has to go to fit it.
 *
 * Returns whether it was new. The ceiling drops the *oldest* rather than
 * refusing the newest: refusing went deaf — scroll far enough down a feed and
 * the fifty slots are full of videos already gone by, the one on screen is
 * never recorded, and the button has nothing to offer for it.
 */
function noteMedia(m, item, limit = MAX_SNIFFED) {
  if (m.has(item.url)) return false;
  while (m.size >= limit) makeRoom(m);
  m.set(item.url, item);
  return true;
}
