"use strict";

/**
 * The standalone download window.
 *
 * Two views in one window: pick a quality, then watch it download. The main
 * app is never raised — a grab from the browser should feel like IDM's little
 * download box, not like launching a program.
 *
 * `$`, `bytes`, `rate`, `duration` and `eta` come from format.js.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

let settings = null;

/* ------------------------------------------------------------------ *
 * Chrome
 * ------------------------------------------------------------------ */

let toastTimer = null;
function toast(message) {
  const el = $("toast");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 2600);
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e));
    return null;
  }
}

/** Set an element's text and hide it entirely when there is nothing to say. */
function say(id, text, className) {
  const el = $(id);
  el.textContent = text || "";
  if (className !== undefined) el.className = className;
  el.hidden = !text;
}

function closeWindow() {
  getCurrentWindow().close();
}

/**
 * Ask for a taller window when what is laid out does not fit.
 *
 * How much of a window's height the desktop keeps for its own title bar is not
 * something the backend can know — it differs by desktop and theme, and here
 * it comes out of the size the backend asked for. So nothing guesses: the body
 * reports its own shortfall in its own coordinates and the window grows by
 * exactly that. Self-limiting, since a window that fits has no shortfall left
 * to report.
 */
async function fitWindow() {
  const body = document.querySelector(".pane-body");
  // A window manager applies a resize when it feels like it, and the answer
  // may not be the size that was asked for, so measure again afterwards and
  // keep asking until there is nothing left to ask for. Bounded, because a
  // desktop that refuses to grow the window must not be argued with for ever.
  for (let attempt = 0; attempt < 5; attempt++) {
    await new Promise(requestAnimationFrame);
    const short = body.scrollHeight - body.clientHeight;
    if (short <= 1) return;
    await invoke("fit_window", { grow: short }).catch(() => {});
    await new Promise((done) => setTimeout(done, 120));
  }
}

/* ------------------------------------------------------------------ *
 * Picking a format
 * ------------------------------------------------------------------ */

let info = null;
let kind = "both";
let chosen = null;
/** Other URLs this grab could resolve through, from the extension. */
let candidates = [];
/**
 * A media file to fetch outright, set only when no page would resolve.
 * It bypasses yt-dlp: there is nothing left to extract, and on a machine
 * without yt-dlp at all it is the only way the grab can still work.
 */
let direct = null;
/* The candidate `direct` came from, kept whole: a file fetched outside the
 * browser needs the headers the browser would have sent with it. */
let directMedia = null;
/** Bumped on every reset, so a probe that lands late cannot paint over the
 *  page the window has since been pointed at. */
let probeSeq = 0;
/**
 * How long the player under the button said its video is, or 0 if it never
 * said. The one fact about the video on screen that is not a reading of the
 * page — see `verdict`.
 */
let playedSeconds = 0;
/** The page title the grab arrived with, so "Try again" can say it again. */
let pageTitle = "";
/** A file offered only after the user insists; see `refuse`. */
let guessed = null;

/**
 * Which tabs a format belongs under.
 *
 * A video-only format is still a "video + audio" choice: picking one pairs it
 * with the best audio track and yt-dlp muxes the two. DASH sites — which is
 * now most of them, YouTube and Facebook included — serve *only* separate
 * streams, so grouping by what a format literally contains would leave the one
 * tab people actually want permanently empty.
 */
function tabsFor(f) {
  const hasVideo = f.vcodec !== "none";
  const hasAudio = f.acodec !== "none";
  if (hasVideo && hasAudio) return ["both"];
  if (hasVideo) return ["both", "video"];
  return ["audio"];
}

/** Is this row one that has to borrow an audio track from elsewhere? */
function needsAudio(f) {
  return kind === "both" && f.acodec === "none";
}

function heightOf(f) {
  const m = /(\d+)\s*[x×]\s*(\d+)/.exec(f.resolution || "");
  return m ? Number(m[2]) : 0;
}

function codecName(c) {
  if (!c || c === "none") return "";
  // yt-dlp reports full profile strings like "avc1.640028"; the family is the
  // only part worth showing.
  return c.split(".")[0].replace("mp4a", "aac").replace("avc1", "h264");
}

/**
 * Video codecs a Linux desktop is not equipped to decode out of the box.
 *
 * HEVC is patent-encumbered, so Fedora and most other distributions ship no
 * decoder for it: GStreamer here has an h265 *parser* and no h265 decoder at
 * all, which is why Firefox, GNOME Videos and everything else built on it play
 * such a file as sound over a black screen. TikTok serves the same video twice,
 * 1080p in HEVC and 720p in H.264, and describes the HEVC as the better one —
 * so the picker offered it, the download worked perfectly, and what came out
 * was indistinguishable from having downloaded only the audio.
 *
 * A judgement about this machine, so it is only ever a default and a warning:
 * the row stays selectable for anyone whose player handles it.
 */
const UNDECODABLE = /^(?:h265|hev1|hvc1|hevc)$/i;

function playable(f) {
  return !UNDECODABLE.test(codecName(f.vcodec));
}

function describe(f) {
  const bits = [];
  if (f.ext) bits.push(f.ext);
  const v = codecName(f.vcodec);
  const a = codecName(f.acodec);
  if (v) bits.push(v);
  if (a) bits.push(a);
  // Say so rather than leave a silent-looking row in the video+audio tab.
  else if (needsAudio(f)) bits.push("+ best audio");
  if (f.note) bits.push(f.note);
  if (!playable(f)) bits.push("may play without picture here");
  return bits.join(" · ");
}

function label(f) {
  if (f.vcodec === "none") {
    return f.tbr ? `${Math.round(f.tbr)} kbps` : "audio";
  }
  const h = heightOf(f);
  return h ? `${h}p` : f.resolution || f.formatId;
}

/**
 * The fallback covers a format that turns out to carry sound already, or a
 * page with no separate audio stream to pair with.
 */
function formatExpression(f) {
  return needsAudio(f) ? `${f.formatId}+bestaudio/${f.formatId}` : f.formatId;
}

