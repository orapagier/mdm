"use strict";

/* ------------------------------------------------------------------ *
 * Type tables
 * ------------------------------------------------------------------ */

/**
 * Resource types worth inspecting. Deliberately excludes "media": those are
 * the range requests a playing <video> issues, and cancelling them breaks
 * playback. Page media is offered through the sniffer instead.
 */
const CAPTURABLE_TYPES = new Set([
  "main_frame",
  "sub_frame",
  "other",
  "object",
  "object_subrequest",
]);

/** Types the browser renders itself. Never intercept these. */
const NEVER_MIME = new Set([
  "text/html",
  "text/css",
  "text/javascript",
  "text/xml",
  "text/plain",
  "application/javascript",
  "application/ecmascript",
  "application/json",
  "application/ld+json",
  "application/xml",
  "application/xhtml+xml",
  "application/rss+xml",
  "application/atom+xml",
  "image/svg+xml",
  "application/x-www-form-urlencoded",
]);

const NEVER_MIME_PREFIX = ["image/", "font/", "application/font"];

/** Unambiguous "this is a file" types. */
const ALWAYS_MIME = new Set([
  "application/octet-stream",
  "application/zip",
  "application/x-zip-compressed",
  "application/x-rar-compressed",
  "application/vnd.rar",
  "application/x-7z-compressed",
  "application/x-tar",
  "application/gzip",
  "application/x-gzip",
  "application/x-bzip2",
  "application/x-xz",
  "application/zstd",
  "application/x-msdownload",
  "application/x-msi",
  "application/x-executable",
  "application/x-elf",
  "application/x-sharedlib",
  "application/vnd.debian.binary-package",
  "application/x-debian-package",
  "application/x-redhat-package-manager",
  "application/x-rpm",
  "application/vnd.android.package-archive",
  "application/x-apple-diskimage",
  "application/x-iso9660-image",
  "application/x-diskimage",
  "application/x-appimage",
  "application/x-bittorrent",
  "application/pdf",
  "application/epub+zip",
  "application/vnd.ms-cab-compressed",
  "application/java-archive",
  "application/x-deb",
]);

/** Office/document families that arrive under long vendor MIME names. */
const ALWAYS_MIME_PREFIX = [
  "application/vnd.openxmlformats-officedocument.",
  "application/vnd.oasis.opendocument.",
  "application/vnd.ms-",
];

/**
 * MIME types a server sends when it has not actually identified the file:
 * lazy defaults rather than deliberate claims. Only these may be overridden
 * by the file extension.
 */
const GENERIC_MIME = new Set([
  "",
  "text/plain",
  "application/octet-stream",
  "binary/octet-stream",
  "application/binary",
  "application/download",
  "application/force-download",
  "application/unknown",
  "*/*",
]);

/**
 * Extensions no server ever legitimately renders inline. These outrank a
 * *generic* Content-Type, because misconfigured servers routinely label
 * archives as text/plain and the extension is then the better evidence.
 */
const BINARY_EXTENSIONS = new Set([
  "zip","rar","7z","tar","gz","tgz","bz2","tbz","xz","txz","zst","lz4","lzma","cab","arj",
  "iso","img","deb","rpm","apk","appimage","flatpak","snap","dmg","pkg","msi","exe",
  "jar","whl","crx","xpi","vsix","vdi","vmdk","qcow2","ova","dmp","bin",
]);

/** Extensions that mark a file even when the server sends a useless MIME. */
const FILE_EXTENSIONS = new Set([
  // archives
  "zip","rar","7z","tar","gz","tgz","bz2","tbz","xz","txz","zst","lz4","lzma","cab","arj",
  // packages / images
  "iso","img","deb","rpm","apk","appimage","flatpak","snap","msi","exe","dmg","pkg","jar","whl","crx","xpi","vsix",
  // documents
  "pdf","doc","docx","xls","xlsx","ppt","pptx","odt","ods","odp","rtf","epub","mobi","azw3","djvu","chm",
  // media
  "mp4","mkv","webm","avi","mov","wmv","flv","m4v","mpg","mpeg","ts","m2ts","ogv",
  "mp3","flac","wav","aac","ogg","oga","opus","m4a","wma","alac","aiff",
  // data / dev
  "csv","tsv","sqlite","db","dmp","bin","iso","vdi","vmdk","qcow2","ova","torrent","patch","diff",
]);

