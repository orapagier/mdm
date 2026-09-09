//! Lifecycle for the main window.
//!
//! The window is built when something wants it and destroyed when it is
//! closed, rather than built once at startup and thereafter only hidden.
//!
//! On Linux a live webview is not free the way hiding a window suggests: it
//! costs a WebKitWebProcess of its own plus a share of the WebKitNetworkProcess
//! behind it, together several times the engine they sit in front of. A
//! `--background` start — which is how the browser launches us, and so how
//! most sessions begin — would otherwise pay all of that at login for a window
//! nobody has asked to see.
//!
//! The cost of destroying it is that an arriving capture can find no window to
//! emit into. Deliveries are therefore primed the way `video` primes its own:
//! the payload is parked, the window is built, and its scripts collect it once
//! they are running.

use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "main";

/// What the window opens at. This lived in `tauri.conf.json` as `app.windows[0]`
/// until the window stopped being built at startup; the numbers are that entry.
const SIZE: (f64, f64) = (1040.0, 680.0);
const MIN_SIZE: (f64, f64) = (760.0, 460.0);

/// An event the window is to receive once it has scripts to receive it.
#[derive(Clone, Serialize)]
pub struct Message {
    pub event: String,
    pub payload: serde_json::Value,
}

/// The most recent message, until the window collects it.
#[derive(Default)]
pub struct Pending(Mutex<Option<Message>>);

impl Pending {
    pub fn take(&self) -> Option<Message> {
        self.0.lock().unwrap().take()
    }
}

/// Bring the window up, building it when it is not there.
pub fn focus(app: &AppHandle) {
    match app.get_webview_window(LABEL) {
        Some(window) => {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
        None => {
            if let Err(e) = build(app) {
                log::error!("could not open the main window: {e}");
            }
        }
    }
}

/// Hand the window an event, building it first when it is closed.
pub fn deliver(app: &AppHandle, event: &str, payload: serde_json::Value) {
    // Both routes are primed every time, for the reason `video::deliver` gives:
    // a window still loading its scripts cannot receive an event, and a window
    // already up will never ask for the pending value — so neither alone covers
    // both cases.
    if let Some(pending) = app.try_state::<Pending>() {
        *pending.0.lock().unwrap() = Some(Message {
            event: event.to_string(),
            payload: payload.clone(),
        });
    }
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.emit(event, payload);
    }
    focus(app);
}

/// Build the window. Public because a foreground start opens one directly.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("My Download Manager")
        .inner_size(SIZE.0, SIZE.1)
        .min_inner_size(MIN_SIZE.0, MIN_SIZE.1)
        .resizable(true)
        .center()
        .focused(true)
        .build()?;
    Ok(())
}
