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
    // the window is about to download came out of a page, and the server
    // behind it may only answer a request that looks like it did too.
    headers: Option<Vec<mdm_core::model::Header>>,
    referrer: Option<String>,
    // What the response called itself, where the window saw it. A manifest is
    // routed to the stream downloader rather than the plain fetcher, and on a
    // CDN that serves its playlist from an opaque path the type is the only
    // thing that says so — without it the playlist was saved as a file.
    mime: Option<String>,
) -> Cmd<i64> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("no URL given".into());
    }
    let mut job = mdm_core::engine::job_from_url(&url);
    job.directory = directory.filter(|d| !d.is_empty());
    job.headers = headers.unwrap_or_default();
    job.referrer = referrer.unwrap_or_default();
    job.mime = mime.unwrap_or_default();
    // What the picker weighed the chosen formats at. yt-dlp fetches the video
    // stream and then the audio, so without this the bar is scaled to the first
    // of them and rescales downwards when the second starts.
    job.size = size.filter(|s| *s > 0).unwrap_or(-1);
    // Passed through as it came: `None` leaves the engine to decide, and only
    // an answer someone actually gave overrides it.
    job.use_ytdlp = use_ytdlp;
    job.format_id = format_id;
    job.start_paused = start_paused.unwrap_or(false);
    if let Some(name) = filename.filter(|n| !n.is_empty()) {
        // For yt-dlp the name is a stem it will extend with the real
        // container; for a direct download it is the filename outright. Which
        // of the two it becomes rests on a decision the engine makes later, so
        // the engine is asked rather than second-guessed. Reading the caller's
        // own flag instead is what dropped the name whenever the engine went
        // the other way: yt-dlp fell back to `%(title)s [%(id)s]` over a CDN
        // path, which is a filename far past what any filesystem will take.
        if mdm_core::engine::wants_ytdlp(&job) {
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
pub async fn probe_media(
    engine: State<'_, Arc<Engine>>,
    url: String,
    // Whether this URL is one the window believes names a video, and so is
    // worth a second and third try against a site that answers a share of
    // requests with a challenge page. Absent, it is treated as a guess.
    insist: Option<bool>,
    // Files the browser was seen fetching in that tab. An extraction offering
    // one of them is, beyond argument, the video being watched — which is the
    // only way to tell a page that resolved from a page that resolved to the
    // post above the one under the button.
    streams: Option<Vec<String>>,
) -> Cmd<MediaInfo> {
    let settings = engine.settings();
    ytdlp::probe(
        &url,
        Some(settings.ytdlp_cookies_from.as_str()),
        &settings.ytdlp_extra_args,
        insist.unwrap_or(false),
        &streams.unwrap_or_default(),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn ytdlp_available() -> bool {
    ytdlp::available()
}

/// Where this install put the browser extension.
///
/// It is carried by every way of installing MDM — the .deb and .rpm stage it
/// under /usr/lib, the Windows installer copies it to %APPDATA%, install.sh
/// leaves it in the data directory — and until now nothing said so anywhere.
/// Somebody who installed from a release build got the app, got the native
/// host, and had no way to find the half that does the capturing short of
/// cloning the repository and building it, which is the one thing a release
/// build exists to avoid.
#[derive(serde::Serialize)]
pub struct ExtensionAssets {
    /// The signed .xpi, which Firefox installs in one click.
    pub firefox: Option<String>,
    /// The unpacked Chromium folder, which "Load unpacked" takes.
    pub chrome: Option<String>,
}

/// Every place an install of any shape could have left them, in the order a
/// real machine should be asked.
///
/// Listed rather than derived because there is no single answer: the bundler
/// names the resource directory after the product, the NSIS script copies out
/// of the install directory into %APPDATA% so an uninstall does not take the
/// package with it, and install.sh writes the data directory because a
/// checkout is not somewhere a file should still be needed months later.
fn extension_dirs(app: &AppHandle) -> Vec<std::path::PathBuf> {
    use tauri::Manager;
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();

    // What the bundler staged, wherever this build actually is. First because
    // it is the only one that is right by construction rather than by guess.
    if let Ok(dir) = app.path().resource_dir() {
        dirs.push(dir.clone());
        // Tauri stages resources under `resources/` in some layouts and beside
        // the binary in others; asking for both costs a stat.
        dirs.push(dir.join("resources"));
    }
    if let Ok(dir) = app.path().app_data_dir() {
        dirs.push(dir);
    }

    #[cfg(unix)]
    {
        // The package layouts. The bundler names this directory after the
        // product, spaces and all — see the same list in linux/postinstall.sh,
        // which has to find the native host in exactly the same way.
        for p in [
            "/usr/lib/My Download Manager",
            "/usr/lib/my-download-manager",
            "/usr/lib/mdm",
            "/usr/libexec/mdm",
            "/usr/share/mdm",
        ] {
            dirs.push(std::path::PathBuf::from(p));
        }
        // install.sh's, which is also where a packaged install's per-user copy
        // would be if one is ever written.
        if let Some(home) = std::env::var_os("HOME") {
            let home = std::path::PathBuf::from(home);
            dirs.push(
                std::env::var_os("XDG_DATA_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("mdm"),
            );
        }
    }

    // The development tree, so this works before anything has been installed.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            // target/release/mdm -> target/
            if let Some(target) = dir.parent() {
                dirs.push(target.to_path_buf());
            }
        }
    }
    dirs
}

/// Find the extension, wherever this install left it.
#[tauri::command]
pub fn extension_assets(app: AppHandle) -> ExtensionAssets {
    let dirs = extension_dirs(&app);
    let find = |names: &[&str], want_dir: bool| -> Option<String> {
        for dir in &dirs {
            for name in names {
                let p = dir.join(name);
                let ok = if want_dir { p.is_dir() } else { p.is_file() };
                if ok {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
        }
        None
    };
    ExtensionAssets {
        // Both names, because the bundler renames the signed package as it
        // stages it and install.sh does not.
        firefox: find(&["mdm-firefox.xpi", "mdm-firefox-signed.xpi"], false),
        // A folder rather than the .zip: "Load unpacked" is what Chrome, Brave,
        // Edge and Vivaldi offer without a store listing, and it takes a
        // directory. The .zip is for uploading to a store dashboard.
        chrome: find(&["mdm-chrome"], true),
    }
}

/// Hand the .xpi to Firefox, which is the click that installs it.
///
/// Not `xdg-open`: a .xpi has no registered handler on most desktops, so
/// opening it lands in an archive manager or a "choose an application" dialog
/// — the file arrives, and the one browser that could do anything with it is
/// not offered. Firefox is named outright instead, and told to open the file
/// as a URL, which is what makes it show its own "Add" prompt.
///
/// A machine without Firefox is not a failure to hide: the caller falls back
/// to revealing the file, and the message says which browser it wanted.
#[cfg(unix)]
#[tauri::command]
pub fn install_firefox_extension(path: String) -> Cmd<()> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("{} no longer exists", p.display()));
    }
    let url = format!("file://{}", p.display());
    // Flatpak last: a native Firefox and a Flatpak one can both be present,
    // and the native one is the one whose profile install.sh registered the
    // native messaging host into.
    let candidates: [(&str, Vec<String>); 4] = [
        ("firefox", vec![url.clone()]),
        ("firefox-esr", vec![url.clone()]),
        ("librewolf", vec![url.clone()]),
        (
            "flatpak",
            vec!["run".into(), "org.mozilla.firefox".into(), url.clone()],
        ),
    ];
    for (bin, args) in candidates {
        if std::process::Command::new(bin).args(&args).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("Firefox was not found on this machine".into())
}

#[cfg(windows)]
#[tauri::command]
pub fn install_firefox_extension(path: String) -> Cmd<()> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("{} no longer exists", p.display()));
    }
    let url = format!("file:///{}", p.display().to_string().replace('\\', "/"));
    for bin in [
        r"C:\Program Files\Mozilla Firefox\firefox.exe",
        r"C:\Program Files (x86)\Mozilla Firefox\firefox.exe",
        "firefox.exe",
    ] {
        if std::process::Command::new(bin).arg(&url).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("Firefox was not found on this machine".into())
}

/// Which Chromium browsers are on this machine, by the name a person calls them.
///
/// It used to open one of them at `chrome://extensions`, and that never worked.
/// Chromium refuses to navigate to a `chrome://` address given on the command
/// line — the check is deliberate, it is what stops a program talking a browser
/// into opening its own settings — so the browser started, ignored the address
/// and showed a new tab. Which looked, correctly, like MDM opening an empty
/// window for no reason and calling it the extensions page. Reported as exactly
/// that.
///
/// There is no supported way to do the thing that was being attempted, and
/// there is no point attempting it badly. Chromium will not install an unpacked
/// extension on a program's say-so — which is the right answer to software
/// offering to add code to your browser — so `Load unpacked` is the whole of
/// the path and it needs a person at the keyboard. So this stops pretending and
/// returns what is actually useful: the names to put in the instructions, so
/// they say "Brave" to somebody using Brave rather than listing four browsers
/// and leaving them to find themselves in it.
///
/// Empty is a real answer and the caller says so: no Chromium browser here.
#[cfg(unix)]
#[tauri::command]
pub fn chromium_browsers() -> Vec<String> {
    // Brave first among equals: it is the one people arrive from when Firefox
    // is not the browser they use. Beyond that the order is arbitrary.
    const BROWSERS: [(&str, &str); 8] = [
        ("brave-browser", "Brave"),
        ("brave", "Brave"),
        ("google-chrome", "Chrome"),
        ("google-chrome-stable", "Chrome"),
        ("chromium", "Chromium"),
        ("chromium-browser", "Chromium"),
        ("microsoft-edge", "Edge"),
        ("vivaldi", "Vivaldi"),
    ];
    let mut found: Vec<String> = Vec::new();
    for (bin, name) in BROWSERS {
        // Looked for rather than started. Two launchers for one browser --
        // `brave-browser` and `brave` -- must not become two names in a list a
        // person is reading.
        if mdm_core::which::which(bin).is_some() && !found.iter().any(|n| n == name) {
            found.push(name.to_string());
        }
    }
    found
}

#[cfg(windows)]
#[tauri::command]
pub fn chromium_browsers() -> Vec<String> {
    const BROWSERS: [(&str, &str); 5] = [
        (r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe", "Brave"),
        (r"C:\Program Files\Google\Chrome\Application\chrome.exe", "Chrome"),
        (r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe", "Chrome"),
        (r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe", "Edge"),
        (r"C:\Program Files\Vivaldi\Application\vivaldi.exe", "Vivaldi"),
    ];
    let mut found: Vec<String> = Vec::new();
    for (path, name) in BROWSERS {
        if std::path::Path::new(path).exists() && !found.iter().any(|n| n == name) {
            found.push(name.to_string());
        }
    }
    found
}

#[tauri::command]
pub fn install_hint(package: String) -> String {
    mdm_core::distro::install(&package)
}

/// Open the standalone download window, empty, for the toolbar button.
#[tauri::command]
pub fn open_video_window(app: AppHandle) {
    crate::video::open(&app, String::new(), String::new(), 0.0, Vec::new());
}

/// The request the download window was opened for.
///
/// A window that is still loading cannot receive an event, so it collects the
/// request itself once its scripts are running.
#[tauri::command]
pub fn take_pending_video(pending: State<'_, crate::video::Pending>) -> Option<crate::video::Request> {
    pending.take()
}

/// Collect whatever arrived for the main window while it was closed.
///
/// The window is torn down on close, so a capture can land with nothing to
/// emit into; it is parked instead, and this is the load-time pull that pairs
/// with the live `mdm://batch` and `mdm://media` events.
#[tauri::command]
pub fn take_pending_main(
    pending: State<'_, crate::window::Pending>,
) -> Option<crate::window::Message> {
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
#[cfg(unix)]
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

/// `explorer` doubles as both "open" (double-click behaviour on a file or
/// folder) and, with `/select,`, "reveal this file in its folder" — there is
/// no separate reveal-only helper on Windows the way `xdg-open` needs one.
#[cfg(windows)]
#[tauri::command]
pub fn open_path(path: String, reveal: bool) -> Cmd<()> {
    use std::os::windows::process::CommandExt as _;

    // explorer is the one path consumer on Windows that will not accept `/`
    // as a separator. Everything else here treats the two interchangeably —
    // `exists()` below happily confirms a mixed-separator path — so a path
    // that is correct for every other purpose arrives at explorer unusable.
    // And explorer never says so: handed a path it cannot parse it silently
    // opens a default folder instead, Documents for a bare path and the
    // last-browsed folder after `/select,`. That is exactly what "Open" and
    // "Open folder" landing in two unrelated places looks like. `/` is not
    // legal in a Windows filename, so rewriting every one is unambiguous.
    let p = std::path::PathBuf::from(path.replace('/', "\\"));
    if !p.exists() {
        return Err(format!("{} no longer exists", p.display()));
    }
    let mut cmd = std::process::Command::new("explorer");
    if reveal {
        // explorer's own command-line parser (not the usual
        // CommandLineToArgvW) wants exactly `/select,"path"` — the comma
        // outside the quotes, the path inside. Handing `/select,<path>` to
        // `Command::arg()` as one argument used to work for paths with no
        // spaces, but `arg()` quotes an argument containing spaces by
        // wrapping the *whole* string — comma included — in one extra pair
        // of quotes. Explorer doesn't recognise that shape as the /select,
        // switch at all, and silently falls back to a default folder
        // (typically Documents) instead of erroring, which is why nearly any
        // real download title reproduced this. Windows filenames can never
        // contain `"`, so quoting the path ourselves needs no escaping, and
        // `raw_arg` is what lets those quotes reach explorer exactly as
        // written instead of being re-quoted by `Command`.
        let mut raw = std::ffi::OsString::from("/select,\"");
        raw.push(p.as_os_str());
        raw.push("\"");
        cmd.raw_arg(raw);
    } else {
        cmd.arg(&p);
    }
    // explorer.exe returns exit code 1 on plain success (a long-standing
    // quirk), so spawning and not waiting on a status is deliberate here.
    cmd.spawn().map_err(err)?;
    Ok(())
}

/// Native folder chooser via whichever dialog helper the desktop provides.
///
/// This avoids a GTK/portal dependency in-process; both helpers are present on
/// essentially every desktop install and print the chosen path on stdout.
#[cfg(unix)]
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
        if mdm_core::which::which(bin).is_none() {
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

/// The native Win32 common item dialog via `rfd`, in-process — there is no
/// standalone CLI folder-chooser to shell out to the way zenity/kdialog are.
#[cfg(windows)]
#[tauri::command]
pub fn pick_directory(start: Option<String>) -> Cmd<Option<String>> {
    let start = start.unwrap_or_default();
    let mut dialog = rfd::FileDialog::new().set_title("Choose a download folder");
    if !start.is_empty() {
        dialog = dialog.set_directory(&start);
    }
    Ok(dialog.pick_folder().map(|p| p.display().to_string()))
}

#[cfg(unix)]
#[tauri::command]
pub fn read_clipboard_url() -> Option<String> {
    for (bin, args) in [
        ("wl-paste", vec!["--no-newline"]),
        ("xclip", vec!["-selection", "clipboard", "-o"]),
        ("xsel", vec!["--clipboard", "--output"]),
    ] {
        if mdm_core::which::which(bin).is_none() {
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

#[cfg(windows)]
#[tauri::command]
pub fn read_clipboard_url() -> Option<String> {
    let text = arboard::Clipboard::new().ok()?.get_text().ok()?;
    let text = text.trim();
    (text.starts_with("http://") || text.starts_with("https://")).then(|| text.to_string())
}

/* ------------------------------------------------------------------ *
 * Updates
 * ------------------------------------------------------------------ */

/// What the frontend needs to offer an update, or `None` when this is already
/// the newest build.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOffer {
    pub version: String,
    pub current_version: String,
    pub notes: String,
    pub date: String,
}

/// Is there a newer version, and what is it?
///
/// Answers `None` for every reason that is not "yes": no update, no network,
/// an endpoint that is not there yet. An update check is a convenience, and a
/// convenience that reports its own failures to the user is a nuisance — the
/// reason goes to the log instead, where someone looking for it will find it.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Cmd<Option<UpdateOffer>> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            log::debug!("no updater on this build: {e}");
            return Ok(None);
        }
    };
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(UpdateOffer {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone().unwrap_or_default(),
            date: update.date.map(|d| d.to_string()).unwrap_or_default(),
        })),
        Ok(None) => Ok(None),
        Err(e) => {
            log::debug!("update check failed: {e}");
            Ok(None)
        }
    }
}

/// Fetch the new version and hand it to the installer.
///
/// This one *does* report its failures: the user asked for it, and an update
/// that quietly does nothing is worse than one that says why it could not.
/// The app is replaced and restarted by the installer, so nothing after a
/// successful call here runs for long.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Cmd<()> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = app.updater().map_err(err)?;
    // Checked again rather than carried over from `check_update`: holding a
    // half-hour-old `Update` across two commands buys nothing, and re-asking
    // costs one request against a URL that is certainly warm.
    let Some(update) = updater.check().await.map_err(err)? else {
        return Err("there is no update to install".into());
    };
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(err)?;
    Ok(())
}
