"use strict";

/**
 * Reads back downloads the page built in memory.
 *
 * A growing number of sites — chat clients above all — never link to a file at
 * all. They fetch the bytes with script, wrap them in a Blob and click an
 * `<a download>` at the handle `URL.createObjectURL` gave them. What reaches
 * the browser's download list is then `blob:https://site/<uuid>`, which names
 * nothing outside the document that made it: no downloader, MDM included, can
 * fetch it. Those downloads always fell through to Firefox, which is why a
 * photo saved from one chat was captured and the same photo from another was
 * not — the difference was never the chat, it was how the page delivered it.
 *
 * This script closes that gap. It runs in the page, where the handle is
 * meaningful, and answers the background page with the bytes themselves.
 */

(() => {
  /**
   * How long a revoked blob is kept alive.
   *
   * The usual shape is `a.click(); URL.revokeObjectURL(url)` on the very next
   * line, so by the time the download event reaches the extension the handle
   * is already dead. Firefox's own download holds its own reference and
   * survives that; we have no such reference, so revocation is deferred just
   * long enough for the hand-off to read the blob. Nothing else changes: the
   * page's call still happens, a moment later, and the memory is still freed.
   */
  const RETAIN_MS = 45_000;

  /**
   * Defer the page's revocations.
   *
   * `exportFunction` is what lets a content script put a function of its own
   * into the page's global: assigning directly would only replace the property
   * on our side of the Xray wrapper, where the page never looks.
   */
  function retainBlobs() {
    if (typeof exportFunction !== "function") return null;
    const page = window.wrappedJSObject;
    if (!page || !page.URL || typeof page.URL.revokeObjectURL !== "function") return null;

    const original = page.URL.revokeObjectURL;
    page.URL.revokeObjectURL = exportFunction(function (url) {
      setTimeout(() => {
        try {
          original.call(page.URL, url);
        } catch {
          /* the page may have torn its own URL object down */
        }
      }, RETAIN_MS);
    }, page);

    return () => {
      try {
        page.URL.revokeObjectURL = original;
      } catch {
        /* nothing to put back into a page that has gone */
      }
    };
  }

  // Wrapped immediately, before the page has run a line: a setting read is
  // asynchronous and the first download can happen sooner than it resolves.
  // Deferring a revocation costs a page nothing, so the safe order is to do it
  // and undo it, rather than to miss the downloads that arrive in between.
  let undo = null;
  try {
    undo = retainBlobs();
  } catch (e) {
    console.warn("[mdm] could not defer blob revocation:", e.message);
  }

  browser.storage.local
    .get("settings")
    .then(({ settings }) => {
      if ((settings || {}).captureBlobs === false && undo) {
        undo();
        undo = null;
      }
    })
    .catch(() => {});

  /* ---------------------------------------------------------------- *
   * Reading one back
   * ---------------------------------------------------------------- */

  /**
   * base64 without a data: round-trip.
   *
   * `btoa` takes a string, so the bytes are walked in chunks small enough to
   * pass through `String.fromCharCode` as arguments — a whole multi-megabyte
   * buffer at once overflows the argument list and throws.
   */
  function base64(buffer) {
    const bytes = new Uint8Array(buffer);
    const CHUNK = 0x8000;
    let binary = "";
    for (let i = 0; i < bytes.length; i += CHUNK) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
    }
    return btoa(binary);
  }

  async function readBlob(url, limit) {
    const response = await fetch(url);
    const blob = await response.blob();
    if (blob.size > limit) {
      return { ok: false, error: `blob is ${blob.size} bytes, over the limit` };
    }
    return {
      ok: true,
      size: blob.size,
      mime: blob.type || "",
      data: base64(await blob.arrayBuffer()),
    };
  }

  browser.runtime.onMessage.addListener((msg) => {
    if (!msg || msg.type !== "mdm-read-blob") return undefined;
    // Every frame on the origin is asked, because the background page cannot
    // know which one made the blob. Returning nothing means "not mine", and
    // lets the asker move on to the next frame; only the frame that can
    // actually resolve the handle answers.
    if (!String(msg.url || "").startsWith(`blob:${location.origin}/`)) return undefined;
    return readBlob(msg.url, msg.limit).catch((e) => ({
      ok: false,
      error: e.message || String(e),
    }));
  });
})();
