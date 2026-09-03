"use strict";

/* ------------------------------------------------------------------ *
 * Header helpers
 * ------------------------------------------------------------------ */

/** webRequest gives headers as [{name, value}]; collapse to a lowercase map. */
function headerMap(list) {
  const out = Object.create(null);
  for (const h of list || []) {
    const k = h.name.toLowerCase();
    const v = h.value !== undefined ? h.value : (h.binaryValue || []).join("");
    // Duplicate headers (notably Set-Cookie) join with ", " per RFC 9110.
    out[k] = k in out ? out[k] + ", " + v : v;
  }
  return out;
}

/** Content-Type minus parameters, lowercased: "text/html; charset=x" -> "text/html". */
function mimeOf(headers) {
  const ct = headers["content-type"];
  if (!ct) return "";
  return ct.split(";", 1)[0].trim().toLowerCase();
}

/** Content-Length as a number, or -1 when absent/unparseable (chunked responses). */
function sizeOf(headers) {
  const cl = headers["content-length"];
  if (!cl) return -1;
  const n = Number.parseInt(cl, 10);
  return Number.isFinite(n) && n >= 0 ? n : -1;
}

/**
 * Mirrors the server advertised for this exact file, newest standard first.
 *
 * RFC 6249 ("Metalink/HTTP") lets a server answer a download with
 * `Link: <https://mirror/file.iso>; rel=duplicate; pri=1` for every other place
 * the same bytes live. Handing those to aria2 alongside the original lets one
 * file be pulled from several servers at once — the one thing a single-source
 * downloader, IDM included, structurally cannot do.
 *
 * Only absolute http(s) URLs are taken: a relative or non-HTTP `rel=duplicate`
 * is either a server bug or someone hoping we will fetch something else.
 */
