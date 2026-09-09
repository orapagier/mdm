//! Unix-socket IPC between the app and the browser's native messaging host.
//!
//! One line of JSON per message in each direction. The socket lives in
//! `$XDG_RUNTIME_DIR/mdm/` with 0700 on the directory, so only this user can
//! reach it — anything that can connect here can queue downloads as them.

use crate::engine::Engine;
use crate::model::{Header, Job};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(windows)]
use tokio::net::windows::named_pipe::ServerOptions;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub url: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    pub url: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub kind: String,
    /// Whatever tells one of these apart at a glance — an image's pixel size,
    /// say. The MIME is no help when a page offers two hundred JPEGs.
    #[serde(default)]
    pub note: String,
}

/// Another URL the same grab might resolve through.
///
/// The page a video sits on is often not the video's own — a feed, a timeline —
/// and yt-dlp can make nothing of it. The extension reads the alternatives out
/// of the page and the window tries them in turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub url: String,
    /// "media" for a file, "page" for something to extract from.
    #[serde(default)]
    pub kind: String,
    /// What the server called it, when the sniffer saw the response. A CDN
    /// path often carries no extension, and this is the only thing left to
    /// name the file by if nothing on the page resolves.
    #[serde(default)]
    pub mime: String,
    /// Sound with no picture, as far as the extension could tell.
    ///
    /// Worth carrying because the response does not say: Facebook serves the
    /// audio track of a DASH video as `video/mp4` like everything else, and
    /// saving it produces a download that completes, weighs a couple of
    /// hundred kilobytes and plays as a black screen. It only matters when no
    /// page resolves and a file is all that is left to offer.
    #[serde(default)]
    pub audio_only: bool,
    /// Half of a DASH pair: a picture track or a sound track, never both.
    ///
    /// A site that serves video this way has no single file to hand over —
    /// the player fetches two and plays them together — so either one saved on
    /// its own downloads to 100% and is not the video. It matters only when no
    /// page resolved and a file is the last thing left to offer, and then it
    /// is the difference between a download and a disappointment.
    #[serde(default)]
    pub partial: bool,
    /// A manifest: the whole stream written down, rather than a file.
    ///
    /// Carried rather than re-derived in the window, because the window only
    /// has the address and the address is not always enough. A CDN is under no
    /// obligation to end a playlist in `.m3u8` — the one this was written
    /// against serves both its manifest and its segments from paths that are
    /// nothing but a token, and only `Content-Type` tells them apart. The
    /// extension has the response; the window does not, so it is told.
    #[serde(default)]
    pub stream: bool,
    /// Which post this file belongs to, as far as the extension could tell.
    ///
    /// "this" is the post the button was pressed on, "other" a neighbour in
    /// the feed, and empty means nothing known — which is not the same as
    /// "other". A feed preloads the posts below the one on screen, so the tab
    /// is full of whole, playable files belonging to videos nobody asked for,
    /// and this is what keeps one of them from being offered as the answer
    /// when no page resolves.
    #[serde(default)]
    pub post: String,
    /// Where this came from, and so how much it is worth believing.
    ///
    /// "player" is the file the `<video>` under the button has open — not a
    /// reading of the page but the element's own state, and the only evidence
    /// here that a feed cannot mislead. "page" is what the markup said, "tab"
    /// what the sniffer saw fetched anywhere in the tab, "stream" an address
    /// worked back out of a media URL. Empty when nobody said.
    #[serde(default)]
    pub origin: String,
    /// What the browser would send asking for this file.
    ///
    /// Only the media candidates carry these, and only they need to: a page
    /// goes to yt-dlp, which brings its own cookies. A file goes straight to
    /// the downloader, out of the browser entirely — and a CDN that signs its links for
    /// one session hands back 403 to a request that arrives bare.
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub referrer: String,
}

