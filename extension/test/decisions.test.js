"use strict";
/**
 * Tests for the three modules background.js no longer has to be loaded to
 * exercise: job.js, singleuse.js and mediarecord.js.
 *
 * All three are deliberately free of any `browser.*` reference, so they run
 * here exactly as the extension runs them — as classic scripts sharing one
 * global scope, loaded whole rather than lifted out by name.
 *
 * Run: node extension/test/decisions.test.js
 */

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const SRC = path.join(__dirname, "..", "src");
const context = vm.createContext({ console, TextDecoder, TextEncoder, URL, btoa, atob });
for (const file of ["util.js", "capture.js", "job.js", "singleuse.js", "mediarecord.js"]) {
  vm.runInContext(fs.readFileSync(path.join(SRC, file), "utf8"), context, { filename: file });
}
const {
  forwardableHeaders,
  buildJob,
  extensionForMime,
  originOfUrl,
  originOfBlob,
  handoffDeadlineFor,
  wantsPreempt,
  singleUseRefreshDue,
  makeRoom,
  noteMedia,
} = context;
/** Top-level `const`s live in the vm's lexical scope, not on the context. */
const ev = (name) => vm.runInContext(name, context);
const MAX_SNIFFED = ev("MAX_SNIFFED");
const SINGLE_USE_HANDOFF_MS = ev("SINGLE_USE_HANDOFF_MS");
const SINGLE_USE_REFRESH_MS = ev("SINGLE_USE_REFRESH_MS");

let passed = 0;
const failures = [];

function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failures.push(name + "\n      " + e.message);
  }
}

/* ---------------- job.js ---------------- */

check("headers the fetcher must set itself are never forwarded", () => {
  const out = forwardableHeaders([
    { name: "Range", value: "bytes=0-1023" },
    { name: "Accept-Encoding", value: "gzip" },
    { name: "Host", value: "example.com" },
    { name: "Cookie", value: "session=abc" },
    { name: "User-Agent", value: "Mozilla/5.0" },
  ]);
  const names = out.map((h) => h.name.toLowerCase());
  // Forwarding Range in particular would truncate every segmented download.
  assert.ok(!names.includes("range"), "Range was forwarded");
  assert.ok(!names.includes("accept-encoding"));
  assert.ok(!names.includes("host"));
  // The two that carry the session are exactly the ones worth keeping.
  // Joined rather than compared structurally: these come out of the vm's own
  // realm, where an Array is not this realm's Array.
  assert.strictEqual(names.sort().join(","), "cookie,user-agent");
});

check("a header with no value is not forwarded as an empty one", () => {
  const out = forwardableHeaders([
    { name: "X-Binary", binaryValue: [1, 2, 3] },
    { name: "X-Real", value: "" },
  ]);
  assert.strictEqual(out.length, 1);
  assert.strictEqual(out[0].name, "X-Real");
  assert.strictEqual(out[0].value, "");
  assert.strictEqual(forwardableHeaders(null).length, 0);
});

check("a job carries the tab and the page that asked for it", () => {
  const req = {
    headers: [{ name: "Cookie", value: "s=1" }],
    documentUrl: "https://example.com/page",
    cookieStoreId: "firefox-container-3",
    tabId: 7,
  };
  const job = buildJob(req, { url: "https://cdn.example.com/movie.mp4" }, {}, "click");
  assert.strictEqual(job.url, "https://cdn.example.com/movie.mp4");
  assert.strictEqual(job.referrer, "https://example.com/page");
  assert.strictEqual(job.cookieStoreId, "firefox-container-3");
  assert.strictEqual(job.tabId, 7);
  assert.strictEqual(job.reason, "click");
  assert.strictEqual(job.source, "webRequest");
});

check("a request from no tab says so rather than claiming tab 0", () => {
  const job = buildJob({ headers: [] }, { url: "https://example.com/f.zip" }, {}, "x");
  assert.strictEqual(job.tabId, -1);
  assert.strictEqual(job.referrer, "");
  assert.strictEqual(job.cookieStoreId, "");
});

check("a file the browser did not name gets one from its type", () => {
  assert.strictEqual(extensionForMime("image/jpeg"), ".jpg");
  // Parameters and case are the server's business, not ours.
  assert.strictEqual(extensionForMime("VIDEO/MP4; codecs=avc1"), ".mp4");
  assert.strictEqual(extensionForMime("application/x-unheard-of"), "");
  assert.strictEqual(extensionForMime(""), "");
  assert.strictEqual(extensionForMime(undefined), "");
});

