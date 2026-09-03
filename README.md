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

Three independent nets, because none of them is sufficient alone:

1. `webRequest.onHeadersReceived` (blocking) — decides from
   `Content-Disposition`, `Content-Type`, `Content-Length` and the file
   extension, then hands over the URL **together with the request headers
   captured at `onBeforeSendHeaders`**. Replaying the original `Cookie`,
   `Referer` and `Authorization` is what makes captured downloads actually
   resolve instead of returning 403.
2. `downloads.onCreated` — the backstop for anything the first net missed
   ("Save Link As", script-initiated downloads), cancelled and erased once the
   daemon confirms. Erased *and* removed from disk: a small file can finish
   inside the hand-off, and a download cancelled a moment too late would leave
   a second copy under Firefox's own name.
3. A content script, for downloads that never had a URL to begin with — see
   **In-memory downloads** below.

### Images

A browser exists to *show* pictures, so an image response is a view until
something says otherwise, and capturing every one of them would mean opening a
photo in a tab saved it to disk instead of putting it on screen. Two things
say otherwise: `Content-Disposition: attachment`, and a URL that spells out a
download — `?dl=1` and its cousins, which is what chat and gallery sites append
when the button says Download. Those are captured, and the size floor does not
apply to them: it exists to keep MDM out of automatic captures, and a photo
saved on purpose is a real download at 200 KB.

The **Grab images from this page…** context menu reads the live DOM instead,
offering every picture the page actually put on screen — with the size each one
turned out to be, which is what tells a photograph from an icon. Sniffing them
as they load would be useless here: a page has hundreds and the badge would
drown.

### In-memory downloads

A growing number of sites never link to a file at all. They fetch the bytes
with script, wrap them in a `Blob` and click an `<a download>` at the handle
`URL.createObjectURL` returned. What reaches the downloads API is then
`blob:https://site/<uuid>` — a name for an object inside one document, which
nothing outside that document can resolve. This is why a photo saved from one
Facebook chat was captured and the same photo from another was not: the
difference was never the chat, it was whether the page handed the browser a URL
or a blob.

So MDM asks the page. A content script reads the blob back where it is
meaningful and hands the bytes to the app, which writes them out; the browser's
own copy is cancelled and removed. Two details make it reliable:

* **Revocation is deferred.** The usual shape is `a.click()` followed
  immediately by `URL.revokeObjectURL`, so the handle is dead before the
  extension hears about the download. Firefox's own transfer holds a reference
  and survives that; MDM has none, so the page's revocation is delayed by 45
  seconds. It still happens, and the memory is still freed.
* **There is a ceiling.** The bytes travel base64-encoded through native
  messaging, so blobs over 24 MB are left to Firefox. Blob downloads are
  photos, exports and generated documents; anything genuinely large came from a
  server, and a server can be fetched from properly.

## Streaming video

Streaming sites are not capturable and no amount of rule-tweaking changes that:
the player fetches ranged fragments into a MediaSource, the `<video>` src is a
`blob:` URL that exists only inside the page, and the underlying streams are
*separate* video-only and audio-only URLs behind expiring signatures. Grabbing
them directly yields a silent, truncated file.

So instead a content script floats a **Download** button over any sizeable
`<video>`, and clicking it sends the *page* URL to yt-dlp, which resolves it
into real formats and offers a quality picker.

**The page URL is often not the video's own**, though. Click Download on a
feed and the address bar still says `facebook.com` or `x.com/home`, which no
extractor can make anything of — and that is why the button used to fail until
the video had been opened and played, at which point the address bar finally
named it. So the click now sends what the *page* knows as well: the permalink
nearest the player (a post always carries one, because that is what its
timestamp links to), any file the page declares in `og:video`, a schema.org
`VideoObject` or its own page state, the `<source>` elements, and whatever the
sniffer has watched the player fetch. The window works down that list and takes
the first that resolves, showing which one answered. None of it needs the player to have
started, which is the point: a video can be grabbed without watching it first.
If nothing resolves but a plain media URL was among the candidates, that file
is offered for download as it is — there is nothing to extract, so yt-dlp is
skipped entirely, and the browser's own cookies, `Referer` and `User-Agent`
travel with it so a link signed per session is still answered. That skip is
stated outright rather than left to be inferred from the URL, because a site
serves its files from its own name: a TikTok video comes off
`v16-webapp.tiktok.com`, which reads as "a TikTok page" to anything guessing
from the host. Guessing is only what settles it when nobody has looked.

