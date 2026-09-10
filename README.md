# My Download Manager (MDM)

**New here? [Setup-guide.md](Setup-guide.md) is the step-by-step** — installing,
building, bundling, both browser extensions, and what to do when something
does not connect. What follows is why the thing is shaped the way it is.

An IDM-style download manager for Linux and Windows, in Firefox and every
Chromium browser — yours, not rented. It captures **every** download the
browser makes — binaries, archives, documents, media — and fetches it with up
to 32 parallel connections, in its own process, with no external downloader.

```
browser ──webRequest/downloads──▶ extension ──native messaging──▶ mdm-host
                                                                      │
                                                        socket / named pipe
                                                                      ▼
                                                            mdm (Tauri app)
                                                          ┌────────┴────────┐
                                            segmented HTTP fetcher    HLS/DASH
                                                                   + MP4 remux
                                                                      │
                                                        yt-dlp (optional, for
                                                        pages that hide their
                                                          manifests in JS)
```

## Why this shape

**Firefox, not Chromium.** Chrome's Manifest V3 removed blocking `webRequest`,
which is the only way to divert a download before the browser commits to it.
Firefox kept it, and additionally lets a blocking listener return a *Promise* —
so a request can be held open while the daemon confirms it accepted the job,
then cancelled. That makes double-downloads structurally impossible rather than
merely unlikely.

**Our own engine, not aria2.** IDM's speed comes from dynamic segmentation:
when a connection finishes its slice it splits the largest remaining slice and
steals half, so nothing idles at the tail. MDM does that in process, and adds
the part no external downloader can do for us — choosing the connection count
by *measuring* the file being downloaded rather than by asking the user to
guess. Bytes land in a `.mdmdownload` file beside a small state file recording
exactly which ranges are complete, so a crash resumes instead of restarting.

This replaced an aria2 daemon, and the daemon is gone: there is no second
process to install, supervise, or have fail on a machine that never needed it.

**More than one server per file.** A server answering a download may advertise
its mirrors in the response, as `Link: <https://…>; rel=duplicate` (RFC 6249).
The capture reads those headers — it is the only part of the system that ever
sees them — and carries every mirror alongside the original. They are then
sources for *one* file: connections are dealt across them round-robin, so a
slow mirror costs one connection rather than the download. On a mirrored file
that is a multiple, not a percentage, and it is the one thing a single-source
downloader structurally cannot do. Mirrors are stored on the row, because a
resume never gets to see those headers again.

**Streams without ffmpeg.** HLS and DASH are parsed, fetched segment by
segment and rebuilt into a single MP4 in process — including the case where the
picture and the sound arrive as two separate segment sets. The samples are
copied, never re-encoded, so the result is bit-identical to what the server
sent. This is format-driven rather than site-driven, so it needs no per-site
extractor and does not rot when a site is redesigned.

The same muxers finish the job for an *extracted* video too. A site that
serves picture and sound as two adaptive streams — every YouTube format above
360p — is resolved by yt-dlp, then fetched here with all the connections an
ordinary download gets, and merged here: MP4 by the muxer above, WebM by a
Matroska writer beside it. That is the whole of what ffmpeg was ever carried
for, and it is carried no longer. Measured against ffmpeg's own merge of the
identical streams, the output is packet-identical — every frame, every
timestamp, in both containers.

Neither muxer understands a codec. A track is copied by lifting its
description — an MP4 `stsd` entry, a Matroska `TrackEntry` — and writing it
out untouched, so AV1 rebuilds exactly as well as H.264 and Opus as well as
AAC. What that forbids is mixing the two families in one file, because *that*
would mean translating a description rather than copying it; so the default
format asks for both halves from one family, and on a machine with no ffmpeg
at all it asks again rather than give up.

The weight of a merged download is known before the first byte, because both
streams weighed themselves at extraction; and because the fetching is MDM's
own, a merge pauses, resumes and reports speed like any other download.

**yt-dlp is optional, and looks after itself.** It is only reached for a *page*
that hides its manifest behind its own JavaScript — a per-site problem this
project has no business trying to own. Even then it is asked to *resolve*
rather than to download, wherever what it resolved is something MDM can fetch
itself.

Because that is the one dependency which genuinely has to move — sites change
weekly, and a yt-dlp a few months old fails in ways that look like the app
being broken — MDM keeps its own copy under its data directory, fetches it on
first run and checks daily for a newer one. A failure that looks like
staleness brings that check forward rather than waiting out the day. A yt-dlp
already on PATH is used as it is and never written to: that copy belongs to
whoever installed it.

