"use strict";

/**
 * Formatting shared by the main window and the download window.
 *
 * Loaded as a plain script before either page's own code, so both spell sizes,
 * rates and times the same way. No module system: this app has no bundler and
 * two <script> tags cost less than adding one.
 */

const $ = (id) => document.getElementById(id);

function bytes(n) {
  if (n === null || n === undefined || n < 0) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let v = n, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return i === 0 ? `${n} B` : `${v < 10 ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}

function rate(n) {
  return n > 0 ? `${bytes(n)}/s` : "—";
}

function duration(secs) {
  if (secs === null || secs === undefined || secs < 0) return "—";
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60), s = secs % 60;
  if (m < 60) return `${m}m ${s}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

function eta(d) {
  if (d.status !== "active" || d.downloadSpeed <= 0 || d.totalBytes <= 0) return "—";
  const left = d.totalBytes - d.completedBytes;
  if (left <= 0) return "—";
  return duration(Math.floor(left / d.downloadSpeed));
}