/// Things the extension asks the *window* to do, as opposed to the engine.
#[derive(Debug, Clone)]
pub enum UiRequest {
    Focus,
    Batch {
        links: Vec<Link>,
        page_url: String,
        title: String,
    },
    Media {
        items: Vec<MediaItem>,
        page_url: String,
        title: String,
    },
    /// A streaming page the user asked to grab; the window opens the format
    /// picker rather than queueing blindly, since quality is a real choice.
    VideoPage {
        url: String,
        title: String,
        /// How long the `<video>` element under the button says its video is,
        /// or 0 when it had not loaded enough to say.
        ///
        /// The one fact about the video on screen that does not depend on
        /// reading the page correctly, and it survives a player running on
        /// MediaSource, where the element's src is a `blob:` and names
        /// nothing. The window checks what it resolved against it: every post
        /// in a feed extracts just as cleanly as the right one, and almost
        /// none of them is the same length.
        seconds: f64,
        /// Fallbacks, best first, for when the page itself resolves to nothing.
        candidates: Vec<Candidate>,
    },
    /// A download the browser handed over. It is recorded but deliberately
    /// not running: the window offers it the way IDM does, with a folder, a
    /// name and a Start button, and nothing is fetched until that is pressed.
    Started {
        id: i64,
        filename: String,
        directory: String,
        url: String,
    },
}

/// Bind the socket and serve forever.
///
/// A stale socket from a crashed process is removed; a live one means another
/// instance owns it and this call fails, which is how single-instance works.
#[cfg(unix)]
pub async fn serve(engine: Arc<Engine>, ui: mpsc::Sender<UiRequest>) -> Result<()> {
    let path = crate::paths::socket_path();
    crate::paths::ensure_dirs()?;

    if path.exists() {
        if probe(&path).await {
            anyhow::bail!("another MDM instance is already running");
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;
    log::info!("ipc listening on {}", path.display());

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("accept failed: {e}");
                continue;
            }
        };
        let engine = engine.clone();
        let ui = ui.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, engine, ui).await {
                log::debug!("ipc connection ended: {e:#}");
            }
        });
    }
}

/// Is something actually listening, or is this a leftover socket file?
#[cfg(unix)]
pub async fn probe(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Create and serve a named pipe forever.
///
/// There is no socket *file* to leave stale here — a named pipe is a kernel
/// object, not a filesystem entry — so single-instance works by trying to
/// connect as a client first: success means another instance already owns the
/// name.
///
/// A Windows named pipe instance serves exactly one client and then is spent,
/// which is why the loop creates the *next* instance before handing the
/// current one off to `handle` — there must always be one instance waiting
/// to accept, or a second launch racing the handoff would find nothing there.
#[cfg(windows)]
pub async fn serve(engine: Arc<Engine>, ui: mpsc::Sender<UiRequest>) -> Result<()> {
    crate::paths::ensure_dirs()?;
    let path = crate::paths::socket_path();

    if probe(&path).await {
        anyhow::bail!("another MDM instance is already running");
    }

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&path)
        .with_context(|| format!("creating named pipe {}", path.display()))?;
    log::info!("ipc listening on {}", path.display());

    loop {
        if let Err(e) = server.connect().await {
            log::warn!("accept failed: {e}");
            continue;
        }
        let connected = server;
        server = ServerOptions::new()
            .create(&path)
            .with_context(|| format!("creating named pipe {}", path.display()))?;

        let engine = engine.clone();
        let ui = ui.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(connected, engine, ui).await {
                log::debug!("ipc connection ended: {e:#}");
            }
        });
    }
}

/// Is something actually listening on the pipe?
#[cfg(windows)]
pub async fn probe(path: &Path) -> bool {
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(path)
        .is_ok()
}

