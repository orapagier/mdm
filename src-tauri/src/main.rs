// Release builds must not open a console window on any platform.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod video;
mod window;

use mdm_core::engine::Engine;
use mdm_core::ipc::{self, UiRequest};
use mdm_core::{config, paths};
use tauri::Emitter;
use tokio::sync::mpsc;

/// Tell the desktop which application these windows belong to.
///
/// GTK3's Wayland backend takes a toplevel's `app_id` from the *program name*,
/// not from the GTK application id, and a desktop entry claims a window by
/// matching that against its own `StartupWMClass`. Where the two disagree the
/// panel has no entry to take an icon from and draws a generic placeholder.
///
/// Stated here rather than left to whatever `argv[0]` happened to be, because
/// it has to be one exact string and there are two desktop entries to satisfy:
/// the one `install.sh` writes, and the one Tauri's own bundler generates
/// inside the .deb and .rpm. That second one is not ours to choose --- the
/// bundler templates it from the binary name and offers no setting for it ---
/// so it says `StartupWMClass=mdm`, and `mdm` is therefore what this must be.
/// It was `io.mdm.app` before, which matched the script's entry and not the
/// package's, so the placeholder simply moved from one install to the other.
///
/// glib is already linked in under Tauri, so this needs no crate of its own.
/// It must run before GTK starts, which is why it is the first thing `main`
/// does: `gtk_init` sets the program name itself if nothing else has.
#[cfg(target_os = "linux")]
fn claim_desktop_identity() {
    extern "C" {
        fn g_set_prgname(prgname: *const std::os::raw::c_char);
    }
    let id = std::ffi::CString::new(APP_ID).expect("no interior nul");
    unsafe { g_set_prgname(id.as_ptr()) };
}

/// What this window calls itself to the desktop.
///
/// Deliberately *not* `tauri.conf.json`'s `identifier`, which the two used to
/// share. That one is a bundle identifier --- reverse-DNS, and what macOS and
/// Windows record an installation under --- while this is a Linux `app_id`,
/// whose only job is to equal the `StartupWMClass` of the installed desktop
/// entry. Tying them together meant a bundle identifier could not be corrected
/// without silently changing which windows the panel could put an icon on.
#[cfg(target_os = "linux")]
const APP_ID: &str = "mdm";

/// Put a fatal startup error in front of whoever launched from a menu, where
/// stderr goes nowhere.
///
/// Two helpers rather than one: zenity is a GNOME assumption, and a KDE
/// desktop — Kubuntu and Debian KDE included — commonly ships kdialog and no
/// zenity at all, which would make this failure entirely silent.
#[cfg(unix)]
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
        if mdm_core::which::which(bin).is_none() {
            continue;
        }
        if std::process::Command::new(bin).args(&args).status().is_ok() {
            return;
        }
    }
}

