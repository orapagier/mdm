"use strict";
/**
 * Tests for what the page-side recorder keeps out of Resource Timing.
 *
 * src/content/streams.js is the answer to a download that came back six
 * seconds long: the manifest describing a whole film is fetched once before
 * the first frame, and everything the background remembers is thrown away
 * every time Chromium stops the service worker. The page's own record survives
 * both, and this is the filter that decides what is worth keeping out of it.
 *
 * Which makes the filter load-bearing in one direction and merely untidy in
 * the other. A manifest it fails to recognise is the bug all over again; a
 * stray API call it keeps costs one candidate the app tries and discards. So
 * the cases below are weighted that way — every manifest shape seen in the
 * wild, and the near-misses that must not be mistaken for one.
 *
 * Run: node extension/test/streams.test.js
 */

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const SRC = path.join(__dirname, "..", "src", "content", "streams.js");
const text = fs.readFileSync(SRC, "utf8");

/* Only the deciding part is lifted. The rest of the file is a
 * PerformanceObserver and a message listener, neither of which exists outside a
 * document, and neither of which decides anything. */
const context = vm.createContext({ console });
const wanted = [
  /const MANIFEST_PATH = [^\n]*/,
  /const MANIFEST_HINT = [^\n]*/,
  /function looksLikeManifest\([\s\S]*?\n}/,
];
for (const re of wanted) {
  const m = re.exec(text);
  assert.ok(m, `streams.js no longer contains ${re}`);
  vm.runInContext(m[0], context, { filename: "streams.js (extracted)" });
}
const { looksLikeManifest } = context;

let passed = 0;
const failures = [];
function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failures.push(name + " — " + e.message);
  }
}

const keeps = (url) => assert.ok(looksLikeManifest(url), `missed: ${url}`);
const drops = (url) => assert.ok(!looksLikeManifest(url), `wrongly kept: ${url}`);

check("the ordinary shapes, however the address is dressed", () => {
  keeps("https://cdn.example.com/master.m3u8");
  keeps("https://cdn.example.com/hls/index-f1-v1-a1.m3u8");
  keeps("https://cdn.example.com/v/playlist.m3u8?token=abc&exp=123");
  keeps("https://cdn.example.com/stream.mpd");
  keeps("https://cdn.example.com/dash/manifest.mpd?s=1");
  keeps("https://cdn.example.com/audio.m3u");
});

check("a playlist with a path after it, which is how some CDNs sign one", () => {
  // The signature is a *path segment* after the playlist name rather than a
  // query, so anything anchored to the end of the address would miss it.
  keeps("https://cdn.example.com/master.m3u8/sig/9f2c1a");
});

check("the player's own token, where the extension is the only clue", () => {
  keeps("https://100.wowstreamingsofast.lol/XSSL_d67UmS6q9bU-EepsQK5U9.m3u8");
});

check("a playlist named by the query rather than by the path", () => {
  keeps("https://cdn.example.com/getlink?id=8812&type=m3u8");
  keeps("https://cdn.example.com/api/stream?format=hls");
});

check("segments are not the stream, whatever they end in", () => {
  // The reported download: one of these, complete, 2.7 MB, and six seconds of
  // a film. Keeping it here would put it back at the top of the candidates.
  drops("https://100.wowstreamingsofast.lol/XSSL_d67UmS6q9bU-EepsQK5U9.ts");
  drops("https://cdn.example.com/hls/seg-142-v1-a1.ts");
  drops("https://cdn.example.com/dash/chunk-stream0-00042.m4s");
  drops("https://cdn.example.com/init.mp4");
});

check("ordinary page traffic is left where it is", () => {
  drops("https://www.example.com/");
  drops("https://www.example.com/movie/moana-2");
  drops("https://cdn.example.com/app.js");
  drops("https://cdn.example.com/poster.jpg");
  drops("https://api.example.com/v1/episodes?id=7781");
  drops("https://cdn.example.com/video.mp4");
});

check("a name that merely contains the word is not a playlist", () => {
  // "index", "master" and "playlist" are the commonest words on the web. Only
  // the ones carrying a playlist extension count.
  drops("https://www.example.com/index.html");
  drops("https://cdn.example.com/playlist.json");
  drops("https://cdn.example.com/master.css");
});

check("nothing that is not an address the browser fetched over http", () => {
  drops("blob:https://www.example.com/9b1a-4c");
  drops("data:application/x-mpegurl;base64,I0VYVE0zVQ==");
  drops("");
  drops("about:blank");
});

console.log("\n  " + passed + " passed, " + failures.length + " failed\n");
for (const f of failures) console.error("  FAIL: " + f);
process.exit(failures.length ? 1 : 0);
