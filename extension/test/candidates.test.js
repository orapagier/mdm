"use strict";
/**
 * Tests for choosing what a "Download this video" click stands for.
 *
 * Both functions run in contexts that cannot be loaded whole outside a
 * browser — `videoCandidates` lives among `browser.*` listeners, `mediaIn`
 * inside the content script's IIFE — so each is lifted out of its source and
 * run against stubs. Lifted rather than copied: a test carrying its own copy
 * would go on passing after the real one changed.
 *
 * Run: node extension/test/candidates.test.js
 */

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const SRC = path.join(__dirname, "..", "src");
/**
 * The window's own source. It makes the last of these decisions — which file
 * to offer when no page resolved — and gets it wrong in the same way, so it
 * is tested beside the two that feed it rather than not at all.
 */
const UI = path.join(__dirname, "..", "..", "ui");

/** Lift a function out of a source file by name, by matching braces. */
function lift(file, name, indent = "", dir = SRC) {
  const text = fs.readFileSync(path.join(dir, file), "utf8");
  for (const prefix of ["async function ", "function "]) {
    const start = text.indexOf(`${indent}${prefix}${name}(`);
    if (start === -1) continue;
    let depth = 0;
    for (let i = text.indexOf("{", start); i < text.length; i++) {
      if (text[i] === "{") depth++;
      else if (text[i] === "}" && --depth === 0) return text.slice(start, i + 1);
    }
    throw new Error(`${name} has unbalanced braces`);
  }
  throw new Error(`${name} is not in ${file}`);
}

/** Lift a top-level `const` out by name, value and all. */
function constant(file, name, indent = "", dir = SRC) {
  const text = fs.readFileSync(path.join(dir, file), "utf8");
  const m = new RegExp(`${indent}const ${name}\\s*=\\s*([\\s\\S]*?);\\n`).exec(text);
  assert.ok(m, `${name} is not in ${file}`);
  return m[1];
}

/* ------------------------------------------------------------------ *
 * Which file a grab means
 * ------------------------------------------------------------------ */

const bg = vm.createContext({ console, URL, atob, JSON });
vm.runInContext(
  `
  ${lift("util.js", "withoutByteRange")}
  const RANGE_PARAMS = ${constant("util.js", "RANGE_PARAMS")};
  const URL_FACTS = ${constant("util.js", "URL_FACTS")};
  ${lift("util.js", "decodeTag")}
  ${lift("util.js", "urlFacts")}
  ${lift("util.js", "pageForStream")}
  // The sniffer's record: tab id -> (url -> what was seen). Insertion order
  // is arrival order, which is exactly what the ranking has to survive.
  const TAB = 1;
  let tabMedia = new Map();
  function setSniffed(entries) {
    tabMedia = new Map([[TAB, new Map(entries.map((e) => [e.url, e]))]]);
  }
  // The browser's headers are fetched per candidate; irrelevant here.
  async function headersForUrl() { return []; }
  ${lift("background.js", "videoCandidates")}
  `,
  bg,
  { filename: "background.js (extracted)" }
);
const { videoCandidates, setSniffed } = bg;

const PAGE = "https://www.tiktok.com/";
/** TikTok's HEVC capability probe: two seconds, played at every page load. */
const WARMUP =
  "https://sf16-website-login.neutral.ttwstatic.com/obj/tiktok_web_login_static" +
  "/tiktok/webapp/main/webapp-desktop/playback1.mp4";
const REAL =
  "https://v16-webapp.tiktok.com/ad2adb4e/6a9b818c/video/tos/alisg/tos-alisg-pv-0037" +
  "/02eae647a8ba4110a03f1e3958039431/?a=1988&mime_type=video_mp4";

/* ---------------- Facebook's own account of a stream ---------------- */

const FB_PAGE = "https://www.facebook.com/";
/** The post the button was pressed on, and the one above it in the feed. */
const THIS_POST = "10155529876156509";
const OTHER_POST = "10155529876100000";

/**
 * A Facebook CDN URL, built the way Facebook builds them.
 *
 * Every file it serves carries `efg`: base64 JSON naming the post the stream
 * belongs to and the encode it is. The tags here are verbatim from a live
 * extraction — the audio track really is described as
 * `dash_v3_426_crf_23_main_3.0_frag_2_audio`, and it really does come back
 * as `Content-Type: video/mp4` like everything else, which is the whole
 * reason the URL has to be read at all.
 */
function fbcdn(name, tag, videoId) {
  // Assembled as text, not through JSON.stringify: a Facebook video id is
  // past 2^53, so building the payload out of a JavaScript number would
  // round it and quietly test a different id than the live one.
  const efg = Buffer.from(
    `{"vencode_tag":"${tag}","video_id":${videoId},"duration_s":44}`
  ).toString("base64url");
  return `https://video.xx.fbcdn.net/o1/v/t2/f2/m412/${name}.mp4?_nc_cat=106&efg=${efg}`;
}

const FB_AUDIO = fbcdn("AQPFerc", "dash_v3_426_crf_23_main_3.0_frag_2_audio", THIS_POST);
const FB_VIDEO = fbcdn("AQPZoRF", "dash_vp9-basic-gen2_1080p", THIS_POST);
const FB_OTHER = fbcdn("AQNmewD", "xpv_progressive.FACEBOOK..C3.400.sve_sd", OTHER_POST);

/**
 * The shape that actually did the damage: a whole file, picture and sound
 * together, belonging to the post above the one on screen — and naming no
 * post at all, because Facebook leaves `video_id` off its progressive URLs
 * and gives only an asset id, which nothing in the page markup matches.
 */
const FB_UNNAMED =
  "https://video.xx.fbcdn.net/o1/v/t2/f2/m412/AQNmewD.mp4?_nc_cat=111&efg=" +
  Buffer.from(
    '{"vencode_tag":"xpv_progressive.FACEBOOK..C3.400.sve_sd","xpv_asset_id":1058356515425744}'
  ).toString("base64url");

