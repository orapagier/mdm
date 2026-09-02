"use strict";
/**
 * Tests for the capture decision logic and header parsing.
 *
 * util.js and capture.js are deliberately free of any `browser.*` reference so
 * they can be exercised exactly as the extension runs them: as classic scripts
 * sharing one global scope.
 *
 * Run: node extension/test/capture.test.js
 */

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const assert = require("assert");

const SRC = path.join(__dirname, "..", "src");
const context = vm.createContext({ console, TextDecoder, URL });
for (const file of ["util.js", "capture.js"]) {
  vm.runInContext(fs.readFileSync(path.join(SRC, file), "utf8"), context, {
    filename: file,
  });
}
const {
  classify,
  parseContentDisposition,
  deriveFilename,
  sanitizeFilename,
  extensionOf,
  hostMatches,
  headerMap,
  mirrorsOf,
} = context;

const CFG = {
  enabled: true,
  minSize: 1024 * 1024,
  blockedSites: [],
  blockedExtensions: [],
};
const fresh = () => ({ bypass: new Set() });

/** Build the (req, res) pair the real listeners assemble. */
function scenario({
  url = "https://example.com/file.bin",
  method = "GET",
  type = "main_frame",
  status = 200,
  headers = {},
} = {}) {
  return [
    { method, url, type, headers: [] },
    { statusCode: status, headers, url },
  ];
}

let passed = 0;
const failures = [];

function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failures.push(name + "\n      " + e.message);
  }
}

function expectCapture(name, opts, cfg = CFG, state = fresh()) {
  check(name, () => {
    const [req, res] = scenario(opts);
    const v = classify(req, res, cfg, state);
    assert.ok(v.capture, "expected capture, got skip (" + v.reason + ")");
  });
}

function expectSkip(name, opts, cfg = CFG, state = fresh()) {
  check(name, () => {
    const [req, res] = scenario(opts);
    const v = classify(req, res, cfg, state);
    assert.ok(!v.capture, "expected skip, got capture (" + v.reason + ")");
  });
}

/* ---------------- things that must be captured ---------------- */

expectCapture("Content-Disposition attachment", {
  headers: { "content-disposition": 'attachment; filename="report.pdf"' },
});

expectCapture("attachment ignores the size threshold", {
  headers: { "content-disposition": "attachment", "content-length": "512" },
});

expectCapture("large octet-stream", {
  headers: {
    "content-type": "application/octet-stream",
    "content-length": "50000000",
  },
});

expectCapture("archive by extension despite a useless MIME", {
  url: "https://example.com/release.tar.gz",
  headers: { "content-type": "text/plain", "content-length": "9000000" },
});

expectCapture("ISO with no Content-Length", {
  url: "https://cdn.example.com/fedora.iso",
  headers: { "content-type": "application/octet-stream" },
});

expectCapture("standalone video navigation", {
  url: "https://example.com/clip.mp4",
  headers: { "content-type": "video/mp4", "content-length": "80000000" },
});

expectCapture("deb package", {
  url: "https://example.com/pkg.deb",
  headers: {
    "content-type": "application/vnd.debian.binary-package",
    "content-length": "4000000",
  },
});

/* ---------------- things that must NOT be captured ---------------- */

expectSkip("HTML pages", {
  url: "https://example.com/",
  headers: { "content-type": "text/html; charset=utf-8", "content-length": "40000" },
});

expectSkip("images", {
  url: "https://example.com/photo.png",
  headers: { "content-type": "image/png", "content-length": "9000000" },
});

expectSkip("JSON API responses", {
  headers: { "content-type": "application/json", "content-length": "5000000" },
});

expectSkip("in-page media playback", {
  url: "https://example.com/stream.mp4",
  type: "media",
  headers: { "content-type": "video/mp4", "content-length": "80000000" },
});

expectSkip("range responses", {
  status: 206,
  headers: { "content-type": "video/mp4", "content-length": "80000000" },
});

expectSkip("POST-initiated downloads", {
  method: "POST",
  headers: { "content-type": "application/octet-stream", "content-length": "9000000" },
});

expectSkip("blob URLs", {
  url: "blob:https://example.com/2a8f-4c1e",
  headers: { "content-type": "application/octet-stream", "content-length": "9000000" },
});

expectSkip("data URLs", {
  url: "data:application/octet-stream;base64,AAAA",
  headers: { "content-type": "application/octet-stream" },
});

