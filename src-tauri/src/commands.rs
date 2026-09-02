//! Commands exposed to the web frontend.

use mdm_core::engine::{Engine, Snapshot};
use mdm_core::model::{Job, Queue, Settings};
use mdm_core::ytdlp::{self, MediaInfo};
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Commands return a plain string error because that is what reaches the
/// frontend as a rejected promise; the detail is preserved via `{:#}`.
type Cmd<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tauri::command]
pub fn get_snapshot(engine: State<'_, Arc<Engine>>) -> Cmd<Snapshot> {
    engine.snapshot().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn get_settings(engine: State<'_, Arc<Engine>>) -> Settings {
    engine.settings()
}

#[tauri::command]
pub async fn set_settings(
    engine: State<'_, Arc<Engine>>,
    settings: Settings,
) -> Cmd<()> {
    engine
        .update_settings(settings)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn add_download(
    engine: State<'_, Arc<Engine>>,
    url: String,
    directory: Option<String>,
    use_ytdlp: Option<bool>,
    format_id: Option<String>,
    filename: Option<String>,
    start_paused: Option<bool>,
    size: Option<i64>,
    // Only a direct download carries these, and only it needs them: the URL
    // the window is about to hand aria2 came out of a page, and the server
    // behind it may only answer a request that looks like it did too.
    headers: Option<Vec<mdm_core::model::Header>>,
    referrer: Option<String>,
) -> Cmd<i64> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("no URL given".into());
    }
    let mut job = mdm_core::engine::job_from_url(&url);
    job.directory = directory.filter(|d| !d.is_empty());
    job.headers = headers.unwrap_or_default();
    job.referrer = referrer.unwrap_or_default();
    // What the picker weighed the chosen formats at. yt-dlp fetches the video
    // stream and then the audio, so without this the bar is scaled to the first
    // of them and rescales downwards when the second starts.
    job.size = size.filter(|s| *s > 0).unwrap_or(-1);
    job.use_ytdlp = use_ytdlp.unwrap_or(false);
    job.format_id = format_id;
    job.start_paused = start_paused.unwrap_or(false);
    if let Some(name) = filename.filter(|n| !n.is_empty()) {
        // For yt-dlp the name is a stem it will extend with the real
        // container; for a direct download it is the filename outright.
        if job.use_ytdlp {
            job.output_name = Some(name);
        } else {
            job.filename = name;
        }
    }
    engine.submit(job).await.map_err(|e| format!("{e:#}"))
}

/// Queue several URLs at once, reporting per-URL failures rather than
/// aborting the whole batch on the first bad one.
#[tauri::command]
pub async fn add_many(
    engine: State<'_, Arc<Engine>>,
    urls: Vec<String>,
    directory: Option<String>,
) -> Cmd<Vec<String>> {
    let mut failures = Vec::new();
    for url in urls {
        let url = url.trim().to_string();
        if url.is_empty() {
            continue;
        }
        let mut job: Job = mdm_core::engine::job_from_url(&url);
        job.directory = directory.clone().filter(|d| !d.is_empty());
        if let Err(e) = engine.submit(job).await {
            failures.push(format!("{url}: {e:#}"));
        }
    }
    Ok(failures)
}

