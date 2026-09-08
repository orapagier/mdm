"use strict";

/* Chrome, Edge and every other Chromium browser expose the extension API as
 * `chrome`; Firefox exposes it as `browser`. On Manifest V3 both return
 * promises when no callback is given, so for everything this extension does
 * the two are the same API under two names.
 *
 * Aliasing once, here, is what keeps this a single codebase: nothing else in
 * the extension has to know which browser it is running in. `??=` rather than
 * `=` because Firefox defines both, and its `chrome` is the *callback*
 * flavour — overwriting `browser` with it would quietly turn every awaited
 * call into a promise that never settles.
 */
globalThis.browser ??= globalThis.chrome;
