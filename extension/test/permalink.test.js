"use strict";
/**
 * Tests for finding the post a video belongs to.
 *
 * video-panel.js runs as a content script — it builds a shadow root and talks
 * to `browser.*`, so it cannot be loaded whole outside a page. But the part
 * worth testing is pure tree arithmetic, so the function is lifted out of the
 * source and run against a stub DOM. Lifted rather than copied: a test that
 * carried its own copy of the algorithm would keep passing after the real one
 * changed.
 *
 * Run: node extension/test/permalink.test.js
 */

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const SOURCE = path.join(__dirname, "..", "src", "content", "video-panel.js");
const text = fs.readFileSync(SOURCE, "utf8");

/** Lift one top-level-in-the-IIFE function out by name, by matching braces. */
function extract(name) {
  const start = text.indexOf(`  function ${name}(`);
  assert.notStrictEqual(start, -1, `${name} is not in ${path.basename(SOURCE)}`);
  let depth = 0;
  for (let i = text.indexOf("{", start); i < text.length; i++) {
    if (text[i] === "{") depth++;
    else if (text[i] === "}" && --depth === 0) return text.slice(start, i + 1);
  }
  throw new Error(`${name} has unbalanced braces`);
}

/** Lift a top-level-in-the-IIFE `const` out by name, value and all. */
function constant(name) {
  const m = new RegExp(`  const ${name}\\s*=\\s*([\\s\\S]*?);\\n`).exec(text);
  assert.ok(m, `${name} is not in ${path.basename(SOURCE)}`);
  return m[1];
}

/* ---------------- stub DOM ---------------- */

/**
 * Just enough of a document: elements know their parent, and the page knows
 * its anchors. `permalinkNear` asks for nothing else.
 */
let anchors = [];
const el = (tag) => ({ tag, children: [], parentElement: null });
function child(parent, node) {
  node.parentElement = parent;
  parent.children.push(node);
  return node;
}
/** A run of `n` nested elements, for standing in as a site's deep markup. */
function nest(parent, n) {
  let node = parent;
  for (let i = 0; i < n; i++) node = child(node, el("div"));
  return node;
}
function anchor(parent, href) {
  const a = child(parent, el("a"));
  a.getAttribute = (k) => (k === "href" ? href : null);
  anchors.push(a);
  return a;
}
function page() {
  anchors = [];
  return el("div");
}
const video = (parent) => child(parent, el("video"));

const context = vm.createContext({ URL, console });
vm.runInContext(
  `
  const location = { href: "https://www.facebook.com/" };
  const document = { querySelectorAll: (s) => (s === "a[href]" ? anchors : []) };
  const MEDIA_SECTION = ${constant("MEDIA_SECTION")};
  const NOT_AN_ID = ${constant("NOT_AN_ID")};
  const MAX_CLIMB = ${constant("MAX_CLIMB")};
  function absolute(url) { try { return new URL(url, location.href).href; } catch { return ""; } }
  ${extract("namesOneMedia")}
  ${extract("permalinkNear")}
  `,
  context,
  { filename: "video-panel.js (extracted)" }
);
const { permalinkNear, namesOneMedia } = context;
const FB = "https://www.facebook.com";

/* ---------------- harness ---------------- */

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
const run = (v) => {
  context.anchors = anchors;
  return permalinkNear(v);
};

/* ---------------- tests ---------------- */

check("picks the permalink of the post the video is in, not the first on the page", () => {
  // A feed is a column of posts; document order says nothing about which one
  // the player is sitting in.
  const root = page();
  const postA = child(root, el("div"));
  const postB = child(root, el("div"));
  anchor(nest(postA, 4), "/pageA/videos/1111/");
  anchor(nest(postB, 4), "/pageB/videos/2222/");
  assert.strictEqual(run(video(nest(postB, 20))), `${FB}/pageB/videos/2222/`);
});

check("reaches a link twenty levels above the player", () => {
  // The depth that used to be the ceiling. Facebook nests a feed video about
  // this far below its post.
  const root = page();
  anchor(nest(root, 2), "/page/posts/abc123/");
  assert.strictEqual(run(video(nest(root, 25))), `${FB}/page/posts/abc123/`);
});

