"use strict";

/**
 * The IDM-style floating "Download this video" button.
 *
 * Streaming sites do not serve a file you can intercept: the player fetches
 * ranged fragments into a MediaSource and the <video> src is a blob: URL that
 * exists only inside the page. So rather than trying to capture the stream,
 * this offers a button that hands the *page* URL to yt-dlp, which knows how to
 * resolve it into real formats.
 *
 * The panel lives in a shadow root attached to a fixed-position host, so no
 * page CSS reaches it and nothing in the page's own layout is disturbed.
 */

(() => {
  /**
   * The marker for "this document already has a panel".
   *
   * It has to live in the DOM, not on the content script's global. Each
   * injection gets a fresh sandbox — reloading the extension, or a second copy
   * of it being installed, re-runs this file with a global that knows nothing
   * about the panel already floating over the video. The page's own DOM is the
   * only thing every injection can see, so the previous panel is found there
   * and taken down before this one goes up.
   */
  const HOST_ID = "mdm-video-panel";
  document.getElementById(HOST_ID)?.remove();

  /** Ignore decorative loops, ad bumpers and tracking pixels. */
  const MIN_WIDTH = 240;
  const MIN_HEIGHT = 135;

  let host = null;
  let shadow = null;
  let button = null;
  let label = null;
  let tracked = null;
  let hideTimer = null;
  let rafPending = false;
  let enabled = true;

  /* ---------------------------------------------------------------- *
   * Panel construction
   * ---------------------------------------------------------------- */

  function build() {
    host = document.createElement("div");
    host.id = HOST_ID;
    host.style.cssText =
      "position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647;" +
      "pointer-events:none;border:0;margin:0;padding:0;";
    // documentElement rather than body: some players replace body content.
    (document.documentElement || document.body).appendChild(host);

    shadow = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      :host { all: initial; }
      .btn {
        position: fixed;
        display: none;
        align-items: center;
        gap: 6px;
        padding: 7px 12px;
        border: 0;
        border-radius: 8px;
        background: rgba(20, 22, 28, .86);
        color: #fff;
        font: 500 13px/1 system-ui, -apple-system, "Segoe UI", sans-serif;
        cursor: pointer;
        pointer-events: auto;
        box-shadow: 0 3px 14px rgba(0, 0, 0, .45);
        backdrop-filter: blur(6px);
        transition: opacity .12s ease, transform .12s ease;
        opacity: .92;
      }
      .btn:hover { opacity: 1; transform: translateY(-1px); background: #2f6feb; }
      .btn[data-state="busy"] { opacity: .7; cursor: default; }
      .btn[data-state="busy"]:hover { background: rgba(20,22,28,.86); transform: none; }
      .arrow { font-size: 14px; line-height: 1; }
    `;

    button = document.createElement("button");
    button.className = "btn";
    button.setAttribute("part", "mdm-download");
    const arrow = document.createElement("span");
    arrow.className = "arrow";
    arrow.textContent = "⤓"; // ⤓
    label = document.createElement("span");
    label.textContent = "Download";
    button.append(arrow, label);
    button.addEventListener("click", onClick, true);

    shadow.append(style, button);
  }

  /* ---------------------------------------------------------------- *
   * Which video to offer
   * ---------------------------------------------------------------- */

  function candidates() {
    return [...document.querySelectorAll("video")].filter((v) => {
      const r = v.getBoundingClientRect();
      if (r.width < MIN_WIDTH || r.height < MIN_HEIGHT) return false;
      // Off-screen or hidden players are not what the user is looking at.
      if (r.bottom <= 0 || r.top >= innerHeight) return false;
      const cs = getComputedStyle(v);
      return cs.visibility !== "hidden" && cs.display !== "none" && cs.opacity !== "0";
    });
  }

  /** The largest visible video wins; that is the one being watched. */
  function primary() {
    let best = null;
    let bestArea = 0;
    for (const v of candidates()) {
      const r = v.getBoundingClientRect();
      const area = r.width * r.height;
      if (area > bestArea) {
        bestArea = area;
        best = v;
      }
    }
    return best;
  }

  function place() {
    rafPending = false;
    if (!enabled || !button) return;

    // Fullscreen has its own UI layer and the button would sit over controls.
    if (document.fullscreenElement) {
      button.style.display = "none";
      return;
    }

    const video = primary();
    tracked = video;
    if (!video) {
      button.style.display = "none";
      return;
    }

    const r = video.getBoundingClientRect();

    // Work against the part of the player actually on screen. Clamping to the
    // viewport instead would park the button above a half-scrolled video —
    // floating on the page rather than sitting on the player.
    const top = Math.max(r.top, 0);
    const bottom = Math.min(r.bottom, innerHeight);
    const left = Math.max(r.left, 0);
    const right = Math.min(r.right, innerWidth);

    const PAD = 12;
    button.style.display = "inline-flex";
    const w = button.offsetWidth || 140;
    const h = button.offsetHeight || 32;

    // Too little of the player visible to sit on top of.
    if (bottom - top < h + PAD * 2 || right - left < w + PAD * 2) {
      button.style.display = "none";
      return;
    }

    button.style.top = `${top + PAD}px`;
    button.style.left = `${right - w - PAD}px`;
  }

  function schedule() {
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(place);
  }

  /* ---------------------------------------------------------------- *
   * Action
   * ---------------------------------------------------------------- */

  async function onClick(event) {
    event.preventDefault();
    event.stopPropagation();
    if (button.dataset.state === "busy") return;

    button.dataset.state = "busy";
    label.textContent = "Sending…";

    // A blob:/MediaSource src cannot be fetched outside the page, so the page
    // URL is the only useful thing to hand over; yt-dlp resolves it properly.
    const src = tracked && tracked.currentSrc ? tracked.currentSrc : "";
    const direct = /^https?:\/\//i.test(src) ? src : "";

    try {
      const reply = await browser.runtime.sendMessage({
        type: "grabVideo",
        pageUrl: location.href,
        title: document.title,
        videoSrc: direct,
      });
      if (reply && reply.ok) {
        label.textContent = "Opening MDM…";
      } else {
        // Say what went wrong; "unavailable" sent people hunting for a
        // download that was never accepted.
        label.textContent = (reply && reply.error) || "MDM unavailable";
        button.title = (reply && reply.error) || "";
      }
    } catch (e) {
      label.textContent = "MDM unavailable";
      button.title = e.message || "";
    }

    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      button.dataset.state = "";
      label.textContent = "Download";
    }, 2200);
  }

  /* ---------------------------------------------------------------- *
   * Wiring
   * ---------------------------------------------------------------- */

  function start() {
    build();
    place();

    // Capture phase: players stop propagation of their own media events.
    for (const evt of ["loadedmetadata", "play", "playing", "durationchange"]) {
      document.addEventListener(evt, schedule, true);
    }
    addEventListener("scroll", schedule, { passive: true, capture: true });
    addEventListener("resize", schedule, { passive: true });
    document.addEventListener("fullscreenchange", schedule, true);

    // SPA navigation (YouTube) swaps the player without any page load, and
    // layout shifts do not raise events, so a slow poll backs up the listeners.
    setInterval(() => {
      // A newer injection will have taken our host out of the document and put
      // its own up; stop touching a panel nobody can see any more.
      if (host && !host.isConnected) return;
      schedule();
    }, 1000);
  }

  browser.storage.local
    .get("settings")
    .then(({ settings }) => {
      enabled = (settings || {}).videoButton !== false;
      if (enabled) start();
    })
    .catch(() => start());

  browser.storage.onChanged.addListener((changes, area) => {
    if (area !== "local" || !changes.settings) return;
    enabled = (changes.settings.newValue || {}).videoButton !== false;
    if (!enabled && button) button.style.display = "none";
    else schedule();
  });
})();
