# My Download Manager (MDM)

An IDM-style download manager for Linux and Firefox — yours, not rented. It
captures **every** download the browser makes — binaries, archives, documents,
media — and fetches it with up to 16 parallel connections through aria2.

```
Firefox ──webRequest/downloads──▶ extension ──native messaging──▶ mdm-host
                                                                      │
                                                              unix socket
                                                                      ▼
                                              mdm (Tauri app) ──JSON-RPC──▶ aria2c
                                                       │
                                                       └──▶ yt-dlp (streams)
```

## Why this shape

**Firefox, not Chromium.** Chrome's Manifest V3 removed blocking `webRequest`,
which is the only way to divert a download before the browser commits to it.
Firefox kept it, and additionally lets a blocking listener return a *Promise* —
so a request can be held open while the daemon confirms it accepted the job,
then cancelled. That makes double-downloads structurally impossible rather than
merely unlikely.

**aria2, not a hand-rolled engine.** IDM's speed comes from dynamic
segmentation: when a connection finishes its slice it splits the largest
remaining slice and steals half, so nothing idles at the tail. aria2 does the
same thing via `--min-split-size`, and brings resume, session persistence and
FTP/BitTorrent along for free.

**More than one server per file.** A server answering a download may advertise
its mirrors in the response, as `Link: <https://…>; rel=duplicate` (RFC 6249).
The capture reads those headers — it is the only part of the system that ever
sees them — and hands every mirror to aria2 alongside the original. aria2 then
treats them as sources for *one* file: it spreads its connections across them,
prefers whichever proves fastest, and drops the dead ones. On a mirrored file
that is a multiple, not a percentage, and it is the one thing a single-source
downloader structurally cannot do. Mirrors are stored on the row, because a
resume never gets to see those headers again.

**A daemon, not a process per download.** One long-lived `aria2c` on a private
port with a random secret gives real queueing, live pause/resume and global
throttling through one RPC surface.

## Capture rules

Two independent nets, because neither is sufficient alone:

1. `webRequest.onHeadersReceived` (blocking) — decides from
   `Content-Disposition`, `Content-Type`, `Content-Length` and the file
   extension, then hands over the URL **together with the request headers
   captured at `onBeforeSendHeaders`**. Replaying the original `Cookie`,
   `Referer` and `Authorization` is what makes captured downloads actually
   resolve instead of returning 403.
2. `downloads.onCreated` — the backstop for anything the first net missed
   ("Save Link As", script-initiated downloads), cancelled and erased once the
   daemon confirms.

## Streaming video

Streaming sites are not capturable and no amount of rule-tweaking changes that:
the player fetches ranged fragments into a MediaSource, the `<video>` src is a
`blob:` URL that exists only inside the page, and the underlying streams are
*separate* video-only and audio-only URLs behind expiring signatures. Grabbing
them directly yields a silent, truncated file.

So instead a content script floats a **Download** button over any sizeable
`<video>`, and clicking it sends the *page* URL to yt-dlp, which resolves it
into real formats and offers a quality picker. Since DASH sites serve their
best video without sound, a video-only format is listed under **Video + audio**
and picking it pairs the stream with the best audio (`<id>+bestaudio/<id>`) for
yt-dlp to mux. The **Video, no sound** tab is for deliberately taking the
silent stream on its own.

The picker is a **window of its own**, and starting a download adds a progress
strip beneath the buttons rather than replacing the picker. Clicking a button
on a web page should not raise a whole application, so the main window is
never shown or focused for anything the browser starts — it stays exactly as
it was, hidden included.

A captured *file* opens the same window with the format list left out: a name,
a folder and the same three buttons. Accepting a download from the browser is
not the same as agreeing to fetch it, so the row is created **paused** the
moment it is taken off Firefox's hands — closing the window loses nothing, and
nothing is fetched until Start is pressed. Paused survives a restart for the
same reason: it is a decision, not a state left over from last time. That also means one setting governs
the announcement: the window itself. The extension no longer raises a
notification of its own, which answered to a second switch the app's
"Desktop notifications" had no say over.

Two things make a grab start quickly rather than appearing to hang:

*Extraction is done once, not twice.* Resolving a page — fetching it, fetching
the player script, solving the JS challenge — is the slow part, and the picker
has already paid it. Its raw result is kept for five minutes and handed to the
download as `--load-info-json`, which takes a YouTube start from about ten
seconds to about one, and halves how hard the site is hit.

*aria2 is asked to report.* yt-dlp's own progress hook fires exactly once, at
100%, when an external downloader owns the transfer — so a download would sit
at zero bytes for its whole life and then jump to done. `--summary-interval=1`
makes aria2 print its own readout, which is parsed back into live bytes, speed
and connection count; the exact byte count still comes from the finished file.

The picker's answer is also cached and shared: the window and the request that
opened it ask at the same moment, and one extraction per page serves both.

Deliberately **not** captured, because they cannot work out of process:

| Case | Why |
|---|---|
| `blob:` / `data:` URLs | Exist only inside the page; no external fetch is possible |
| POST-initiated downloads | Cannot be replayed as a GET |
| `206 Partial Content` | A range request — usually a `<video>` element playing |
| `type: "media"` | In-page playback; cancelling it breaks the player |
| Below the size threshold | Segmenting a 4 KB file costs more than it saves |