/* ------------------------------------------------------------------ *
 * What the page says about itself
 * ------------------------------------------------------------------ */

const panel = vm.createContext({ console, URL });
vm.runInContext(
  `
  const MEDIA_KEY = ${constant("content/video-panel.js", "MEDIA_KEY", "  ")};
  const MEDIA_URL = ${constant("content/video-panel.js", "MEDIA_URL", "  ")};
  const MAX_NODES = ${constant("content/video-panel.js", "MAX_NODES", "  ")};
  const LONG_ID = ${constant("content/video-panel.js", "LONG_ID", "  ")};
  const ID_KEY = ${constant("content/video-panel.js", "ID_KEY", "  ")};
  const MAX_CLIMB = ${constant("content/video-panel.js", "MAX_CLIMB", "  ")};
  ${lift("content/video-panel.js", "mediaIn", "  ")}
  ${lift("content/video-panel.js", "idsNear", "  ")}
  ${lift("content/video-panel.js", "mediaForIds", "  ")}
  const POST_URL = ${constant("content/video-panel.js", "POST_URL", "  ")};
  const MEDIA_SECTION = ${constant("content/video-panel.js", "MEDIA_SECTION", "  ")};
  const NOT_AN_ID = ${constant("content/video-panel.js", "NOT_AN_ID", "  ")};
  let location = { hostname: "www.tiktok.com", href: "https://www.tiktok.com/" };
  function setHost(h) { location = { hostname: h, href: "https://" + h + "/" }; }
  function setAddress(href) { location = { hostname: new URL(href).hostname, href }; }
  ${lift("content/video-panel.js", "permalinkFromId", "  ")}
  ${lift("content/video-panel.js", "namesId", "  ")}
  ${lift("content/video-panel.js", "statedMedia", "  ")}
  ${lift("content/video-panel.js", "namesOtherMedia", "  ")}
  ${lift("content/video-panel.js", "addressId", "  ")}
  `,
  panel,
  { filename: "video-panel.js (extracted)" }
);
const { mediaIn, idsNear, mediaForIds, permalinkFromId, namesId, setHost } = panel;
const { statedMedia, addressId, setAddress } = panel;
const MOST_NAMED = Number(constant("content/video-panel.js", "MOST_NAMED", "  "));

/* ------------------------------------------------------------------ *
 * What the window falls back to when no page resolves
 * ------------------------------------------------------------------ */

const win = vm.createContext({ console, URL });
vm.runInContext(
  `
  const GENERIC_TITLE = ${constant("video.js", "GENERIC_TITLE", "", UI)};
  const NAME_LIMIT = ${constant("video.js", "NAME_LIMIT", "", UI)};
  ${lift("video.js", "firstLine", "", UI)}
  ${lift("video.js", "kindSuffix", "", UI)}
  ${lift("video.js", "suggestedName", "", UI)}
  let info = null;
  let kind = "both";
  /** What the window would put in the "Save as" box for this extraction. */
  function nameFor(extraction) {
    info = extraction;
    return suggestedName();
  }
  const MIME_EXTENSION = ${constant("video.js", "MIME_EXTENSION", "", UI)};
  const MAX_STEM = ${constant("video.js", "MAX_STEM", "", UI)};
  ${lift("video.js", "nameFromUrl", "", UI)}
  ${lift("video.js", "directName", "", UI)}
  // The window's furniture, cut down to what the choice touches: a status
  // line, and the two boxes a direct file fills in.
  const fields = {};
  function $(id) { return (fields[id] ||= { value: "", hidden: true, title: "" }); }
  let status = { text: "", className: "" };
  function say(id, text, className) { if (id === "vid-status") status = { text, className }; }
  function fitWindow() {}
  let candidates = [];
  let direct = null;
  let directMedia = null;
  let guessed = null;
  let playedSeconds = 0;
  const lengthSlack = ${constant("video.js", "lengthSlack", "", UI)};
  ${lift("video.js", "verdict", "", UI)}
  ${lift("video.js", "offerToStart", "", UI)}
  ${lift("video.js", "takeFile", "", UI)}
  ${lift("video.js", "refuse", "", UI)}
  ${lift("video.js", "offerDirect", "", UI)}
  /** What the window would be showing after all of that. */
  function shown() {
    return {
      url: direct,
      name: $("vid-name").value,
      said: status.text,
      tone: status.className,
      // Whether a download can be begun from here at all.
      canStart: !$("vid-start").hidden,
      // The way forward offered when the window declines to answer.
      anyway: !$("vid-anyway").hidden,
      force: guessed ? guessed.url : null,
    };
  }
  /** Offer these files, and report what the window would show. */
  function offer(files, feed) {
    for (const k of Object.keys(fields)) delete fields[k];
    candidates = files.map((f) => ({ kind: "media", mime: "video/mp4", ...f }));
    direct = null;
    guessed = null;
    status = { text: "", className: "" };
    offerDirect("no page resolved", feed);
    return shown();
  }
  /** Press "Download it anyway". */
  function insist() {
    takeFile(guessed, "taken at your word", "hint bad");
    return shown();
  }
  /** Ask the length check what it makes of an extraction. */
  function judge(seconds, info) {
    playedSeconds = seconds;
    return verdict(info);
  }
  `,
  win,
  { filename: "video.js (extracted)" }
);
const { offer, insist, judge, nameFor } = win;

/* ---------------- harness ---------------- */

let passed = 0;
const failures = [];
function check(name, fn) {
  try {
    const r = fn();
    if (r && typeof r.then === "function") {
      return r.then(
        () => passed++,
        (e) => failures.push(name + " — " + e.message)
      );
    }
    passed++;
  } catch (e) {
    failures.push(name + " — " + e.message);
  }
  return Promise.resolve();
}

/* The lifted code builds its arrays in the vm's realm, where they are not
 * reference-equal to this file's Array; copying them back keeps the
 * assertions about content rather than about which realm made them. */