A page the window believes in is asked about more than once before its refusal
is believed. A site
behind a bot wall serves a challenge to a share of the requests that reach it,
and an extractor reports that as "unable to extract" — from outside, identical
to a page it genuinely cannot read. Six attempts in eight succeeded against one
TikTok video, so believing the first refusal failed a quarter of the grabs on
pages that were perfectly readable. Refusals that are settled facts — a private
video, an unsupported URL, a post blocked for this IP — are still believed the
first time, and so are refusals from candidates that are only guesses: three
asks each across four guesses is how resolving a feed video came to take
minutes rather than seconds. Resolution starts the moment the click arrives
rather than when the window is ready to ask, on both the page and the best
candidate the extension found, since which of the two names a video is exactly
what differs between a post's own page and a feed.

What the sniffer remembers is forgotten on a real navigation, not on a
*rewritten address*. An infinite feed pushes the current post's URL into the
address bar as you scroll without loading anything, and clearing on the tab's
URL changing could not tell that from going somewhere else — so every scroll of
TikTok's home feed threw away what the player had just fetched, and Download
found nothing to offer, while `/explore`, which does not rewrite the address,
worked perfectly. The per-tab ceiling drops the oldest entry rather than
refusing the newest, for the same reason: refusing went deaf part-way down a
feed, holding fifty videos already scrolled past and never the one on screen.

On a feed the post is identified rather than guessed at, because on the worst
of them nothing else survives. TikTok's home feed, measured on a live page,
publishes **zero** post links, plays through a MediaSource so the player's src
is a `blob:` no downloader outside the document can fetch, and keeps the feed's
items out of the page state — every route to naming the video is closed at
once. What is left is the id on the row's own markup, and the address of a post
is a function of its id: `tiktok.com/@i/video/<id>`, where `@i` is TikTok's own
placeholder handle and resolves to whichever account owns the post. That is
site knowledge, which this file otherwise avoids; a feed naming its videos
nowhere else leaves no general reading to prefer, and what is built is offered
as one more candidate rather than as the answer.

Where a post *does* show a link, the id still does the work. Which video the button means is settled by *visible* area and by what is
playing, not by which box is biggest. A feed stacks full-size players, and
mid-scroll two are on screen at once — the one half off the bottom is not the
one being watched even when its own box is larger — while a feed keeps a whole
column of players in the document and plays exactly one. Playing is weighted
rather than absolute, since pausing before pressing Download is an ordinary
thing to do.

A feed labels its rows
with the post's own id, on an element id or a data attribute, and that label is
the one thing saying which post the player is inside — true whether or not the
post shows a link, and whether or not the player's src is a `blob:` nobody
outside the page can fetch. A link carrying that id is that post's permalink,
and the record carrying it in the page's state is that post's record: both are
read out by id, which is why a home feed resolves at all.

Two things decide *which* file that is, because both were got wrong. Order of
arrival is not order of importance — TikTok opens a page by playing a
two-second clip in a hidden element to find out whether the browser can decode
HEVC, and taking the oldest media in the tab downloaded that warm-up instead of
the video — so plain files are ranked newest first, which is the one the player
is on. And a byte range written into a URL is taken off before the file is
asked for: a DASH player fetches a stream a slice at a time, Facebook pins the
slice in the URL as `bytestart`/`byteend`, and saving that yields exactly what
it says — a few hundred kilobytes out of the middle of a file, with no header
on the front, which reports complete and then plays in nothing.

**A response does not always say what it is, but the URL sometimes does.**
Facebook plays a video by fetching two files, and labels both
`Content-Type: video/mp4` — the picture and the *sound on its own*, a couple of
hundred kilobytes that download cleanly, report complete and play as a black
screen. Nothing in the response tells them apart. What does is the `efg`
parameter Facebook signs every CDN URL with: base64 JSON naming the encode
(`dash_v3_426_crf_23_main_3.0_frag_2_audio`) and the post
(`"video_id":10155529876156509`). So a file is ranked by what the site says it
is — sound alone last, and a stream belonging to some other post below the one
the markup around the player identified, which is the answer to a feed handing
over the video from the post above. Read out of the payload's *text*, because a
Facebook video id is past 2^53 and `JSON.parse` quietly rounds it to an id
belonging to nothing. All of it is ordering, never exclusion: a file the site
describes in no way at all stays exactly where it was, and where nothing is
knowable the list is untouched.

**Resolving a page is not the same as resolving the right one.** Every post in
a feed is a real video with a real address, so a candidate that extracts
cleanly is no evidence at all of having extracted the video under the button —
and a grab that comes back with the post above the one on screen downloads
perfectly, plays perfectly, and is the wrong video. What settles it is not
another reading of the page but the file: the sniffer knows exactly which
streams the tab fetched, and an extraction that offers one of those *is* the
video being watched. Compared by the last path segment of the URL, because a
CDN hands the same file out from a different edge host under a fresh signature
every time and only the file token stays put — TikTok's is 38 characters,
Facebook's over a hundred, and both are identical between what the page hands
its own player and what the extractor returns. So candidates are tried until
one is tied to the stream, and where none can be — nothing has played yet, or
the site names its files something as generic as `/main.mp4`, which is not an
identity and is ignored — the picker says so instead of presenting a guess with
a title and a thumbnail that look every bit as authoritative when they are
wrong.

