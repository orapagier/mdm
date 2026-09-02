// Release builds must not open a console window on any platform.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod video;

use mdm_core::engine::Engine;
use mdm_core::ipc::{self, UiRequest};
use mdm_core::{config, paths};
use tauri::{Emitter, Manager};
use tokio::sync::mpsc;

/// Tell the desktop which application these windows belong to.
///
/// GTK3's Wayland backend takes a toplevel's `app_id` from the *program name*,
/// not from the GTK application id — so a binary called `mdm` announces itself
/// as "mdm", nothing matches `io.mdm.app.desktop`, and the panel, having no
/// entry to take an icon from, draws a generic placeholder. Naming ourselves
/// after the desktop entry is what makes the two agree.
///
/// glib is already linked in under Tauri, so this needs no crate of its own.
/// It must run before GTK starts, which is why it is the first thing `main`
/// does: `gtk_init` sets the program name itself if nothing else has.
#[cfg(target_os = "linux")]
fn claim_desktop_identity() {
    extern "C" {
        fn g_set_prgname(prgname: *const std::os::raw::c_char);
    }
    let id = std::ffi::CString::new(IDENTIFIER).expect("no interior nul");
    unsafe { g_set_prgname(id.as_ptr()) };
}

/// The application id: `tauri.conf.json`'s `identifier`, and the base name of
/// the desktop entry `install.sh` writes. All three have to say the same thing.
const IDENTIFIER: &str = "io.mdm.app";

