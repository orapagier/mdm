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