#[tauri::command]
pub async fn pause(engine: State<'_, Arc<Engine>>, id: i64) -> Cmd<()> {
    engine.pause(id).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn resume(engine: State<'_, Arc<Engine>>, id: i64) -> Cmd<()> {
    engine.resume(id).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn retry(engine: State<'_, Arc<Engine>>, id: i64) -> Cmd<()> {
    engine.retry(id).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn remove(
    engine: State<'_, Arc<Engine>>,
    id: i64,
    delete_file: bool,
) -> Cmd<()> {
    engine
        .remove(id, delete_file)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn pause_all(engine: State<'_, Arc<Engine>>) -> Cmd<()> {
    engine.pause_all().await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn resume_all(engine: State<'_, Arc<Engine>>) -> Cmd<()> {
    engine.resume_all().await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn clear_finished(engine: State<'_, Arc<Engine>>) -> Cmd<usize> {
    engine.clear_finished().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn get_queues(engine: State<'_, Arc<Engine>>) -> Cmd<Vec<Queue>> {
    engine.queues().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn save_queue(engine: State<'_, Arc<Engine>>, queue: Queue) -> Cmd<()> {
    engine.save_queue(&queue).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn delete_queue(engine: State<'_, Arc<Engine>>, name: String) -> Cmd<()> {
    engine.delete_queue(&name).map_err(|e| format!("{e:#}"))
}

/// Ask yt-dlp what formats a page offers, for the quality picker.
#[tauri::command]
pub async fn probe_media(engine: State<'_, Arc<Engine>>, url: String) -> Cmd<MediaInfo> {
    let settings = engine.settings();
    ytdlp::probe(
        &url,
        Some(settings.ytdlp_cookies_from.as_str()),
        &settings.ytdlp_extra_args,
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn ytdlp_available() -> bool {
    ytdlp::available()
}

/// The command that installs `package` on this machine.
///
/// The frontend cannot read `/etc/os-release`, and a hint that says `dnf` on a
/// Debian desktop is a wrong turn rather than an instruction, so the advice is
/// resolved here and handed over as text.
#[tauri::command]
pub fn install_hint(package: String) -> String {
    mdm_core::distro::install(&package)
}

/// Open the standalone download window, empty, for the toolbar button.
#[tauri::command]
pub fn open_video_window(app: AppHandle) {
    crate::video::open(&app, String::new(), String::new(), Vec::new());
}

/// The request the download window was opened for.
///
/// A window that is still loading cannot receive an event, so it collects the
/// request itself once its scripts are running.
#[tauri::command]
pub fn take_pending_video(pending: State<'_, crate::video::Pending>) -> Option<crate::video::Request> {
    pending.take()
}

/// Start a capture the window has been holding, at the folder and name the
/// user settled on.
///
/// The row already exists — it was created paused the moment the browser
/// handed the download over, so a closed window loses nothing — and this is
/// the point at which bytes are actually asked for.
#[tauri::command]
pub async fn start_capture(
    app: AppHandle,
    engine: State<'_, Arc<Engine>>,
    id: i64,
    directory: Option<String>,
    filename: Option<String>,
) -> Cmd<()> {
    engine
        .set_target(id, directory.as_deref(), filename.as_deref())
        .map_err(|e| format!("{e:#}"))?;
    engine.resume(id).await.map_err(|e| format!("{e:#}"))?;
    // The folder and name fields are gone now, so the window no longer needs
    // the height it was given to show them.
    crate::video::shrink_to_progress(&app);
    Ok(())
}

/// Make the download window tall enough for what the page has laid out.
///
/// The page is the only thing that can measure this, so it does, and this
/// simply obeys — see `video::grow`.
#[tauri::command]
pub fn fit_window(app: AppHandle, grow: f64) -> Cmd<()> {
    crate::video::grow(&app, grow).map_err(err)
}

/// Open a finished file, or reveal its folder, using the desktop's handler.
#[tauri::command]
pub fn open_path(path: String, reveal: bool) -> Cmd<()> {
    let p = std::path::PathBuf::from(&path);
    let target = if reveal {
        p.parent().map(|d| d.to_path_buf()).unwrap_or(p)
    } else {
        p
    };
    if !target.exists() {
        return Err(format!("{} no longer exists", target.display()));
    }
    std::process::Command::new("xdg-open")
        .arg(&target)
        .spawn()
        .map_err(err)?;
    Ok(())
}

/// Native folder chooser via whichever dialog helper the desktop provides.
///
/// This avoids a GTK/portal dependency in-process; both helpers are present on
/// essentially every desktop install and print the chosen path on stdout.
#[tauri::command]
pub fn pick_directory(start: Option<String>) -> Cmd<Option<String>> {
    let start = start.unwrap_or_default();

    let attempts: Vec<(&str, Vec<String>)> = vec![
        (
            "zenity",
            vec![
                "--file-selection".into(),
                "--directory".into(),
                "--title=Choose a download folder".into(),
                format!("--filename={}/", start.trim_end_matches('/')),
            ],
        ),
        (
            "kdialog",
            vec!["--getexistingdirectory".into(), start.clone()],
        ),
    ];

    for (bin, args) in attempts {
        if mdm_core::supervisor::which(bin).is_none() {
            continue;
        }
        let out = std::process::Command::new(bin).args(&args).output();
        match out {
            Ok(o) if o.status.success() => {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                return Ok((!path.is_empty()).then_some(path));
            }
            // A non-zero exit is the user cancelling, which is not an error.
            Ok(_) => return Ok(None),
            Err(_) => continue,
        }
    }
    Err("no folder chooser found — install zenity or kdialog".into())
}

#[tauri::command]
pub fn read_clipboard_url() -> Option<String> {
    for (bin, args) in [
        ("wl-paste", vec!["--no-newline"]),
        ("xclip", vec!["-selection", "clipboard", "-o"]),
        ("xsel", vec!["--clipboard", "--output"]),
    ] {
        if mdm_core::supervisor::which(bin).is_none() {
            continue;
        }
        if let Ok(o) = std::process::Command::new(bin).args(&args).output() {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if text.starts_with("http://") || text.starts_with("https://") {
                    return Some(text);
                }
            }
        }
    }
    None
}