**Nothing to install.** YouTube hides its stream URLs behind a JavaScript
challenge, which yt-dlp solves by running the page's own code in a JavaScript
runtime it cannot supply itself. The usual answer is to install Node — ninety
eight megabytes of toolchain for a few milliseconds of arithmetic. MDM fetches
QuickJS instead: two megabytes, beside its yt-dlp, and yt-dlp is pointed
straight at it by path rather than expected to find it on anyone's PATH. A
Deno or Node already installed still wins, because that one was somebody's
choice.

What that adds up to: on a machine with no yt-dlp, no ffmpeg and no JavaScript
anything, MDM downloads and merges a 4K YouTube video by itself.

## Capture rules

Four independent nets, because none of them is sufficient alone:

0. `webRequest.onBeforeRequest` (blocking) — the only one that acts *before*
   the browser asks the server anything, and it exists for the one case where
   asking second is asking too late. See **Links that answer once** below. It
   is not registered at all until the app has a host to use it on.
1. `webRequest.onHeadersReceived` (blocking) — decides from
   `Content-Disposition`, `Content-Type`, `Content-Length` and the file
   extension, then hands over the URL **together with the request headers
   captured at `onBeforeSendHeaders`**. Replaying the original `Cookie`,
   `Referer` and `Authorization` is what makes captured downloads actually
   resolve instead of returning 403.
2. The `downloads` API — the backstop for anything the first net missed
   ("Save Link As", script-initiated downloads), cancelled and erased once the
   daemon confirms. Erased *and* removed from disk: a small file can finish
   inside the hand-off, and a download cancelled a moment too late would leave
   a second copy under the browser's own name. Where the browser offers
   `downloads.onDeterminingFilename` — Chromium does, Firefox does not — the
   hand-off happens *there* rather than at `onCreated`, because it is the one
   point at which the browser is still holding the download: named but not yet
   placed, and not yet asked about. See **Chromium** below.
3. A content script, for downloads that never had a URL to begin with — see
   **In-memory downloads** below.

### Links that answer once

File hosts hand out an address that is good for a single request: serve it
once, and everything after that gets the landing page. Every net above net 0
works the same way — watch a response go past, then ask the server for the same
file again — and on a host like that the second ask is the page, because the
browser's own request already spent the link. The download fails, and it fails
*after* MDM has cancelled the browser's copy, so nothing is saved at all.

MDM notices when that happens: a URL that answered with a page, where the
capture had watched a file arrive, is a spent link rather than a player page,
and the host goes on a list (Settings ▸ **Sites MDM asks first**). What the
list means is that MDM goes *first* there. The next request to that host is
held before it is sent, MDM makes it instead, and the browser's request — still
held, never sent — is cancelled only if what came back was a file. So there is
exactly one request, and it is MDM's.

**The list says who to ask carefully, not who to give up on.** It used to do
both: a capture from a listed host was declined on sight, on the reasoning that
the browser had already spent the address and a second request would only fetch
the landing page. That is true of an address which really is good for one
request, and quite wrong about the hosts that merely look like one. A host lands
on this list the first time *any* capture from it comes back a page, and a page
comes back for several reasons that have nothing to do with the address being
spent — a cooldown between two downloads, a request that went out without the
referrer the host wants, a CDN node that had not yet heard of the token. One
unlucky download put a host on the list, and everything from it afterwards went
to the browser for good.

So a listed host is asked rather than assumed about, and `Engine::preempt` is
what makes asking safe: it opens the connection, decides from the response
rather than from the address, and where a file comes back it downloads on *that
same connection* instead of opening another. The address is asked exactly once
more. If it answers with the file, that answer is the download and the browser's
copy is cancelled; if it answers with a page then the link really was spent, MDM
declines, and the browser's own request — the one actually holding the file —
keeps it. `crates/mdm-core/tests/single_use.rs` runs both halves against a
loopback server that really does spend its links.

Two things follow from there being only one:

* **One connection.** A link that answers once cannot be probed and then
  segmented, so a pre-empted download is written straight out of the response
  the pre-emption opened. No probe, no second connection, and no resume — a
  spent link has nothing to resume into, and offering one would turn a lost
  download into a stuck one.
