"use strict";
/**
 * The click net: net 0 for a browser that cannot hold a request.
 *
 * What this has to get right is not the happy path. It cancels a click before
 * it knows whether MDM wants the address, so every way of being wrong ends
 * with somebody's click having done nothing — and on a file host, a click that
 * does nothing is indistinguishable from a broken site. So most of what is
 * checked here is the failing open: no app, no answer, a page rather than a
 * file, a host that was never on the list.
 *
 * Run: node extension/test/preempt.test.js
 */

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const SRC = path.join(__dirname, "..", "src");

/**
 * Load preempt.js as a browser would, with the extension API stubbed.
 *
 * `permissions` is what makes this Firefox or Chromium: the file asks the
 * manifest whether this build was granted `webRequestBlocking`, because what
 * differs between the two is that permission and not a user-agent string.
 */
function load({
  permissions = [],
  hosts = [],
  reply = null,
  href = "https://site.test/",
  popupsBlocked = false,
} = {}) {
  const listeners = {};
  const navigations = [];
  const sent = [];
  let storageChanged = null;

  const context = {
    console,
    URL,
    setTimeout,
    clearTimeout,
    location: { href },
    addEventListener: (type, fn) => ((listeners[type] ||= []).push(fn)),
    window: {
      // What a browser returns: a window when the popup is allowed, null when
      // the blocker takes it. The click net has already awaited MDM's answer by
      // the time it calls this, so the gesture is spent and null is the common
      // case rather than the exotic one.
      open: (url) => {
        if (popupsBlocked) return null;
        navigations.push({ url, newTab: true });
        return {};
      },
    },
    browser: {
      runtime: {
        getManifest: () => ({ permissions }),
        sendMessage: async (msg) => {
          sent.push(msg);
          if (reply instanceof Error) throw reply;
          if (typeof reply === "function") return reply(msg);
          return reply;
        },
      },
      storage: {
        local: { get: async () => ({ singleUseHosts: hosts }) },
        onChanged: { addListener: (fn) => (storageChanged = fn) },
      },
    },
  };
  // `window.location` and the bare `location` are the same object, so a script
  // assigning `window.location.href` and one assigning `location.href` are the
  // same assignment — which is what the file does to send the browser after a
  // link MDM did not take.
  context.window.location = context.location;
  Object.defineProperty(context.location, "href", {
    get: () => href,
    set: (u) => navigations.push({ url: u, newTab: false }),
  });

  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(SRC, "content", "preempt.js"), "utf8"), context, {
    filename: "preempt.js",
  });
  return { context, listeners, navigations, sent, changed: () => storageChanged };
}

/** A left click on a link, shaped the way the DOM delivers one. */
function clickOn(url, extra = {}) {
  const link = { nodeType: 1, tagName: "A", href: url, target: extra.target || "" };
  let prevented = false;
  return {
    event: {
      button: 0,
      ctrlKey: false,
      metaKey: false,
      shiftKey: false,
      altKey: false,
      defaultPrevented: false,
      composedPath: () => [link],
      target: link,
      preventDefault: () => (prevented = true),
      ...extra,
    },
    link,
    wasPrevented: () => prevented,
  };
}

/** Let the storage read at the top of the file settle before clicking. */
const settled = () => new Promise((r) => setImmediate(r));

const tests = [];
const check = (name, fn) => tests.push({ name, fn });

/* --------------------------------------------------------------- *
 * Which browser this is
 * --------------------------------------------------------------- */

check("Firefox registers nothing — its held request is the better net", async () => {
  const { listeners } = load({ permissions: ["webRequest", "webRequestBlocking"] });
  await settled();
  assert.strictEqual(listeners.click, undefined, "the click net ran alongside net 0");
  assert.strictEqual(listeners.auxclick, undefined);
});

check("Chromium registers the click and the middle click", async () => {
  const { listeners } = load({ permissions: ["webRequest"] });
  await settled();
  assert.strictEqual(listeners.click.length, 1);
  // The middle button opens a download in a new tab, and spends the link
  // exactly as a left click does.
  assert.strictEqual(listeners.auxclick.length, 1);
});

/* --------------------------------------------------------------- *
 * Which clicks it acts on
 * --------------------------------------------------------------- */

check("a host MDM has been caught out by is taken before the browser", async () => {
  const { listeners, navigations, sent } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: true },
  });
  await settled();
  const c = clickOn("https://filekeeper.net/get/abc123");
  await listeners.click[0](c.event);
  assert.ok(c.wasPrevented(), "the browser was allowed to spend the link");
  assert.deepStrictEqual(
    sent.map((m) => m.type),
    ["preemptClick"]
  );
  assert.strictEqual(navigations.length, 0, "the click was sent on as well as taken");
});

check("a subdomain of a listed host counts", async () => {
  const { listeners, sent } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: true },
  });
  await settled();
  await listeners.click[0](clickOn("https://dl3.filekeeper.net/get/abc").event);
  assert.strictEqual(sent.length, 1, "a download link on a subdomain was left to the browser");
});

check("every other host is left entirely alone", async () => {
  const { listeners, navigations, sent } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
  });
  await settled();
  const c = clickOn("https://example.com/a.zip");
  await listeners.click[0](c.event);
  assert.ok(!c.wasPrevented(), "an ordinary link was cancelled");
  assert.strictEqual(sent.length, 0);
  assert.strictEqual(navigations.length, 0, "the file was navigated to by hand");
});