If the daemon is unreachable or slow to answer, capture **fails open**: Firefox
downloads the file itself. A broken download manager must never mean a broken
browser.

## Install

`install.sh` builds, installs into `~/.local`, registers the native messaging
host and packages the extension. It reads `/etc/os-release` first, so every
dependency it finds missing is named as *your* package manager spells it —
including the Rust version check, which matters on the releases that freeze an
older toolchain than this needs.

**Debian, Ubuntu, Mint, Pop!\_OS…**

```bash
sudo apt install aria2 yt-dlp ffmpeg nodejs zip zenity libnotify-bin \
                 build-essential pkg-config libwebkit2gtk-4.1-dev libdbus-1-dev \
                 rustc cargo
./install.sh
```

**Fedora, RHEL, Nobara…**

```bash
sudo dnf install aria2 yt-dlp ffmpeg nodejs zip zenity libnotify \
                 gcc gcc-c++ make pkgconf-pkg-config webkit2gtk4.1-devel dbus-devel \
                 rust cargo
./install.sh
```

**Arch, Manjaro, EndeavourOS…**

```bash
sudo pacman -S aria2 yt-dlp ffmpeg nodejs zip zenity libnotify \
               base-devel pkgconf webkit2gtk-4.1 dbus rust
./install.sh
```

Two things bite on Debian and Ubuntu specifically, because a stable release
freezes a version for years and both of these move faster than that:

* **Rust.** The workspace needs 1.85 or newer; Debian 12 ships 1.63 and Ubuntu
  22.04 is not much better. `install.sh` refuses to start a build that would
  fail three hundred lines in and points at [rustup.rs](https://rustup.rs),
  which is the right answer on those releases.
* **yt-dlp.** YouTube breaks extraction on a rhythm no frozen package can
  follow, and `apt` will happily report an eight-month-old build as up to date.
  If videos fail while the package is current, `pipx install yt-dlp` tracks
  upstream.

`nodejs` is there for yt-dlp, not for this app — nothing here is written in
JavaScript that Node runs. YouTube obfuscates the `n` parameter on every stream
URL behind a JavaScript challenge, and yt-dlp needs a runtime to execute it;
without one it silently drops every format and reports `The page needs to be
reloaded`, which is a description of neither the cause nor the cure. yt-dlp
enables only `deno` by default, so MDM passes `--js-runtimes` for whichever of
deno, node, quickjs or bun it finds installed. Any one of them is enough.

Then load the extension: `about:debugging#/runtime/this-firefox` →
"Load Temporary Add-on…" → pick `extension/manifest.json`.

Temporary add-ons vanish on restart. For a permanent install Firefox requires a
signed package — submit `target/mdm-firefox.xpi` to addons.mozilla.org
(unlisted self-distribution signing is free), or use Developer Edition with
`xpinstall.signatures.required=false`.

A packaged Firefox reads native messaging manifests from wherever its own
package was built to look: `~/.mozilla` for Debian's `firefox-esr` and
Mozilla's `.deb`, `~/snap/firefox` for the snap Ubuntu installs by default,
`~/.var/app` for the Flatpak. `install.sh` writes to every tree present, and
prints the one `flatpak override` a sandboxed Firefox additionally needs before
it may launch a host binary from `~/.local/bin`.

## Is it fast?

Measure, do not assume — and do not measure the way it is tempting to. Running
every trial of A and then every trial of B reads path drift as a result: on one
ordinary connection, the *same* 100 MB file from the *same* host over a single
connection took 35 s, then 269 s, then 112 s within twenty minutes. Any A/B
laid out sequentially across that would have "proved" whatever ran during the
good minutes.

```bash
python3 packaging/bench.py https://example.com/big.iso --connections 1,16 --rounds 3
```

Every variant runs once per round, the order rotates, and the verdict is
withheld unless the variants separate by more than a single variant varies
between rounds. It will say `INCONCLUSIVE` and mean it.

## Tests

```bash
cargo test                            # engine logic: categories, scheduling, aria2 options
node extension/test/capture.test.js   # capture rules and header parsing
```

The capture tests load `util.js` and `capture.js` in a bare VM context — those
two files hold no `browser.*` reference precisely so the decision logic can be
exercised without a browser.

For an end-to-end check against a server that actually honours `Range`
(Python's `http.server` does not, and would silently mask a broken splitter):

```bash
python3 packaging/range-server.py /some/dir 8732 &
cargo run --example mdm-cli -- http://127.0.0.1:8732/yourfile.bin
python3 packaging/test-native-host.py target/debug/mdm-host
```

## Layout

| Path | What |
|---|---|
| `extension/` | Firefox MV3 extension: capture rules, video button, popup, options |
| `crates/mdm-core/` | Engine, aria2 client, SQLite store, scheduler, IPC |
| `crates/mdm-host/` | Native messaging bridge (dependency-free, std only) |
| `src-tauri/` | Desktop app and its commands |
| `ui/` | Frontend — plain HTML/CSS/JS, no bundler |
| `packaging/` | Icon generator, Range-capable test server, native-host harness |

## Data

| Path | What |
|---|---|
| `~/.config/mdm/settings.toml` | Settings |
| `~/.local/share/mdm/mdm.db` | History and queues |
| `~/.local/share/mdm/aria2.session` | Unfinished transfers, restored on launch |
| `$XDG_RUNTIME_DIR/mdm/mdm.sock` | IPC socket (0700) |