/**
 * What the name has to say about the pick.
 *
 * Two picks from one page are two different files, and the extension does not
 * always tell them apart: opus audio and an AV1+opus mux of the same video are
 * both `.webm`. Landing them on one name is how an audio-only download ends up
 * reporting the video that is already saved.
 */
function kindSuffix() {
  return kind === "audio" ? "_audio" : kind === "video" ? "_video" : "";
}

/**
 * The track `bestaudio` will resolve to — the top of the audio tab.
 *
 * Bitrate is what yt-dlp goes by and what the audio tab is ordered by, so in
 * all but the odd case this is the same track. Size settles a tie, because a
 * site that states no bitrate — Facebook does not — would otherwise leave the
 * order untouched and hand back whichever track the extractor happened to
 * list first, which by yt-dlp's convention is the worst one.
 */
function bestAudio() {
  return info.formats
    .filter((x) => tabsFor(x).includes("audio"))
    .sort(
      (a, b) => (b.tbr || 0) - (a.tbr || 0) || (b.filesize || 0) - (a.filesize || 0)
    )[0];
}

/**
 * What the whole job will weigh, muxing aside.
 *
 * A row in the video+audio tab is mostly a picture with no sound in it: the
 * pick is paired with `bestaudio` and the two are muxed, so the file that
 * lands weighs both. Reporting the video stream alone is what offered a 27
 * minute video at 16 MB and then saved 43 MB of it — the audio is a fixed
 * weight, so the thinner the picked quality the further out the number is.
 *
 * The download strip wants the same figure for a second reason: yt-dlp
 * fetches the video stream and then the audio stream, so what the downloader
 * can report mid-job is only the stream in flight, and a bar scaled to that
 * rescales downwards — 99% back to 67% — the moment the audio starts.
 */
function expectedSize(f) {
  if (!f.filesize) return null;
  if (!needsAudio(f)) return f.filesize;
  // Being a little under is what the engine's floor is there for.
  return f.filesize + (bestAudio()?.filesize || 0);
}

/**
 * Titles that name a *kind* of thing rather than a particular one.
 *
 * Facebook calls every reel "Video". Taken as a name that lands the first one
 * as Video.mp4 and every one after it as Video_2, Video_3 — nothing on disk
 * saying which post any of them came from, and no way to tell whether the one
 * already saved is the one being fetched again.
 */
const GENERIC_TITLE = /^(?:video|watch|reel|reels|post|photo|clip|untitled|home)$/i;

/** As long a name as is worth having, and the ceiling the stem is cut to. */
const NAME_LIMIT = 120;

/**
 * The first thing the poster wrote, as a name.
 *
 * A description is a paragraph, not a title, so only its opening line is
 * taken and only as much of that as reads like a name. It is nonetheless the
 * best name a Facebook video has: signed in, every one of them is titled
 * "Video" and the post's own text is the only thing that tells them apart.
 */
function firstLine(text) {
  const line = String(text || "")
    .split("\n")
    .map((l) => l.trim())
    .find(Boolean);
  if (!line) return "";
  // Cut at a sentence end when there is one early enough to be a title, so a
  // three-paragraph post does not become a hundred-character filename with a
  // half-sentence at the end of it.
  const stop = line.search(/[.!?](?:\s|$)/);
  return (stop > 8 && stop < NAME_LIMIT ? line.slice(0, stop) : line).trim();
}