/// Put a fatal startup error in front of whoever launched from a menu, where
/// stderr goes nowhere.
///
/// Two helpers rather than one: zenity is a GNOME assumption, and a KDE
/// desktop — Kubuntu and Debian KDE included — commonly ships kdialog and no
/// zenity at all, which would make this failure entirely silent.
fn show_startup_failure(detail: &str) {
    let text = format!("Could not start the download engine:\n\n{detail}");
    let attempts: Vec<(&str, Vec<String>)> = vec![
        (
            "zenity",
            vec![
                "--error".into(),
                "--title=My Download Manager".into(),
                format!("--text={text}"),
            ],
        ),
        (
            "kdialog",
            vec![
                "--title".into(),
                "My Download Manager".into(),
                "--error".into(),
                text.clone(),
            ],
        ),
    ];
    for (bin, args) in attempts {
        if mdm_core::supervisor::which(bin).is_none() {
            continue;
        }
        if std::process::Command::new(bin).args(&args).status().is_ok() {
            return;
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    claim_desktop_identity();

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("mdm=info,mdm_core=info"),
    )
    .init();

    // Launched by the native messaging host: come up without stealing focus.
    let background = std::env::args().any(|a| a == "--background");

    // URLs may arrive from the desktop entry (Exec=mdm %u) or an mdm: handler.
    let urls: Vec<String> = std::env::args().skip(1).filter_map(normalise_url).collect();

    if let Err(e) = paths::ensure_dirs() {
        eprintln!("could not create application directories: {e:#}");
        std::process::exit(1);
    }

    // If an instance already owns the socket, hand it the focus request and
    // exit rather than starting a second engine on the same database.
    if already_running() {
        if urls.is_empty() {
            log::info!("another instance is running; asking it to show itself");
            let _ = send_to_running(r#"{"type":"focus"}"#.to_string());
        } else {
            // Hand the URLs to the instance that owns the database, rather
            // than starting a second engine that would fight over it.
            log::info!("forwarding {} url(s) to the running instance", urls.len());
            for url in &urls {
                let msg = serde_json::json!({
                    "type": "download",
                    "job": { "url": url, "source": "cli" },
                });
                let _ = send_to_running(msg.to_string());
            }
        }
        return;
    }

    let settings = config::load();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let engine = match runtime.block_on(Engine::start(settings)) {
        Ok(e) => e,
        Err(e) => {
            // Without aria2 there is no download manager, so fail loudly and
            // point at the fix rather than starting a useless window.
            eprintln!("MDM could not start its download engine:\n  {e:#}");
            show_startup_failure(&format!("{e:#}"));
            std::process::exit(1);
        }
    };

    for url in &urls {
        let mut job = mdm_core::engine::job_from_url(url);
        job.source = "cli".into();
        match runtime.block_on(engine.submit(job)) {
            Ok(id) => log::info!("queued #{id} from the command line"),
            Err(e) => eprintln!("could not queue {url}: {e:#}"),
        }
    }

    // Clipboard watching is opt-in; on X11 it polls, so only start it when the
    // user asked for it. Started here rather than in `setup` because that hook
    // runs outside the Tokio runtime the watcher needs.
    let clip_rx = if engine.settings().clipboard_watch {
        let (clip_tx, clip_rx) = mpsc::channel::<String>(8);
        mdm_core::clipboard::watch(runtime.handle(), clip_tx).then_some(clip_rx)
    } else {
        None
    };

    let (ui_tx, ui_rx) = mpsc::channel::<UiRequest>(32);

    // Serve the extension's socket for as long as the app lives.
    {
        let engine = engine.clone();
        let ui_tx = ui_tx.clone();
        runtime.spawn(async move {
            if let Err(e) = ipc::serve(engine, ui_tx).await {
                log::error!("ipc server stopped: {e:#}");
            }
        });
    }

    let app_engine = engine.clone();
    tauri::Builder::default()
        .manage(engine.clone())
        .manage(video::Pending::default())
        // The shape the download window is currently in, so it is only ever
        // resized when it actually has to change.
        .manage(std::sync::Mutex::<Option<video::Kind>>::new(None))
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::get_settings,
            commands::set_settings,
            commands::add_download,
            commands::add_many,
            commands::pause,
            commands::resume,
            commands::retry,
            commands::remove,
            commands::pause_all,
            commands::resume_all,
            commands::clear_finished,
            commands::get_queues,
            commands::save_queue,
            commands::delete_queue,
            commands::probe_media,
            commands::ytdlp_available,
            commands::install_hint,
            commands::open_path,
            commands::pick_directory,
            commands::read_clipboard_url,
            commands::open_video_window,
            commands::take_pending_video,
            commands::start_capture,
            commands::fit_window,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let clip_rx = clip_rx;

            if background {
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            }

            // Push engine state to the frontend as it changes.
            let mut rx = app_engine.subscribe();
            let emit_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(snapshot) => {
                            let _ = emit_handle.emit("mdm://snapshot", snapshot);
                        }
                        // Lagged means the UI fell behind; the next snapshot is
                        // a full state anyway, so simply carry on.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            log::debug!("frontend lagged {n} snapshots");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            if let Some(mut clip_rx) = clip_rx {
                let clip_handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    while let Some(url) = clip_rx.recv().await {
                        let _ = clip_handle.emit("mdm://clipboard", url);
                    }
                });
            }

            // Requests that need the window rather than the engine.
            let ui_handle = handle.clone();
            let probe_engine = app_engine.clone();
            let mut ui_rx = ui_rx;
            tauri::async_runtime::spawn(async move {
                while let Some(req) = ui_rx.recv().await {
                    // Anything the browser started gets the small window;
                    // raising the library for it would be exactly what the
                    // browser button is meant to avoid.
                    match req {
                        UiRequest::VideoPage { url, title } => {
                            // Start resolving the page now rather than when the
                            // window gets round to asking: the extraction is
                            // the slow part, and the webview takes a moment to
                            // come up. The answer is cached, so the window's
                            // own request finds it already waiting.
                            let engine = probe_engine.clone();
                            let page = url.clone();
                            tauri::async_runtime::spawn(async move {
                                let settings = engine.settings();
                                let _ = mdm_core::ytdlp::probe(
                                    &page,
                                    Some(settings.ytdlp_cookies_from.as_str()),
                                    &settings.ytdlp_extra_args,
                                )
                                .await;
                            });
                            video::open(&ui_handle, url, title);
                            continue;
                        }
                        UiRequest::Started {
                            id,
                            filename,
                            directory,
                            url,
                        } => {
                            video::show_download(&ui_handle, id, filename, directory, url);
                            continue;
                        }
                        _ => {}
                    }

                    let Some(window) = ui_handle.get_webview_window("main") else {
                        continue;
                    };
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                    match req {
                        UiRequest::Focus => {}
                        UiRequest::VideoPage { .. } | UiRequest::Started { .. } => {
                            unreachable!("handled above")
                        }
                        UiRequest::Batch {
                            links,
                            page_url,
                            title,
                        } => {
                            let _ = window.emit(
                                "mdm://batch",
                                serde_json::json!({
                                    "links": links, "pageUrl": page_url, "title": title
                                }),
                            );
                        }
                        UiRequest::Media {
                            items,
                            page_url,
                            title,
                        } => {
                            let _ = window.emit(
                                "mdm://media",
                                serde_json::json!({
                                    "items": items, "pageUrl": page_url, "title": title
                                }),
                            );
                        }
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window leaves the engine running so captures
            // from the browser still work; quitting is an explicit action.
            // The download window is genuinely closed instead, so the next
            // grab opens a clean one rather than inheriting the last video.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == video::LABEL {
                    return;
                }
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("building the MDM window")
        .run(move |_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                let engine = engine.clone();
                tauri::async_runtime::block_on(async move {
                    engine.shutdown().await;
                });
            }
        });
}

/// Cheap synchronous liveness check on the IPC socket.
fn already_running() -> bool {
    let path = paths::socket_path();
    path.exists() && std::os::unix::net::UnixStream::connect(&path).is_ok()
}

fn send_to_running(line: String) -> std::io::Result<()> {
    use std::io::Write;
    let mut sock = std::os::unix::net::UnixStream::connect(paths::socket_path())?;
    sock.write_all(line.as_bytes())?;
    sock.write_all(b"\n")?;
    sock.flush()
}

/// Accept a bare http(s) URL, or one wrapped in our own `mdm:` scheme so the
/// desktop entry can be registered as a protocol handler.
fn normalise_url(arg: String) -> Option<String> {
    if arg.starts_with("--") {
        return None;
    }
    let candidate = match arg.strip_prefix("mdm:") {
        // Both mdm:https://... and mdm://https://... appear in the wild
        // depending on which app builds the link.
        Some(rest) => rest.trim_start_matches("//").to_string(),
        None => arg,
    };
    (candidate.starts_with("http://") || candidate.starts_with("https://"))
        .then_some(candidate)
}
