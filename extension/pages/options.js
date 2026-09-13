"use strict";

const $ = (id) => document.getElementById(id);
const LINES = (s) => s.split("\n").map((x) => x.trim()).filter(Boolean);
const BOOLS = ["enabled", "sniffMedia", "videoButton", "captureImages", "captureBlobs"];

async function load() {
  const { cfg } = await browser.runtime.sendMessage({ type: "getState" });
  for (const k of BOOLS) $(k).checked = !!cfg[k];
  $("handoffTimeoutMs").value = cfg.handoffTimeoutMs;
  $("blockedSites").value = cfg.blockedSites.join("\n");
  $("blockedExtensions").value = cfg.blockedExtensions.join("\n");
}

$("save").addEventListener("click", async () => {
  const settings = {
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