* **A page is handed straight back.** The extension holds the request without
  knowing whether it is a download — it cannot know, because nothing has asked
  the server yet — so a file host's own pages are held too. The app answers
  from the response: HTML is "not mine", the held request goes ahead, and the
  page loads as though none of this happened.

Holding a request open before it is sent is exactly what Manifest V3 took away,
so on Chromium the same job is done a step earlier — at the click.

**Chromium catches the click instead.** A download does not begin with a
request; it begins with somebody pressing a link, and a click is still
cancellable on every browser there is. So on these hosts, and only on these
hosts, the extension cancels the click and hands the address to MDM, which
means the browser is never told to navigate and never spends the one answer.

The difference from Firefox's version is what happens when the address turns
out to be a page. A held request can be released — it was never sent, so it
simply goes ahead. A cancelled click cannot be released, and the navigation has
to be made again from the content script. That costs one extra request, which
is harmless precisely because a page is by definition an address that answers
twice; it is only the *file* links on these hosts that answer once. Where the
browser can hold the real request it does, and the click net stays out of the
way.

**And a second Chromium net, for the download that is not a link at all.** The
click net can only cancel a click on an `<a>`, and on these hosts the download
button routinely is not one: a single-page app renders a `<button>`, asks its
own API where the file is, and navigates by assigning to `location`. There is
no anchor to cancel, and `location` cannot be patched — it is unforgeable, by
specification — so nothing in the page can catch that navigation. This is the
reported failure on filekeeper.net in Brave: MDM saw nothing, the browser made
the request, and the one answer the address had was gone before MDM was told a
download existed.

What can catch it is `declarativeNetRequest`, which redirects a request *before*
it is sent — and a redirect is a hold, provided something lets go again.
`pages/handoff.js` is what lets go: it disarms the rule the moment it loads, so
its own navigation cannot be redirected back to it; asks MDM; and then either
goes back to the page (MDM has the file) or navigates to the address after all
(it was a page, or MDM is not running, or nothing answered). Every path out ends
in one of those two, because a tab left sitting on an extension page is a
navigation the user asked for and did not get.

The rule is *armed by a click* rather than left standing, and that is the whole
of the cost control. A standing rule would send ordinary browsing on these hosts
through the handoff page too — a flash and a round trip per page. So the click
net arms it when it meets a click it cannot itself handle, which is the moment
just before a scripted download navigates and is otherwise nothing like ordinary
browsing. A form submit arms it as well: the classic file-host flow posts
`op=download2` and the server answers with a redirect, and while a POST cannot
be replayed out of process, the redirect it produces is a GET and a GET is what
the rule is waiting for. The arming lasts twelve seconds, or until the handoff
page takes it down, whichever comes first.

**A link nothing has followed is still good.** The refusal above is about a
link the browser has already spent. Right-click ▸ *Download with MDM*, an
address pasted into the window, an `mdm:` link: nothing has requested any of
those, so MDM's request is the first one and the file is there to be had.
Refusing those was refusing the one thing that reliably works on such a host,
and the job now says which of the two it is.

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

**Except the manifest, which is never the entry to drop.** Age is the wrong
measure for a stream: the manifest is fetched once, before the first frame, so
it is always the oldest thing in the record, while the segments it lists arrive
every few seconds for as long as anyone watches. Fifty slots is about five
minutes of playback, after which oldest-out had thrown away the one entry
describing the whole video and kept fifty slices of it — so Download at six
minutes into a film came back with a 1.6 MB file that no player would open. It
was one HLS segment, correct in every respect and six seconds long. Manifests
now survive their own segments, a fragment is ranked as half a video the way a
DASH audio track already was, and where the tab holds exactly one manifest the
window offers it outright: one manifest is not a guess between videos, it is
the stream being played. It is fetched piece by piece and rebuilt into a single
MP4 by the stream downloader, which is the same path a `.m3u8` pasted by hand
takes.

Recognising one is a matter of the response as well as the address. A CDN is
under no obligation to end a playlist in `.m3u8` or a segment in `.ts`, and the
one this was written against names neither — its segments are a bare token and
only `Content-Type: video/mp2t` says what they are.