const urls = (list) => Array.from(list, (c) => c.url);
const media = (list) => Array.from(list).filter((c) => c.kind === "media").map((c) => c.url);
const found = (data) => Array.from(mediaIn(data));

/* ---------------- ranking ---------------- */

const tests = [
  check("the video being watched beats the warm-up clip fetched at page load", async () => {
    // Both are video/mp4 in the same tab, and the warm-up arrived first. Taken
    // in arrival order it won twice, and TikTok's two-second HEVC probe was
    // saved in place of what the user was watching.
    setSniffed([
      { url: WARMUP, mime: "video/mp4", kind: "media", at: 1000 },
      { url: REAL, mime: "video/mp4", kind: "media", at: 5000 },
    ]);
    const got = await videoCandidates({ pageUrl: PAGE, candidates: [] }, 1);
    assert.strictEqual(media(got)[0], REAL, "the warm-up clip was picked again");
  }),

  check("a manifest still outranks every plain file, however recent", async () => {
    setSniffed([
      { url: "https://cdn.example.com/master.m3u8", mime: "application/x-mpegurl", kind: "stream", at: 1000 },
      { url: REAL, mime: "video/mp4", kind: "media", at: 9000 },
    ]);
    const got = await videoCandidates({ pageUrl: PAGE, candidates: [] }, 1);
    assert.strictEqual(media(got)[0], "https://cdn.example.com/master.m3u8");
  }),

  check("what the player element itself is using leads the files", async () => {
    setSniffed([{ url: REAL, mime: "video/mp4", kind: "media", at: 9000 }]);
    const got = await videoCandidates(
      { pageUrl: PAGE, candidates: [{ url: "https://cdn.example.com/current.mp4", kind: "media" }] },
      1
    );
    assert.strictEqual(media(got)[0], "https://cdn.example.com/current.mp4");
  }),

  check("the audio track is not offered as the video", async () => {
    // Facebook plays a DASH video by fetching two files, and labels both
    // `video/mp4`. The audio arrived last, so newest-first put it in front,
    // and what was saved was a couple of hundred kilobytes that finished,
    // reported complete, and played as a black screen.
    setSniffed([
      { url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 },
      { url: FB_AUDIO, mime: "video/mp4", kind: "media", at: 9000 },
    ]);
    const got = await videoCandidates({ pageUrl: FB_PAGE, candidates: [] }, 1);
    assert.strictEqual(media(got)[0], FB_VIDEO, "the sound was offered as the video");
    // Still offered, just not first: it is better than nothing when the video
    // stream is the one that cannot be fetched.
    assert.ok(media(got).includes(FB_AUDIO), "the audio was dropped rather than ranked");
    assert.strictEqual(
      Array.from(got).find((c) => c.url === FB_AUDIO).audioOnly,
      true,
      "the window was not told the file is sound only"
    );
  }),

  check("a stream belonging to another post loses to this post's", async () => {
    // Scrolling a feed leaves the post above still fetching. Its file is
    // whole, plays perfectly, and is the wrong video — the one failure a
    // download manager cannot apologise its way out of.
    setSniffed([
      { url: FB_OTHER, mime: "video/mp4", kind: "media", at: 9000 },
      { url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 },
    ]);
    const got = await videoCandidates(
      { pageUrl: FB_PAGE, candidates: [], ids: [THIS_POST] },
      1
    );
    assert.strictEqual(media(got)[0], FB_VIDEO, "the post above it was downloaded");
  }),

  check("a named stream beats an unattributed file from up the feed", async () => {
    // Facebook names the post on its DASH streams and not on its progressive
    // ones, so the wrong video usually arrives anonymous while the right one
    // can be identified. Knowing one of the two is enough to order them.
    setSniffed([
      { url: FB_UNNAMED, mime: "video/mp4", kind: "media", at: 9000 },
      { url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 },
    ]);
    const got = await videoCandidates(
      { pageUrl: FB_PAGE, candidates: [], ids: [THIS_POST] },
      1
    );
    assert.strictEqual(media(got)[0], FB_VIDEO, "an unattributed file won anyway");
  }),

  check("with no post to compare against, a whole file beats half of one", async () => {
    // Nothing off the markup means no basis for preferring either post, and a
    // guess would only trade one wrong video for another. What can still be
    // said is that one of them is a complete file and the other is a DASH
    // track that will download to 100% and play with no sound.
    setSniffed([
      { url: FB_OTHER, mime: "video/mp4", kind: "media", at: 9000 },
      { url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 },
    ]);
    const got = await videoCandidates({ pageUrl: FB_PAGE, candidates: [] }, 1);
    assert.deepStrictEqual(media(got), [FB_OTHER, FB_VIDEO]);
  }),

  check("the right post's half beats the wrong post's whole", async () => {
    // The two rules conflict here, and the order they are applied in decides
    // which mistake gets made. Downloading the post above the one on screen is
    // the one that cannot be explained away; the right video without its sound
    // is at least the right video — and is what the page below is recovered
    // from, so it does not stay half a video for long.
    setSniffed([
      { url: FB_OTHER, mime: "video/mp4", kind: "media", at: 9000 },
      { url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 },
    ]);
    const got = await videoCandidates(
      { pageUrl: FB_PAGE, candidates: [], ids: [THIS_POST] },
      1
    );
    assert.strictEqual(media(got)[0], FB_VIDEO);
  }),

  check("a stream's own address yields the page that resolves it", async () => {
    // The whole point of reading `efg`. A Facebook feed publishes no permalink
    // for a reel and plays it through a blob nobody outside the page can
    // fetch, so every reading of the DOM comes up empty and the grab used to
    // fall through to saving the raw DASH stream — half a video. The file
    // names its own post, and that is a page yt-dlp extracts properly: every
    // quality, and the sound.
    setSniffed([{ url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 }]);
    const got = await videoCandidates({ pageUrl: FB_PAGE, candidates: [] }, 1);
    const pages = Array.from(got).filter((c) => c.kind === "page").map((c) => c.url);
    assert.deepStrictEqual(pages, [`https://www.facebook.com/watch/?v=${THIS_POST}`]);
    // And it leads the file it was recovered from, since a page is a choice of
    // qualities and the file is one stream of one of them.
    assert.strictEqual(urls(got)[0], `https://www.facebook.com/watch/?v=${THIS_POST}`);
  }),

  check("a recovered page survives however many permalinks the page offers", async () => {
    // It is the one candidate that can still work when every reading of the
    // DOM has failed, so a ceiling must not be able to drop it.
    setSniffed([{ url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 }]);
    const pages = Array.from({ length: 9 }, (_, i) => ({
      url: `https://www.facebook.com/some.one/videos/10000000000000${i}/`,
      kind: "page",
    }));
    const got = await videoCandidates({ pageUrl: FB_PAGE, candidates: pages }, 1);
    assert.ok(
      urls(got).includes(`https://www.facebook.com/watch/?v=${THIS_POST}`),
      "the address recovered from the stream was crowded out"
    );
    assert.ok(urls(got).includes(FB_VIDEO), "so was the only fetchable file");
  }),

  check("the file the player has open leads, however old", async () => {
    // Provenance, not arrival. The tab has been fetching for other posts the
    // whole time it has been scrolled, and the newest of those is not the one
    // under the button — but the `<video>` element's own src is, whenever it
    // has one, and that is a fact about the element rather than a reading of
    // the page around it.
    setSniffed([{ url: REAL, mime: "video/mp4", kind: "media", at: 9000 }]);
    const got = await videoCandidates(
      {
        pageUrl: PAGE,
        candidates: [{ url: "https://v16.tiktok.com/a/b/oPlayingRightNowAbcdef/", kind: "media", origin: "player" }],
      },
      1
    );
    assert.strictEqual(media(got)[0], "https://v16.tiktok.com/a/b/oPlayingRightNowAbcdef/");
    assert.strictEqual(
      Array.from(got).find((c) => c.origin === "player").url,
      "https://v16.tiktok.com/a/b/oPlayingRightNowAbcdef/",
      "the window was not told which file the player had open"
    );
  }),

  check("the player's own file outranks a whole file from another post", async () => {
    // The two signals in direct conflict: one file is complete and names a
    // post the markup does not know, the other is what the element is
    // actually playing. Playing wins — it is the only one tied to the click.
    setSniffed([{ url: FB_OTHER, mime: "video/mp4", kind: "media", at: 9000 }]);
    const got = await videoCandidates(
      {
        pageUrl: FB_PAGE,
        candidates: [{ url: "https://video.xx.fbcdn.net/o1/v/t2/AQwhatIsPlaying123.mp4", kind: "media", origin: "player" }],
        ids: [THIS_POST],
      },
      1
    );
    assert.strictEqual(media(got)[0], "https://video.xx.fbcdn.net/o1/v/t2/AQwhatIsPlaying123.mp4");
  }),

  check("every file says which post it belongs to, or says nothing", async () => {
    // Carried on the candidate rather than only used for the ranking, because
    // the window is where a file is fallen back to when no page resolves — and
    // "this is the video on screen" and "this is the best of what the tab was
    // fetching" are different things to tell someone about a download.
    setSniffed([
      { url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 },
      { url: FB_OTHER, mime: "video/mp4", kind: "media", at: 9000 },
      { url: FB_UNNAMED, mime: "video/mp4", kind: "media", at: 7000 },
    ]);
    const got = await videoCandidates(
      { pageUrl: FB_PAGE, candidates: [], ids: [THIS_POST] },
      1
    );
    const post = (url) => Array.from(got).find((c) => c.url === url).post;
    assert.strictEqual(post(FB_VIDEO), "this", "this post's own stream was not claimed");
    assert.strictEqual(post(FB_OTHER), "other", "a neighbour's stream passed as this one's");
    // Silent rather than negative: a file the site did not describe is not
    // evidence of anything, and calling it a neighbour's would be a guess.
    assert.strictEqual(post(FB_UNNAMED), "", "an unattributed file was given a post");
  }),

  check("with no post to compare against, no file claims to be one", async () => {
    setSniffed([{ url: FB_VIDEO, mime: "video/mp4", kind: "media", at: 5000 }]);
    const got = await videoCandidates({ pageUrl: FB_PAGE, candidates: [] }, 1);
    assert.strictEqual(Array.from(got).find((c) => c.url === FB_VIDEO).post, "");
  }),

  check("the player's own file is this post's whatever the markup knows", async () => {
    // The one candidate a feed cannot mislead: it is not a reading of the page
    // but what the element under the button has open.
    setSniffed([]);
    const playing = "https://v16.tiktok.com/a/b/oPlayingRightNowAbcdef/";
    const got = await videoCandidates(
      { pageUrl: PAGE, candidates: [{ url: playing, kind: "media", origin: "player" }] },
      1
    );
    assert.strictEqual(Array.from(got).find((c) => c.url === playing).post, "this");
  }),

  check("a byte range is taken off a file before it is offered", async () => {
    setSniffed([
      {
        url: "https://scontent.example.com/v/AQM9.mp4?oh=00_AQIX&bytestart=642858&byteend=3477824",
        mime: "video/mp4",
        kind: "media",
        at: 5000,
      },
    ]);
    const got = await videoCandidates({ pageUrl: PAGE, candidates: [] }, 1);
    assert.strictEqual(
      media(got)[0],
      "https://scontent.example.com/v/AQM9.mp4?oh=00_AQIX",
      "a slice of a stream was offered as the video"
    );
  }),

  check("pages never crowd the last file out of the list", async () => {
    // Every page here fails to resolve; the file is then the only thing left
    // that can work, and a ceiling that dropped it left nothing at all.
    setSniffed([{ url: REAL, mime: "video/mp4", kind: "media", at: 5000 }]);
    const pages = Array.from({ length: 9 }, (_, i) => ({
      url: `https://www.tiktok.com/@a/video/70000000000000000${i}`,
      kind: "page",
    }));
    const got = await videoCandidates({ pageUrl: PAGE, candidates: pages }, 1);
    assert.ok(got.length <= 6, "the ceiling stopped applying");
    assert.ok(urls(got).includes(REAL), "the only fetchable file was dropped");
  }),

  check("the page's own address is never offered back as a candidate", async () => {
    setSniffed([]);
    const got = await videoCandidates(
      { pageUrl: PAGE, candidates: [{ url: PAGE, kind: "page" }] },
      1
    );
    assert.deepStrictEqual(urls(got), []);
  }),

  check("a file carries the headers the browser would have sent; a page does not", async () => {
    setSniffed([{ url: REAL, mime: "video/mp4", kind: "media", at: 5000 }]);
    const got = await videoCandidates(
      { pageUrl: PAGE, candidates: [{ url: "https://www.tiktok.com/@a/video/7000000000000000000", kind: "page" }] },
      1
    );
    const page = got.find((c) => c.kind === "page");
    const file = got.find((c) => c.kind === "media");
    assert.strictEqual(page.referrer, "", "a page was given a referrer it does not need");
    assert.strictEqual(file.referrer, PAGE, "the file lost the page it came from");
  }),
];