expectSkip("below the size threshold", {
  headers: { "content-type": "application/octet-stream", "content-length": "1024" },
});

expectSkip("redirects", {
  status: 302,
  headers: { "content-type": "application/octet-stream", "content-length": "9000000" },
});

expectSkip("XHR", {
  type: "xmlhttprequest",
  headers: { "content-type": "application/octet-stream", "content-length": "9000000" },
});

expectSkip(
  "excluded site",
  { headers: { "content-disposition": "attachment" } },
  { ...CFG, blockedSites: ["example.com"] }
);

expectSkip(
  "excluded subdomain inherits the rule",
  {
    url: "https://cdn.example.com/x.zip",
    headers: { "content-disposition": "attachment" },
  },
  { ...CFG, blockedSites: ["example.com"] }
);

expectSkip(
  "excluded extension",
  {
    url: "https://example.com/doc.pdf",
    headers: { "content-disposition": "attachment" },
  },
  { ...CFG, blockedExtensions: ["pdf"] }
);

expectSkip(
  "extension disabled",
  { headers: { "content-disposition": "attachment" } },
  { ...CFG, enabled: false }
);

/* ---------------- one-shot bypass ---------------- */

check("bypass applies once, then is consumed", () => {
  const state = { bypass: new Set(["https://example.com/file.bin"]) };
  const [req, res] = scenario({ headers: { "content-disposition": "attachment" } });
  assert.ok(!classify(req, res, CFG, state).capture, "first call should skip");
  assert.strictEqual(state.bypass.size, 0, "bypass should be consumed");
  assert.ok(classify(req, res, CFG, state).capture, "second call should capture");
});

/* ---------------- Content-Disposition parsing ---------------- */

check("plain quoted filename", () => {
  assert.strictEqual(
    parseContentDisposition('attachment; filename="hello world.zip"').filename,
    "hello world.zip"
  );
});

check("RFC 5987 filename* is decoded as UTF-8", () => {
  assert.strictEqual(
    parseContentDisposition(
      "attachment; filename*=UTF-8''Na%C3%AFve%20r%C3%A9sum%C3%A9.pdf"
    ).filename,
    "Naïve résumé.pdf"
  );
});

check("filename* wins over filename", () => {
  assert.strictEqual(
    parseContentDisposition(
      "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''real%20name.pdf"
    ).filename,
    "real name.pdf"
  );
});

check("semicolons inside a quoted filename do not split parameters", () => {
  assert.strictEqual(
    parseContentDisposition('attachment; filename="a;b.zip"').filename,
    "a;b.zip"
  );
});

check("inline disposition is reported, not treated as attachment", () => {
  assert.strictEqual(parseContentDisposition("inline").disposition, "inline");
});

check("missing header yields no filename", () => {
  assert.strictEqual(parseContentDisposition(undefined).filename, null);
});

/* ---------------- filename derivation ---------------- */

check("Content-Disposition beats the URL", () => {
  assert.strictEqual(
    deriveFilename("https://example.com/x/y?z=1", {
      "content-disposition": 'attachment; filename="real.tar.gz"',
    }),
    "real.tar.gz"
  );
});

check("URL fallback strips the query string", () => {
  assert.strictEqual(
    deriveFilename("https://e.com/a/b/file.zip?sig=abc", {}),
    "file.zip"
  );
});

check("percent-escapes in the path are decoded", () => {
  assert.strictEqual(deriveFilename("https://e.com/my%20file.pdf", {}), "my file.pdf");
});

check("a pathless URL still yields a name", () => {
  assert.strictEqual(deriveFilename("https://example.com/", {}), "download");
});

/* ---------------- sanitising ---------------- */

check("path components are stripped, not escaped", () => {
  assert.strictEqual(sanitizeFilename("../../etc/passwd"), "passwd");
  assert.strictEqual(sanitizeFilename("..\\..\\windows\\x.dll"), "x.dll");
  // Nothing may survive that could climb out of the download directory.
  for (const name of ["../../etc/passwd", "a/../../b.zip", "/abs/path.iso"]) {
    const out = sanitizeFilename(name);
    assert.ok(!out.includes("/") && !out.includes("\\"), "separator survived: " + out);
    assert.ok(!out.startsWith("."), "leading dot survived: " + out);
  }
});

check("leading dots are stripped so files are not hidden", () => {
  assert.strictEqual(sanitizeFilename(".bashrc"), "bashrc");
});