**But the record has to still exist to be ranked, and on Chromium it did not.**
Manifests surviving their own segments is a fix inside a map that the browser
throws away. A Manifest V3 background is a service worker, stopped once it has
been idle for thirty seconds, and every map in it goes with it. A film is two
hours long. Press Download an hour in and the worker has been stopped and
restarted many times over: what it holds is whatever arrived since the last
restart, which is segments, and the one address describing the whole video was
recorded before the first frame and lost with the first restart. So the
reported download — myflixerz.day, played in an embedded player, 2.7 MB, six
seconds of the film — was not a ranking failure at all. There was nothing left
to rank.

Two fixes, because the record fails in two ways:

* **The sniffer's record is kept where a stopped worker cannot lose it.**
  `storage.session` is memory the browser holds rather than memory the script
  holds: it lives as long as the browser session, is never written to disk, and
  is there again when the worker comes back. The map is hydrated from it at
  startup and mirrored back on a two-second debounce — debounced because the
  thing being recorded is a stream, and a storage write per segment would be
  the most expensive thing the extension does.
* **The page is asked what it fetched.** `src/content/streams.js` runs at
  `document_start` in every frame and keeps the manifests out of Resource
  Timing, which is a per-document record the browser maintains for the life of
  the document. It does not care that the background was stopped, it does not
  evict the oldest entry to make room, and it was populated whether or not
  anything was watching. In every frame because a streaming site serves its
  player in an iframe from another origin and the manifest is fetched *there* —
  the document in the address bar has no record of it at all. The frame the
  Download button was pressed in is ranked first, because a streaming page
  carries advertising frames and they fetch streams of their own.

The two nets recognise a manifest differently on purpose, and between them
cover both ways a CDN can hide one: the sniffer goes by the response's
`Content-Type`, so it catches a playlist served from a path that is nothing but
a token, and the page-side recorder goes by the address, so it catches one the
background never saw. A manifest both of them found is listed once.

When nothing turns one up, the window now says which failure it is. Offering a
fragment used to come with a sentence about a *feed* — which post of several
the file belongs to — and on a site streaming one film that reads as a
non-sequitur about something the page does not have. Worse, it is reassuring in
the wrong direction: it says the file may be the wrong video, when what is
actually wrong with it is that it is six seconds of the right one.

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

*yt-dlp reports for itself.* It used to be handed an external downloader,
whose readout had to be parsed back out of its output because yt-dlp's own
progress hook fires exactly once, at 100%, when something else owns the
transfer. With no external downloader left, its own `[download]` line is the
progress, and `--concurrent-fragments` is what keeps a fragmented stream from
arriving one fragment at a time.

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
sudo apt install yt-dlp ffmpeg nodejs zip zenity libnotify-bin \
                 build-essential pkg-config libwebkit2gtk-4.1-dev libdbus-1-dev \
                 rustc cargo
./install.sh
```

**Fedora, RHEL, Nobara…**

```bash
sudo dnf install yt-dlp ffmpeg nodejs zip zenity libnotify \
                 gcc gcc-c++ make pkgconf-pkg-config webkit2gtk4.1-devel dbus-devel \
                 rust cargo
./install.sh
```

**Arch, Manjaro, EndeavourOS…**

```bash
sudo pacman -S yt-dlp ffmpeg nodejs zip zenity libnotify \
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

Temporary add-ons vanish on restart, and Firefox installs nothing permanently
that Mozilla has not signed. No installer gets around that — dropping an `.xpi`
into `~/.mozilla/extensions` stopped installing anything in Firefox 74 — so
this one does not pretend to. Signing is free and costs one submission: send
`target/mdm-firefox.xpi` to addons.mozilla.org as an unlisted, self-distributed
add-on, and save what comes back as `packaging/mdm-firefox-signed.xpi`.

From then on `install.sh` copies that to `~/.local/share/mdm/mdm-firefox.xpi`
and offers to open Firefox on it, which is a one-click permanent install: the
click is "Add" in Firefox's own prompt. `MDM_XPI=/path/to.xpi` points the
installer at a signed package kept somewhere else, and an install with no
terminal to answer it — piped through `curl`, or run from a script — prints the
`file://` line instead of asking. Firefox Developer Edition, Nightly and ESR
take the unsigned package as it is, with `xpinstall.signatures.required=false`.

A packaged Firefox reads native messaging manifests from wherever its own
package was built to look: `~/.mozilla` for Debian's `firefox-esr` and
Mozilla's `.deb`, `~/snap/firefox` for the snap Ubuntu installs by default,
`~/.var/app` for the Flatpak. `install.sh` writes to every tree present, and
prints the one `flatpak override` a sandboxed Firefox additionally needs before
it may launch a host binary from `~/.local/bin`.