/// `MessageBoxW` needs no spawned helper process — the dialog is always
/// present on Windows, unlike zenity/kdialog which may both be absent.
#[cfg(windows)]
fn show_startup_failure(detail: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let text = HSTRING::from(format!("Could not start the download engine:\n\n{detail}"));
    let title = HSTRING::from("My Download Manager");
    unsafe {
        MessageBoxW(None, &text, &title, MB_OK | MB_ICONERROR);
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

    // If an instance already owns the socket, hand this launch to it and exit
    // rather than starting a second engine on the same database.
    if hand_off(&urls) {
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
            // Only genuinely fatal causes reach here now — the database, or
            // the application directories. A missing downloader tool does not:
            // the engine warns and fetches in-process instead.
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
        // Checks GitHub for a newer version and verifies its signature against
        // the public key in tauri.conf.json. It only ever *offers*: the check
        // is asked for by the frontend on launch and nothing installs itself.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(engine.clone())
        .manage(video::Pending::default())
        .manage(window::Pending::default())
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
            commands::take_pending_main,
            commands::start_capture,
            commands::fit_window,
            commands::check_update,
            commands::install_update,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let clip_rx = clip_rx;

            // A background start builds no window at all. It used to build one
            // and hide it, which kept a WebKitWebProcess alive from login for a
            // window nobody had asked to see; `window::focus` puts one up the
            // moment anything actually wants it.
            if !background {
                if let Err(e) = window::build(&handle) {
                    log::error!("could not open the main window: {e}");
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
                        UiRequest::VideoPage {
                            url,
                            title,
                            seconds,
                            candidates,
                        } => {
                            // Start resolving now rather than when the window
                            // gets round to asking: the extraction is the slow
                            // part, and the webview takes a moment to come up.
                            // The answer is cached, so the window's own request
                            // finds it already waiting.
                            //
                            // Both the page and the best candidate, because
                            // which of them the window asks about first depends
                            // on which of them names a video. On a post's own
                            // page that is the page; on a feed the page is the
                            // feed, resolves to nothing, and the answer is in
                            // the candidate the extension put first. Warming
                            // whichever turns out to be wrong costs only
                            // background time — a feed's address fails in about
                            // a second — while warming the right one is the
                            // difference between the window opening onto an
                            // answer and opening onto a spinner.
                            let best = candidates.iter().find(|c| c.kind != "media");
                            for page in [Some(url.clone()), best.map(|c| c.url.clone())]
                                .into_iter()
                                .flatten()
                                .collect::<std::collections::BTreeSet<_>>()
                            {
                                let engine = probe_engine.clone();
                                tauri::async_runtime::spawn(async move {
                                    let settings = engine.settings();
                                    let _ = mdm_core::ytdlp::probe(
                                        &page,
                                        Some(settings.ytdlp_cookies_from.as_str()),
                                        &settings.ytdlp_extra_args,
                                        // Worth insisting on here whatever it
                                        // is: nobody is waiting on this yet, so
                                        // a retry costs no time anyone can see.
                                        true,
                                        // This is the warm-up, run to fill the
                                        // cache before the window asks. The
                                        // window does the matching, with the
                                        // files it has; caching an answer here
                                        // would only cache it against nothing.
                                        &[],
                                    )
                                    .await;
                                });
                            }
                            video::open(&ui_handle, url, title, seconds, candidates);
                            continue;
                        }
                        UiRequest::Started {
                            id,
                            filename,
                            directory,
                            url,
                        } => {
                            video::show_download(
                                &ui_handle,
                                id,
                                filename,
                                directory,
                                url.clone(),
                            );
                            // A URL that answers with a *page* is a quality to
                            // choose rather than a file to save. The engine
                            // finds that out too, but only once the download
                            // has been started and failed — by which point all
                            // it can do is hand yt-dlp the address and take
                            // whichever format it settles on. Asked here, while
                            // the window is still offering the capture and not
                            // a byte has been fetched, the answer arrives in
                            // time to put the format picker up instead.
                            let engine = probe_engine.clone();
                            let handle = ui_handle.clone();
                            tauri::async_runtime::spawn(async move {
                                let Some(d) = engine.download(id) else { return };
                                // The browser knew what it was handing over
                                // whenever it had seen the response; only a
                                // capture that arrived without a type — a link
                                // off the context menu — has anything to ask.
                                if !d.mime.is_empty() && !d.mime.starts_with("text/") {
                                    return;
                                }
                                if !mdm_core::ytdlp::available() {
                                    return; // no formats could be offered anyway
                                }
                                match mdm_core::fetch::is_page(
                                    &d.url,
                                    &d.filename,
                                    &d.headers,
                                    Some(&d.referrer),
                                )
                                .await
                                {
                                    Ok(true) => {}
                                    Ok(false) => return,
                                    // Unreachable, refused, or simply slow: the
                                    // capture stands as it is, and the download
                                    // itself will meet whatever this met.
                                    Err(e) => {
                                        log::debug!(
                                            "#{id}: could not tell whether {} is a page \
                                             ({e:#}); offering it as a file",
                                            d.url
                                        );
                                        return;
                                    }
                                }
                                // Only while it is still an offer. The user may
                                // have pressed Start in the meantime, and
                                // withdrawing a running download to ask a
                                // question about it would throw away the bytes
                                // it had already fetched.
                                let Some(d) = engine.download(id) else { return };
                                if d.status != mdm_core::model::Status::Paused {
                                    return;
                                }
                                if let Err(e) = engine.remove(id, false).await {
                                    log::warn!("#{id}: could not withdraw the capture: {e:#}");
                                    return;
                                }
                                log::info!(
                                    "#{id}: {} is a page — offering its formats instead",
                                    d.url
                                );
                                video::open(&handle, d.url, String::new(), 0.0, Vec::new());
                            });
                            continue;
                        }
                        _ => {}
                    }

                    // Each of these builds the window when it is closed, so a
                    // capture arriving at a torn-down UI is not dropped.
                    match req {
                        UiRequest::Focus => window::focus(&ui_handle),
                        UiRequest::VideoPage { .. } | UiRequest::Started { .. } => {
                            unreachable!("handled above")
                        }
                        UiRequest::Batch {
                            links,
                            page_url,
                            title,
                        } => window::deliver(
                            &ui_handle,
                            "mdm://batch",
                            serde_json::json!({
                                "links": links, "pageUrl": page_url, "title": title
                            }),
                        ),
                        UiRequest::Media {
                            items,
                            page_url,
                            title,
                        } => window::deliver(
                            &ui_handle,
                            "mdm://media",
                            serde_json::json!({
                                "items": items, "pageUrl": page_url, "title": title
                            }),
                        ),
                    }
                }
            });

            Ok(())
        })
        // Neither window is held open: both are genuinely closed, so the next
        // one opens clean and the webview's WebKit processes are given back in
        // between. Nothing to intercept, so there is no window-event handler.
        .build(tauri::generate_context!())
        .expect("building the MDM app")
        .run(move |_app, event| match event {
            // Closing the last window is routine now rather than a request to
            // quit: the engine stays up so browser captures still work, and
            // `window::focus` builds a new window when one is wanted. Ending
            // the process remains an explicit act from outside (stop.sh).
            tauri::RunEvent::ExitRequested { api, .. } => api.prevent_exit(),
            tauri::RunEvent::Exit => {
                let engine = engine.clone();
                tauri::async_runtime::block_on(async move {
                    engine.shutdown().await;
                });
            }
            _ => {}
        });
}

