"use strict";

// No npm dependencies: run with node --test tests/*.test.js.
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

class Element {
  constructor() {
    this.listeners = {};
    this.attributes = {};
    this.style = {};
    this.children = [];
    this.disabled = false;
    this.textContent = "";
  }
  addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
  async emit(type, extras = {}) {
    const event = { preventDefault() { this.defaultPrevented = true; }, ...extras };
    for (const fn of this.listeners[type] || []) await fn(event);
    return event;
  }
  setAttribute(name, value) { this.attributes[name] = value; }
  removeAttribute(name) { delete this.attributes[name]; }
  replaceChildren(...children) { this.children = children; }
}

function setup() {
  const calls = [], prompts = [];
  const elements = new Map();
  const context = vm.createContext({
    document: {
      createElement: () => new Element(),
      getElementById: (id) => {
        if (!elements.has(id)) elements.set(id, new Element());
        return elements.get(id);
      },
      querySelectorAll: () => [],
    },
    window: { __TAURI__: {
      core: { invoke: async (cmd, args) => { calls.push({ cmd, args }); } },
      event: { listen: async () => {} },
    } },
    confirm: (message) => { prompts.push(message); return context.confirmResult; },
    setTimeout: () => 0, clearTimeout: () => {},
  });
  for (const file of ["format.js", "app.js"]) {
    const source = fs.readFileSync(path.join(__dirname, "../ui", file), "utf8");
    // Defer only the startup call; all application functions remain unmodified.
    vm.runInContext(source.replace('init().catch((e) => toast(`Startup failed: ${e}`));', ''), context);
  }
  return { context, calls, prompts, elements };
}

function row() {
  const refs = {};
  for (const key of ["file", "sub", "size", "fill", "bar", "pct", "speed", "eta", "actions"]) {
    refs[key] = new Element();
  }
  refs.lastSignature = "";
  return { refs };
}
const download = { id: 1, filename: "archive.zip", url: "https://example.com/archive.zip",
  directory: "/downloads", category: "Archives", status: "active", totalBytes: 100,
  completedBytes: 0, downloadSpeed: 10 };

test("Cancel never removes a partial download", async () => {
  const { context, calls } = setup();
  context.confirmResult = false;
  const actions = context.actionsFor(() => ({ ...download, completedBytes: 50 }));
  await actions.at(-1).emit("click");
  assert.equal(calls.length, 0);
});

test("cached row actions use the latest progress and filename", async () => {
  const { context, calls, prompts } = setup();
  context.confirmResult = true;
  const entry = row();
  context.updateRow(entry, download);
  const remove = entry.refs.actions.children.at(-1);
  context.updateRow(entry, { ...download, filename: "resolved.zip", completedBytes: 50 });
  assert.equal(entry.refs.actions.children.at(-1), remove);
  await remove.emit("click");
  assert.match(prompts[0], /resolved.zip.*delete the partial file/);
  assert.equal(calls[0].args.deleteFile, true);
});

test("completed files are kept and unknown-size completion fills the bar", async () => {
  const { context, calls } = setup();
  context.confirmResult = true;
  const entry = row();
  context.updateRow(entry, { ...download, status: "complete", totalBytes: 0 });
  assert.equal(entry.refs.fill.style.width, "100.0%");
  await entry.refs.actions.children.at(-1).emit("click");
  assert.equal(calls[0].args.deleteFile, false);
});

test("search combines with status and category filters", () => {
  const { context } = setup();
  vm.runInContext('search = "archive"; filter = {kind: "paused"}', context);
  assert.equal(context.visible(download), false);
  assert.equal(context.visible({ ...download, status: "paused" }), true);
  vm.runInContext('search = "example.com"; filter = {kind: "category", value: "Archives"}', context);
  assert.equal(context.visible(download), true);
  assert.equal(context.visible({ ...download, category: "Video" }), false);
});

function dialog() {
  const dlg = new Element(), form = new Element(), menu = new Element();
  const save = new Element(), cancel = new Element();
  form.querySelector = () => menu;
  form.querySelectorAll = (selector) =>
    selector === 'button[value="cancel"]' ? [cancel] : [save, cancel];
  menu.before = (error) => { form.error = error; };
  dlg.querySelector = () => form;
  dlg.open = true;
  dlg.close = (value) => { dlg.returnValue = value; dlg.open = false; };
  return { dlg, form, save, cancel };
}

test("Escape after an earlier save does not submit again", async () => {
  const { context } = setup();
  const { dlg, form } = dialog();
  let saved = 0;
  context.wireDialogSubmit(dlg, async () => { saved++; });
  await form.emit("submit", { submitter: { value: "ok" } });
  assert.equal(saved, 1);
  dlg.open = true; // showModal does not clear returnValue.
  await dlg.emit("cancel");
  await dlg.emit("close");
  assert.equal(saved, 1);
});

test("failed submission keeps the dialog open and supports retry", async () => {
  const { context } = setup();
  const { dlg, form, save } = dialog();
  let attempts = 0;
  context.wireDialogSubmit(dlg, async () => {
    if (++attempts === 1) throw new Error("Disk full");
  });
  await form.emit("submit", { submitter: { value: "ok" } });
  assert.equal(dlg.open, true);
  assert.equal(form.error.hidden, false);
  assert.match(form.error.textContent, /Disk full/);
  assert.equal(save.disabled, false);
  await form.emit("submit", { submitter: { value: "ok" } });
  assert.equal(dlg.open, false);
});

test("pending saves reject duplicate submissions and Escape", async () => {
  const { context } = setup();
  const { dlg, form } = dialog();
  let finish, saved = 0;
  context.wireDialogSubmit(dlg, () => {
    saved++;
    return new Promise((resolve) => { finish = resolve; });
  });
  const pending = form.emit("submit", { submitter: { value: "ok" } });
  assert.equal((await dlg.emit("cancel")).defaultPrevented, true);
  await form.emit("submit", { submitter: { value: "ok" } });
  assert.equal(saved, 1);
  finish();
  await pending;
});

test("Cancel submits no backend work", async () => {
  const { context } = setup();
  const { dlg, form } = dialog();
  context.wireDialogSubmit(dlg, () => { assert.fail("must not save"); });
  const event = await form.emit("submit", { submitter: { value: "cancel" } });
  assert.equal(event.defaultPrevented, undefined);
});

test("schedule rejects invalid clock minutes instead of normalizing them", () => {
  const { context } = setup();
  for (const input of ["12:60", "00:99", "24:00", "", "nonsense"]) {
    assert.equal(context.timeToMinutes(input), null);
  }
  assert.equal(context.timeToMinutes("23:59"), 1439);
  assert.equal(context.timeToMinutes("00:00"), 0);
});


test("Cancel button closes an invalid form without validation or saving", async () => {
  const { context } = setup();
  const { dlg, cancel } = dialog();
  context.wireDialogSubmit(dlg, () => { assert.fail("must not save"); });
  await cancel.emit("click");
  assert.equal(dlg.open, false);
  assert.equal(dlg.returnValue, "cancel");
});

test("open-file action follows filename changes without rebuilding buttons", async () => {
  const { context, calls } = setup();
  const entry = row();
  context.updateRow(entry, { ...download, status: "complete" });
  const open = entry.refs.actions.children[0];
  context.updateRow(entry, { ...download, status: "complete", filename: "final.zip" });
  assert.equal(entry.refs.actions.children[0], open);
  await open.emit("click");
  assert.equal(calls[0].cmd, "open_path");
  assert.equal(calls[0].args.path, "/downloads/final.zip");
});
