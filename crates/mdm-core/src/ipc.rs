//! Unix-socket IPC between the app and the browser's native messaging host.
//!
//! One line of JSON per message in each direction. The socket lives in
//! `$XDG_RUNTIME_DIR/mdm/` with 0700 on the directory, so only this user can
//! reach it — anything that can connect here can queue downloads as them.

use crate::engine::Engine;
use crate::model::Job;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

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
pub async fn probe(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

async fn handle(
    stream: UnixStream,
    engine: Arc<Engine>,
    ui: mpsc::Sender<UiRequest>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
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
        "hello" | "ping" => json!({ "type": "pong", "ok": true }),

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
