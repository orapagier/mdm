"use strict";

/* The page a held request waits on, for the browser that cannot hold one.
 *
 * Manifest V3 took away the blocking listener that lets Firefox keep a request
 * open while MDM goes and asks the server. What it left is declarativeNetRequest,
 * which can redirect a request before it is sent — and a redirect is a hold, so
 * long as something takes responsibility for letting go again. This page is that
 * something.
 *
 * The sequence, and why each step is where it is:
 *
 *   1. Disarm first, before anything else can go wrong. The rule that sent the
 *      browser here is removed the moment this page loads, so the navigation
 *      made below cannot be redirected back to this page. That is the whole of
 *      the loop protection, and it does not depend on a rule being added
 *      correctly under a failure — only on one being removed.
 *   2. Ask MDM. The address has not been requested by anything yet, so MDM's
 *      request is the first one, and on a host that spends its links that is
 *      the difference between having the file and having the landing page.
 *   3. Let go, whichever way it went. Taken means the file is downloading in
 *      MDM and this tab should go back where it came from; not taken — a page,
 *      a refusal, an app that is not running, an answer that never came — means
 *      the browser goes to the address after all, exactly as if none of this had
 *      happened.
 *
 * Step 3 runs on every path out of step 2, including the ones nobody thought of:
 * a tab left sitting on this page is a navigation the user asked for and did not
 * get, which is a worse failure than any download.
 */

/* In the fragment rather than the query, and that is not a style choice. The
 * redirect is written by a declarativeNetRequest substitution, which pastes the
 * matched address in verbatim — it does not percent-encode it. As a query
 * parameter the first `&` in the original address would have ended the value,
 * and the tokens these hosts sign their links with contain `&` often enough
 * that it would have worked for most links and silently truncated the rest.
 * Everything after the first `#` is the fragment, so there is nothing to
 * truncate. */
function addressFromFragment() {
  const raw = location.hash.slice(1);
  if (!raw) return new URLSearchParams(location.search).get("u") || "";
  // Verbatim first. The substitution does not encode, so the fragment already
  // *is* the address, and decoding it again would turn a `%2F` inside a signed
  // token into a slash — a different address, on a host that will not answer to
  // it. Decoding is only the fallback for a browser that encoded the fragment
  // on the way in, which is why it is tried second and only when the raw text
  // is not already an address.
  if (/^https?:\/\//i.test(raw)) return raw;
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

const target = addressFromFragment();

const status = document.getElementById("status");
document.getElementById("url").textContent = target;

/** Send the browser where it was going, and stop being in the way. */
function releaseTo(url) {
  if (!/^https?:\/\//i.test(url)) {
    // Nothing safe to navigate to. Going back is still better than sitting on
    // an extension page the user never asked for.
    goBack();
    return;
  }
  // `replace`, so this page does not become an entry in the history the user
  // has to click past on the way back.
  location.replace(url);
}

/** Leave the page the click came from on screen, having taken the download. */
function goBack() {
  // A download opened in a new tab has no history to go back to; closing is
  // what "put this back how it was" means there. A tab that will not close --
  // one the extension did not open -- falls through to the blank page, which
  // is why the message below is left saying what happened.
  if (history.length > 1) {
    history.back();
    return;
  }
  window.close();
}

/**
 * How long to sit here.
 *
 * The background gives MDM ten seconds and MDM gives the server six, so an
 * answer arrives well inside this. It exists for the case where no answer
 * arrives at all — a service worker that was stopped mid-question, an extension
 * being reloaded — where the only wrong thing to do is keep waiting.
 */
const GIVE_UP_MS = 12000;

(async () => {
  // Before anything that can fail: see the note at the top.
  try {
    await browser.runtime.sendMessage({ type: "disarmPreempt" });
  } catch {
    /* the background is gone; the rules go with it */
  }

  if (!target) return releaseTo(target);

  let taken = false;
  try {
    const reply = await Promise.race([
      browser.runtime.sendMessage({ type: "preemptHeld", url: target, pageUrl: document.referrer }),
      new Promise((resolve) => setTimeout(() => resolve(null), GIVE_UP_MS)),
    ]);
    taken = !!(reply && reply.accepted);
  } catch {
    /* no background to ask; the browser gets it */
  }

  if (taken) {
    status.textContent = "MDM has it — returning to the page.";
    goBack();
  } else {
    releaseTo(target);
  }
})();
