"use strict";

/* What the app is handed, and the pieces a URL is named from.
 *
 * No `browser.*` anywhere in this file, deliberately: building a job out of a
 * request is a decision, and a decision that needs an event page to run is one
 * nothing can check. Loaded as a classic script into the same global scope as
 * util.js and capture.js, whose helpers it uses.
 */

/**
 * Headers that must not be replayed by the downloader. Hop-by-hop headers are
 * connection-scoped, and Range/Accept-Encoding/Host must be set by the fetcher
 * itself — forwarding Range in particular would truncate every segmented
 * download.
 */
const STRIP_HEADERS = new Set([
  "host","connection","keep-alive","proxy-authorization","proxy-connection",
  "te","trailer","transfer-encoding","upgrade","content-length","range",
  "if-range","accept-encoding","if-modified-since","if-none-match",
  "sec-fetch-dest","sec-fetch-mode","sec-fetch-site","sec-fetch-user",
  "upgrade-insecure-requests","priority",
]);

function forwardableHeaders(list) {
  const out = [];
  for (const h of list || []) {
    if (STRIP_HEADERS.has(h.name.toLowerCase())) continue;
    if (h.value === undefined) continue;
    out.push({ name: h.name, value: h.value });
  }
  return out;
}

function buildJob(req, details, headers, reason) {
  const filename = deriveFilename(details.url, headers);
  return {
    url: details.url,
    // Other servers holding the same bytes, if this one said so. Only the
    // webRequest net sees response headers, so only it can find them.
    mirrors: mirrorsOf(headers, details.url),
    filename,
    size: sizeOf(headers),
    mime: mimeOf(headers),
    headers: req.headers,
    referrer: req.documentUrl || "",
    cookieStoreId: req.cookieStoreId || "",
    tabId: req.tabId ?? -1,
    reason,
    source: "webRequest",
  };
}

/** Enough of a mapping to name a file the browser did not name. */
const MIME_EXTENSION = {
  "image/jpeg": ".jpg", "image/png": ".png", "image/gif": ".gif",
  "image/webp": ".webp", "image/avif": ".avif", "image/bmp": ".bmp",
  "image/tiff": ".tif", "image/heic": ".heic", "image/svg+xml": ".svg",
  "application/pdf": ".pdf", "application/zip": ".zip", "application/json": ".json",
  "text/plain": ".txt", "text/csv": ".csv", "text/html": ".html",
  "video/mp4": ".mp4", "video/webm": ".webm", "video/quicktime": ".mov",
  "audio/mpeg": ".mp3", "audio/ogg": ".ogg", "audio/wav": ".wav",
};

function extensionForMime(mime) {
  return MIME_EXTENSION[(mime || "").split(";", 1)[0].trim().toLowerCase()] || "";
}

function originOfBlob(url) {
  try {
    return new URL(url.replace(/^blob:/i, "")).origin;
  } catch {
    return "";
  }
}

function originOfUrl(url) {
  try {
    return new URL(url).origin;
  } catch {
    return "";
  }
}
