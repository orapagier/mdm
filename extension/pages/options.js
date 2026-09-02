"use strict";

const $ = (id) => document.getElementById(id);
const LINES = (s) => s.split("\n").map((x) => x.trim()).filter(Boolean);
const BOOLS = ["enabled", "sniffMedia", "videoButton"];

async function load() {
  const { cfg } = await browser.runtime.sendMessage({ type: "getState" });
  for (const k of BOOLS) $(k).checked = !!cfg[k];
  $("minSize").value = (cfg.minSize / (1024 * 1024)).toString();
  $("handoffTimeoutMs").value = cfg.handoffTimeoutMs;
  $("blockedSites").value = cfg.blockedSites.join("\n");
  $("blockedExtensions").value = cfg.blockedExtensions.join("\n");
}

$("save").addEventListener("click", async () => {
  const settings = {
    minSize: Math.max(0, Number($("minSize").value) || 0) * 1024 * 1024,
    handoffTimeoutMs: Math.min(10000, Math.max(200, Number($("handoffTimeoutMs").value) || 1500)),
    blockedSites: LINES($("blockedSites").value),
    blockedExtensions: LINES($("blockedExtensions").value).map((e) => e.replace(/^\./, "").toLowerCase()),
  };
  for (const k of BOOLS) settings[k] = $(k).checked;
  await browser.runtime.sendMessage({ type: "setSettings", settings });
  $("saved").textContent = "Saved.";
  setTimeout(() => ($("saved").textContent = ""), 1800);
});

load();