/* ---------------- page state ---------------- */

tests.push(
  check("a video named in the page's state is found however deep it sits", () => {
    // The shape TikTok ships, cut down: the item lives several scopes below
    // the root and names its file under playAddr.
    const state = {
      __DEFAULT_SCOPE__: {
        "webapp.video-detail": {
          itemInfo: {
            itemStruct: {
              id: "7000000000000000000",
              video: {
                cover: "https://p16.example.com/cover.jpeg",
                playAddr: REAL,
                downloadAddr: REAL + "&dl=1",
              },
            },
          },
        },
      },
    };
    const seen = found(state);
    assert.ok(seen.includes(REAL), "playAddr was not read");
    assert.ok(seen.includes(REAL + "&dl=1"), "downloadAddr was not read");
    assert.ok(
      !seen.some((u) => u.includes("cover.jpeg")),
      "a thumbnail was read as the video"
    );
  })
);

tests.push(
  check("a key naming a list of addresses is followed into the list", () => {
    const state = { video: { bitrateInfo: [{ PlayAddr: { UrlList: [REAL, REAL + "&x=2"] } }] } };
    assert.deepStrictEqual(found(state), [REAL, REAL + "&x=2"]);
  })
);

tests.push(
  check("a plain .mp4 is taken whatever it is filed under", () => {
    assert.deepStrictEqual(found({ anything: { at: "https://example.com/clip.mp4?t=1" } }), [
      "https://example.com/clip.mp4?t=1",
    ]);
  })
);

