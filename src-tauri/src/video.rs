//! The standalone download window.
//!
//! IDM puts a download in a small window of its own and leaves the library
//! alone unless you ask for it. Everything that starts in the browser does the
//! same here: it raises *this* window and never touches the main one, which on
//! a browser-launched app is deliberately hidden.
//!
//! The window has two shapes. A video page gets the format picker with a
//! progress strip beneath it; a captured file has nothing to choose, so it
//! gets the strip alone.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

pub const LABEL: &str = "video";

/// Tall enough for the picker: a format list, the name field and the buttons.
const PICKER_SIZE: (f64, f64) = (660.0, 740.0);
/// A captured file being offered: a name, a folder and the buttons.
const FILE_SIZE: (f64, f64) = (620.0, 300.0);
/// The same window once it is running and the choices are behind it.
const RUNNING_SIZE: (f64, f64) = (620.0, 215.0);

/// Shrink to the running shape, now that there is nothing left to choose.
///
/// The page checks afterwards whether that was too far and asks for the
/// difference back, so this only has to be roughly right.
pub fn shrink_to_progress(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.set_size(LogicalSize::new(RUNNING_SIZE.0, RUNNING_SIZE.1));
    }
}

/// Give the window `extra` more logical pixels of height.
///
/// How tall a window has to be to show a given layout is not knowable from
/// here: the title bar is drawn by the desktop, its height differs per theme,
/// and on this desktop it comes out of the size we ask for. So the page
/// measures the shortfall in its own coordinates and asks for exactly that,
/// which is right on every desktop without knowing anything about any of them.
pub fn grow(app: &AppHandle, extra: f64) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window(LABEL) else {
        return Ok(());
    };
    if extra < 1.0 {
        return Ok(());
    }
    let scale = window.scale_factor()?;
    let inner = window.inner_size()?.to_logical::<f64>(scale);
    log::debug!("download window short by {extra:.0}px; growing from {:.0}", inner.height);
    window.set_size(LogicalSize::new(
        inner.width,
        (inner.height + extra).min(screen_limit(&window, scale)),
    ))
}

/// Never taller than the screen it is on, however much the page asks for.
fn screen_limit(window: &WebviewWindow, scale: f64) -> f64 {
    window
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| m.size().to_logical::<f64>(scale).height * 0.9)
        .unwrap_or(1080.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A page to pick a format from.
    Video,
    /// A capture waiting to be started, then watched.
    File,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// Monotonic. The window can learn of the same request twice — once by
    /// collecting the pending value as it starts, once from the event — and
    /// this is how it tells a repeat from a genuinely new one.
    pub id: u64,
    pub kind: Kind,
    /// Empty when the user opened the picker themselves rather than from a page.
    pub url: String,
    pub title: String,
    /// Where it would be saved, so the window can offer to change it.
    pub directory: String,
    /// The row to follow. Set for a file, absent until a video is started.
    pub download_id: Option<i64>,
}

/// The most recent request, until the window collects it.
#[derive(Default)]
pub struct Pending(Mutex<Option<Request>>);

impl Pending {
    pub fn take(&self) -> Option<Request> {
        self.0.lock().unwrap().take()
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Show the format picker for a page.
pub fn open(app: &AppHandle, url: String, title: String) {
    deliver(
        app,
        Request {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            kind: Kind::Video,
            url,
            title,
            directory: String::new(),
            download_id: None,
        },
    );
}

/// Offer a captured download: where to put it, and whether to start it.
pub fn show_download(app: &AppHandle, id: i64, name: String, directory: String, url: String) {
    deliver(
        app,
        Request {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            kind: Kind::File,
            url,
            title: name,
            directory,
            download_id: Some(id),
        },
    );
}

fn deliver(app: &AppHandle, request: Request) {
    // Both routes are primed every time. A window still loading its scripts
    // cannot receive an event, and a window already up will never ask for the
    // pending value again — so neither alone covers both cases, and the id
    // lets the window discard whichever arrives second.
    if let Some(pending) = app.try_state::<Pending>() {
        *pending.0.lock().unwrap() = Some(request.clone());
    }

    let (w, h) = match request.kind {
        Kind::Video => PICKER_SIZE,
        Kind::File => FILE_SIZE,
    };

    log::debug!(
        "download window: {:?} request #{}",
        request.kind,
        request.id
    );
    match app.get_webview_window(LABEL) {
        Some(window) => {
            // Only when the shape changes: the user may well have sized this
            // window to taste, and undoing that on every download would be its
            // own kind of rude.
            if app
                .try_state::<Mutex<Option<Kind>>>()
                .map(|last| last.lock().unwrap().replace(request.kind) != Some(request.kind))
                .unwrap_or(false)
            {
                let _ = window.set_size(LogicalSize::new(w, h));
            }
            let _ = window.emit("mdm://videoPage", &request);
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
        None => {
            if let Some(last) = app.try_state::<Mutex<Option<Kind>>>() {
                *last.lock().unwrap() = Some(request.kind);
            }
            if let Err(e) = build(app, w, h) {
                log::error!("could not open the download window: {e}");
            }
        }
    }
}

fn build(app: &AppHandle, width: f64, height: f64) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("video.html".into()))
        .title("My Download Manager")
        .inner_size(width, height)
        .min_inner_size(480.0, 150.0)
        .resizable(true)
        .center()
        .focused(true)
        .build()?;
    Ok(())
}