check("a nearer post wins over one that merely encloses it", () => {
  const root = page();
  const outer = child(root, el("div"));
  anchor(nest(outer, 1), "/outer/videos/1/");
  const inner = nest(outer, 6);
  anchor(nest(inner, 1), "/inner/videos/2/");
  assert.strictEqual(run(video(nest(inner, 5))), `${FB}/inner/videos/2/`);
});

check("ignores links that name no single piece of media", () => {
  const root = page();
  const post = child(root, el("div"));
  anchor(nest(post, 2), "/someprofile/");
  anchor(nest(post, 2), "/hashtag/cats/");
  anchor(nest(post, 2), "/reel/9999/");
  assert.strictEqual(run(video(nest(post, 10))), `${FB}/reel/9999/`);
});

check("the page's own address is never offered back as the permalink", () => {
  // Handing the feed back is what produced "Unsupported URL: facebook.com/".
  const root = page();
  anchor(nest(root, 1), `${FB}/`);
  assert.strictEqual(run(video(nest(root, 5))), "");
});

check("no permalink anywhere is an empty answer, not a crash", () => {
  const root = page();
  anchor(nest(root, 1), "/just/a/profile/");
  assert.strictEqual(run(video(nest(root, 8))), "");
});

check("a link beyond the climb limit is not reached", () => {
  // The bound exists so a pathological page cannot make this walk forever;
  // past it the answer is honestly nothing rather than a wrong post.
  const root = page();
  anchor(nest(root, 1), "/page/videos/1/");
  assert.strictEqual(run(video(nest(root, 200))), "");
});

check("a relative permalink is resolved against the page", () => {
  const root = page();
  anchor(nest(root, 1), "/pageC/videos/7/");
  assert.ok(run(video(nest(root, 6))).startsWith("https://"));
});


/* ---------------- sections are not videos ---------------- */

/* Both of these were picked out of a real Facebook feed and handed to yt-dlp
 * as the video's address. They are navigation: a hashtag's videos, and the
 * Reels tab. A post's markup is full of links like them. */

const names = (href) => namesOneMedia(new URL(href, `${FB}/`));

for (const href of [
  "/watch/hashtag/onevoice27/?__cft__%5B0%5D=AZgngAWB6a86qnZEwEeC87YaNnEVFTC4",
  "/reel/?s=tab",
  "/watch/",
  "/watch/live/",
  "/videos/",
  "/reels/tab/",
  "/posts/",
  "/explore/",
]) {
  check(`${href} names no single video`, () => assert.strictEqual(names(href), false));
}

for (const href of [
  "/reel/1234567890",
  "/pageB/videos/2222/",
  "/watch/?v=987654321",
  "/user/status/1234567890",
  "/p/CxYz123/",
  "/shorts/dQw4w9WgXcQ",
  "/@someone/video/7212345678",
  "/permalink.php?story_fbid=123&id=456",
  "/video.php?v=789012",
]) {
  check(`${href} names one video`, () => assert.strictEqual(names(href), true));
}

check("a section link is passed over for the real permalink beside it", () => {
  // The hashtag sits in the post body, nearer the player than the timestamp
  // in the header — so being rejected outright is what matters, not distance.
  const root = page();
  const post = child(root, el("div"));
  anchor(nest(post, 2), "/watch/hashtag/onevoice27/");
  const header = nest(post, 1);
  anchor(header, "/pageB/videos/778899/");
  assert.strictEqual(run(video(nest(post, 12))), `${FB}/pageB/videos/778899/`);
});

check("a feed with only section links yields nothing rather than one of them", () => {
  const root = page();
  const post = child(root, el("div"));
  anchor(nest(post, 2), "/reel/?s=tab");
  anchor(nest(post, 2), "/watch/hashtag/x/");
  assert.strictEqual(run(video(nest(post, 10))), "");
});

/* ---------------- report ---------------- */

console.log("\n  " + passed + " passed, " + failures.length + " failed\n");
for (const f of failures) console.error("  FAIL: " + f);
process.exit(failures.length ? 1 : 0);