### Linux, and a redistributable package

`install.sh` builds from source and installs for one user, under `~/.local`,
needing no root; `./uninstall.sh` takes that install back off. Neither touches
a packaged install, and the check that keeps them apart is the manifest's own
`path`: a native messaging manifest naming a host binary outside `~/.local/bin`
belongs to the package, or to whoever wrote it by hand, and is left where it
is.

`./bundle.sh` is the other half, and installs nothing. It produces two
packages:

    target/release/bundle/rpm/My Download Manager-1.1.0-1.x86_64.rpm
    target/release/bundle/deb/My Download Manager_1.1.0_amd64.deb

Each is one self-contained file, in the same sense the Windows `setup.exe` is:
it carries the app, the native host and the signed extension, and nothing has
to be shipped beside it.

    sudo dnf install ./My*.rpm     # Fedora, RHEL, openSUSE
    sudo apt install ./My*.deb     # Debian, Ubuntu, Mint

What makes them work rather than merely install is `src-tauri/linux/`. A
package's post-install script is the counterpart of `installer.nsh`: it writes
the native messaging manifests that let the extension reach `mdm-host`, which
is the difference between an app that captures downloads and an app that sits
there waiting to be told about one.

Two things differ from Windows, and both follow from where the package
installs. It runs as root and installs for the machine, so the manifests go to
the *system* directories rather than one user's home — `/usr/lib/mozilla` and
`/usr/lib64/mozilla` for Firefox, `/etc/opt/chrome` and `/etc/chromium` for the
Chromium family. Brave has no directory of its own; it reads those last two,
which is what registers it. And because the manifests are written at install
time rather than shipped as package files, the post-*remove* script takes them
away again — carefully, since both package managers reuse that script for an
upgrade, where deleting them would leave the newly installed app unregistered.

`mdm.db` is deliberately kept on uninstall — by the package's post-remove
script, and by `uninstall.sh` unless it is given `--purge`. The same choice the
Windows uninstaller makes.

**Where they will run.** `bundle.sh` links against the glibc of the
machine that builds them, and glibc has no forward compatibility, so packages
built on a current Fedora refuse to start on Ubuntu 22.04 — silently, after
installing without complaint, which is the worst way for this to fail. For
packages meant for other people:

    ./packaging/build-in-container.sh

That builds both in an Ubuntu 22.04 container, which puts the floor at glibc
2.35 and takes glibc out of the picture: every distribution carrying
webkit2gtk-4.1 — Tauri's real requirement, and the actual limit — already has
a glibc at least that old. Ubuntu 22.04+, Debian 12+ and Fedora 38+ are
covered; Fedora 36 and openSUSE Leap 15.6 have the glibc but no
webkit2gtk-4.1, and RHEL 9 and its rebuilds ship only webkit2gtk3, the
libsoup2 build, so no build flag reaches them. One container build produces
both packages — Tauri assembles the .rpm in Rust rather than by shelling out
to rpmbuild, so an Ubuntu image can produce a package Fedora installs.

What the packages cannot register is a **Flatpak or Snap browser**. Those read
their manifests from inside their own sandbox, where a system directory is not
visible, so a machine whose Firefox came from Flatpak or Snap still wants
`install.sh` — which writes to every tree that exists, sandboxes included.

yt-dlp is not carried either, for the same reason as on Windows: it is only
reached for sites that hide their video behind a page, and the app fetches and
updates its own copy.

### Windows, and a redistributable installer

`install.ps1` is the Windows counterpart: it installs per-user into
`%LOCALAPPDATA%\Programs\mdm`, writes the native messaging manifest and the
registry value Firefox finds it through, registers `mdm://`, and needs no
Administrator prompt at any point.

For a machine that is not this one, `.\install.ps1 -Installer` additionally
produces a double-click setup:

    target\release\bundle\nsis\My Download Manager_1.1.0_x64-setup.exe

It carries the app, the native host and the signed extension, and its NSIS
hooks do the same registration the script does — so a machine that runs the
`.exe` ends up in the same state as one that ran the script. Uninstalling
removes the binaries, the manifest and both registry keys, and deliberately
keeps `mdm.db`: an uninstall is not a request to lose a download history.

Unlike the script's own install, the packaged one registers an uninstall entry,
so it appears in Windows' "Installed apps" list.

