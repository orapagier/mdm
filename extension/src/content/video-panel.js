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

  /**
   * The video being watched.
   *
   * Two readings, because neither alone survives a feed. *Visible* area rather
   * than area: a feed stacks full-size players and scrolling leaves two of
   * them on screen at once, where the one half off the bottom is not the one
   * being watched even when its own box is the larger. And playing outranks
   * paused, because a feed keeps a column of players in the document and plays
   * exactly one — which is the whole answer to which post the user is looking
   * at, on a page where the address bar and the markup both say very little.
   *
   * Weighted rather than absolute: pausing the video before pressing Download
   * is an ordinary thing to do, and it must not leave the button pointing at
   * whatever is playing further down the page.
   */
  function primary() {
    let best = null;
    let bestScore = 0;
    for (const v of candidates()) {
      const r = v.getBoundingClientRect();
      const onScreen =
        Math.max(0, Math.min(r.bottom, innerHeight) - Math.max(r.top, 0)) *
        Math.max(0, Math.min(r.right, innerWidth) - Math.max(r.left, 0));
      const score = onScreen * (v.paused ? 1 : 4);
      if (score > bestScore) {
        bestScore = score;
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
   * What this video could be fetched from
   * ---------------------------------------------------------------- */

  /**
   * Paths that mark a link as one post's own address.
   *
   * A video in a feed has no URL of its own in the address bar — the bar says
   * "facebook.com" or "x.com/home" — but the post around it always carries a
   * permalink, because that is what its timestamp links to. Finding it is what
   * lets the button work on a video nobody has opened, let alone played.
   */
  /** Sections whose *next* path segment names a piece of media. */
  const MEDIA_SECTION =
    /^(?:watch|video|videos|reel|reels|clip|clips|status|post|posts|shorts|embed|p|v)$/i;

  /**
   * Words that sit where an identifier would and name nothing — a site's own
   * navigation, which is most of what a feed page is built from.
   */
  const NOT_AN_ID =
    /^(?:hashtag|tab|tabs|search|live|explore|browse|following|followers|saved|category|categories|page|pages|me|new|popular|trending|feed|home|all|watch|videos?|reels?|shorts)$/i;

  /**
   * Does this URL name one piece of media?
   *
   * A section on its own does not, and on a feed that is the whole difference:
   * "/watch/hashtag/onevoice27" is a hashtag's videos and "/reel/?s=tab" is the
   * Reels tab. Both sit inside a post's own markup as ordinary navigation, so
   * matching the section alone handed one of them to yt-dlp as the video's
   * address — which is how a hashtag feed came back as "Unsupported URL".
   *
   * So what follows the section has to look like an identifier: present, and
   * either plainly numeric or long enough to be an id rather than a word.
   */
  function namesOneMedia(u) {
    const segments = u.pathname.split("/").filter(Boolean);
    for (let i = 0; i < segments.length - 1; i++) {
      if (!MEDIA_SECTION.test(segments[i])) continue;
      const id = segments[i + 1];
      if (NOT_AN_ID.test(id)) continue;
      if (/^\d+$/.test(id) || /^[A-Za-z0-9_.-]{5,}$/.test(id)) return true;
    }
    // The older query forms: facebook's video.php and permalink.php, and the
    // ?v= that YouTube and facebook/watch both still use.
    if (/\/(?:permalink|story|video)\.php$/i.test(u.pathname)) {
      return /[?&](?:v|story_fbid|fbid|id)=[A-Za-z0-9_.-]+/i.test(u.search);
    }
    return /[?&]v=[A-Za-z0-9_.-]{5,}/i.test(u.search);
  }

  /**
   * How far up either chain to climb before giving up.
   *
   * Generous, because this is a bound on a pathological DOM rather than a
   * guess at a normal one — see `permalinkNear`, which measures distance
   * instead of assuming it.
   */
  const MAX_CLIMB = 60;

  function absolute(url) {
    try {
      return new URL(url, location.href).href;
    } catch {
      return "";
    }
  }

  /**
   * The post permalink nearest this video.
   *
   * Nearest by tree distance, not by document order: a feed is a column of
   * posts, and the one being watched is the one the player sits inside. So
   * every candidate link is walked up until it meets the video's own ancestry,
   * and the link whose meeting point is closest to the video wins. That is the
   * link belonging to the same post, whatever a site builds its posts out of.
   *
   * Measuring the distance rather than assuming it is what makes this work on
   * a deep page. Facebook nests a feed video some twenty elements below the
   * post around it; widening a fixed ten levels never reached the timestamp
   * link, which left the feed's own address as the only thing to hand over —
   * and "https://www.facebook.com/" is a page yt-dlp rightly refuses.
   */
  function permalinkNear(video) {
    // The video's ancestry, each element mapped to its distance from the
    // player. Anything a link's own ancestry runs into is a shared container.
    const chain = new Map();
    let node = video;
    for (let d = 0; node && d < MAX_CLIMB; d++, node = node.parentElement) {
      chain.set(node, d);
    }

    let best = "";
    let bestMeeting = Infinity;
    for (const a of document.querySelectorAll("a[href]")) {
      const href = absolute(a.getAttribute("href") || "");
      if (!/^https?:/i.test(href) || href === location.href) continue;
      let u;
      try {
        u = new URL(href);
      } catch {
        continue;
      }
      if (!namesOneMedia(u)) continue;

      for (let p = a, d = 0; p && d < MAX_CLIMB; d++, p = p.parentElement) {
        const meeting = chain.get(p);
        if (meeting === undefined) continue;
        if (meeting < bestMeeting) {
          bestMeeting = meeting;
          best = href;
        }
        break;
      }
    }
    return best;
  }

  /**
   * The permalink whose link box sits over the video.
   *
   * A fallback for players the ancestor walk cannot reach. YouTube's hover
   * preview is one shared player the page moves into whichever card is under
   * the pointer, so how far the card's own link is from the video depends on a
   * layout that is none of our business. What holds regardless is that the
   * link covers the thumbnail the player was dropped on top of — geometry says
   * plainly what the DOM shape only sometimes does.
   */
  function permalinkOver(video) {
    const r = video.getBoundingClientRect();
    const area = r.width * r.height;
    if (!area) return "";

    let best = "";
    let bestOverlap = 0;
    for (const a of document.querySelectorAll("a[href]")) {
      const href = absolute(a.getAttribute("href") || "");
      if (!/^https?:/i.test(href) || href === location.href) continue;
      let u;
      try {
        u = new URL(href);
      } catch {
        continue;
      }
      if (!namesOneMedia(u)) continue;

      const b = a.getBoundingClientRect();
      const overlap =
        Math.max(0, Math.min(r.right, b.right) - Math.max(r.left, b.left)) *
        Math.max(0, Math.min(r.bottom, b.bottom) - Math.max(r.top, b.top));
      if (overlap > bestOverlap) {
        bestOverlap = overlap;
        best = href;
      }
    }
    // A few pixels of contact is the card next door, not this one.
    return bestOverlap > area * 0.3 ? best : "";
  }

  /**
   * Keys under which a site writes down where its video actually is.
   *
   * schema.org settles the first two; the rest are what the players themselves
   * use. `playAddr` and `downloadAddr` are TikTok's, and they earn their place
   * because TikTok's own state is the one spot its video is named in full —
   * the feed carries no permalink to hand yt-dlp, and what the sniffer watches
   * a player fetch is only ever the slices it asked for.
   */
  const MEDIA_KEY =
    /^(?:contenturl|embedurl|playaddr|downloadaddr|playurl|play_url|videourl|video_url|urllist)$/i;

  /** A URL that names a media file by its container. */
  const MEDIA_URL = /\.(?:mp4|m4v|webm|mov|m3u8|mpd)(?:[?#]|$)/i;

  /**
   * A ceiling on how much of one page's state is taken.
   *
   * Not a test of whether the state is about one video — counting URLs is no
   * use for that, because one video names several: TikTok writes down a
   * bitrate ladder and two addresses for each rung, which is eight URLs for a
   * single post. Which of them belong to the post on screen is `statedMedia`'s
   * question. This is only here so a page that names hundreds does not hand
   * hundreds over.
   */
  const MOST_NAMED = 8;

  /** A ceiling on the walk: page state runs to megabytes on a busy feed. */
  const MAX_NODES = 200000;

  /**
   * Every media URL written down inside one blob of the page's own JSON.
   *
   * A site that builds its player with script still has to say somewhere what
   * that player will fetch, and this is where it says it: schema.org in a
   * `ld+json` tag, or the state a framework ships so it can rehydrate itself.
   * Reading it names a video without the player having fetched anything at
   * all, which is the one thing neither the DOM nor the sniffer can offer on a
   * video nobody has scrolled to yet.
   */
  function mediaIn(data) {
    const out = [];
    let budget = MAX_NODES;
    const walk = (v, key) => {
      if (budget-- <= 0) return;
      if (Array.isArray(v)) return v.forEach((item) => walk(item, key));
      if (typeof v === "string") {
        if (/^https?:\/\//i.test(v) && (MEDIA_KEY.test(key) || MEDIA_URL.test(v))) out.push(v);
        return;
      }
      if (!v || typeof v !== "object") return;
      for (const k of Object.keys(v)) walk(v[k], k);
    };
    walk(data, "");
    return [...new Set(out)];
  }

  /**
   * A post id looks like this: a long run of digits and nothing else.
   *
   * Sites that hand out numeric ids hand out big ones — TikTok's posts are
   * nineteen digits, Facebook's fifteen or more — and nothing else in an
   * attribute is that long by accident.
   *
   * Fifteen, not eight. Eight digits is not a post id, it is a timestamp, a
   * view count, a React key or a pixel value, and taking one for an id did
   * real damage rather than merely wasting an extraction: an eight-digit run
   * appears *inside* plenty of nineteen-digit ids, so the wrong number matched
   * a neighbouring post's permalink, and the grab resolved cleanly, downloaded
   * perfectly and produced the wrong video.
   */
  const LONG_ID = /\d{15,25}/g;

  /**
   * Does this URL carry that id, as an id rather than as some digits?
   *
   * `includes` is not the question. Ids sit in a URL among other numbers and
   * are themselves long runs of digits, so a plain substring test lets one id
   * match inside another — which is how a post's own link came back as the
   * link belonging to a completely different post further down the feed.
   */
  function namesId(href, id) {
    const at = href.indexOf(id);
    if (at < 0) return false;
    const before = href[at - 1];
    const after = href[at + id.length];
    return !/\d/.test(before || "") && !/\d/.test(after || "");
  }

  /**
   * The post ids written into the markup around the player, nearest first.
   *
   * A feed has to label its rows to keep track of them, and it labels them
   * with the post's own id — on an element id, or a data attribute. That label
   * is the one thing on a feed that says *which* post the player is currently
   * inside, and it is there whether or not the post carries a visible link and
   * whether or not the player's src is a blob nobody outside the page can
   * fetch. Climbing outward means the nearest label wins, which is the row the
   * video sits in rather than the feed that contains it.
   */
  function idsNear(video) {
    const out = new Set();
    const take = (value) => {
      for (const m of String(value || "").match(LONG_ID) || []) out.add(m);
    };
    let node = video;
    for (let d = 0; node && d < MAX_CLIMB && out.size < 8; d++, node = node.parentElement) {
      take(node.id);
      const data = node.dataset;
      if (data) for (const key of Object.keys(data)) take(data[key]);
    }
    return [...out];
  }

  /**
   * The post's own address, found by its id rather than by its position.
   *
   * `permalinkNear` measures tree distance, which is the best available answer
   * when nothing identifies the post. When something does, this is better:
   * a link carrying this post's id *is* this post's link, however the feed is
   * laid out and however far away the anchor sits.
   */
  function permalinkById(ids) {
    if (!ids.length) return "";
    const links = [...document.querySelectorAll("a[href]")];
    for (const id of ids) {
      for (const a of links) {
        const href = absolute(a.getAttribute("href") || "");
        if (!href || href === location.href || !namesId(href, id)) continue;
        let u;
        try {
          u = new URL(href);
        } catch {
          continue;
        }
        if (namesOneMedia(u)) return href;
      }
    }
    return "";
  }

  /**
   * How a site addresses a post given nothing but its id.
   *
   * Normally there is nothing to build: a post shows its own link and
   * `permalinkById` finds it. A feed need not show one, and TikTok's home feed
   * shows none at all — measured on a live page, it publishes zero post links,
   * plays through a MediaSource so the player's src is a `blob:` nobody
   * outside the document can fetch, and keeps the feed's items out of the page
   * state. Every route to naming the video is closed except the id on the
   * row's own markup, and for a site like that the address is a function of
   * the id.
   *
   * `@i` is TikTok's own placeholder handle: it resolves to whichever account
   * owns the post, so the author's name is not needed. Site knowledge, which
   * this file otherwise avoids — but a feed that names its videos nowhere else
   * leaves no general reading to prefer, and what is built here is offered as
   * one more candidate rather than as the answer. If it resolves to nothing,
   * everything else still gets its turn.
   */
  const POST_URL = [
    [/(^|\.)tiktok\.com$/i, (id) => `https://www.tiktok.com/@i/video/${id}`],
  ];

  /** The post's address built from its id, where the site allows it. */
  function permalinkFromId(ids) {
    if (!ids.length) return "";
    const rule = POST_URL.find(([host]) => host.test(location.hostname));
    // The nearest id: the row the player sits in, not the feed around it.
    return rule ? rule[1](ids[0]) : "";
  }

  /** Fields under which a record states its own id. */
  const ID_KEY = /^(?:id|itemid|item_id|aweme_id|videoid|video_id|postid|post_id)$/i;

  /**
   * The media belonging to one identified post, out of a page's whole state.
   *
   * A feed's state describes every post in it, so reading the lot says nothing
   * about which one is on screen — that is why it is not read on a feed at
   * all. But given the id the markup around the player just supplied, the
   * right record can be picked out of the pile, and only its media taken. That
   * is what lets a feed video be resolved when the player's src is a blob and
   * there is nothing else to go on.
   */
  function mediaForIds(data, ids) {
    if (!ids.length) return [];
    const wanted = new Set(ids);
    const out = [];
    let budget = MAX_NODES;
    const walk = (v) => {
      if (budget-- <= 0 || out.length || !v || typeof v !== "object") return;
      if (Array.isArray(v)) return v.forEach(walk);
      for (const key of Object.keys(v)) {
        const value = v[key];
        if (ID_KEY.test(key) && wanted.has(String(value))) {
          const found = mediaIn(v);
          if (found.length) {
            out.push(...found);
            return;
          }
        }
      }
      for (const key of Object.keys(v)) walk(v[key]);
    };
    walk(data);
    return out;
  }

  /** Every blob of the page's own JSON, parsed. */
  function pageState() {
    const out = [];
    for (const el of document.querySelectorAll('script[type="application/json"]')) {
      try {
        out.push(JSON.parse(el.textContent || ""));
      } catch {
        /* not ours to read */
      }
    }
    return out;
  }

  /** Media the page declares in its own metadata, played or not. */
  function declaredMedia(ids) {
    const out = [];
    const meta = [
      'meta[property="og:video:secure_url"]',
      'meta[property="og:video:url"]',
      'meta[property="og:video"]',
      'meta[name="twitter:player:stream"]',
    ];
    for (const sel of meta) {
      for (const el of document.querySelectorAll(sel)) out.push(absolute(el.content || ""));
    }
    // schema.org VideoObject: the one place a site is expected to say plainly
    // where its video lives. Taken whole, because a page's schema.org block is
    // about that page.
    for (const el of document.querySelectorAll('script[type="application/ld+json"]')) {
      let data;
      try {
        data = JSON.parse(el.textContent || "");
      } catch {
        continue;
      }
      for (const url of mediaIn(data)) out.push(absolute(url));
    }
    // The page's own state, which is where a single-page site keeps what its
    // markup no longer says out loud.
    //
    // Which post it is talking about is settled by the state itself, and the
    // markup around the player says which of them to take. The address bar
    // cannot settle it, which is what this used to ask: a feed rewrites the
    // bar to the post you scroll to, so it reads as one video's own page while
    // the state behind it still describes every row loaded — and reading that
    // whole handed the first row's file over as the video on screen.
    const wanted = (ids && ids.length ? ids : [addressId()]).filter(Boolean);
    for (const data of pageState()) {
      const found = statedMedia(data, wanted);
      for (const url of found.slice(0, MOST_NAMED)) out.push(absolute(url));
    }
    return out;
  }

  /**
   * The media in one blob of page state that belongs to the post on screen.
   *
   * The record the markup pointed at, where the blob has one. Failing that,
   * everything the blob names — but only once it is clear the blob names
   * nobody else, because a feed's state describes fifty posts and taking it
   * whole means offering a stranger's video under this post's name.
   */
  function statedMedia(data, wanted) {
    const mine = mediaForIds(data, wanted);
    if (mine.length) return mine;
    return namesOtherMedia(data, wanted) ? [] : mediaIn(data);
  }

  /**
   * Does this blob put a video under some post other than the one on screen?
   *
   * The question that decides whether a page's state can be read whole, asked
   * of the state rather than of the address. Each media URL is attributed to
   * the nearest post named above it, exactly as `mediaForIds` picks one out;
   * a page about one video attributes its files to that video or to nobody,
   * and anything else is a listing whose records are not ours to take.
   */
  function namesOtherMedia(data, wanted) {
    const mine = new Set(wanted);
    let budget = MAX_NODES;
    let other = false;
    const walk = (v, key, post) => {
      if (budget-- <= 0 || other) return;
      if (Array.isArray(v)) return v.forEach((item) => walk(item, key, post));
      if (typeof v === "string") {
        if (post && !mine.has(post) && /^https?:\/\//i.test(v) &&
            (MEDIA_KEY.test(key) || MEDIA_URL.test(v))) other = true;
        return;
      }
      if (!v || typeof v !== "object") return;
      // The post this record is about, if it says: it stands for everything
      // below it, until a nested record names another.
      let here = post;
      for (const k of Object.keys(v)) {
        if (ID_KEY.test(k) && String(v[k] ?? "")) here = String(v[k]);
      }
      for (const k of Object.keys(v)) walk(v[k], k, here);
    };
    walk(data, "", "");
    return other;
  }

  /**
   * The id the page's own address names, where it names one video.
   *
   * Only for reading the page's state, and only when the markup around the
   * player named nothing: it says which record of a blob the page is talking
   * about, on a page whose markup carries no id of its own.
   */
  function addressId() {
    let u;
    try {
      u = new URL(location.href);
    } catch {
      return "";
    }
    const segments = u.pathname.split("/").filter(Boolean);
    for (let i = 0; i < segments.length - 1; i++) {
      if (!MEDIA_SECTION.test(segments[i])) continue;
      const id = segments[i + 1];
      if (!NOT_AN_ID.test(id) && /^[A-Za-z0-9_.-]{5,}$/.test(id)) return id;
    }
    return (/[?&]v=([A-Za-z0-9_.-]{5,})/i.exec(u.search) || [])[1] || "";
  }

  /**
   * Everything that could resolve to this video, best first.
   *
   * Ordered by how specific it is: the file itself, then the page that is only
   * about this video, then the page we happen to be on.
   */
  function targets(video) {
    const out = [];
    const seen = new Set();
    const add = (url, kind, origin = "") => {
      if (!/^https?:\/\//i.test(url || "") || seen.has(url)) return;
      seen.add(url);
      out.push({ url, kind, origin });
    };

    // What the markup around the player says this post is. Read once: it
    // answers both questions below — which link is this post's, and which
    // record in the page's state describes it.
    const ids = video ? idsNear(video) : [];

    // Marked "player", and nothing else is. Everything else here is a reading
    // of the page — which post the markup says this is, which link sits
    // nearest, what the tab has been fetching — and every one of those can be
    // read wrongly on a feed. This is not a reading: it is the file this
    // element has open, and if the element under the button is the right one
    // then so is this, whatever any page says. A player using MediaSource has
    // a `blob:` here and contributes nothing, which is the honest answer.
    if (video) {
      add(video.currentSrc, "media", "player");
      add(absolute(video.getAttribute("src") || ""), "media", "player");
      for (const source of video.querySelectorAll("source")) {
        add(absolute(source.getAttribute("src") || source.dataset.src || ""), "media", "player");
      }
    }
    for (const url of declaredMedia(ids)) add(url, "media", "page");

    // The id first, where there is one: a link carrying this post's id is this
    // post's link, which beats any reading of where the anchor happens to sit.
    // The other two are offered after rather than instead — they agree on an
    // ordinary post, and where they disagree the app can try each in turn.
    if (video) {
      add(permalinkById(ids), "page");
      add(permalinkFromId(ids), "page");
      add(permalinkNear(video), "page");
      add(permalinkOver(video), "page");
    }
    const canonical = document.querySelector('link[rel="canonical"]');
    if (canonical) add(absolute(canonical.getAttribute("href") || ""), "page");
    const ogUrl = document.querySelector('meta[property="og:url"]');
    if (ogUrl) add(absolute(ogUrl.content || ""), "page");

    return out;
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

    // Asked again here rather than taken from the last placement, because a
    // feed need not raise an event when it moves. TikTok slides the next post
    // in with a transform: no scroll, no resize, nothing to place against, and
    // the answer left over from the last one-second poll can be the post
    // before this one. The click is the moment the question is actually being
    // asked, so it is the moment to read the page.
    const video = primary() || tracked;
    tracked = video;

    // A blob:/MediaSource src cannot be fetched outside the page, so the page
    // URL is the only useful thing to hand over; yt-dlp resolves it properly.
    const src = video && video.currentSrc ? video.currentSrc : "";
    const direct = /^https?:\/\//i.test(src) ? src : "";

    try {
      const reply = await browser.runtime.sendMessage({
        type: "grabVideo",
        pageUrl: location.href,
        title: document.title,
        videoSrc: direct,
        // How long the element under the button says its video is.
        //
        // Not a reading of the page — the page is what a feed misleads about —
        // but what this element has loaded, and it is there even when the src
        // is a `blob:` nobody outside the document can fetch. It is the one
        // fact about the video on screen that survives every other thread
        // going cold, and the window checks what it resolved against it: a
        // neighbouring post extracts just as cleanly and is almost never the
        // same length.
        seconds: video && Number.isFinite(video.duration) ? video.duration : 0,
        // Read out of the DOM at the moment of the click, so a video that has
        // never been played still has somewhere to be fetched from.
        candidates: targets(video),
        // The post's own id, sent on as well as used here. Only the sniffer
        // knows what the tab has been fetching, and on a feed several posts
        // are fetching at once — so which of those files is *this* post's is a
        // question only the background script is in a position to ask, and
        // only with the id the markup gave up.
        ids: video ? idsNear(video) : [],
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
