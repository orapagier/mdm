"use strict";

/* The Chromium entry point.
 *
 * Firefox's MV3 takes a list of background scripts and runs them in one event
 * page. Chromium takes exactly one service worker file, so this is that file:
 * it pulls in the same scripts, in the same order, sharing the same
 * global scope. `importScripts` is synchronous and classic-script, which is
 * what these are — none of them is a module, and none needs to be.
 *
 * Paths are absolute from the extension root rather than relative to this
 * file, so moving it does not silently break the load order.
 */
importScripts(
  "/src/polyfill.js",
  "/src/util.js",
  "/src/capture.js",
  "/src/job.js",
  "/src/singleuse.js",
  "/src/mediarecord.js",
  "/src/native.js",
  "/src/background.js"
);
