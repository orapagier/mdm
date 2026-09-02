"use strict";

/**
 * Connection to the LDM native messaging host.
 *
 * The port is deliberately long-lived rather than one-shot: under Manifest V3
 * the background context is an event page that Firefox may suspend, and an
 * open native port keeps it resident. That matters here because a suspended
 * page cannot service a *blocking* webRequest listener promptly.
 */
const Native = (() => {
  const HOST = "io.ldm.host";
  const BACKOFF_MS = [500, 1000, 2000, 5000, 10000, 30000];

  let port = null;
  let attempt = 0;
  let reconnectTimer = null;
  let nextId = 1;
  const pending = new Map(); // id -> {resolve, reject, timer}
  const listeners = new Set();

  let available = false;

  function connect() {
    if (port) return port;
    try {
      port = browser.runtime.connectNative(HOST);
    } catch (e) {
      console.warn("[ldm] connectNative failed:", e);
      port = null;
      scheduleReconnect();
      return null;
    }
    port.onMessage.addListener(onMessage);
    port.onDisconnect.addListener(onDisconnect);
    available = true;
    attempt = 0;
    post({ type: "hello", version: browser.runtime.getManifest().version });
    return port;
  }

  function onDisconnect(p) {
    const err = p.error || (browser.runtime.lastError ?? null);
    if (err) console.warn("[ldm] native host disconnected:", err.message || err);
    port = null;
    available = false;
    // Nothing can be answered any more; fail every waiter so blocking
    // listeners resolve instead of hanging the request.
    for (const [, w] of pending) {
      clearTimeout(w.timer);
      w.reject(new Error("native host disconnected"));
    }
    pending.clear();
    notify({ type: "hostState", connected: false });
    scheduleReconnect();
  }

  function scheduleReconnect() {
    if (reconnectTimer) return;
    const delay = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
    attempt++;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delay);
  }

  function onMessage(msg) {
    if (msg && msg.id && pending.has(msg.id)) {
      const w = pending.get(msg.id);
      pending.delete(msg.id);
      clearTimeout(w.timer);
      w.resolve(msg);
      return;
    }
    notify(msg);
  }

  function notify(msg) {
    for (const fn of listeners) {
      try {
        fn(msg);
      } catch (e) {
        console.error("[ldm] listener error", e);
      }
    }
  }

  function post(msg) {
    const p = connect();
    if (!p) return false;
    try {
      p.postMessage(msg);
      return true;
    } catch (e) {
      console.warn("[ldm] postMessage failed:", e);
      return false;
    }
  }

  /**
   * Send a message and await the host's reply.
   * Rejects on timeout — callers treat that as "let Firefox handle it".
   */
  function request(msg, timeoutMs = 2000) {
    return new Promise((resolve, reject) => {
      const id = String(nextId++);
      const p = connect();
      if (!p) return reject(new Error("native host unavailable"));
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error("native host timeout"));
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer });
      try {
        p.postMessage({ ...msg, id });
      } catch (e) {
        pending.delete(id);
        clearTimeout(timer);
        reject(e);
      }
    });
  }

  return {
    connect,
    post,
    request,
    isAvailable: () => available,
    onMessage: (fn) => listeners.add(fn),
  };
})();