check("an origin is read from a URL, and from the URL inside a blob handle", () => {
  assert.strictEqual(originOfUrl("https://example.com:8443/a/b?c=1"), "https://example.com:8443");
  assert.strictEqual(originOfUrl("not a url"), "");
  // Only a document of the blob's own origin can resolve the handle.
  assert.strictEqual(originOfBlob("blob:https://example.com/1234-5678"), "https://example.com");
  assert.strictEqual(originOfBlob("BLOB:https://example.com/x"), "https://example.com");
  assert.strictEqual(originOfBlob("blob:nonsense"), "");
});

/* ---------------- singleuse.js ---------------- */

check("a listed host is given the longer deadline, and nothing else is", () => {
  const hosts = ["filekeeper.net"];
  assert.strictEqual(
    handoffDeadlineFor("https://filekeeper.net/download/abc", hosts),
    SINGLE_USE_HANDOFF_MS
  );
  // File hosts hand the file to a numbered edge node; same site, same tokens.
  assert.strictEqual(handoffDeadlineFor("https://dl3.filekeeper.net/get/x", hosts), SINGLE_USE_HANDOFF_MS);
  // 0 means "use the ordinary deadline", which is the answer for everyone else.
  assert.strictEqual(handoffDeadlineFor("https://example.com/big.iso", hosts), 0);
  // A suffix is not a subdomain.
  assert.strictEqual(handoffDeadlineFor("https://notfilekeeper.net/d", hosts), 0);
  assert.strictEqual(handoffDeadlineFor("https://filekeeper.net/d", []), 0);
  assert.strictEqual(handoffDeadlineFor("not a url", hosts), 0);
});

check("the blocking listener exists only while there is a host for it", () => {
  assert.ok(wantsPreempt(["filekeeper.net"], true));
  // Nothing behind the wait: no hosts, no listener.
  assert.ok(!wantsPreempt([], true));
  assert.ok(!wantsPreempt(null, true));
  // And never where the browser cannot block in the first place.
  assert.ok(!wantsPreempt(["filekeeper.net"], false));
});

check("the app is asked again once the throttle has run out", () => {
  const asked = 1_000_000;
  assert.ok(!singleUseRefreshDue(asked + 1, asked));
  assert.ok(!singleUseRefreshDue(asked + SINGLE_USE_REFRESH_MS - 1, asked));
  assert.ok(singleUseRefreshDue(asked + SINGLE_USE_REFRESH_MS, asked));
  // A forced ask ignores the throttle: that is what forced means.
  assert.ok(singleUseRefreshDue(asked + 1, asked, true));
  // The first ask of a session has nothing to wait for.
  assert.ok(singleUseRefreshDue(Date.now(), 0));
});

/* ---------------- mediarecord.js ---------------- */

const stream = (url) => ({ url, kind: "stream", mime: "application/x-mpegurl", at: 1 });
const slice = (url) => ({ url, kind: "media", mime: "video/mp2t", at: 2 });

check("a full record gives up a fragment before it gives up a manifest", () => {
  const m = new Map();
  m.set("manifest", stream("manifest"));
  m.set("seg-1", slice("seg-1"));
  m.set("seg-2", slice("seg-2"));
  makeRoom(m);
  // The manifest is always the oldest thing here — it is fetched once, before
  // the first frame — so oldest-out would have thrown away the one entry that
  // describes the whole video.
  assert.ok(m.has("manifest"), "the manifest was dropped");
  assert.ok(!m.has("seg-1"), "the oldest fragment should have gone");
});

check("a record of nothing but manifests loses the oldest after all", () => {
  const m = new Map();
  m.set("a", stream("a"));
  m.set("b", stream("b"));
  makeRoom(m);
  assert.deepStrictEqual([...m.keys()], ["b"]);
});

check("the manifest survives a watch long enough to fill the record", () => {
  const m = new Map();
  noteMedia(m, stream("master.m3u8"));
  // Five minutes of playback at the rate a fragmented stream emits.
  for (let i = 0; i < MAX_SNIFFED * 3; i++) noteMedia(m, slice("seg-" + i));
  assert.ok(m.has("master.m3u8"), "the manifest was crowded out");
  assert.strictEqual(m.size, MAX_SNIFFED);
});

check("the newest is recorded rather than refused, and never twice", () => {
  const m = new Map();
  assert.ok(noteMedia(m, slice("a")), "a new URL should be recorded");
  // Refusing the newest went deaf: scroll far enough down a feed and the one
  // on screen is never recorded.
  assert.ok(!noteMedia(m, slice("a")), "the same URL should not be recorded twice");
  assert.strictEqual(m.size, 1);

  const small = new Map();
  noteMedia(small, slice("old"), 2);
  noteMedia(small, slice("mid"), 2);
  noteMedia(small, slice("new"), 2);
  assert.deepStrictEqual([...small.keys()], ["mid", "new"]);
});

/* ---------------- report ---------------- */

console.log("\n  " + passed + " passed, " + failures.length + " failed\n");
for (const f of failures) console.error("  FAIL: " + f);
process.exit(failures.length ? 1 : 0);