tests.push(
  check("nothing but URLs comes back", () => {
    assert.deepStrictEqual(found({ playAddr: "not a url", contentUrl: "/relative.mp4" }), []);
  })
);

tests.push(
  check("one video's whole bitrate ladder survives the ceiling", () => {
    // The shape yt-dlp reports for a single TikTok post: three rungs, two
    // addresses each. A ceiling of four rejected exactly the page the state
    // was being read for, so the ceiling has to clear a ladder like this.
    const one = {
      video: {
        playAddr: REAL,
        downloadAddr: REAL + "&dl=1",
        bitrateInfo: [
          { PlayAddr: { UrlList: [REAL + "&r=1", REAL + "&r=2"] } },
          { PlayAddr: { UrlList: [REAL + "&r=3", REAL + "&r=4"] } },
          { PlayAddr: { UrlList: [REAL + "&r=5", REAL + "&r=6"] } },
        ],
      },
    };
    assert.strictEqual(found(one).length, 8, "a single post names eight addresses");
    assert.ok(MOST_NAMED >= 8, "the ceiling would cut a single post's ladder short");
  })
);

tests.push(
  check("a state blob big enough to hang the page is walked no further", () => {
    const wide = { items: Array.from({ length: 300000 }, (_, i) => ({ n: i })) };
    const started = Date.now();
    found(wide);
    assert.ok(Date.now() - started < 4000, "the walk had no ceiling");
  })
);

/* ---------------- identifying the post on screen ---------------- */

/** A stub element chain: a player nested inside a labelled feed row. */
function nest(labels) {
  let node = null;
  for (const attrs of labels) {
    node = { ...attrs, parentElement: node };
  }
  return node;
}

tests.push(
  check("the post's id is read off the markup wrapping the player", () => {
    // The shape a feed uses to keep track of its rows: the player's wrapper is
    // named after the post. It is the only thing on a home feed that says
    // which post the player is currently inside.
    const video = nest([
      { id: "main-content-homepage_hot" },
      { id: "column-item-video-container-7678211224177282322" },
      { id: "xgwrapper-0-7678211224177282322" },
      { id: "" },
    ]);
    assert.deepStrictEqual(Array.from(idsNear(video)), ["7678211224177282322"]);
  })
);

tests.push(
  check("the nearest label wins over the one further out", () => {
    const video = nest([
      { id: "feed-99999999999999999" },
      { id: "item-7678211224177282322" },
      { id: "" },
    ]);
    assert.strictEqual(Array.from(idsNear(video))[0], "7678211224177282322");
  })
);

tests.push(
  check("a data attribute carries the id just as well as an element id", () => {
    const video = nest([{ dataset: { itemId: "7678211224177282322" } }, { id: "" }]);
    assert.ok(Array.from(idsNear(video)).includes("7678211224177282322"));
  })
);

tests.push(
  check("an id is not found inside a longer id", () => {
    // The bug that downloaded a stranger's video. Ids are long runs of digits
    // and they sit in a URL among other digits, so a substring test lets one
    // id match inside another — and the "permalink carrying this post's id"
    // turned out to be the permalink of a different post further down the
    // feed, which resolved cleanly and downloaded perfectly.
    const other = "https://www.tiktok.com/@a/video/7678211224177282322";
    assert.ok(!namesId(other, "78211224"), "an id matched inside another id");
    assert.ok(!namesId(other, "7678211224177282"), "a prefix matched as the whole");
    assert.ok(namesId(other, "7678211224177282322"), "the real id stopped matching");
    // Delimited by anything that is not a digit, wherever it sits.
    assert.ok(namesId("https://x.test/p/1234567890123456?s=1", "1234567890123456"));
  })
);