/* ------------------------------------------------------------------ *
 * Decision
 * ------------------------------------------------------------------ */

const SKIP = (reason) => ({ capture: false, reason });
const TAKE = (reason) => ({ capture: true, reason });

/**
 * Decide whether a response should be diverted to LDM.
 *
 * `req`  – recorded request context {method, url, type}
 * `res`  – {statusCode, headers (lowercased map), url}
 * `cfg`  – user settings
 * `state`– {bypass: Set<string>} URLs the user chose to hand back to Firefox
 */
function classify(req, res, cfg, state) {
  if (!cfg.enabled) return SKIP("extension disabled");

  // --- Things we structurally cannot re-fetch out of process --------
  const url = res.url || req.url;
  if (!/^https?:\/\//i.test(url)) return SKIP("non-http scheme");

  // A POST-initiated download cannot be replayed as a GET by aria2; the
  // server would reject it or hand back a different body.
  if (req.method !== "GET") return SKIP("non-GET method");

  // 206 means a range request: either media playback or a browser resume.
  // 3xx never carries the body. Only a plain 200 is a whole file.
  if (res.statusCode !== 200) return SKIP("status " + res.statusCode);

  if (!CAPTURABLE_TYPES.has(req.type)) return SKIP("resource type " + req.type);

  // --- User overrides ----------------------------------------------
  if (state.bypass.has(url)) {
    state.bypass.delete(url);
    return SKIP("user bypass");
  }
  const host = hostOf(url);
  if (host && cfg.blockedSites.some((s) => hostMatches(host, s)))
    return SKIP("site excluded");

  const headers = res.headers;
  const mime = mimeOf(headers);
  const size = sizeOf(headers);
  const name = deriveFilename(url, headers);
  const ext = extensionOf(name);

  if (cfg.blockedExtensions.includes(ext)) return SKIP("extension excluded");

  // --- Strongest signal: the server explicitly said "save me" -------
  const cd = parseContentDisposition(headers["content-disposition"]);
  if (cd.disposition === "attachment") return TAKE("content-disposition");

  // Below here everything is size-gated: grabbing a 4 KB file adds latency
  // and gains nothing, and tiny hits are usually API responses.
  const bigEnough = size < 0 || size >= cfg.minSize;

  // An unmistakably binary extension beats a *generic* Content-Type, but never
  // a specific one. A server that says "application/json" or "text/html" has
  // identified its response deliberately — commonly a 200-with-error-page at a
  // /thing.zip URL, which would be worse saved than rendered.
  if (ext && BINARY_EXTENSIONS.has(ext) && GENERIC_MIME.has(mime))
    return bigEnough ? TAKE("binary extension ." + ext) : SKIP("below size threshold");

  // --- Types the browser is meant to render ------------------------
  if (NEVER_MIME.has(mime)) return SKIP("renderable mime " + mime);
  if (NEVER_MIME_PREFIX.some((p) => mime.startsWith(p)))
    return SKIP("renderable mime " + mime);

  const mimeSaysFile =
    ALWAYS_MIME.has(mime) || ALWAYS_MIME_PREFIX.some((p) => mime.startsWith(p));
  if (mimeSaysFile)
    return bigEnough ? TAKE("mime " + mime) : SKIP("below size threshold");

  // Standalone audio/video navigations are downloads; in-page playback was
  // already filtered out by the resource-type check above.
  if (mime.startsWith("video/") || mime.startsWith("audio/"))
    return bigEnough ? TAKE("media navigation") : SKIP("below size threshold");

  if (ext && FILE_EXTENSIONS.has(ext))
    return bigEnough ? TAKE("extension ." + ext) : SKIP("below size threshold");

  return SKIP("no download signal");
}

/* ------------------------------------------------------------------ *
 * Host matching
 * ------------------------------------------------------------------ */

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

/** "example.com" matches example.com and any subdomain of it. */
function hostMatches(host, pattern) {
  const p = String(pattern || "").trim().toLowerCase().replace(/^\*\./, "");
  if (!p) return false;
  return host === p || host.endsWith("." + p);
}
