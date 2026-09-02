"use strict";

const $ = (id) => document.getElementById(id);

function humanSize(n) {
  if (!Number.isFinite(n) || n < 0) return "unknown size";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n < 10 && i > 0 ? n.toFixed(1) : Math.round(n)} ${u[i]}`;
}

/**
 * Firefox 127+ grants host permissions at install, but a user can revoke them
 * at any time from about:addons — and without them the webRequest listeners
 * see nothing, silently. Offer a one-click way back.
 */
async function checkPermissions() {
  let granted = true;
  try {
    granted = await browser.permissions.contains({ origins: ["<all_urls>"] });
  } catch {
    return; // older browser without the API: assume manifest permissions hold
  }
  document.getElementById("perm-warn").hidden = granted;
  if (granted) return;

  // permissions.request must be called from a user gesture, hence the button.
  document.getElementById("perm-grant").addEventListener("click", async () => {
    const ok = await browser.permissions.request({ origins: ["<all_urls>"] });
    if (ok) window.close();
  });
}

async function init() {
  await checkPermissions();
  const [tab] = await browser.tabs.query({ active: true, currentWindow: true });
  const state = await browser.runtime.sendMessage({ type: "getState", tabId: tab?.id });
  if (!state) return;

  $("dot").classList.toggle("on", state.connected);
  $("status").textContent = state.connected
    ? "Connected to the MDM daemon."
    : "MDM is not running — downloads stay in Firefox.";
  $("enabled").checked = state.cfg.enabled;

  const list = $("media");
  if (!state.media.length) {
    $("noMedia").hidden = false;
  } else {
    for (const m of state.media) {
      const li = document.createElement("li");
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = decodeURIComponent(new URL(m.url).pathname.split("/").pop() || m.url);
      const meta = document.createElement("div");
      meta.className = "meta";
      meta.textContent = `${m.kind === "stream" ? "stream" : m.mime || "media"} · ${humanSize(m.size)}`;
      const btn = document.createElement("button");
      btn.textContent = "Download";
      btn.addEventListener("click", () => {
        browser.runtime.sendMessage({ type: "download", url: m.url, referrer: tab?.url, tabId: tab?.id });
        btn.textContent = "Sent";
        btn.disabled = true;
      });
      li.append(name, meta, btn);
      list.append(li);
    }
  }

  $("enabled").addEventListener("change", (e) =>
    browser.runtime.sendMessage({ type: "setSettings", settings: { enabled: e.target.checked } })
  );
  $("open").addEventListener("click", () =>
    browser.runtime.sendMessage({ type: "openApp" })
  );
  $("opts").addEventListener("click", () => browser.runtime.openOptionsPage());
}

init();