tests.push(
  check("a number that is not long enough to be a post id is not one", () => {
    // Thirteen digits is a millisecond timestamp, which every feed is full of.
    const video = nest([{ dataset: { at: "1756900000000", n: "99999999" } }, { id: "" }]);
    assert.deepStrictEqual(Array.from(idsNear(video)), []);
  })
);

tests.push(
  check("short numbers in the markup are not post ids", () => {
    const video = nest([{ id: "col-3" }, { dataset: { index: "12" } }, { id: "" }]);
    assert.deepStrictEqual(Array.from(idsNear(video)), []);
  })
);

tests.push(
  check("one post is picked out of a feed's whole state by its id", () => {
    // The case the home feed turns on: the state describes every post, and
    // only the id read off the markup says which of them is on screen.
    const feed = {
      itemList: [
        { id: "7000000000000000001", video: { playAddr: "https://cdn.example.com/wrong-a.mp4" } },
        { id: "7678211224177282322", video: { playAddr: REAL } },
        { id: "7000000000000000002", video: { playAddr: "https://cdn.example.com/wrong-b.mp4" } },
      ],
    };
    const got = Array.from(mediaForIds(feed, ["7678211224177282322"]));
    assert.deepStrictEqual(got, [REAL], "the wrong post's video was taken");
  })
);

tests.push(
  check("an id the state does not mention yields nothing rather than a guess", () => {
    const feed = { itemList: [{ id: "7000000000000000001", video: { playAddr: REAL } }] };
    assert.deepStrictEqual(Array.from(mediaForIds(feed, ["7678211224177282322"])), []);
  })
);

tests.push(
  check("with no id to go on, a feed's state is not read at all", () => {
    const feed = { itemList: [{ id: "7000000000000000001", video: { playAddr: REAL } }] };
    assert.deepStrictEqual(Array.from(mediaForIds(feed, [])), []);
  })
);

tests.push(
  check("a feed's state is not read whole because the address names a post", () => {
    // The reading this replaces asked the address bar whether the page was
    // about one video. A feed rewrites the bar to the post you scroll to, so
    // it answered yes while the state behind it still described the rows
    // loaded when the page opened — and the first of those was handed over as
    // the video on screen, a stranger's post, downloaded in full.
    setAddress("https://www.tiktok.com/@someone/video/7678211224177282322");
    const feed = {
      itemList: [
        { id: "7000000000000000001", video: { playAddr: "https://cdn.example.com/further-down.mp4" } },
        { id: "7000000000000000002", video: { playAddr: "https://cdn.example.com/further-still.mp4" } },
      ],
    };
    assert.deepStrictEqual(Array.from(statedMedia(feed, ["7678211224177282322"])), []);
  })
);

tests.push(
  check("the record the markup pointed at is taken out of that same feed", () => {
    const feed = {
      itemList: [
        { id: "7000000000000000001", video: { playAddr: "https://cdn.example.com/further-down.mp4" } },
        { id: "7678211224177282322", video: { playAddr: REAL } },
      ],
    };
    assert.deepStrictEqual(Array.from(statedMedia(feed, ["7678211224177282322"])), [REAL]);
  })
);

tests.push(
  check("a blob that names nobody else is still read whole", () => {
    // A page about one video has no need to say whose video it is, and the
    // state of one is where a site that stopped writing its markup out loud
    // keeps the only address the file has. Nothing here belongs to anyone
    // else, so there is nobody to confuse it with.
    const one = { props: { video: { playAddr: REAL, downloadAddr: REAL + "&dl=1" } } };
    assert.deepStrictEqual(Array.from(statedMedia(one, ["7678211224177282322"])), [
      REAL,
      REAL + "&dl=1",
    ]);
  })
);

tests.push(
  check("a post's own page is read whole through the id in its address", () => {
    // The state names the post, so the blob names somebody — and what says it
    // is *this* somebody is the address, which on a page about one video is
    // the video's own. That is the id the record is matched on when the markup
    // around the player carried none.
    const page = {
      itemInfo: { itemStruct: { id: "7678211224177282322", video: { playAddr: REAL } } },
    };
    setAddress("https://www.tiktok.com/@someone/video/7678211224177282322");
    assert.deepStrictEqual(Array.from(statedMedia(page, [addressId()])), [REAL]);
  })
);