/// Hand this launch to the instance that already owns the database, if there
/// is one. `true` means it was taken and there is nothing left to do here.
///
/// One connection, asked for once. The previous shape asked "is anything
/// there?" by opening the socket and dropping it, then opened it a second time
/// to speak — which on Windows was a bug with teeth: a named pipe instance
/// serves exactly one client, so the liveness check *consumed* the one that was
/// waiting, and the open right behind it raced the server's creation of a
/// replacement and came back `ERROR_PIPE_BUSY`. The error was discarded, so
/// clicking the Start Menu entry while the app was already running in the
/// background did nothing whatsoever — no window, no message, no log. Speaking
/// on the connection we already hold removes the race instead of narrowing it.
fn hand_off(urls: &[String]) -> bool {
    use std::io::Write;

    let Some(mut conn) = connect_to_running() else {
        return false;
    };
    let lines: Vec<String> = if urls.is_empty() {
        log::info!("another instance is running; asking it to show itself");
        vec![r#"{"type":"focus"}"#.to_string()]
    } else {
        // Hand the URLs to the instance that owns the database, rather than
        // starting a second engine that would fight over it.
        log::info!("forwarding {} url(s) to the running instance", urls.len());
        urls.iter()
            .map(|url| {
                serde_json::json!({
                    "type": "download",
                    "job": { "url": url, "source": "cli" },
                })
                .to_string()
            })
            .collect()
    };
    for line in lines {
        if let Err(e) = writeln!(conn, "{line}") {
            // Starting a second engine on the same database would be worse
            // than this launch doing nothing, so the answer is still "taken".
            log::error!("could not reach the running instance: {e}");
            break;
        }
    }
    let _ = conn.flush();
    true
}

/// Open the IPC socket of a running instance, or `None` if there is not one.
#[cfg(unix)]
fn connect_to_running() -> Option<std::os::unix::net::UnixStream> {
    let path = paths::socket_path();
    if !path.exists() {
        return None;
    }
    std::os::unix::net::UnixStream::connect(&path).ok()
}

/// Same, over a named pipe — there is no socket *file* to check for first, so
/// this is just "can we open it".
///
/// `ERROR_PIPE_BUSY` is not "no instance": it means every instance of the pipe
/// is occupied this moment. The server creates the next one as soon as it has
/// accepted, so a short wait finds it. Anything else — the pipe not existing
/// above all — is answered immediately, so a launch with nothing running is
/// not delayed at all.
#[cfg(windows)]
fn connect_to_running() -> Option<std::fs::File> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let path = paths::socket_path();
    for _ in 0..50 {
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(pipe) => return Some(pipe),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
    log::warn!("the running instance never freed up its pipe");
    None
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