/// Serve one connection, over either transport — a Unix socket and a named
/// pipe both satisfy `AsyncRead + AsyncWrite`, so the JSON-lines protocol
/// itself does not need to know which one it is running over.
async fn handle<S>(stream: S, engine: Arc<Engine>, ui: mpsc::Sender<UiRequest>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Exactly one reply per line, no exceptions — see `dispatch`.
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(msg) => {
                let mut reply = dispatch(&msg, &engine, &ui).await;
                // Echo the request id so the extension can match the response.
                if let Some(id) = msg.get("id") {
                    reply["id"] = id.clone();
                }
                reply
            }
            Err(e) => {
                log::warn!("malformed ipc line: {e}");
                json!({ "accepted": false, "error": "malformed request" })
            }
        };
        let mut bytes = serde_json::to_vec(&reply)?;
        bytes.push(b'\n');
        write.write_all(&bytes).await?;
        write.flush().await?;
    }
    Ok(())
}

/// Answer one message.
///
/// Every branch returns a reply, and the signature enforces it. The native
/// host writes a message and then blocks reading its reply, so a branch that
/// answered nothing would stall the browser-to-app bridge for its whole read
/// timeout and then make it re-send — and the next thing the user asked for
/// would time out instead of arriving.
async fn dispatch(
    msg: &Value,
    engine: &Arc<Engine>,
    ui: &mpsc::Sender<UiRequest>,
) -> Value {
    match msg.get("type").and_then(Value::as_str).unwrap_or("") {
        // The list travels with every pong because it is what tells the
        // extension which hosts to hold a request on — see the `preempt`
        // branch below. Sent unasked rather than fetched: the extension asks
        // this question once per connection anyway, and a list it has not got
        // is a download that goes to the browser.
        "hello" | "ping" => json!({
            "type": "pong",
            "ok": true,
            "singleUseHosts": engine.settings().single_use_hosts,
        }),

        // A request the browser is holding, not one it has made.
        //
        // The whole of the difference from "download" is timing, and timing is
        // the whole of the problem: by the time an ordinary capture can act,
        // the browser has already asked the server for the file, and a host
        // that spends its links has nothing left to give. Here nothing has
        // been asked yet, so MDM makes the one request there is.
        //
        // Answered from what came back rather than from the address, because
        // the extension held this request without knowing whether it was a
        // download: `accepted:false` means "a page — go ahead", and the
        // browser's own request, still held, proceeds as though none of this
        // happened.
        "preempt" => {
            let Some(payload) = msg.get("job") else {
                return json!({ "accepted": false, "error": "no job in request" });
            };
            let job: Job = match serde_json::from_value(payload.clone()) {
                Ok(j) => j,
                Err(e) => {
                    log::warn!("bad job payload: {e}");
                    return json!({ "accepted": false, "error": e.to_string() });
                }
            };
            let url = job.url.clone();
            // The browser is blocked on this answer and the native host stops
            // waiting after ten seconds, so a server that will not answer must
            // not be allowed to hold up the click. Giving up here costs
            // nothing that was not already lost: a server that has sent no
            // headers has served nothing.
            let taken = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                engine.preempt(job),
            )
            .await;
            match taken {
                Ok(Ok(Some(id))) => {
                    // Brought forward rather than offered. An ordinary capture
                    // opens a window with a Start button because nothing has
                    // been fetched yet; this one is already running — the
                    // response it is being written from is the reason the
                    // browser's request was cancelled — so there is nothing
                    // left to decide, and the only thing missing is somewhere
                    // to watch it. The browser shows nothing at all for a
                    // cancelled request, so without this the click would look
                    // like it had done nothing.
                    let _ = ui.send(UiRequest::Focus).await;
                    json!({ "accepted": true, "downloadId": id })
                }
                Ok(Ok(None)) => json!({ "accepted": false, "error": "that address is a page" }),
                Ok(Err(e)) => {
                    log::warn!("could not take {url} before the browser: {e:#}");
                    json!({ "accepted": false, "error": format!("{e:#}") })
                }
                Err(_) => {
                    log::warn!("{url} did not answer in time; leaving it to the browser");
                    json!({ "accepted": false, "error": "the server did not answer in time" })
                }
            }
        }

        "download" => {
            let Some(payload) = msg.get("job") else {
                return json!({ "accepted": false, "error": "no job in request" });
            };
            let mut job: Job = match serde_json::from_value(payload.clone()) {
                Ok(j) => j,
                Err(e) => {
                    log::warn!("bad job payload: {e}");
                    return json!({ "accepted": false, "error": e.to_string() });
                }
            };
            let url = job.url.clone();
            // A host that has already been caught handing out one-time
            // addresses is refused here rather than attempted and failed.
            // Accepting is what cancels the browser's download, and the
            // browser's request is the only one such an address will answer —
            // so the useful thing MDM can do with this one is decline it. The
            // extension leaves a download it was not allowed to place with the
            // browser, which is exactly where it will work.
            //
            // Only where the browser has in fact already asked, which is what
            // `spends_the_link` reads off the job. A link the user right-clicked
            // has not been requested by anything, so MDM's request is the first
            // one and the refusal would be turning down the one job on such a
            // host it can actually do — the whole reason the address is
            // single-use is that whoever asks first gets the file.
            if job.spends_the_link() {
                if let Some(host) = engine.single_use_host(&url) {
                    log::info!("leaving {url} to the browser: {host} serves single-use links");
                    return json!({
                        "accepted": false,
                        "error": format!(
                            "{host} serves single-use links — leaving this to the browser"
                        ),
                    });
                }
            }
            // Taking it off the browser's hands is not the same as agreeing to
            // fetch it. The row is created so the capture is not lost, but it
            // waits for the window's Start button.
            job.start_paused = true;
            match engine.submit(job).await {
                Ok(id) => {
                    // Offer it, rather than leaving a notification to say it
                    // went somewhere and nothing to say how it is getting on.
                    let row = engine.download(id);
                    let _ = ui
                        .send(UiRequest::Started {
                            id,
                            filename: row.as_ref().map(|d| d.filename.clone()).unwrap_or_default(),
                            directory: row.map(|d| d.directory).unwrap_or_default(),
                            url: url.clone(),
                        })
                        .await;
                    json!({ "accepted": true, "downloadId": id })
                }
                Err(e) => {
                    log::error!("submit of {url} failed: {e:#}");
                    json!({ "accepted": false, "error": format!("{e:#}") })
                }
            }
        }

        // Batch and media open a picker in the window rather than queueing
        // blindly — a page can easily have hundreds of links.
        "batch" => {
            let links: Vec<Link> =
                serde_json::from_value(msg.get("links").cloned().unwrap_or(json!([])))
                    .unwrap_or_default();
            let _ = ui
                .send(UiRequest::Batch {
                    links,
                    page_url: str_field(msg, "pageUrl"),
                    title: str_field(msg, "title"),
                })
                .await;
            json!({ "accepted": true })
        }

        "media" => {
            let items: Vec<MediaItem> =
                serde_json::from_value(msg.get("items").cloned().unwrap_or(json!([])))
                    .unwrap_or_default();
            let _ = ui
                .send(UiRequest::Media {
                    items,
                    page_url: str_field(msg, "pageUrl"),
                    title: str_field(msg, "title"),
                })
                .await;
            json!({ "accepted": true })
        }

        "videoPage" => {
            let url = str_field(msg, "url");
            if url.is_empty() {
                return json!({ "accepted": false, "error": "no url" });
            }
            let candidates: Vec<Candidate> =
                serde_json::from_value(msg.get("candidates").cloned().unwrap_or(json!([])))
                    .unwrap_or_default();
            let _ = ui
                .send(UiRequest::VideoPage {
                    url,
                    title: str_field(msg, "title"),
                    seconds: msg.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
                    candidates,
                })
                .await;
            json!({ "accepted": true })
        }

        "focus" => {
            let _ = ui.send(UiRequest::Focus).await;
            json!({ "accepted": true })
        }

        other => {
            log::debug!("unknown ipc message type {other:?}");
            json!({ "accepted": false, "error": "unknown message type" })
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