function mirrorsOf(headers, originalUrl) {
  const link = headers["link"];
  if (!link) return [];

  const out = [];
  const seen = new Set([originalUrl]);
  // Split on commas that separate entries, not commas inside <...> or "...".
  for (const entry of splitLinkHeader(link)) {
    const target = /^\s*<([^>]*)>/.exec(entry);
    if (!target) continue;
    if (!/;\s*rel\s*=\s*"?duplicate"?/i.test(entry)) continue;

    let url;
    try {
      url = new URL(target[1].trim(), originalUrl).href;
    } catch {
      continue;
    }
    if (!/^https?:\/\//i.test(url) || seen.has(url)) continue;
    seen.add(url);
    // `pri` is 1..999999, lowest first; absent sorts last among equals.
    const pri = /;\s*pri\s*=\s*"?(\d+)"?/i.exec(entry);
    out.push({ url, pri: pri ? Number(pri[1]) : 1000000 });
  }

  // A ceiling because a hostile or misconfigured server can list hundreds, and
  // aria2 would try to open a connection to each.
  return out.sort((a, b) => a.pri - b.pri).slice(0, 8).map((m) => m.url);
}

/** Split a Link header on top-level commas only. */
function splitLinkHeader(value) {
  const parts = [];
  let depth = 0;
  let quoted = false;
  let start = 0;
  for (let i = 0; i < value.length; i++) {
    const c = value[i];
    if (quoted) {
      if (c === '"' && value[i - 1] !== "\\") quoted = false;
    } else if (c === '"') quoted = true;
    else if (c === "<") depth++;
    else if (c === ">") depth--;
    else if (c === "," && depth <= 0) {
      parts.push(value.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(value.slice(start));
  return parts;
}

/* ------------------------------------------------------------------ *
 * Byte ranges
 * ------------------------------------------------------------------ */

/**
 * Query parameters by which a player pins one slice of a stream.
 *
 * A DASH player asks for a file a piece at a time, and some sites write the
 * piece into the URL rather than into a `Range` header: Facebook appends
 * `bytestart`/`byteend`, YouTube uses `range`.
 */
const RANGE_PARAMS = ["bytestart", "byteend", "range"];

/**
 * The same URL with any byte range taken off it.
 *
 * Saved as it stands, a ranged URL yields exactly what it asks for — a slice
 * out of the middle of a file, with no header on the front. Three of these
 * were saved from Facebook and every one of them reported complete: 1 KB,
 * 16 KB and 2.8 MB, the last being precisely `byteend - bytestart + 1` bytes,
 * and all three rejected by ffprobe as "no tfhd was found". Nothing will play
 * them. Asking without the range asks for the whole stream, which is what the
 * user pressed the button for.
 */
function withoutByteRange(url) {
  try {
    const u = new URL(url);
    let ranged = false;
    for (const p of RANGE_PARAMS) {
      if (!u.searchParams.has(p)) continue;
      u.searchParams.delete(p);
      ranged = true;
    }
    return ranged ? u.href : url;
  } catch {
    return url;
  }
}

/* ------------------------------------------------------------------ *
 * What a media URL says about itself
 * ------------------------------------------------------------------ */

/**
 * Sites that write a stream's own description into its address, and how to
 * read it.
 *
 * Facebook signs every file it serves with an `efg` parameter carrying
 * base64 JSON — `{"vencode_tag":"dash_..._audio","video_id":10155529876…}` —
 * and that payload answers the two questions nothing else on the wire can.
 * The response certainly cannot: a Facebook video, its 1080p DASH stream and
 * the *audio track on its own* all come back as `Content-Type: video/mp4`,
 * with the audio weighing a couple of hundred kilobytes and looking in every
 * other respect like a small video.
 *
 * Instagram is the same CDN and the same parameter.
 */
const URL_FACTS = [
  {
    host: /(^|\.)(?:fbcdn\.net|cdninstagram\.com)$/i,
    param: "efg",
    read: (text) => ({
      // Which post the stream belongs to. A feed has several playing or
      // preloading at once, and this is the only thing that tells their files
      // apart — they are otherwise the same shape from the same host.
      //
      // Read out of the text rather than off the parsed object, because a
      // Facebook video id does not survive being a JavaScript number:
      // 10155529876156509 is past 2^53 and `JSON.parse` quietly returns
      // ...508, an id belonging to nothing, which matches no post and would
      // have made this whole reading a no-op that looked like it worked.
      videoId: (/"video_id"\s*:\s*"?(\d+)/.exec(text) || [])[1] || "",
      // `dash_v3_426_crf_23_main_3.0_frag_2_audio` — the sound with no
      // picture, which is a whole download that plays as a black screen.
      audioOnly: /(?:^|_)audio(?:_|$)/i.test(String(JSON.parse(text).vencode_tag ?? "")),
      // One half of a DASH pair — picture with no sound, or sound with no
      // picture. Saving either as "the video" produces a file that downloads
      // perfectly and is not the thing anyone asked for. Only Facebook's
      // `xpv_progressive` encodes carry both in one file.
      partial: /^dash_/i.test(String(JSON.parse(text).vencode_tag ?? "")),
    }),
    // The post this stream belongs to, as an address anything can resolve.
    page: (id) => `https://www.facebook.com/watch/?v=${id}`,
  },
];

/**
 * The page that would resolve a media URL, where the URL names its own post.
 *
 * The one route out of a feed that rests on a fact rather than a reading of
 * the DOM. When no permalink can be found — Facebook's feed publishes none for
 * a reel, and the player's src is a `blob:` nobody outside the page can fetch
 * — what is left is the file the player pulled, and that file's address says
 * which post it belongs to. Handing back `facebook.com/watch/?v=<id>` turns
 * that into a page yt-dlp extracts properly, with sound, at every quality,
 * instead of the half a video the raw stream would have been.
 */
function pageForStream(url) {
  const { videoId } = urlFacts(url);
  if (!videoId) return "";
  let host;
  try {
    host = new URL(url).hostname;
  } catch {
    return "";
  }
  const site = URL_FACTS.find((s) => s.host.test(host));
  return site && site.page ? site.page(videoId) : "";
}

/** Base64url, with the padding a URL leaves off, as text. */
function decodeTag(raw) {
  const padded = raw.replace(/-/g, "+").replace(/_/g, "/");
  return atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
}

/**
 * What the site itself says this file is, where it says anything at all.
 *
 * Deliberately incurious everywhere else: an unknown host, a missing
 * parameter or a payload that will not decode all answer "nothing known",
 * which leaves every judgement exactly where it was.
 */
function urlFacts(url) {
  const unknown = { videoId: "", audioOnly: false, partial: false };
  let u;
  try {
    u = new URL(url);
  } catch {
    return unknown;
  }
  const site = URL_FACTS.find((s) => s.host.test(u.hostname));
  const raw = site && u.searchParams.get(site.param);
  if (!raw) return unknown;
  try {
    return { ...unknown, ...site.read(decodeTag(raw)) };
  } catch {
    // Their format, not ours: it may change without notice, and when it does
    // the grab must go on working the way it did before this was read at all.
    return unknown;
  }
}

/* ------------------------------------------------------------------ *
 * Content-Disposition (RFC 6266 + RFC 5987 ext-value)
 * ------------------------------------------------------------------ */

function decodeExtValue(raw) {
  // charset'language'pct-encoded  e.g.  UTF-8''Na%C3%AFve%20file.pdf
  const m = /^([\w!#$%&+^_`{}~-]+)'[^']*'(.*)$/.exec(raw);
  if (!m) return null;
  const charset = m[1].toLowerCase();
  const bytes = [];
  const enc = m[2];
  for (let i = 0; i < enc.length; i++) {
    if (enc[i] === "%" && i + 2 < enc.length) {
      const b = Number.parseInt(enc.slice(i + 1, i + 3), 16);
      if (Number.isNaN(b)) return null;
      bytes.push(b);
      i += 2;
    } else {
      bytes.push(enc.charCodeAt(i) & 0xff);
    }
  }
  try {
    // TextDecoder covers utf-8 and iso-8859-1, the only charsets RFC 8187 allows.
    return new TextDecoder(charset === "utf-8" ? "utf-8" : "iso-8859-1", {
      fatal: false,
    }).decode(new Uint8Array(bytes));
  } catch {
    return null;
  }
}

/**
 * Parse a Content-Disposition value.
 * Returns {disposition, filename} — filename is null when not supplied.
 * filename* wins over filename, as RFC 6266 §4.3 requires.
 */
function parseContentDisposition(value) {
  if (!value) return { disposition: "", filename: null };

  const disposition = value.split(";", 1)[0].trim().toLowerCase();
  let plain = null;
  let extended = null;

  // Walk parameters; quoted strings may contain ';' so we can't naively split.
  const re = /;\s*([\w!#$%&*+.^_`|~-]+)\s*=\s*("(?:[^"\\]|\\.)*"|[^;]*)/g;
  let m;
  while ((m = re.exec(value)) !== null) {
    const name = m[1].toLowerCase();
    let val = m[2].trim();
    if (val.startsWith('"')) val = val.slice(1, -1).replace(/\\(.)/g, "$1");
    if (name === "filename*" && extended === null) extended = decodeExtValue(val);
    else if (name === "filename" && plain === null) plain = val;
  }

  return { disposition, filename: extended || plain };
}

/* ------------------------------------------------------------------ *
 * Filenames
 * ------------------------------------------------------------------ */

const RESERVED = /[\x00-\x1f\x7f/\\]/g;

/** Make a server-supplied name safe to hand to the filesystem. */
function sanitizeFilename(name) {
  if (!name) return "";
  // RFC 6266 says a filename parameter carries no path components, and
  // recipients must strip any that appear. Taking the last segment is both
  // what the spec asks for and what stops "../../etc/passwd" from escaping
  // the download directory once the name reaches aria2's `out` option.
  let out = String(name).split(/[/\\]/).pop() || "";
  out = out.replace(RESERVED, "_").trim();
  // A leading dot would hide the file; ".." would escape the directory.
  while (out.startsWith(".")) out = out.slice(1);
  if (out.length > 200) {
    const dot = out.lastIndexOf(".");
    const ext = dot > 0 && out.length - dot <= 12 ? out.slice(dot) : "";
    out = out.slice(0, 200 - ext.length) + ext;
  }
  return out;
}

/** Last path segment of a URL, percent-decoded, query/fragment stripped. */
function filenameFromUrl(url) {
  try {
    const u = new URL(url);
    const seg = u.pathname.split("/").filter(Boolean).pop() || "";
    let name = seg;
    try {
      name = decodeURIComponent(seg);
    } catch {
      /* malformed escapes: keep the raw segment */
    }
    return sanitizeFilename(name);
  } catch {
    return "";
  }
}

/** Best available name: Content-Disposition, then URL, then a generic fallback. */
function deriveFilename(url, headers) {
  const cd = parseContentDisposition(headers["content-disposition"]);
  const fromCd = sanitizeFilename(cd.filename);
  if (fromCd) return fromCd;
  const fromUrl = filenameFromUrl(url);
  if (fromUrl) return fromUrl;
  return "download";
}

function extensionOf(name) {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return "";
  const ext = name.slice(dot + 1).toLowerCase();
  return /^[a-z0-9]{1,12}$/.test(ext) ? ext : "";
}

/**
 * Split `data:[<mime>][;charset=…][;base64],<payload>` into bytes.
 *
 * base64 either way, because that is the shape the app's job format takes —
 * which for an already-base64 URL means passing the payload straight through.
 */
function decodeDataUrl(url) {
  const comma = url.indexOf(",");
  if (comma < 0) return null;
  const meta = url.slice("data:".length, comma);
  const body = url.slice(comma + 1);
  const mime = meta.split(";", 1)[0].trim().toLowerCase();

  try {
    if (/;base64\s*$/i.test(meta)) {
      const data = body.replace(/\s+/g, "");
      const padding = (data.match(/=*$/) || [""])[0].length;
      return { mime, data, size: Math.floor((data.length * 3) / 4) - padding };
    }
    // A percent-encoded body is text. Encode it to bytes before base64, or a
    // multi-byte character is cut in half on the way through.
    const bytes = new TextEncoder().encode(decodeURIComponent(body));
    let binary = "";
    const CHUNK = 0x8000;
    for (let i = 0; i < bytes.length; i += CHUNK) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
    }
    return { mime, data: btoa(binary), size: bytes.length };
  } catch (e) {
    console.warn("[mdm] could not decode a data: url:", e.message);
    return null;
  }
}