check("with no list at all, nothing is touched", async () => {
  const { listeners, sent } = load({ permissions: ["webRequest"], hosts: [] });
  await settled();
  const c = clickOn("https://filekeeper.net/get/abc");
  await listeners.click[0](c.event);
  assert.ok(!c.wasPrevented());
  assert.strictEqual(sent.length, 0);
});

check("a click the page already handled is not taken twice", async () => {
  const { listeners, sent } = load({ permissions: ["webRequest"], hosts: ["filekeeper.net"] });
  await settled();
  const c = clickOn("https://filekeeper.net/get/abc", { defaultPrevented: true });
  await listeners.click[0](c.event);
  assert.strictEqual(sent.length, 0);
});

check("a non-http link is not a download", async () => {
  const { listeners, sent } = load({ permissions: ["webRequest"], hosts: ["filekeeper.net"] });
  await settled();
  await listeners.click[0](clickOn("mailto:someone@filekeeper.net").event);
  assert.strictEqual(sent.length, 0);
});

/* --------------------------------------------------------------- *
 * Failing open — the half that matters
 * --------------------------------------------------------------- */

check("an address that turns out to be a page is followed after all", async () => {
  const { listeners, navigations } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: false, reason: "that address is a page" },
  });
  await settled();
  const c = clickOn("https://filekeeper.net/faq");
  await listeners.click[0](c.event);
  assert.ok(c.wasPrevented());
  assert.deepStrictEqual(navigations, [{ url: "https://filekeeper.net/faq", newTab: false }]);
});

check("an app that is not running is not a lost click", async () => {
  const { listeners, navigations } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: false, reason: "MDM is not running" },
  });
  await settled();
  await listeners.click[0](clickOn("https://filekeeper.net/get/abc").event);
  assert.strictEqual(navigations.length, 1, "the click did nothing at all");
});

check("a background script that has gone is not a lost click either", async () => {
  const { listeners, navigations } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: new Error("Extension context invalidated"),
  });
  await settled();
  await listeners.click[0](clickOn("https://filekeeper.net/get/abc").event);
  assert.deepStrictEqual(navigations, [
    { url: "https://filekeeper.net/get/abc", newTab: false },
  ]);
});

check("a click that would have opened a tab opens one", async () => {
  const { listeners, navigations } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: false },
  });
  await settled();
  await listeners.click[0](
    clickOn("https://filekeeper.net/get/abc", { ctrlKey: true }).event
  );
  assert.deepStrictEqual(navigations, [
    { url: "https://filekeeper.net/get/abc", newTab: true },
  ]);
});

check("a blocked popup lands in this tab rather than nowhere", async () => {
  // The failure this prevents: `window.open` runs after the await, so it is no
  // longer a user gesture and the browser refuses it. Refused silently — it
  // returns null rather than throwing — so the click simply did nothing, on
  // exactly the button the whole feature exists to make work.
  const { listeners, navigations } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: false },
    popupsBlocked: true,
  });
  await settled();
  await listeners.click[0](
    clickOn("https://filekeeper.net/get/abc", { ctrlKey: true }).event
  );
  assert.deepStrictEqual(navigations, [
    { url: "https://filekeeper.net/get/abc", newTab: false },
  ]);
});

check("target=_blank is a new tab too", async () => {
  const { listeners, navigations } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: false },
  });
  await settled();
  await listeners.click[0](
    clickOn("https://filekeeper.net/get/abc", { target: "_blank" }).event
  );
  assert.strictEqual(navigations[0].newTab, true);
});

/* --------------------------------------------------------------- *
 * What it tells the background
 * --------------------------------------------------------------- */

check("the page the click came from is carried, for the Referer", async () => {
  const { listeners, sent } = load({
    permissions: ["webRequest"],
    hosts: ["filekeeper.net"],
    reply: { accepted: true },
    href: "https://filekeeper.net/file/abc123",
  });
  await settled();
  await listeners.click[0](clickOn("https://dl3.filekeeper.net/get/abc").event);
  // Spread into this realm's Object before comparing: the message is built
  // inside the vm, so it is a plain object with a different prototype and
  // deepStrictEqual counts that as a difference.
  assert.deepStrictEqual({ ...sent[0] }, {
    type: "preemptClick",
    url: "https://dl3.filekeeper.net/get/abc",
    pageUrl: "https://filekeeper.net/file/abc123",
  });
});

check("the list can arrive after the page has loaded", async () => {
  // MDM learns a host the moment a capture from it fails, and the background
  // republishes as it learns. A tab already open when that happens is exactly
  // the tab the next download will be started from.
  const { listeners, sent, changed } = load({ permissions: ["webRequest"], hosts: [] });
  await settled();
  changed()({ singleUseHosts: { newValue: ["filekeeper.net"] } }, "local");
  await listeners.click[0](clickOn("https://filekeeper.net/get/abc").event);
  assert.strictEqual(sent.length, 1, "a host learned this session was not picked up");
});

/* --------------------------------------------------------------- */

(async () => {
  let passed = 0;
  const failures = [];
  for (const { name, fn } of tests) {
    try {
      await fn();
      passed++;
    } catch (e) {
      failures.push({ name, message: e.message });
    }
  }
  console.log(`\n  ${passed} passed, ${failures.length} failed\n`);
  for (const f of failures) console.log(`  FAIL: ${f.name} — ${f.message}`);
  process.exit(failures.length ? 1 : 0);
})();