check("control characters are replaced", () => {
  assert.strictEqual(sanitizeFilename("a b\nc.zip"), "a b_c.zip");
});

check("long names keep their extension", () => {
  const out = sanitizeFilename("x".repeat(300) + ".tar.gz");
  assert.ok(out.length <= 200, "length was " + out.length);
  assert.ok(out.endsWith(".gz"), "lost extension: " + out.slice(-10));
});

/* ---------------- misc helpers ---------------- */

check("extensionOf rejects junk", () => {
  assert.strictEqual(extensionOf("a.zip"), "zip");
  assert.strictEqual(extensionOf("archive.tar.gz"), "gz");
  assert.strictEqual(extensionOf("noext"), "");
  assert.strictEqual(extensionOf("trailing."), "");
  assert.strictEqual(extensionOf(".bashrc"), "");
});

check("hostMatches covers subdomains but not suffix collisions", () => {
  assert.ok(hostMatches("example.com", "example.com"));
  assert.ok(hostMatches("cdn.example.com", "example.com"));
  assert.ok(hostMatches("example.com", "*.example.com"));
  assert.ok(!hostMatches("notexample.com", "example.com"));
});

check("duplicate headers are joined, not lost", () => {
  const m = headerMap([
    { name: "Set-Cookie", value: "a=1" },
    { name: "Set-Cookie", value: "b=2" },
  ]);
  assert.strictEqual(m["set-cookie"], "a=1, b=2");
});

/* ---------------- mirrors (RFC 6249) ---------------- */

const ORIGIN = "https://a.example.com/debian.iso";

/* `mirrorsOf` runs inside the vm context, so the arrays it returns carry that
   realm's Array.prototype; spreading brings them back into this one, which is
   what deepStrictEqual insists on. */

check("rel=duplicate links become mirrors, best priority first", () => {
  const m = mirrorsOf(
    headerMap([
      { name: "Link", value: '<https://c.example.com/debian.iso>; rel=duplicate; pri=3' },
      { name: "Link", value: '<https://b.example.com/debian.iso>; rel=duplicate; pri=1' },
    ]),
    ORIGIN
  );
  assert.deepStrictEqual([...m], [
    "https://b.example.com/debian.iso",
    "https://c.example.com/debian.iso",
  ]);
});

check("several entries in one Link header are all read", () => {
  const m = mirrorsOf(
    headerMap([{
      name: "Link",
      value: '<https://b.example.com/x.iso>; rel=duplicate; pri=1, ' +
             '<https://c.example.com/x.iso>; rel=duplicate; pri=2',
    }]),
    ORIGIN
  );
  assert.strictEqual(m.length, 2);
});

check("a comma inside a quoted parameter does not split the entry", () => {
  const m = mirrorsOf(
    headerMap([{
      name: "Link",
      value: '<https://b.example.com/x.iso>; rel=duplicate; geo="se,no"',
    }]),
    ORIGIN
  );
  assert.deepStrictEqual([...m], ["https://b.example.com/x.iso"]);
});

check("links that are not duplicates of this file are ignored", () => {
  const m = mirrorsOf(
    headerMap([
      { name: "Link", value: '<https://b.example.com/style.css>; rel=preload' },
      { name: "Link", value: '<https://b.example.com/meta.xml>; rel=describedby' },
    ]),
    ORIGIN
  );
  assert.deepStrictEqual([...m], []);
});

check("non-http and self-referential mirrors are dropped", () => {
  const m = mirrorsOf(
    headerMap([
      { name: "Link", value: '<ftp://b.example.com/x.iso>; rel=duplicate' },
      { name: "Link", value: "<" + ORIGIN + ">; rel=duplicate" },
    ]),
    ORIGIN
  );
  assert.deepStrictEqual([...m], []);
});

check("a flood of mirrors is capped", () => {
  const many = Array.from({ length: 40 }, (_, i) =>
    ({ name: "Link", value: `<https://m${i}.example.com/x.iso>; rel=duplicate; pri=${i + 1}` }));
  assert.strictEqual(mirrorsOf(headerMap(many), ORIGIN).length, 8);
});

check("no Link header means no mirrors, not a crash", () => {
  assert.deepStrictEqual([...mirrorsOf(headerMap([]), ORIGIN)], []);
});

/* ---------------- report ---------------- */

console.log("\n  " + passed + " passed, " + failures.length + " failed\n");
for (const f of failures) console.error("  FAIL: " + f);
process.exit(failures.length ? 1 : 0);