**Provenance decides how much a candidate is worth believing.** The file the
`<video>` element under the button has open is not a reading of the page — it
is what that element is playing — so it is the one piece of evidence a feed
cannot mislead, and it both leads the ranking and is what a resolved page is
checked against. Next to it is a file the site itself attributes to the post
the markup named. When no page can be tied to the video but one of those is a
whole file, that file is taken instead of the page: one of the two is certainly
the video on screen at whatever quality the player chose, and the other is a
coin toss. A quality picker is not worth being shown the wrong video.

**The player knows how long its video is, and a feed cannot lie about that.**
Every other thread can go cold at once — TikTok's home feed publishes no
permalink, plays through a `blob:` nobody outside the document can fetch, and
keeps its rows out of the page state — and the `<video>` element still says
`duration`. It is a fact about the element under the button rather than a
reading of the page, which is the thing a feed misleads about, so it is what a
resolved page is now checked against: every post in a feed extracts just as
cleanly as the right one, with a real title and a real thumbnail, and the one
thing a neighbour almost never shares is its running time. A length that
disagrees is not a doubt, it is an answer, and that page is dropped rather than
kept as a fallback. Two seconds of slack, or two per cent for something long
enough to accumulate more than that.

**A guess does not get to arrive under a Start button.** When no page resolves
and nothing ties any file to the post on screen, there is no reading left that
can tell the right video from the one below it — so the window declines to
answer instead of filling in the likeliest file and leaving Start live over it.
What it offers instead is the two things that actually help: *Try again*, which
usually works, because what refused was a site turning away a share of the
requests that reach it rather than a page that cannot be read — yt-dlp reports
the two identically, and only the failure is uncached; and *Download it
anyway*, for anyone who would rather have the guess than nothing. Declining is
not withholding: the file is one press away, and it is still called a guess
when it is taken.

**A feed loads the posts below the one you are watching.** So the tab's traffic
is not evidence about the video on screen, and treating it as such went wrong
in both directions at once. Checking a resolved page against it called the
*right* page a mismatch — the window said "could not be matched to the video on
screen" over the correct video — and would have called a wrong page a match the
moment it resolved to a post the feed had preloaded. Falling back to it when no
page resolved offered the newest file in the tab, which on a feed that preloads
is the post furthest *ahead*: a grab came back with a video five posts down,
downloaded in full, under "the file the page is playing will be downloaded as
it is". Now every file says which post it belongs to — this one, a neighbour,
or nothing known — and the tab at large stands in only where the page is about
one video, where it is that video's.

**A feed rewrites the address bar as you scroll.** Which took the one reading
that decided how much of a page's own state belongs to this video: an address
naming one video meant the state around it was that video's, and everything it
named could be taken. TikTok pushes the current post's URL into the bar without
loading anything, so the address read as one video's own page while the state
behind it still described every row loaded — and the first of those was handed
over as the video on screen. The state is asked instead of the address. Each
media URL in it is attributed to the nearest post named above it; the record the
markup pointed at is taken, and the blob is only read whole once it is clear it
names nobody else.

**Facebook calls every video "Video".** Signed in, that is the literal title
yt-dlp returns for all of them, and naming a download from it filed a whole
page's output under the uploader's name with an id after it — "Ka-Banat
Online-News Channel Video 1393340332303393" — which says who posted it and
nothing about which video it is. The post's own words are in `description`, so
that is what a download is named after when the title is a category rather than
a name: its first line, cut at the first sentence end that reads as a title.
Signed *out*, the same page titles itself "61K views · 516 reactions | Sunog sa
bukirang…", which is that same text with a view count stapled to the front — so
the description is the better half of the pair either way.

**An id is not found inside another id.** The permalink "carrying this post's
id" was matched with a substring test, and ids are long runs of digits sitting
in URLs among other digits — so a short number lifted off the markup matched
*inside* a neighbouring post's nineteen-digit id, and the wrong post's link was
returned as this post's. It is now matched as a whole number, and what counts
as an id at all was raised from eight digits to fifteen: eight digits is a
timestamp, a view count or a pixel value, and a feed is full of them.