function suggestedName() {
  if (!info) return "";
  const titled = (info.title || "").trim();
  let base = titled;
  if (!base || GENERIC_TITLE.test(base)) {
    // What the poster wrote, where the site keeps it apart from the title.
    // Facebook calls every video "Video" and puts the post's own words in the
    // description, so naming these after the uploader filed a page's whole
    // output under one name with an id after it — "David Bombal Video
    // 1102368352460029" — which says who posted it and nothing about which
    // video it is.
    base = firstLine(info.description);
  }
  if (!base || GENERIC_TITLE.test(base)) {
    // Nothing written anywhere. Who posted it and the site's own id for it:
    // between them they identify the video the way its title was supposed to.
    base = [info.uploader, titled || "video", info.id].filter(Boolean).join(" ");
  }
  // The video's name, kept as its name. Only what a path cannot hold is taken
  // out — a separator, a control character, a leading dot that would hide the
  // file — because everything else about a title is what makes it readable:
  // its spaces, its punctuation, and the half of the world's titles that is
  // not spelled in ASCII. The engine sanitises this again on the way in, and
  // between them nothing is left that a filename may not contain.
  const stem = base
    .replace(/[\u0000-\u001f\u007f/\\]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .replace(/^\.+/, "")
    .slice(0, NAME_LIMIT)
    .trim();
  return (stem || "video") + kindSuffix();
}

/** Set once the user types a name of their own, so switching tabs stops
 *  rewriting the field underneath them. */
let nameEdited = false;

function drawFormats() {
  const host = $("vid-formats");
  if (!info) return host.replaceChildren();

  const rows = info.formats
    .filter((f) => tabsFor(f).includes(kind))
    .sort((a, b) => (heightOf(b) - heightOf(a)) || ((b.tbr || 0) - (a.tbr || 0)));

  if (!rows.length) return host.replaceChildren();

  // Listed by resolution, but selected by what will actually play: the best
  // row this machine has a decoder for, and only failing that the best row
  // there is. The list keeps its order either way — reading 1080p above 720p
  // is what makes it a quality list — so an HEVC row that is passed over for
  // the default is still sitting there, still labelled, still one click away.
  chosen = rows.find(playable) || rows[0];
  host.replaceChildren(
    ...rows.map((f) => {
      const row = document.createElement("label");
      row.className = "fmt";
      row.innerHTML =
        '<input type="radio" name="fmt">' +
        '<span class="res"></span><span class="kind"></span><span class="meta"></span>';
      row.querySelector(".res").textContent = label(f);
      row.querySelector(".kind").textContent = describe(f);
      // What will land on disk, not what the stream on this row weighs: a
      // row that has to borrow an audio track is downloaded with it.
      const size = expectedSize(f);
      row.querySelector(".meta").textContent = size ? bytes(size) : "";
      const radio = row.querySelector("input");
      radio.checked = f === chosen;
      radio.addEventListener("change", () => (chosen = f));
      return row;
    })
  );
}

const TAB_NAME = { both: "video + audio", video: "video without sound", audio: "audio" };

function setTabs() {
  const present = new Set(info ? info.formats.flatMap(tabsFor) : []);
  for (const tab of $("vid-tabs").children) {
    tab.disabled = !present.has(tab.dataset.kind);
  }
  // Land on a tab that actually has something in it — and say so, rather than
  // move the selection under the user and let a download they believe carries
  // picture and sound arrive as neither.
  if (!present.has(kind)) {
    const wanted = kind;
    kind = ["both", "video", "audio"].find((k) => present.has(k)) || "both";
    if (kind !== wanted) {
      say(
        "vid-status",
        `Nothing on this page is ${TAB_NAME[wanted]}; showing ${TAB_NAME[kind]} instead.`,
        "hint bad"
      );
    }
  }
  for (const tab of $("vid-tabs").children) {
    tab.classList.toggle("active", tab.dataset.kind === kind);
  }
}

/** Wipe every trace of the previous page. */
function reset() {
  probeSeq++;
  info = null;
  chosen = null;
  direct = null;
  directMedia = null;
  kind = "both";
  say("vid-status", "");
  // The direct-download note parks its reason here; a stale one would explain
  // a page this window is no longer looking at.
  $("vid-status").removeAttribute("title");
  guessed = null;
  $("vid-anyway").hidden = true;
  offerToStart(true);
  $("vid-head").hidden = true;
  $("vid-tabs").hidden = true;
  $("vid-save").hidden = true;
  $("vid-formats").replaceChildren();
  $("vid-title").textContent = "";
  $("vid-sub").textContent = "";
  $("vid-name").value = "";
  nameEdited = false;
  const thumb = $("vid-thumb");
  thumb.hidden = true;
  thumb.removeAttribute("src");
}

/**
 * Paths that name one piece of media rather than a listing of them.
 *
 * A home page, a feed or a profile is a page *full* of videos. yt-dlp reads
 * one as a playlist and answers with entries and no formats — a perfectly
 * successful extraction of the wrong thing — so anything that looks like a
 * single video's own address is worth asking about first.
 */
/* Kept in step with `namesOneMedia` in the extension's video panel, which uses
 * the same reading to *find* these links; here it only decides which to ask
 * about first. A section like /watch/hashtag/x or /reel/?s=tab names a listing,
 * not a video, and is no more specific than the feed it was found on. */
const MEDIA_SECTION =
  /^(?:watch|video|videos|reel|reels|clip|clips|status|post|posts|shorts|embed|p|v)$/i;
const NOT_AN_ID =
  /^(?:hashtag|tab|tabs|search|live|explore|browse|following|followers|saved|category|categories|page|pages|me|new|popular|trending|feed|home|all|watch|videos?|reels?|shorts)$/i;

function looksSpecific(url) {
  let u;
  try {
    u = new URL(url);
  } catch {
    return false;
  }
  const segments = u.pathname.split("/").filter(Boolean);
  for (let i = 0; i < segments.length - 1; i++) {
    if (!MEDIA_SECTION.test(segments[i])) continue;
    const id = segments[i + 1];
    if (NOT_AN_ID.test(id)) continue;
    if (/^\d+$/.test(id) || /^[A-Za-z0-9_.-]{5,}$/.test(id)) return true;
  }
  if (/\/(?:permalink|story|video)\.php$/i.test(u.pathname)) {
    return /[?&](?:v|story_fbid|fbid|id)=[A-Za-z0-9_.-]+/i.test(u.search);
  }
  return /[?&]v=[A-Za-z0-9_.-]{5,}/i.test(u.search);
}

/**
 * How far apart two lengths may be and still be the same video.
 *
 * Extractors round, containers disagree with streams by a frame or two, and a
 * long video accumulates more of that than a short one — so a floor and a
 * proportion, rather than one number that is too tight for an hour and too
 * loose for a feed.
 */
const lengthSlack = (seconds) => Math.max(2, seconds * 0.02);

/**
 * Is this extraction the video that was on screen?
 *
 * Three answers, because "not proven" and "proven otherwise" are different
 * things and only one of them is a reason to throw a page away.
 *
 * "right" — one of these formats is, byte for byte, a file the player pulled;
 * or the page is the same length as the video the player has loaded.
 *
 * "wrong" — the lengths are known and differ. This is the check that does not
 * depend on reading the page correctly: every post in a feed extracts just as
 * cleanly as the right one, with a real title and a real thumbnail, and the
 * one thing a neighbouring post almost never shares is its running time. The
 * `<video>` element knows its own length even when its src is a `blob:` that
 * names nothing and the whole page has gone quiet.
 *
 * "unconfirmed" — nothing to compare. The page may well be right; it is shown,
 * and it is said out loud that it could not be checked.
 */
function verdict(info) {
  // The exact file the player pulled, which settles it outright.
  if (info.matchesStream === true) return "right";
  const known = playedSeconds > 0 && info.duration > 0;
  if (known && Math.abs(info.duration - playedSeconds) > lengthSlack(info.duration)) {
    return "wrong";
  }
  // There were files to compare against and none of them was this.
  //
  // A weak no on its own — a DASH player fetches fragments an extractor never
  // offers — but it is still the strongest evidence in the building, and it
  // was once allowed to be overridden by the lengths agreeing. That put a
  // coincidence above a measurement: feed clips run six to fifteen seconds, so
  // a neighbouring post lands inside the slack often, and the wrong video came
  // back again. A no here withholds confirmation and the search goes on.
  if (info.matchesStream === false) return "unconfirmed";
  return known ? "right" : "unconfirmed";
}

/**
 * A manifest is a whole stream written down, and yt-dlp reads real formats out
 * of one. A plain media file has nothing to extract.
 */
const MANIFEST = /\.(?:m3u8|mpd)(?:[?#]|$)/i;

/**
 * Everything worth asking yt-dlp about, in order, without repeats.
 *
 * Pages only, plus the manifests among the media. That distinction matters:
 * these patterns are shapes of a *page* URL, and a CDN path is under no
 * obligation to avoid them — Facebook serves its video files from
 * `/o1/v/t2/...`, whose "/v/" made a raw .mp4 look like the most specific page
 * on offer. It was asked about first, failed the way a video file does when
 * something tries to extract a page from it, and that failure became the error
 * the window reported for the whole attempt. A media file is not a page and is
 * no longer treated as one; it is what `offerDirect` falls back to.
 *
 * Among pages, the typed URL leads — it is what the user asked for, or the
 * page the button was pressed on — but only against URLs of equal standing.
 * Press Download on a video in a feed and the page is the feed, while the
 * extension has read the post's own permalink out of the DOM; asking about the
 * feed first spends a slow extraction to learn what its shape already said.
 * Capped, because each one that fails costs another.
 */
function sources() {
  const pages = [
    $("vid-url").value.trim(),
    ...candidates.filter((c) => c.kind !== "media").map((c) => c.url),
  ];
  const streams = candidates
    .filter((c) => c.kind === "media" && MANIFEST.test(c.url))
    .map((c) => c.url);

  // Deduplicated by what the URL *addresses*, not by how it is spelled. A
  // feed hands over its own address three times over — the page the button was
  // pressed on, its `canonical`, its `og:url` — differing only by a trailing
  // slash or a fragment, and each spelling cost a full extraction to learn
  // what the last one already said.
  const seen = new Set();
  const fresh = (u) => {
    if (!/^https?:\/\//i.test(u)) return false;
    let key = u;
    try {
      const parsed = new URL(u);
      parsed.hash = "";
      key = parsed.href.replace(/\/$/, "");
    } catch {
      /* unparseable: compare it as it stands */
    }
    return !seen.has(key) && seen.add(key);
  };

  return [
    ...pages.filter(looksSpecific),
    ...pages.filter((u) => !looksSpecific(u)),
    ...streams,
  ]
    .filter(fresh)
    .slice(0, 4);
}

async function probe(pageTitle) {
  const pageUrl = $("vid-url").value.trim();
  if (!pageUrl) return;
  reset();
  const seq = probeSeq;
  const urls = sources();

  let result = null;
  let failure = null;
  // A page that resolved to a *list* of videos rather than one. Remembered so
  // the failure can say so, since it is a different problem from a page that
  // could not be read at all.
  let listing = false;
  // A page that read cleanly and is a different video, by its own length.
  // Remembered only so the failure can say so — never offered.
  let rejected = null;
  // The best answer so far, and whether it was ever *proved* to be the video
  // under the button rather than merely a page that read cleanly. Every post
  // in a feed is a real video with a real address, so resolving is no evidence
  // at all of having resolved the right one — which is how a grab came back
  // with the video from the post above, correct in every respect but identity.
  let unverified = null;
  let unverifiedUrl = "";
  // What a resolved page is checked against.
  //
  // Only files that could be this post's, because the check is an identity
  // test and a file belonging to some other post answers the wrong question.
  // The player element's own file is the strongest: not a reading of the page
  // but what that element is playing, so a page that does not offer it is
  // about some other video. Failing that, a file the site itself said belongs
  // to the post the markup named.
  //
  // Everything else in the tab stands in only where the page is about one
  // video, and there it is that video's. On a feed it is not: TikTok preloads
  // the posts below the one on screen, so checking against the tab declared
  // the *right* page a mismatch — the warning in the window read "could not be
  // matched to the video on screen" over the correct video — and would have
  // declared a wrong page a match the moment it resolved to a post the feed
  // had preloaded. With nothing trustworthy to compare, `matchesStream` comes
  // back null and the most specific page is taken at its word, which is what
  // this did before there was anything to check at all.
  const played = candidates.filter((c) => c.kind === "media" && c.origin === "player");
  const named = candidates.filter((c) => c.kind === "media" && c.post === "this");
  // A page that is not about one video is a page with several on it, and the
  // tab has been fetching for all of them.
  const feed = !looksSpecific(pageUrl);
  const anyFile = feed ? [] : candidates.filter((c) => c.kind === "media");
  const streams = (played.length ? played : named.length ? named : anyFile).map((c) => c.url);
  for (const [i, url] of urls.entries()) {
    // Name the page being read: extraction is slow enough that "which video is
    // this working on?" is a fair question. After the first, say which attempt
    // this is — the page URL was not the video and something else is being
    // tried, which is worth seeing rather than a status bar that just sits.
    say(
      "vid-status",
      i === 0
        ? pageTitle
          ? `Reading ${pageTitle}…`
          : "Reading page…"
        : `Nothing there. Trying another source (${i + 1} of ${urls.length})…`,
      "hint"
    );
    try {
      // Worth insisting on only where the URL names one video. A site that
      // answers a share of requests with a challenge page deserves a second
      // ask on the address we believe in; spending three asks on each of four
      // guesses is what turned a few seconds into minutes.
      result = await invoke("probe_media", {
        url,
        insist: looksSpecific(url),
        streams,
      });
    } catch (e) {
      if (seq !== probeSeq) return;
      // The first failure is the one worth reporting: it is about the page the
      // user was actually on. The rest are guesses failing.
      failure ??= String(e);
      continue;
    }
    // The window was pointed at another page while this was in flight.
    if (seq !== probeSeq) return;
    // Answering is not the same as answering about a video. A feed or a home
    // page extracts cleanly into a playlist: entries, and no formats to choose
    // between. Taking that as the answer is what left the picker reading
    // "0 formats" for a video the next candidate could have resolved — so it
    // is not an answer, and the remaining sources still get their turn.
    if (!result.formats.length) {
      listing = true;
      result = null;
      continue;
    }
    // Resolving is not the same as resolving to the right video.
    const said = verdict(result);
    if (said === "wrong") {
      // A different length is not a doubt, it is an answer: this page is some
      // other post. Dropped rather than kept as a fallback — a page that reads
      // this cleanly is exactly the thing that gets downloaded in full, plays
      // perfectly, and is somebody else's video.
      rejected ??= Math.round(result.duration);
      result = null;
      continue;
    }
    if (said === "unconfirmed") {
      // May well be right; nothing here can say. Kept in case nothing better
      // turns up, and labelled as unproven if it is used.
      //
      // Stopping here to save the extractions was a false economy: falling out
      // of the loop with an answer marks it *confirmed*, so the window stopped
      // saying it could not be checked — and an unchecked answer presented as
      // a checked one is exactly how the wrong video gets downloaded quietly.
      unverified ??= result;
      unverifiedUrl ||= url;
      result = null;
      continue;
    }
    // Show what actually answered, so a grab from a feed says which post it
    // resolved to rather than silently downloading something else.
    $("vid-url").value = url;
    break;
  }

  // Nothing resolved to the video being watched. Before falling back to a page
  // that merely read cleanly, take a file that is certainly this post's if it
  // is a whole video: one of the two is certainly the right video at whatever
  // quality the player chose, and the other is a coin toss with a title and a
  // thumbnail that look every bit as authoritative when they belong to the
  // post above. A quality picker is not worth being shown the wrong video.
  const certain = played.length ? played : named;
  const wholeCertain = certain.find((c) => !c.partial);
  if (!result && unverified && wholeCertain) {
    return offerDirect(
      "The page this video sits on could not be confirmed as the right one, so " +
        "the file belonging to the post on screen was taken instead.",
      feed
    );
  }

  // No such file either, so the best guess stands — said out loud rather than
  // presented as a fact.
  let unproven = false;
  if (!result && unverified) {
    result = unverified;
    $("vid-url").value = unverifiedUrl;
    unproven = true;
  }

  if (!result) {
    if (seq !== probeSeq) return;
    return offerDirect(
      rejected !== null
        ? `Every page that could be read is a different video: ${rejected}s, ` +
          `where the player on screen says ${Math.round(playedSeconds)}s.`
        : failure ||
          (listing
            ? "Every page tried is a list of videos rather than one video."
            : "Nothing on that page could be read."),
      feed
    );
  }

  info = result;
  say(
    "vid-status",
    unproven
      ? "This is the closest page that could be read — check the title before " +
        "downloading; it could not be matched to the video on screen."
      : "",
    unproven ? "hint bad" : ""
  );
  $("vid-head").hidden = false;
  $("vid-title").textContent = info.title;
  $("vid-sub").textContent = [
    info.extractor,
    info.duration ? duration(Math.round(info.duration)) : null,
    `${info.formats.length} formats`,
  ].filter(Boolean).join(" · ");
  const thumb = $("vid-thumb");
  if (info.thumbnail) {
    thumb.src = info.thumbnail;
    thumb.hidden = false;
  } else {
    thumb.hidden = true;
  }

  setTabs();
  $("vid-tabs").hidden = false;
  drawFormats();
  $("vid-name").value = suggestedName();
  $("vid-save").hidden = false;
  fitWindow();
}

/** Containers a media type implies, for a URL that names none. */
const MIME_EXTENSION = {
  "video/mp4": "mp4", "video/webm": "webm", "video/quicktime": "mov",
  "video/x-matroska": "mkv", "video/mpeg": "mpeg", "video/3gpp": "3gp",
  "video/x-msvideo": "avi", "video/ogg": "ogv",
  "audio/mpeg": "mp3", "audio/mp4": "m4a", "audio/aac": "aac", "audio/ogg": "ogg",
  "audio/opus": "opus", "audio/wav": "wav", "audio/flac": "flac", "audio/webm": "weba",
};

/** Long enough to tell two downloads apart, short enough to read. */
const MAX_STEM = 60;

/**
 * A usable name for a file that only its URL identifies.
 *
 * A CDN serves video from a path like `/o1/v/t2/f2/m412/AQOtK6Sp4dPC_XWT4…`,
 * so the last segment is a hundred characters of opaque token with no
 * extension on the end. Saved as it stands that is a name no one can read, and
 * one the app files under "Other" and the system opens with nothing — the
 * extension is what says it is a video. So the token is trimmed and the
 * container the type implies is put back on.
 */
function directName(url, mime) {
  const raw = nameFromUrl(url);
  const dot = raw.lastIndexOf(".");
  const named = dot > 0 && raw.length - dot <= 6 && /^[a-z0-9]+$/i.test(raw.slice(dot + 1));

  const stem = (named ? raw.slice(0, dot) : raw).slice(0, MAX_STEM) || "video";
  // A media candidate is a video unless its type says otherwise: it came from
  // a <video> element, or from a response the sniffer saw play as one.
  const ext = named
    ? raw.slice(dot + 1)
    : MIME_EXTENSION[(mime || "").split(";", 1)[0].trim().toLowerCase()] || "mp4";
  return `${stem}.${ext}`;
}

/**
 * The file the page is using, fetched as a file.
 *
 * When no page in the chain resolves — a feed with no permalink to find, an
 * extractor that does not know the site, no yt-dlp on the machine at all —
 * there may still be a plain media URL among the candidates. There is no
 * quality to choose then, so the picker stays empty and only the name and
 * folder are offered.
 *
 * Not phrased as a failure when that file exists, because it is not one: the
 * user asked for the video and the video is about to be downloaded. Losing the
 * choice of quality is worth a note, not an error in red — and the reason is
 * kept on the line for anyone who wants to know why there was no choice.
 */
function offerDirect(reason, feed) {
  const files = candidates.filter(
    (c) => c.kind === "media" && /^https?:\/\//i.test(c.url)
  );
  // A whole file, ahead of one the site described as half of a pair.
  //
  // This is the distinction that was missing, and it cost a download twice
  // over. A DASH site does not serve a video: it serves a picture track and a
  // sound track, to be played together, and either one downloads perfectly and
  // is not the video. Facebook labels both `video/mp4`, so first the sound was
  // saved and played as a black screen, and then — once sound was ranked last
  // — the picture was saved and played in silence. Neither is a download worth
  // handing anyone, and only its progressive encodes are one file.
  //
  // Among the whole ones, identity leads. The file the player element itself
  // has open is the one candidate that cannot be about a different post; next
  // is one the site said belongs to the post the markup named; and a file
  // known to belong to a *neighbour* comes after everything, since offering it
  // is the one mistake this window cannot apologise its way out of.
  const pick = (want) => files.find((c) => !c.partial && want(c));
  const whole =
    pick((c) => c.origin === "player") ||
    pick((c) => c.post === "this") ||
    pick((c) => c.post !== "other") ||
    pick(() => true);
  const media = whole || files.find((c) => c.post !== "other") || files[0];
  if (!media) return say("vid-status", reason, "hint bad");

  // Whether this file is the video that was on screen, or only the best of
  // what the tab happened to be fetching. A feed preloads the posts below the
  // one being watched, so on one of those a file nothing ties to the post is a
  // guess — and it was offered as "the file the page is playing", which is how
  // a grab came back with a video five posts further down, downloaded in full
  // and named as though it had been asked for.
  const playing = media.origin === "player";
  const mine = playing || media.post === "this";

  // Was there anything to guess *between*?
  //
  // One whole file in the whole tab is the page's own video, whatever its
  // address says — that is the sniffer doing the job it exists for, on a site
  // whose player runs on MediaSource and whose page yt-dlp cannot read. Half a
  // dozen is a feed fetching for the posts below the one being watched, and
  // picking one of those is the coin toss that downloaded a stranger's video.
  //
  // Counted rather than read off the address, because the address is the other
  // thing a feed rewrites: TikTok pushes the current post's URL into the bar
  // as you scroll, so the page reads as one video's own while the tab is busy
  // with six. Whole files only — a DASH page fetches a picture track and a
  // sound track for the same video, and that is one video, not two.
  const loose = files.filter((c) => !c.partial && c.origin !== "player" && c.post !== "this");
  // Nothing ties this file to the video that was on screen, and it was one of
  // several it could have been. That is a guess, and a guess does not get to
  // arrive pre-filled under a Start button: the wrong file downloads in full,
  // plays perfectly, and looks in every respect like the right video. It is
  // still there to be taken — deliberately.
  if (!mine && (feed || loose.length > 1)) return refuse(reason, media);

  takeFile(
    media,
    // Said plainly when it is half a video, because it will download to 100%
    // and look every bit as finished as one that is not.
    whole && playing
      ? "This is the file the player itself has open, so it is the video on " +
        "screen — but it comes as it is, with no quality to choose."
      : whole && mine
      ? "This is the file the site names as the post on screen, so it is the " +
        "right video — but it comes as it is, with no quality to choose."
      : whole
      ? "No quality to choose for this one — the file the page is playing will " +
        "be downloaded as it is."
      : media.audioOnly
        ? "This page would not resolve, and the only file left is the sound " +
          "track on its own — no picture. Download it only if that is what you want."
        : "This page would not resolve, and the only file left is the picture " +
          "track on its own — no sound. Download it only if that is what you want.",
    whole ? "hint" : "hint bad"
  );
  $("vid-status").title = reason;
}

/** Offer a file to be fetched outright: a name, a folder and the buttons. */
function takeFile(media, note, tone) {
  direct = media.url;
  directMedia = media;
  $("vid-url").value = direct;
  $("vid-name").value = directName(direct, media.mime);
  $("vid-save").hidden = false;
  $("vid-anyway").hidden = true;
  offerToStart(true);
  say("vid-status", note, tone);
  fitWindow();
}

/** Show or hide the two buttons that begin a download. */
function offerToStart(on) {
  $("vid-start").hidden = !on;
  $("vid-later").hidden = !on;
}

/**
 * Decline to answer, rather than answer with a guess.
 *
 * The window used to fill in whichever file looked best and let the Start
 * button do the rest, which on a feed meant downloading a post five rows
 * further down — in full, correctly named, indistinguishable from what was
 * asked for. There is no reading of the page left that can tell those apart,
 * so nothing is filled in. What is offered instead is the two things that
 * actually help: ask again, which usually works because what refused was a
 * site turning away a share of requests rather than a page that cannot be
 * read; and take the file anyway, for anyone who would rather have the guess
 * than nothing.
 */
function refuse(reason, media) {
  guessed = media;
  say(
    "vid-status",
    media
      ? "Nothing here identifies the video on screen. There is a file to be " +
        "had, but a feed loads the posts below the one you are watching, so " +
        "it may well be a different video. Trying again usually works."
      : "Nothing here identifies the video on screen, and there is no file to " +
        "fall back to. Trying again usually works.",
    "hint bad"
  );
  $("vid-status").title = reason;
  $("vid-save").hidden = true;
  offerToStart(false);
  $("vid-force").hidden = !media;
  $("vid-anyway").hidden = false;
  fitWindow();
}

/** Last path segment of a URL, for naming a file nothing else named. */
function nameFromUrl(url) {
  try {
    const seg = new URL(url).pathname.split("/").filter(Boolean).pop() || "";
    return decodeURIComponent(seg).replace(/[\\/]/g, "_") || "video";
  } catch {
    return "video";
  }
}

/* ------------------------------------------------------------------ *
 * Watching it download
 * ------------------------------------------------------------------ */

/** The row the strip is following, or null while nothing has been started. */
let watching = null;
/** A capture the browser handed over, waiting on Start. */
let pendingCapture = null;

async function start(paused) {
  // A capture already has a row — created paused so nothing is lost if this
  // window is closed — so starting it is a resume, not a new submission.
  if (pendingCapture !== null) {
    const id = pendingCapture;
    if (paused) return closeWindow(); // "Download Later" is what it already is
    pendingCapture = null;
    $("pick-actions").hidden = true;
    $("vid-save").hidden = true;
    watching = id;
    $("dl-strip").hidden = false;
    clearStrip();
    await call("start_capture", {
      id,
      directory: $("vid-dir").value.trim() || null,
      filename: $("vid-name").value.trim() || null,
    });
    const snap = await invoke("get_snapshot").catch(() => null);
    if (snap) render(snap);
    fitWindow();
    return;
  }

  const url = $("vid-url").value.trim();
  if (!url) return toast("No URL to download");

  // Only while the box still holds the URL they belong to: the field is
  // editable, and headers signed for one file say nothing about another.
  const carrying = directMedia && directMedia.url === url ? directMedia : null;

  const id = await call("add_download", {
    url,
    directory: $("vid-dir").value.trim() || null,
    // A direct media URL has nothing to extract; handing it to yt-dlp would
    // only put an extractor between aria2 and a file it can already fetch.
    // Said outright rather than left unset, because the engine's own guess is
    // made from the host — and a video site serves its files from its own
    // name, so on a CDN URL that guess is wrong every time.
    useYtdlp: !direct,
    formatId: chosen ? formatExpression(chosen) : null,
    filename: $("vid-name").value.trim() || null,
    startPaused: paused,
    size: chosen ? expectedSize(chosen) : null,
    // What the browser would have sent for it. Without these a CDN that signs
    // its links per session answers 403, and the download never starts.
    headers: carrying ? carrying.headers || [] : null,
    referrer: carrying ? carrying.referrer || "" : null,
  });
  if (id === null) return;

  watching = id;
  $("dl-strip").hidden = false;
  // Cancel meant "never mind"; now the picking is over and the button's job is
  // to get the window out of the way without touching the download.
  $("vid-close").textContent = "Close";
  clearStrip();

  // The snapshot arrives on its own schedule; ask for one now rather than
  // leaving the strip blank for most of a second.
  const snap = await invoke("get_snapshot").catch(() => null);
  if (snap) render(snap);
  fitWindow();
}

/** What to show in place of a speed when there is no speed to show. */
function stateWord(d) {
  switch (d.status) {
    case "paused": return "Paused";
    case "queued": return "Waiting";
    case "failed": return "Failed";
    case "complete": return "Done";
    // Active but nothing transferred yet: a streamed page has to be resolved
    // before a single byte can be asked for.
    default: return "Preparing";
  }
}

/** The strip's one reserved line. Never hidden, so the strip never changes
 *  height and the window never resizes to follow it. The line is ellipsised
 *  from the right, so anything that must survive being cut goes in `tip`. */
function note(text, tone, tip) {
  const el = $("dl-note");
  el.textContent = text;
  el.title = tip || text;
  el.className = tone ? `hint ${tone}` : "hint";
}

/**
 * Blank the strip for a download that has not reported yet.
 *
 * Without this the strip goes on describing the *previous* download — a full
 * bar, "Done", the folder it was saved to — for the moment before the first
 * snapshot arrives, which reads as the new pick having finished instantly.
 */
function clearStrip() {
  $("dl-bar").firstElementChild.style.width = "0%";
  $("dl-bar").className = "bar idle";
  $("dl-pct").textContent = "…";
  $("dl-bytes").textContent = "";
  $("dl-speed").textContent = "Preparing";
  $("dl-conns").textContent = "";
  $("dl-eta").textContent = "";
  $("dl-pause").textContent = "Pause";
  $("dl-pause").hidden = false;
  $("dl-cancel").hidden = false;
  $("dl-open").hidden = true;
  $("dl-folder").hidden = true;
  note("");
}

function render(snapshot) {
  if (watching === null) return;
  const d = (snapshot.downloads || []).find((x) => x.id === watching);
  if (!d) return;

  const pct = d.totalBytes > 0 ? d.completedBytes / d.totalBytes : 0;
  $("dl-bar").firstElementChild.style.width =
    `${Math.min(100, pct * 100).toFixed(1)}%`;
  $("dl-bar").className =
    "bar" +
    (d.status === "complete" ? " done" : d.status === "failed" ? " err" :
     d.status === "active" ? "" : " idle");

  // Always a percentage: the state word has its own slot, and saying "Done"
  // in both of them just reads as a stutter.
  $("dl-pct").textContent = d.totalBytes > 0 ? `${(pct * 100).toFixed(1)}%` : "…";
  $("dl-bytes").textContent =
    d.totalBytes > 0 ? `${bytes(d.completedBytes)} of ${bytes(d.totalBytes)}`
                     : bytes(d.completedBytes);
  // The state word sits where the speed would be, because that is the slot
  // whose meaning it replaces. Anything that changes the *number* of lines in
  // the strip would resize the window under the user's cursor, so nothing does.
  const moving = d.status === "active" && d.completedBytes > 0;
  $("dl-speed").textContent = moving ? rate(d.downloadSpeed) : stateWord(d);
  $("dl-conns").textContent =
    moving && d.connections > 0 ? `${d.connections} conn` : "";
  // Bare "1m 28s" next to a byte count reads as a duration, not a countdown.
  const remaining = eta(d);
  $("dl-eta").textContent = remaining === "—" ? "" : `${remaining} left`;

  // The reserved line, which only the two things too long for the stats row
  // ever use. It keeps its height whether or not it has anything to say.
  if (d.status === "failed") note(d.error || "Download failed.", "bad");
  // The name, not just the folder: a second copy of something already saved is
  // numbered, and this is where that becomes visible.
  else if (d.status === "complete") {
    note(`Saved as ${d.filename}`, "ok", `${d.directory}/${d.filename}`);
  } else note("");

  const done = d.status === "complete";
  const finished = done || d.status === "failed";
  $("dl-pause").textContent = d.status === "paused" ? "Resume" : "Pause";
  $("dl-pause").hidden = finished;
  $("dl-cancel").hidden = finished;
  $("dl-open").hidden = !done;
  $("dl-folder").hidden = !done;
  $("dl-open").dataset.path = `${d.directory}/${d.filename}`;
  $("dl-folder").dataset.path = `${d.directory}/${d.filename}`;
  $("dl-pause").dataset.status = d.status;
}

/* ------------------------------------------------------------------ *
 * Wiring
 * ------------------------------------------------------------------ */

/** A URL the user typed stands alone; the last page's readings are not it. */
function probeTyped() {
  candidates = [];
  // Nor is the last page's player: whatever was on screen when the grab
  // arrived says nothing about a URL typed in afterwards, and checking one
  // against the other would reject a perfectly good page for being a
  // different length than something unrelated.
  playedSeconds = 0;
  pageTitle = "";
  probe();
}

$("vid-probe").addEventListener("click", probeTyped);
$("vid-url").addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    probeTyped();
  }
});

$("vid-tabs").addEventListener("click", (e) => {
  const tab = e.target.closest(".tab");
  if (!tab || tab.disabled) return;
  kind = tab.dataset.kind;
  setTabs();
  drawFormats();
  if (!nameEdited) $("vid-name").value = suggestedName();
});

$("vid-name").addEventListener("input", () => (nameEdited = true));

$("vid-browse").addEventListener("click", async () => {
  const dir = await call("pick_directory", { start: settings?.downloadDir });
  if (dir) $("vid-dir").value = dir;
});

$("vid-close").addEventListener("click", async () => {
  // A capture the user declined should not be left sitting paused for ever.
  if (pendingCapture !== null) {
    const id = pendingCapture;
    pendingCapture = null;
    await call("remove", { id, deleteFile: true });
  }
  closeWindow();
});
// Ask again. The usual reason nothing resolved is a site refusing a share of
// the requests that reach it — yt-dlp reports that exactly like a page it
// cannot read — and only a failure is not cached, so this genuinely re-asks.
$("vid-retry").addEventListener("click", () => probe(pageTitle));
$("vid-force").addEventListener("click", () => {
  if (!guessed) return;
  takeFile(
    guessed,
    "Taken at your word: nothing tied this file to the video that was on " +
      "screen, so check what arrives.",
    "hint bad"
  );
});

$("vid-start").addEventListener("click", () => start(false));
$("vid-later").addEventListener("click", () => start(true));

$("dl-pause").addEventListener("click", (e) => {
  const paused = e.currentTarget.dataset.status === "paused";
  call(paused ? "resume" : "pause", { id: watching });
});
$("dl-cancel").addEventListener("click", async () => {
  const id = watching;
  // Stop following it first: the row disappears from the next snapshot, and a
  // strip still pointed at it would freeze on its last numbers.
  watching = null;
  $("dl-strip").hidden = true;
  await call("remove", { id, deleteFile: true });
  // With no picker to fall back to there is nothing left in the window.
  if ($("pick-actions").hidden) closeWindow();
});
$("dl-close").addEventListener("click", closeWindow);
$("dl-open").addEventListener("click", (e) =>
  call("open_path", { path: e.currentTarget.dataset.path, reveal: false }));
$("dl-folder").addEventListener("click", (e) =>
  call("open_path", { path: e.currentTarget.dataset.path, reveal: true }));

listen("mdm://snapshot", (event) => render(event.payload));

/* ------------------------------------------------------------------ *
 * Startup
 * ------------------------------------------------------------------ */

/** Requests already acted on, so the same grab is not handled twice. */
let lastRequestId = 0;

/** Show the picker, or hide it for a download with nothing left to decide. */
function setKind(kind) {
  const file = kind === "file";
  // A capture has no page to fetch and no formats to choose between, so the
  // whole picking apparatus goes and its head takes the URL row's place.
  $("url-row").hidden = file;
  $("vid-formats").hidden = file;
  $("file-head").hidden = !file;
  $("vid-head").hidden = true;
  $("vid-tabs").hidden = true;
  $("vid-save").hidden = true;
  // A page can be fetched and started at any time, so the picker keeps its
  // buttons throughout. A capture only gets them once it has been offered.
  $("pick-actions").hidden = file;
  $("dl-close").hidden = !file;
}

async function handleRequest(request) {
  if (!request || request.id <= lastRequestId) return;
  lastRequestId = request.id;

  // A new request is a new question, so the strip for the last one goes: the
  // user asked about another download, not for a report on the previous one.
  watching = null;
  pendingCapture = null;
  $("dl-strip").hidden = true;
  $("vid-close").textContent = "Cancel";
  reset();
  $("vid-dir").value = "";
  setKind(request.kind);

  // A capture: recorded but not running. Offer it the way the picker offers a
  // video — a folder, a name, and a button that starts it.
  if (request.kind === "file") {
    $("file-name").textContent = request.title || request.url;
    $("file-name").title = request.title || request.url;
    say("file-sub", request.url, "hint");
    $("file-sub").title = request.url;
    pendingCapture = request.downloadId;
    $("vid-name").value = request.title || "";
    $("vid-dir").value = request.directory || "";
    $("vid-save").hidden = false;
    $("pick-actions").hidden = false;
    fitWindow();
    return;
  }

  candidates = request.candidates || [];
  // What the player under the button had loaded. Sent with the grab because
  // the window cannot see the page, and it is the only fact about the video on
  // screen that does not come from reading that page.
  playedSeconds = Number(request.seconds) || 0;
  pageTitle = request.title || "";
  const url = request.url || (await invoke("read_clipboard_url").catch(() => null)) || "";
  $("vid-url").value = url;
  if (url) await probe(pageTitle);
  else $("vid-url").focus();
}

(async () => {
  settings = await invoke("get_settings").catch(() => null);
  if (!(await invoke("ytdlp_available").catch(() => false))) {
    // The install command is asked for rather than assumed: apt, dnf and
    // pacman machines all end up here and only one of the three lines works.
    const how = await invoke("install_hint", { package: "yt-dlp" })
      .catch(() => "your package manager (the package is called yt-dlp)");
    say("vid-status", `yt-dlp is not installed — run: ${how}`, "hint bad");
  }
  // The window is usually opened *for* a request, which was set before the
  // page existed and so could not have been delivered as an event.
  await handleRequest(await invoke("take_pending_video").catch(() => null));
})();

listen("mdm://videoPage", (event) => handleRequest(event.payload));