tests.push(
  check("the id in the page's own address is read, and only where there is one", () => {
    setAddress("https://www.tiktok.com/@someone/video/7675369248918686984");
    assert.strictEqual(addressId(), "7675369248918686984");
    setAddress("https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert.strictEqual(addressId(), "dQw4w9WgXcQ");
    // A feed, a hashtag listing and a tab name no video and never have.
    setAddress("https://www.tiktok.com/");
    assert.strictEqual(addressId(), "");
    setAddress("https://www.facebook.com/watch/hashtag/onevoice27");
    assert.strictEqual(addressId(), "");
    setAddress("https://www.facebook.com/reel/?s=tab");
    assert.strictEqual(addressId(), "");
  })
);

tests.push(
  check("a feed video is addressed by its id when the feed publishes no links", () => {
    // Measured on the live home feed: zero post links, a blob: src, and no
    // media in the page state. The id on the row's markup is the only thread,
    // and this is the address it leads to — verified to resolve to 16 formats.
    setHost("www.tiktok.com");
    assert.strictEqual(
      permalinkFromId(["7650155054757858568"]),
      "https://www.tiktok.com/@i/video/7650155054757858568"
    );
  })
);

tests.push(
  check("the nearest id is the one addressed, not the feed around it", () => {
    setHost("www.tiktok.com");
    assert.ok(permalinkFromId(["7650155054757858568", "99999999999999999"]).endsWith("7650155054757858568"));
  })
);

tests.push(
  check("nothing is built for a site with no rule, or with no id", () => {
    setHost("www.example.com");
    assert.strictEqual(permalinkFromId(["7650155054757858568"]), "");
    setHost("www.tiktok.com");
    assert.strictEqual(permalinkFromId([]), "");
  })
);

tests.push(
  check("the address built is one the picker will ask about first", () => {
    // looksSpecific in the window and namesOneMedia here have to agree that
    // this names one video, or it would be tried after the feed it came from.
    setHost("www.tiktok.com");
    const built = permalinkFromId(["7650155054757858568"]);
    assert.ok(/\/video\/\d{8,}$/.test(built), "the built address names no video");
  })
);

/* ---------------- the window's last resort ---------------- */

/** The post on screen, a neighbour the feed has preloaded, and what plays. */
const MINE = "https://v16-webapp-prime.tiktok.com/video/tos/alisg/oOnScreenRightNowAbc/";
const NEIGHBOUR = "https://v16-webapp-prime.tiktok.com/video/tos/alisg/oFivePostsFurtherDown/";

tests.push(
  check("a file nothing ties to the post on screen is not offered at all", () => {
    // The grab this is about. Every page failed, the tab held one file — a
    // post five rows further down, which a feed preloads as a matter of course
    // — and the window filled it in under "the file the page is playing will
    // be downloaded as it is", with Start sitting right there. It downloaded
    // perfectly and was somebody else's video. Nothing on the page can tell
    // those two apart, so the window no longer answers: it says so, and offers
    // to ask again.
    const got = offer([{ url: NEIGHBOUR, origin: "tab", post: "" }], true);
    assert.strictEqual(got.url, null, "a guess was filled in anyway");
    assert.strictEqual(got.canStart, false, "Start was left live over a guess");
    assert.strictEqual(got.tone, "hint bad");
    assert.ok(got.anyway, "no way forward was offered");
  })
);

tests.push(
  check("the guess is still there for anyone who insists", () => {
    // Declining to answer is not the same as withholding the file. Somebody
    // who would rather have the guess than nothing can say so, and then it is
    // theirs — with the name filled in and the reason still on the line.
    offer([{ url: NEIGHBOUR, origin: "tab", post: "" }], true);
    const got = insist();
    assert.strictEqual(got.url, NEIGHBOUR, "the file was not kept for the asking");
    assert.strictEqual(got.canStart, true, "it could be taken but not started");
    assert.strictEqual(got.anyway, false, "the offer stayed up after being taken");
    assert.strictEqual(got.tone, "hint bad", "an insisted-on guess is still a guess");
  })
);

tests.push(
  check("a feed that has rewritten its address is still a feed", () => {
    // TikTok pushes the current post's URL into the address bar as you scroll,
    // so the page reads as one video's own page while the tab is busy fetching
    // six. Counting what is actually there catches what the address cannot.
    const others = Array.from({ length: 5 }, (_, i) => ({
      url: NEIGHBOUR + i,
      origin: "tab",
      post: "",
    }));
    const got = offer(others, false);
    assert.strictEqual(got.url, null, "one of six files was picked as the answer");
    assert.ok(got.anyway);
  })
);

tests.push(
  check("one file in the whole tab is the page's own video", () => {
    // The sniffer doing the job it exists for: a site whose player runs on
    // MediaSource and whose page yt-dlp cannot read. There is nothing to guess
    // between, so there is no guess.
    const got = offer([{ url: MINE, origin: "tab", post: "" }], false);
    assert.strictEqual(got.url, MINE);
    assert.strictEqual(got.tone, "hint");
  })
);

tests.push(
  check("two halves of one video are one video, not two", () => {
    // A DASH page fetches a picture track and a sound track for the same post.
    // Counting those as a choice would refuse every such page.
    const got = offer(
      [
        { url: MINE, origin: "tab", post: "", partial: true },
        { url: MINE + "-audio", origin: "tab", post: "", partial: true, audioOnly: true },
      ],
      false
    );
    assert.strictEqual(got.url, MINE, "half a video was withheld as a guess");
    assert.ok(/no sound/.test(got.said), got.said);
  })
);

tests.push(
  check("a page about one video says nothing of the sort", () => {
    // Nothing is being guessed at here: one video on the page, and the file
    // the tab fetched for it is that video's. Warning about it would cry wolf
    // on every ordinary site.
    const got = offer([{ url: MINE, origin: "tab", post: "" }], false);
    assert.strictEqual(got.url, MINE);
    assert.strictEqual(got.tone, "hint");
    assert.ok(/No quality to choose/.test(got.said), got.said);
  })
);

tests.push(
  check("the post's own file leads a neighbour's, whatever order they arrived in", () => {
    const got = offer(
      [
        { url: NEIGHBOUR, origin: "tab", post: "other" },
        { url: MINE, origin: "tab", post: "this" },
      ],
      true
    );
    assert.strictEqual(got.url, MINE, "a neighbour's video was offered first");
    assert.strictEqual(got.tone, "hint");
    assert.ok(/right video/.test(got.said), got.said);
  })
);

tests.push(
  check("the file the player has open leads even that, and says why", () => {
    const got = offer(
      [
        { url: NEIGHBOUR, origin: "tab", post: "this" },
        { url: MINE, origin: "player", post: "this" },
      ],
      true
    );
    assert.strictEqual(got.url, MINE);
    assert.ok(/player itself has open/.test(got.said), got.said);
  })
);

tests.push(
  check("a file known to be a neighbour's is never the answer either", () => {
    const got = offer([{ url: NEIGHBOUR, origin: "tab", post: "other" }], true);
    assert.strictEqual(got.url, null);
    assert.strictEqual(got.canStart, false);
    assert.ok(got.anyway);
  })
);

tests.push(
  check("half a video is still called half a video", () => {
    // The identity of a file and its completeness are different questions, and
    // sorting by the first must not stop the second being said out loud: a
    // DASH track downloads to 100% and plays as a black screen.
    const got = offer(
      [{ url: MINE, origin: "tab", post: "this", partial: true, audioOnly: true }],
      true
    );
    assert.strictEqual(got.url, MINE);
    assert.strictEqual(got.tone, "hint bad");
    assert.ok(/sound track on its own/.test(got.said), got.said);
  })
);

tests.push(
  check("a page the same length as the player is the video on screen", () => {
    // The check that does not depend on reading the page correctly. yt-dlp
    // rounds and the element does not, so they never agree to the digit.
    assert.strictEqual(judge(6.033, { duration: 6, matchesStream: null }), "right");
  })
);

tests.push(
  check("a page of a different length is a different video, and is dropped", () => {
    // The failure nothing else catches: the post next door in the feed
    // extracts just as cleanly, with a real title and a real thumbnail, and
    // the only thing it almost never shares is its running time.
    assert.strictEqual(judge(6.033, { duration: 21, matchesStream: null }), "wrong");
    // Whichever way round they are.
    assert.strictEqual(judge(21.5, { duration: 6, matchesStream: null }), "wrong");
  })
);

tests.push(
  check("a long video is allowed to drift, a short one is not", () => {
    // Two seconds, or two per cent, whichever is the larger: an extractor and
    // a container disagree by more on an hour than on a clip, and rejecting a
    // forty-minute talk over a second and a half would be nonsense.
    assert.strictEqual(judge(3600, { duration: 3612, matchesStream: null }), "right");
    assert.strictEqual(judge(3600, { duration: 3300, matchesStream: null }), "wrong");
    assert.strictEqual(judge(30, { duration: 33, matchesStream: null }), "wrong");
  })
);

tests.push(
  check("with no length on either side there is nothing to say", () => {
    // A player that never loaded metadata, or an extractor that reports no
    // duration. The page may well be right; what it is not is checked.
    assert.strictEqual(judge(0, { duration: 6, matchesStream: null }), "unconfirmed");
    assert.strictEqual(judge(6, { duration: 0, matchesStream: null }), "unconfirmed");
  })
);

tests.push(
  check("the exact file the player pulled outranks every other reading", () => {
    assert.strictEqual(judge(0, { duration: 0, matchesStream: true }), "right");
  })
);

tests.push(
  check("a length that agrees does not overrule a file check that disagrees", () => {
    // The trade that brought the wrong video back. "None of these formats is
    // what the tab pulled" is a weak no — a DASH player fetches fragments an
    // extractor never offers — but it is a measurement, and the lengths
    // agreeing is a coincidence: feed clips run six to fifteen seconds, so a
    // neighbouring post lands inside the slack often enough to matter. Letting
    // the coincidence win bought a few seconds and gave back the one failure
    // this window exists to prevent.
    assert.strictEqual(judge(6.033, { duration: 6, matchesStream: false }), "unconfirmed");
    // A length that disagrees still settles it the other way: the file check
    // withholds confirmation, it does not grant it.
    assert.strictEqual(judge(6.033, { duration: 21, matchesStream: false }), "wrong");
  })
);

tests.push(
  check("an opaque CDN path still comes out as a name with an extension on it", () => {
    const got = offer([{ url: MINE, origin: "player", post: "this" }], false);
    assert.strictEqual(got.name, "oOnScreenRightNowAbc.mp4");
  })
);

/* ---------------- what a download is called ---------------- */

tests.push(
  check("a video is named after what the poster wrote, not after the poster", () => {
    // Signed in, Facebook titles every video on it "Video" and puts the post's
    // own words in the description. Naming these after the uploader filed a
    // page's whole output under one name with an id after it — "Ka-Banat
    // Online-News Channel Video 1393340332303393" — which says who posted it
    // and nothing whatever about which video it is. Verbatim from a live
    // extraction, title and description both.
    assert.strictEqual(
      nameFor({
        title: "Video",
        description:
          "Sunog sa bukirang bahin ang nahitabo sakop sa Purok 1 Tres de Mayo, Barangay " +
          "Bad-as, Placer, Surigao del Norte. Nahitabo ning kahapunon Septyembre 3, 2026.",
        uploader: "Ka-Banat Online-News Channel",
        id: "1393340332303393",
      }),
      "Sunog sa bukirang bahin ang nahitabo sakop sa Purok 1 Tres de Mayo, Barangay " +
        "Bad-as, Placer, Surigao del Norte"
    );
  })
);

tests.push(
  check("a real title is left alone", () => {
    assert.strictEqual(
      nameFor({ title: "this dance is so #cute #fyp #swag", description: "", uploader: "x", id: "1" }),
      "this dance is so #cute #fyp #swag"
    );
  })
);

tests.push(
  check("with nothing written anywhere, who posted it and its id still say which", () => {
    assert.strictEqual(
      nameFor({ title: "Video", description: "", uploader: "David Bombal", id: "1102368352460029" }),
      "David Bombal Video 1102368352460029"
    );
  })
);

tests.push(
  check("a description that is a paragraph is cut to a name, not to a byte count", () => {
    // The first sentence, where there is one early enough to read as a title.
    assert.strictEqual(
      nameFor({ title: "", description: "Kate, you work. Then a second sentence follows it.", uploader: "", id: "" }),
      "Kate, you work"
    );
    // An opener too short to be a name is not one; the line stands instead.
    assert.strictEqual(
      nameFor({ title: "", description: "Hi. Kate, you work at the gym now", uploader: "", id: "" }),
      "Hi. Kate, you work at the gym now"
    );
    // A path separator is not a name; nothing else about the words is touched.
    assert.strictEqual(
      nameFor({ title: "AC/DC live", description: "", uploader: "", id: "" }),
      "AC DC live"
    );
  })
);

/* ---------------- report ---------------- */

Promise.all(tests).then(() => {
  console.log("\n  " + passed + " passed, " + failures.length + " failed\n");
  for (const f of failures) console.error("  FAIL: " + f);
  process.exit(failures.length ? 1 : 0);
});
