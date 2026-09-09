# MDM Setup Guide

Everything needed to build, package, install, and run My Download Manager, and
to get the browser extension talking to it.

Two ways in:

- **[Installing](#installing)** — you have `My Download Manager_1.0.0_x64-setup.exe`
  and want it working. No toolchain, no Administrator prompt.
- **[Building](#building-from-source)** — you have the repository and want to
  produce that `.exe`, or run your own build.

---

## Installing

### 1. Run the installer

Double-click **`My Download Manager_1.0.0_x64-setup.exe`**.

Windows will show a blue **"Windows protected your PC"** box. That is
SmartScreen, and it appears because the installer is not code-signed — not
because anything is wrong with it. Click **More info** → **Run anyway**.

> Signing removes the warning permanently and needs a certificate issued to a
> real identity. See [Code signing](#code-signing).

The installer needs **no Administrator prompt**. Everything it writes is
per-user.

### 2. What it installed, and where

| What | Where |
|---|---|
| The app and the native messaging host | `%LOCALAPPDATA%\Programs\mdm` |
| Downloads database, settings, fetched tools | `%APPDATA%\mdm` |
| Firefox extension package | `%APPDATA%\mdm\mdm-firefox.xpi` |
| Chromium extension folder | `%APPDATA%\mdm\mdm-chrome` |
| Native messaging manifests | `%APPDATA%\mdm\io.mdm.host.json` (Firefox), `%APPDATA%\mdm\io.mdm.host.chrome.json` (Chromium) |
| Start Menu entry | **My Download Manager** |
| Uninstall entry | Settings → Apps → Installed apps |

It also registers the `mdm://` URL scheme and the native messaging host for
Firefox, Chrome, Edge, Chromium, Brave and Vivaldi. A registry value written for
a browser you do not have is inert — nothing is detected, nothing breaks.

### 3. Start the app

Start Menu → **My Download Manager**.

On first run it fetches **yt-dlp** into `%APPDATA%\mdm\bin` and keeps it
current. That is the only thing MDM downloads for itself, and it is reached only
for sites that hide their video behind a page — ordinary files, HLS and DASH are
all handled in-process with no external tool.

### 4. Install the browser extension

MDM captures downloads *through the browser*, so nothing is captured until the
extension is installed. Both browsers can be set up at once; they share the one
running app.

#### Firefox

Open this in Firefox — paste the whole line into the address bar, with your own
username in place of `<you>`:

```
file:///C:/Users/<you>/AppData/Roaming/mdm/mdm-firefox.xpi
```

Click **Add**, then **Okay**. It stays installed across restarts.

#### Chrome, Edge, Brave, Vivaldi

Chromium browsers install an *unpacked folder*, not a file:

1. Open **`chrome://extensions`** — or `edge://extensions`, `brave://extensions`.
   They are the same page.
2. Turn on **Developer mode** (top right).
3. Click **Load unpacked**.
4. Select `%APPDATA%\mdm\mdm-chrome`.

The extension ID will be **`pegdlonllkokelfmdafooihklghlkimh`**. That is pinned
deliberately: the native messaging manifest names exactly that ID, so it has to
be identical on every machine. A different ID means the folder you picked is not
the one the installer wrote.

> Developer mode is needed only because the extension is not yet in a store.
> Once it is listed on Edge Add-ons or the Chrome Web Store it installs like any
> other extension — with the same ID, so nothing else changes.

### 5. Check it worked

Click the MDM icon in the browser toolbar. It should say:

> **Connected to the MDM daemon.**

Then download something over 1 MB. It should appear in MDM's window instead of
the browser's own downloads.

If the popup says *"MDM is not running"*, start the app from the Start Menu and
open the popup again. See [Troubleshooting](#troubleshooting) if it persists.

### 6. Uninstalling

Settings → **Apps** → **Installed apps** → **My Download Manager** → Uninstall.

That removes the binaries, both native messaging manifests, every registry value
and the bundled copies of the extensions. It **keeps** `%APPDATA%\mdm` — your
download history and settings — because an uninstall is not a request to lose
them. Delete that folder by hand if you want it gone.

The uninstaller cannot remove the browser extension: a browser only lets you do
that from inside it.

- Firefox: `about:addons` → Extensions → My Download Manager → Remove
- Chromium: `chrome://extensions` → My Download Manager → **Remove**

---

## Building from source

### Prerequisites

| Needed | For | Install |
|---|---|---|
| **Rust** (stable) | everything | [rustup.rs](https://rustup.rs) |
| **Tauri CLI** | the `.exe` bundle only | `cargo install tauri-cli --locked` |
| **WebView2** | running the app | already on Windows 11; the installer fetches it otherwise |

Nothing else. NSIS is downloaded by the Tauri bundler on first use. There is no
Node, Python, ffmpeg or aria2 requirement — the extension is plain JavaScript
with no build step, and the downloader is self-contained.

### Build and install your own build

```powershell
git clone https://github.com/orapagier/mdm.git
cd mdm
.\install.ps1
```

`install.ps1` builds in release mode, installs to `%LOCALAPPDATA%\Programs\mdm`,
writes both native messaging manifests, registers every browser hive and the
`mdm://` scheme, creates the Start Menu entry, and packages both extensions. No
Administrator prompt at any point. It prints the exact paths to load the
extensions from when it finishes.

> A source install writes **no uninstall entry**, so MDM will not appear in
> Windows' "Installed apps" list. That is expected — use `.\uninstall.ps1`.
> Only the packaged `.exe` registers an uninstaller.

### Build the redistributable installer

```powershell
.\install.ps1 -BundleOnly
```

Produces:

```
target\release\bundle\nsis\My Download Manager_1.0.0_x64-setup.exe
```

`-BundleOnly` leaves this machine untouched: no binaries copied, no registry
values, no Start Menu entry. That matters when you want to test the installer
*as a user would run it*, which cannot be done honestly on a machine the script
has already set up by hand.

Use `.\install.ps1 -Installer` if you want both — install here *and* build the
`.exe`.

The bundle is signed for the updater using `packaging\mdm-updater.key`; the
script loads it for you. A build without that key fails outright rather than
shipping an update nobody can verify.

Two files, both gitignored, both worth backing up:

| File | What it is |
|---|---|
| `packaging\mdm-updater.key` | the private signing key |
| `packaging\mdm-updater.password` | its passphrase, which `install.ps1` reads |

The key has a passphrase because of a Windows limitation, not a security
preference: **Windows cannot hold an empty environment variable** — setting one
to `""` deletes it — so a key generated with no passphrase can never have that
communicated to the bundler from PowerShell. The bundler then decides to ask
interactively and blocks forever on a prompt no scripted build answers, *after*
writing the `.exe`. If a bundle ever seems to take forever, that is what
happened.

To generate a fresh pair:

```powershell
openssl rand -base64 24 > packaging\mdm-updater.password
cargo tauri signer generate --ci -p (Get-Content packaging\mdm-updater.password) `
  -w packaging\mdm-updater.key -f
```

Then paste `packaging\mdm-updater.key.pub` into `tauri.conf.json` as
`plugins.updater.pubkey`.

A new key pair orphans every copy already installed: they verify updates against
the old public key and will refuse anything signed by the new one. Paste the new
`packaging\mdm-updater.key.pub` into `tauri.conf.json` under `plugins.updater.pubkey`.

### Building from a network share

This project lives on a share so both the Windows VM and the Fedora host can
see it. Cargo reads and writes `target/` constantly, and `target/` reaches
**22 GB** — over a redirected share that is painfully slow, and on Windows it
is a UNC path, which some tools handle badly.

Keep the source on the share and put the build output on a local disk:

```powershell
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\mdm-build"
```

On Fedora the path is local, so nothing special is needed:

```bash
export CARGO_TARGET_DIR=~/.cache/mdm-build   # optional
```

`git` on Windows will also refuse a repository on a share until it is trusted:

```powershell
git config --global --add safe.directory '%(prefix)///tsclient/media/Data/dev/mdm'
```

### Uninstall a source install

```powershell
.\uninstall.ps1           # keep the database and settings
.\uninstall.ps1 -Purge    # remove those too
```

### Linux

```bash
./install.sh              # build and install for this user, no root
./uninstall.sh            # take it back off, keep the database and settings
./uninstall.sh --purge    # remove those too
```

Same layout under XDG paths: binaries in `~/.local/bin`, data in
`~/.local/share/mdm`, native messaging manifests written to every Firefox and
Chromium config tree that exists. Files you have already downloaded are
untouched by either script.

`uninstall.sh` removes only what `install.sh` wrote. If MDM is also installed
from the `.rpm` or `.deb`, that install belongs to the package manager and is
left alone — including its native messaging manifests, which are recognised by
naming a host binary outside `~/.local/bin`.

To build the packages instead of installing this way:

```bash
./bundle.sh               # .rpm and .deb; installs nothing
./bundle.sh --rpm         # just the one
```

That is a system-wide install by your package manager, so it replaces
`install.sh` rather than accompanying it — running both leaves two copies, and
`~/.local/bin` is what PATH finds first.

### Tests

```powershell
cargo test --workspace
```

136 tests covering filename safety, scheduling windows, capture rules,
credential handling and the stream muxers. The extension's own JavaScript tests
need Node:

```powershell
node extension\test\capture.test.js
```

---

## The extensions in detail

One codebase, two manifests:

| | Firefox | Chromium |
|---|---|---|
| Manifest | `extension/manifest.json` | `extension/manifest.chrome.json` |
| Built to | `target\mdm-firefox.xpi` | `target\mdm-chrome\` and `target\mdm-chrome.zip` |
| Background | event page, four scripts | service worker (`src/sw.js`) |
| Can cancel a response mid-flight | yes | no — Manifest V3 removed it |
| Can hold a download before it is saved | via net 1 | `onDeterminingFilename` |
| Identity | `mdm@ramlej.local` | `pegdlonllkokelfmdafooihklghlkimh` |

Chromium cannot hold a *response* open and cancel it, so MDM catches downloads
there through the `downloads` API instead — at `onDeterminingFilename`, which
fires after the browser has named the download and before it asks where to put
it. A download cancelled from there never reaches the browser's "Save as"
dialog, so "Ask where to save each file" can stay on without producing two file
pickers for one click. The difference is otherwise invisible in use.

### Publishing the Firefox extension

Firefox will not permanently install an add-on Mozilla has not signed. Submit
`target\mdm-firefox.xpi` at [addons.mozilla.org](https://addons.mozilla.org)
(self-distribution signing is free and unlisted), save the signed result as
`packaging\mdm-firefox-signed.xpi`, and re-run `install.ps1` — from then on it
offers that for one-click install, and the bundled `.exe` carries it.

For development without signing: Firefox Developer Edition with
`xpinstall.signatures.required=false` takes the unsigned package as-is, or use
`about:debugging` → **Load Temporary Add-on** (removed at the next restart).

### Publishing the Chromium extension

Upload `target\mdm-chrome.zip` to:

- **Edge Add-ons** — free, at
  [partner.microsoft.com](https://partner.microsoft.com/en-us/dashboard/microsoftedge/overview)
- **Chrome Web Store** — $5 one-off developer fee

The public key pinned in `manifest.chrome.json` keeps the extension ID identical
whether it is loaded unpacked or installed from a store, so the native messaging
registration keeps working either way.

`packaging\chrome-extension-key.pem` is the private half. It is gitignored, and
it is what lets you publish under that ID — **back it up**.

---

## Settings worth knowing

Open the app → **⚙**.

**Connections per server (ceiling, 1–32)** — a ceiling, not a target. MDM starts
at four connections, doubles while each doubling actually buys more than 12%
throughput, then trims back to the *fewest* connections that go just as fast. A
server happy with two gets two. Setting this to 32 does not mean 32 connections;
it means 32 are allowed if they help.

**Proxy** — blank follows Windows' own proxy settings, which is what your browser
does. `off` forces a direct connection. Anything else is a proxy URL:
`http://host:3128`, `socks5://host:1080`, with `user:pass@` before the host if it
needs a login.

**Site logins** — a username and password per host, for servers that ask for one.
Needed only for links added by hand: a download captured from the browser already
carries the session you are signed in with. Stored in plain text in
`settings.toml`.

**Schedule** — restrict downloads to a time window and to particular days. A
window whose end is earlier than its start runs overnight.

---

## Updating

The app asks GitHub Releases once per launch whether there is a newer version and
shows a bar offering it. Nothing installs itself. A failed check says nothing —
the reason goes to the log, because a dialog about an unreachable update server
on every launch behind a firewall is noise, not information.

### Publishing a release

1. Bump `version` in `src-tauri/tauri.conf.json`.
2. `.\install.ps1 -BundleOnly`
3. Create a GitHub release tagged `v<version>`.
4. Upload the `setup.exe`, its `.sig` (beside it in the same folder), and a
   `latest.json`:

```json
{
  "version": "1.0.1",
  "notes": "What changed",
  "pub_date": "2026-09-08T00:00:00Z",
  "platforms": {
    "windows-x86_64": {
      "signature": "<contents of the .sig file>",
      "url": "https://github.com/orapagier/mdm/releases/download/v1.0.1/My.Download.Manager_1.0.1_x64-setup.exe"
    }
  }
}
```

Updates are verified with their own signature, independent of code signing:
`packaging\mdm-updater.key` signs, the public half in `tauri.conf.json` verifies,
and a build that key did not sign is refused.

> **Back up `packaging\mdm-updater.key` and `packaging\mdm-updater.password`.**
> Both are gitignored. Lose either and no installed copy can ever be updated
> again — a new key pair orphans every copy already out there, because they
> verify against the old public key.

### Code signing

Separate from the update signature above, and the thing that silences
SmartScreen. It needs a certificate issued to a real identity:

| Route | Cost | SmartScreen |
|---|---|---|
| Azure Trusted Signing | ~$10/month | builds reputation; cheapest legitimate route |
| EV certificate | ~$400+/year | trusted immediately |
| OV certificate | ~$200–400/year | warns until reputation builds |

Once you have one, name its thumbprint in `tauri.conf.json` under
`bundle.windows.certificateThumbprint`.

---

## Troubleshooting

**The extension popup says "MDM is not running".**
Start the app from the Start Menu. If it still says that, the native messaging
host is not registered for that browser — re-run the installer, or
`.\install.ps1` for a source install.

**A Chromium extension does nothing at all.**
`chrome://extensions` → **Details** on My Download Manager → **Errors**. A
service worker that fails to start takes the whole extension down with it, and
that page is where the reason appears.

**Downloads fail with a certificate error.**
On Windows MDM uses the system certificate store, so anything the browser trusts,
it trusts. If this appears on Linux, the site is likely serving an incomplete
certificate chain that rustls will not repair on its own.

**The app window will not open, but `mdm.exe` is running.**
Fixed in this version — the app hides its window on close, and a second launch
now brings it back. If it recurs, end `mdm.exe` in Task Manager and start it
again.

**`cargo` hangs at 0 bytes fetching crates.**
Some networks break HTTP/2 multiplexing. Prefix the build:

```powershell
$env:CARGO_HTTP_MULTIPLEXING = "false"
```

or make it permanent in `~/.cargo/config.toml`:

```toml
[http]
multiplexing = false
```

**Where are the logs?**
Run the app from a terminal with `RUST_LOG=mdm=debug,mdm_core=debug`.

---

## Where everything lives

```
%LOCALAPPDATA%\Programs\mdm\      mdm.exe, mdm-host.exe
%APPDATA%\mdm\
    mdm.db                        download history and queues
    settings.toml                 settings, including site logins
    bin\yt-dlp.exe                fetched on first run, kept current
    bin\qjs.exe                   fetched when a site needs JavaScript run
    io.mdm.host.json              native messaging manifest (Firefox)
    io.mdm.host.chrome.json       native messaging manifest (Chromium)
    mdm-firefox.xpi               the Firefox extension, ready to install
    mdm-chrome\                   the Chromium extension, ready to load
```

Downloaded files go wherever you chose — by default `%USERPROFILE%\Downloads`,
sorted into `Video`, `Compressed`, `Documents` and so on unless categories are
switched off.