**A DASH stream is half a video, however completely it downloads.** A site that
serves video this way has no single file to hand over — the player fetches a
picture track and a sound track and plays them together — so when no page
resolved and the raw stream was saved as "the file the page is playing", the
result was a download that reached 100% and was not the video. Facebook labels
both tracks `video/mp4`, so this went wrong twice: first the sound was saved and
played as a black screen, then, once sound was ranked last, the picture was
saved and played in silence. Neither is a download worth handing anyone. The
`efg` parameter says which it is, only `xpv_progressive` encodes carry both, and
a half is now offered only when nothing else exists — labelled as a half.

**The way out of a feed is the file, not the DOM.** Facebook publishes no
permalink for a reel in a feed and plays it through a `blob:` nobody outside the
page can fetch, so every reading of the markup comes up empty and the grab used
to fall through to saving that raw stream. But the stream's own address names
its post — `"video_id":2204546610402296` — and `facebook.com/watch/?v=<id>` is a
page yt-dlp extracts properly: every quality, and the sound. That candidate is
recovered from the file the player pulled and offered after the readings of the
DOM, which are tied to the element that was clicked, and it is guaranteed a slot
however many permalinks the page turns up, because when those fail it is the
only thing left that still works.

A codec nobody stated is not a codec that is absent. yt-dlp writes `"none"`
when a stream is genuinely missing and `null` when it does not know, and
reading the second as the first threw away exactly the formats worth having:
Facebook describes `sd` and `hd` — its two formats carrying picture and sound
in one file — with both codecs null, so both were discarded as storyboards, and
a format whose sound was merely unstated was filed under Audio. Unstated
dimensions were read the same way, which is how TikTok's `download` format, the
whole watermarked video, came to be offered under **Video + audio** labelled
"audio only". Where a tab genuinely has nothing in it the window now says which
tab it moved to, rather than moving the selection silently.

A title that names a *kind* of thing names nothing. Facebook calls every reel
"Video", which lands the first as `Video.mp4` and the rest as `Video_2`,
`Video_3` — nothing on disk saying which post any of them came from. Those fall
back to who posted it and the site's own id, so the file is
`El Mentor Video 1574860944371146` instead.

**The best picture is not the best download if nothing here can decode it.**
TikTok serves the same video twice — 1080p in HEVC and 720p in H.264 — and
describes the HEVC as the better one, so it was chosen. HEVC is
patent-encumbered, which is why Fedora and most other distributions ship no
decoder for it: GStreamer here has an h265 *parser* and no h265 decoder at all,
so Firefox, GNOME Videos and everything built on them play such a file as sound
over a black screen. The download had worked perfectly and was indistinguishable
from one that had fetched only the audio. So the default pick is now the best
row the desktop can actually decode, HEVC rows are labelled *may play without
picture here*, and both stay in the list in their proper order for anyone whose
player handles them. The format expression behind non-picker grabs makes the
same choice and keeps the old expression as its last fallback, so a page
offering nothing but HEVC still resolves.

**A name is kept as the name.** `--restrict-filenames`, which used to be passed
to yt-dlp, is a Windows-and-shell measure: it flattens a title to bare ASCII,
drops every emoji and punctuation mark, and turns each space into an underscore,
so a video plainly called "Songs of the summer" landed as
`Songs_of_the_summer`. A Linux filename is bytes with two rules — no `/` and no
NUL — and yt-dlp honours both already; what it does not bound is length, so
`--trim-filenames` stands in the flag's place. What the picker shows is
sanitised the same way, and a name reaching an output template has its `%`
doubled, because `%` opens a field in one and "100%(title)s deal" was otherwise
resolved into the video's own title.

Since DASH sites serve their
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
| `data:` URLs | The bytes are the URL; there is nothing to fetch |
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
cargo test                              # engine logic: categories, scheduling, aria2 options
node extension/test/capture.test.js     # capture rules and header parsing
node extension/test/permalink.test.js   # finding the post a feed video sits in
node extension/test/candidates.test.js  # which URL, and which file, a grab means
```

If you have moved or renamed the checkout, run `cargo clean` first. Cargo
records absolute paths in `target/` and cannot relocate that cache, so the
stale entries still look fresh and the build follows one of them to a directory
that is gone — surfacing as `tauri-build` failing to read a plugin permission
file. `install.sh` detects this and cleans for you.

The capture tests load `util.js` and `capture.js` in a bare VM context — those
two files hold no `browser.*` reference precisely so the decision logic can be
exercised without a browser. The candidate tests cannot: the background script
is a pile of `browser.*` listeners and the video panel an IIFE in a content
script, so each function is lifted out of its source by name and run against
stubs — including the window's own last resort, from `ui/video.js`, since which
file a grab falls back to is the same question those two are answering. Lifted
rather than copied, so a test cannot go on passing after the code it is about
has changed.

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