What it does *not* install is yt-dlp, which is only reached for sites that hide
their video behind a page — everything else downloads without it.

### Chrome, Edge and the other Chromium browsers

The extension is one codebase with two manifests. `manifest.json` is Firefox's;
`manifest.chrome.json` is Chromium's, and `install.ps1` assembles the second
into `target\mdm-chrome` (plus a `.zip` for the store dashboards) with the
Chromium manifest in place.

Two things differ, and both are Chromium's doing:

- **A response cannot be intercepted; a download still can.**
  `webRequestBlocking` is Manifest V3's one casualty that matters here —
  outside force-installed enterprise extensions, Chromium will not let a
  listener hold a *response* open and cancel it. So `src/background.js`
  registers that listener non-blocking there, and net 2 does the catching.
  What net 1 sees is remembered for it to describe the file with, because
  response headers are the only place a download's real size, type and mirrors
  are stated.

  Net 2 catches it at `downloads.onDeterminingFilename`, not at `onCreated`,
  and the difference is a whole dialog. Chromium settles a download's target in
  a fixed order — generate a name, notify extensions, reserve the path, prompt
  the user — and a listener that returns `true` there holds the download for up
  to fifteen seconds while it answers. That is *before* the browser asks where
  to save the file. Caught one step later at `onCreated`, the browser has
  already put its own "Save as" dialog on screen by the time the hand-off
  finishes, and a browser set to ask where to save each file gives the user two
  dialogs for one click: its own, and then MDM's. Held at the naming step there
  is no prompt, no partial file and nothing to clean up — the cancel is the
  whole of the browser's copy.
- **The id has to be known in advance.** The native messaging manifest names
  who may connect, so the extension's id must exist before the extension is
  ever installed. Chromium derives an id from the public key, so
  `manifest.chrome.json` pins one — `pegdlonllkokelfmdafooihklghlkimh` — and it
  stays that whether the extension is loaded unpacked or from a store listing.
  The private half lives in `packaging/chrome-extension-key.pem`, which is
  gitignored, and is needed only to publish.

Load it from `chrome://extensions` (or `edge://extensions`) with Developer mode
on and "Load unpacked". `install.ps1` writes the `allowed_origins` manifest and
the registry value for Chrome, Edge, Chromium, Brave and Vivaldi.

### Getting the extension without building it

Every way of installing MDM already carries the extension, and until recently
nothing said so. The `.deb` and `.rpm` stage both browsers' copies under
`/usr/lib/My Download Manager`, the Windows installer copies them to
`%APPDATA%\mdm`, and `install.sh` leaves them in the data directory — but a
package manager prints nothing about its own payload, so somebody installing a
release build got a working app, a registered native host, and no indication
that the half doing the capturing was already on the machine. The only visible
route to the add-on was cloning the repository and building it, which is the
one thing a release build exists to avoid.

Three things now point at it, because people arrive from three directions:

* **The app.** It offers the extension by itself the first time it is opened,
  and afterwards keeps the same dialog in **Settings ▸ Browser extension**.
  That is a correction: the button lived in the main toolbar, on the reasoning
  that it is the first thing a new install needs — true, and the wrong
  conclusion. It is needed exactly once, and a permanent button for a one-time
  job sat among the four that get pressed daily. The once is covered where it
  belongs now: `install.sh` asks as the last thing it does, the package prints
  it, and the app raises the dialog on a first run.

  `extension_assets` in `src-tauri/src/commands.rs` finds the copies on this
  machine, whatever shape the install was — the resource directory first,
  because that one is right by construction, then each package layout in turn.
  Firefox's button hands the `.xpi` to Firefox, which is the click that
  installs it.

  Chromium gets instructions rather than a button, and that too is a
  correction. The button used to open the browser at `chrome://extensions` and
  the folder in a file manager; neither did what it looked like. A Chromium
  browser refuses a `chrome://` address passed on the command line — the check
  is deliberate, it is what stops a program talking a browser into opening its
  own settings — so the browser started, ignored the address, and showed an
  empty new tab. Two windows arrived and no extension. There is no supported
  way to install an unpacked extension without a person doing it, which is the
  correct answer to software offering to add code to your browser, so the
  honest interface is the four steps and the folder to paste, with the names of
  the Chromium browsers actually on this machine read off by
  `chromium_browsers`.
* **The package.** `linux/postinstall.sh` prints where the two copies landed as
  it registers the native host, which is the moment the question is being
  asked. `install.sh` does the same for a source install, and copies both out
  of `target/` into the data directory on the way — Chromium re-reads an
  unpacked extension's folder at every start, so one left inside the checkout
  is an extension a `cargo clean` quietly breaks.
* **The release.** `bundle.sh` copies the signed `.xpi` and the Chromium `.zip`
  out beside the packages it builds, ready to upload as release assets. Inside
  a package covers everyone who installs one; it does not cover an AppImage,
  which runs no install script, or a distribution neither package fits.

### Updating itself

The app asks GitHub Releases once per launch whether there is a newer version,
and shows a bar offering it if so. Nothing installs itself, and a failed check
says nothing to the user — the reason goes to the log, because a dialog about
an unreachable update server on every launch behind a firewall is a nuisance
rather than information.

Updates are signed with their own key, independently of any code-signing
certificate: `packaging/mdm-updater.key` signs, the public half in
`tauri.conf.json` verifies, and a build the key did not sign is refused. Losing
the private key means no installed copy can ever be updated again, so it is the
one file in this repo worth backing up somewhere else.

To publish a release:

    $env:TAURI_SIGNING_PRIVATE_KEY_PATH = "$PWD\packaging\mdm-updater.key"
    .\install.ps1 -Installer

That produces the setup `.exe` and a `.sig` beside it. Upload both to a GitHub
release, along with a `latest.json` naming the version and the signature — the
shape Tauri's updater expects:

    {
      "version": "1.1.1",
      "notes": "What changed",
      "pub_date": "2026-09-08T00:00:00Z",
      "platforms": {
        "windows-x86_64": {
          "signature": "<contents of the .sig file>",
          "url": "https://github.com/orapagier/mdm/releases/download/v1.1.1/My.Download.Manager_1.1.1_x64-setup.exe"
        }
      }
    }

The installer is **not** code-signed, so Windows SmartScreen warns on first run
until a certificate is added. That is a separate mechanism from the update
signature above and needs a certificate issued to a real identity; the cheapest
legitimate route is Azure Trusted Signing, and `tauri.conf.json`'s
`bundle.windows.certificateThumbprint` is where it would be named.

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
cargo test                              # engine logic: categories, scheduling, manifests, MP4 muxing
node extension/test/capture.test.js     # capture rules and header parsing
node extension/test/permalink.test.js   # finding the post a feed video sits in
node extension/test/candidates.test.js  # which URL, and which file, a grab means
node extension/test/streams.test.js     # what counts as a manifest in a page's own record
```

`cargo test` includes `tests/single_use.rs`, which runs the whole single-use
decision — take it, or leave it to the browser — against a loopback server that
really does spend its links. It needs no network: the failure it is about is a
*decision*, and a decision can be put in front of a server that behaves the way
the real ones do.

If you have moved or renamed the checkout, run `cargo clean` first. Cargo
records absolute paths in `target/` and cannot relocate that cache, so the
stale entries still look fresh and the build follows one of them to a directory
that is gone — surfacing as `tauri-build` failing to read a plugin permission
file. `install.sh` detects this and cleans for you.

The candidate tests run one case at a time rather than all at once, and that is
load-bearing rather than tidy: every `check` in that file is evaluated where it
is written, so the bodies all start before any of them finishes, and they share
one sniffer record between them. That was harmless only for as long as
`videoCandidates` awaited nothing and so ran to completion before the next case
could touch the record. It awaits now, and every case promptly began reading
whatever the last one had set up.

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
| `crates/mdm-core/` | Engine, HTTP fetcher, HLS/DASH + MP4 remux, SQLite store, scheduler, IPC |
| `crates/mdm-host/` | Native messaging bridge (dependency-free, std only) |
| `src-tauri/` | Desktop app and its commands |
| `ui/` | Frontend — plain HTML/CSS/JS, no bundler |
| `src-tauri/linux/` | Post-install and post-remove scripts for the .deb and .rpm |
| `packaging/Containerfile.build` | Old-glibc toolchain for the redistributable packages |
| `packaging/` | Icon generator, Range-capable test server, native-host harness |

## Data

| Path | What |
|---|---|
| `~/.config/mdm/settings.toml` | Settings |
| `~/.local/share/mdm/mdm.db` | History and queues |
| `$XDG_RUNTIME_DIR/mdm/mdm.sock` | IPC socket (0700) |
