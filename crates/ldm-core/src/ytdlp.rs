//! yt-dlp integration for streaming sites.
//!
//! Streams are the one case a plain HTTP downloader cannot handle: HLS and
//! DASH arrive as thousands of small fragments behind a manifest. yt-dlp does
//! the extraction and muxing, but we hand it aria2 as its downloader so the
//! fragments are still fetched in parallel rather than one at a time.

use crate::supervisor::which;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// One selectable output from `yt-dlp -J`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Format {
    pub format_id: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub resolution: String,
    #[serde(default)]
    pub filesize: Option<i64>,
    #[serde(default)]
    pub vcodec: String,
    #[serde(default)]
    pub acodec: String,
    #[serde(default)]
    pub tbr: Option<f64>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub title: String,
    pub duration: Option<f64>,
    pub thumbnail: Option<String>,
    pub extractor: String,
    pub formats: Vec<Format>,
    pub is_playlist: bool,
    pub entry_count: usize,
}

/// Progress emitted while a yt-dlp download runs.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub downloaded: i64,
    pub total: i64,
    pub speed: i64,
    /// How many connections aria2 currently has open. Reported rather than
    /// assumed: a streamed download is segmented exactly like any other, and
    /// claiming "1" made it look like it was not.
    pub connections: i64,
}

/// What the running process reports back.
#[derive(Debug, Clone)]
pub enum Event {
    Progress(Progress),
    /// Final path, once yt-dlp has muxed and moved the file into place.
    /// yt-dlp names the output itself, so this is the only way to learn it.
    Finished(std::path::PathBuf),
    /// The page title, known as soon as extraction finishes and long before
    /// any bytes arrive. Without it the row shows a bare video id for the
    /// whole download, since that is all the URL reveals.
    Title(String),
    /// A file yt-dlp found already in place and so did not fetch.
    Skipped(String),
}

/// Hand yt-dlp a browser to read cookies from.
///
/// Sites increasingly refuse anonymous extraction; the session the user
/// already has in their browser is the least intrusive way to satisfy that.
fn apply_cookies(cmd: &mut Command, cookies_from: Option<&str>) {
    if let Some(browser) = cookies_from.map(str::trim).filter(|b| !b.is_empty()) {
        cmd.arg("--cookies-from-browser").arg(browser);
    }
}

/// JavaScript runtimes yt-dlp can drive, in its own order of preference,
/// paired with the name each one's binary actually has on disk.
const JS_RUNTIMES: &[(&str, &str)] = &[
    ("deno", "deno"),
    ("node", "node"),
    ("quickjs", "qjs"),
    ("bun", "bun"),
];

/// Enable every JavaScript runtime that is installed.
///
/// YouTube obfuscates the `n` query parameter behind a JS challenge. With no
/// runtime to solve it yt-dlp drops every https format and the extraction
/// surfaces as the bewildering "The page needs to be reloaded" — reloading the
/// page in the browser changes nothing, because the page was never the
/// problem. yt-dlp enables only `deno` by default, which almost no desktop
/// has, so hand it everything present and let it keep its own priority order.
fn apply_js_runtimes(cmd: &mut Command) {
    if !supports_js_runtimes() {
        return;
    }
    for (name, exe) in JS_RUNTIMES {
        if which(exe).is_some() {
            cmd.arg("--js-runtimes").arg(name);
        }
    }
}

/// Does the installed yt-dlp know `--js-runtimes`?
///
/// The flag is recent, and passing it to an older build turns every single
/// extraction into "no such option" — so ask once rather than guess from a
/// version string, which forks and nightlies render meaningless.
fn supports_js_runtimes() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        binary()
            .ok()
            .and_then(|bin| std::process::Command::new(bin).arg("--help").output().ok())
            .is_some_and(|out| String::from_utf8_lossy(&out.stdout).contains("--js-runtimes"))
    })
}

/// Is there no JavaScript runtime at all? Used only to explain a failure.
fn no_js_runtime() -> bool {
    JS_RUNTIMES.iter().all(|(_, exe)| which(exe).is_none())
}

/// Turn a challenge-solving failure into the one thing that fixes it.
///
/// yt-dlp reports this as a property of the page, which sends people reloading
/// and reinstalling; it is really a missing dependency on this machine.
fn js_challenge_advice(line: &str) -> String {
    if no_js_runtime() {
        return format!(
            "YouTube's player challenge could not be solved: yt-dlp needs a \
             JavaScript runtime and none is installed. Fix it with: \
             sudo dnf install nodejs   (deno, bun and quickjs also work). \
             yt-dlp said: {line}"
        );
    }
    if !supports_js_runtimes() {
        return format!(
            "YouTube's player challenge could not be solved: this yt-dlp is too \
             old to use the JavaScript runtime that is installed. Update it \
             with: sudo dnf upgrade yt-dlp. yt-dlp said: {line}"
        );
    }
    format!(
        "YouTube refused the extraction. yt-dlp usually runs a step behind \
         YouTube here, so updating it (sudo dnf upgrade yt-dlp) is the usual \
         fix. yt-dlp said: {line}"
    )
}

/// How long a probe's extraction stays good enough to download from.
///
/// Deliberately short. Signed media URLs and the solved `n` parameter do last
/// hours, but the only case worth optimising is the ordinary one — the picker
/// is open, a quality is chosen, Start is pressed — and keeping the window
/// small means a stale reuse never has to be recovered from.
const INFO_REUSE: std::time::Duration = std::time::Duration::from_secs(300);

/// Where a probe leaves its raw result for the download that follows it.
fn info_cache_path(url: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(url.as_bytes());
    crate::paths::runtime_dir()
        .join("info")
        .join(format!("{digest:x}.info.json"))
}

/// Keep the probe's own JSON so the download does not have to extract again.
///
/// Extraction is the slow half of starting a video: fetching the page, the
/// player script and solving the JS challenge costs seconds even on a fast
/// link, and doing it twice for one download also doubles how hard the site is
/// hit — which is its own way of getting throttled.
fn store_info_json(url: &str, json: &[u8]) {
    let path = info_cache_path(url);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    sweep_info_cache(dir);
    if let Err(e) = std::fs::write(&path, json) {
        log::debug!("could not cache extraction for {url}: {e}");
    }
}

/// Drop entries no download can still use, so the directory cannot grow
/// without bound across a long-running session.
fn sweep_info_cache(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|age| age > INFO_REUSE).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The probe's result for this URL, if it is recent enough to download from.
fn fresh_info_json(url: &str) -> Option<std::path::PathBuf> {
    let path = info_cache_path(url);
    let age = std::fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    (age <= INFO_REUSE).then_some(path)
}

/// The answer to a probe we have already run, without running it again.
fn cached_media_info(url: &str) -> Option<MediaInfo> {
    let raw = std::fs::read(fresh_info_json(url)?).ok()?;
    media_info(&serde_json::from_slice(&raw).ok()?).ok()
}

/// One extraction per page at a time.
///
/// The window asks for the formats at the same moment the request that opened
/// it does, and two yt-dlp processes racing over one page is both twice the
/// work and twice the traffic a rate limiter counts. The loser of the race
/// waits and then finds the answer already cached.
async fn single_flight(url: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = probe_locks().lock().unwrap();
        // Nothing removes entries as they finish — a waiter may still hold a
        // clone — so drop the idle ones once there are enough to bother.
        if locks.len() > 64 {
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks.entry(url.to_owned()).or_default().clone()
    };
    lock.lock_owned().await
}

type ProbeLocks = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

fn probe_locks() -> &'static ProbeLocks {
    static LOCKS: OnceLock<ProbeLocks> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// Does this failure mean the player challenge went unsolved?
fn is_js_challenge_failure(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("the page needs to be reloaded")
        || lower.contains("n challenge solving failed")
        || lower.contains("challenge solver")
}

fn binary() -> Result<std::path::PathBuf> {
    which("yt-dlp")
        .ok_or_else(|| anyhow::anyhow!("yt-dlp not found on PATH — install it with: sudo dnf install yt-dlp"))
}

pub fn available() -> bool {
    which("yt-dlp").is_some()
}

/// Ask yt-dlp what a page offers, without downloading anything.
pub async fn probe(
    url: &str,
    cookies_from: Option<&str>,
    extra: &[String],
) -> Result<MediaInfo> {
    if let Some(info) = cached_media_info(url) {
        return Ok(info);
    }
    let _flight = single_flight(url).await;
    // Whoever we were queued behind has just cached the answer.
    if let Some(info) = cached_media_info(url) {
        return Ok(info);
    }

    let bin = binary()?;
    let mut cmd = Command::new(bin);
    cmd.args([
        "-J",
        "--no-warnings",
        "--no-playlist",
        "--flat-playlist",
        "--socket-timeout",
        "15",
    ]);
    apply_cookies(&mut cmd, cookies_from);
    apply_js_runtimes(&mut cmd);
    for arg in extra {
        cmd.arg(arg);
    }
    let out = cmd
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running yt-dlp -J")?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .find(|l| l.contains("ERROR"))
            .unwrap_or("unknown error")
            .trim();
        // Translate yt-dlp's wall of links into the one action that fixes it.
        if line.contains("not a bot") || line.contains("Sign in to confirm") {
            bail!(
                "YouTube demanded a signed-in session. MDM passes cookies from \
                 {}; check that browser is signed in to YouTube, or set a \
                 different one under Settings.",
                cookies_from.unwrap_or("no browser (disabled in Settings)")
            );
        }
        if is_js_challenge_failure(line) {
            bail!("{}", js_challenge_advice(line));
        }
        bail!("yt-dlp could not read that page: {line}");
    }

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing yt-dlp JSON")?;
    let info = media_info(&v)?;

    // Only a single video is worth keeping: a flat playlist carries no formats
    // to download from, so reusing it would strip the download of its choices.
    if !info.is_playlist && !info.formats.is_empty() {
        store_info_json(url, &out.stdout);
    }
    Ok(info)
}

/// Fold yt-dlp's `-J` output into what the picker needs.
fn media_info(v: &serde_json::Value) -> Result<MediaInfo> {
    let entries = v.get("entries").and_then(|e| e.as_array());
    let formats: Vec<Format> = v
        .get("formats")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(parse_format).collect())
        .unwrap_or_default();

    if formats.is_empty() && entries.is_none() {
        bail!(
            "YouTube returned no downloadable formats for this video — only \
             storyboard images. It is gated server-side; signing in to that \
             browser, or trying a different player client under Settings, \
             sometimes helps."
        );
    }

    Ok(MediaInfo {
        title: v
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("Untitled")
            .to_owned(),
        duration: v.get("duration").and_then(serde_json::Value::as_f64),
        thumbnail: v
            .get("thumbnail")
            .and_then(|t| t.as_str())
            .map(str::to_owned),
        extractor: v
            .get("extractor_key")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned(),
        formats,
        is_playlist: entries.is_some(),
        entry_count: entries.map(|e| e.len()).unwrap_or(0),
    })
}

fn parse_format(v: &serde_json::Value) -> Option<Format> {
    let id = v.get("format_id")?.as_str()?.to_owned();
    // Storyboards are not playable media; they only clutter the picker.
    let vcodec = v.get("vcodec").and_then(|c| c.as_str()).unwrap_or("none");
    let acodec = v.get("acodec").and_then(|c| c.as_str()).unwrap_or("none");
    if vcodec == "none" && acodec == "none" {
        return None;
    }
    Some(Format {
        format_id: id,
        ext: v.get("ext").and_then(|e| e.as_str()).unwrap_or("").to_owned(),
        resolution: v
            .get("resolution")
            .and_then(|r| r.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                match (v.get("width").and_then(|w| w.as_i64()), v.get("height").and_then(|h| h.as_i64())) {
                    (Some(w), Some(h)) => format!("{w}x{h}"),
                    _ => "audio only".into(),
                }
            }),
        filesize: v
            .get("filesize")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| v.get("filesize_approx").and_then(serde_json::Value::as_i64)),
        vcodec: vcodec.to_owned(),
        acodec: acodec.to_owned(),
        tbr: v.get("tbr").and_then(serde_json::Value::as_f64),
        note: v
            .get("format_note")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_owned(),
    })
}

/// Marker prefix on our custom progress lines, so they are trivially
/// distinguishable from yt-dlp's ordinary chatter on the same stream.
const TAG: &str = "@LDM@";

/// Marker for the final-path line emitted by `--exec after_move`.
const FILE_TAG: &str = "@LDMFILE@";

/// Marker for the title line emitted by `--exec pre_process`.
const NAME_TAG: &str = "@LDMNAME@";

pub struct YtDlpHandle {
    pub child: Child,
    /// Last error yt-dlp reported. Without this a failure surfaces only as
    /// "exit status 1", which tells the user nothing they can act on.
    pub last_error: Arc<Mutex<Option<String>>>,
}

/// Start a yt-dlp download, streaming progress over `tx`.
///
/// `connections` is passed through to aria2 so streamed fragments get the same
/// parallelism as ordinary files.
pub async fn download(
    url: &str,
    dir: &Path,
    format: &str,
    connections: u8,
    headers: &[crate::model::Header],
    out_name: Option<&str>,
    cookies_from: Option<&str>,
    extra: &[String],
    tx: mpsc::Sender<Event>,
) -> Result<YtDlpHandle> {
    let bin = binary()?;
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir)
        .arg("--no-warnings")
        .arg("--newline")
        .arg("--no-playlist")
        .arg("--restrict-filenames")
        .arg("-f")
        .arg(format)
        // No forced container: yt-dlp picks one that fits the codecs (mp4 for
        // H.264/AAC, mkv for AV1 or Opus). Forcing mp4 would demand a re-encode
        // that either fails outright or quietly costs quality and minutes of CPU.
        .arg("-o")
        // Only the stem is ours: the container is settled by muxing, so the
        // extension must stay a template or the name would contradict the file.
        .arg(match out_name {
            Some(name) if !name.is_empty() => format!("{name}.%(ext)s"),
            _ => "%(title)s [%(id)s].%(ext)s".to_string(),
        })
        // aria2 only handles plain http/ftp, so a fragmented stream (HLS, and
        // DASH served as segments) never reaches it and reverts to yt-dlp's own
        // downloader — which fetches one fragment at a time unless told
        // otherwise. Matching the connection count keeps those streams as
        // parallel as everything else instead of an order of magnitude slower.
        .arg("--concurrent-fragments")
        .arg(connections.clamp(1, crate::aria2::MAX_CONNECTIONS).to_string())
        .arg("--downloader")
        .arg("aria2c")
        // `--summary-interval=1` is not cosmetic: an external downloader owns
        // the transfer, so yt-dlp's own progress hook fires exactly once, at
        // 100%. Without aria2's own readout a download sits at zero bytes for
        // its entire life and then jumps to done, which reads as a hang.
        .arg("--downloader-args")
        .arg(format!(
            "aria2c:-x{c} -s{c} -k1M --file-allocation=falloc \
             --console-log-level=warn --summary-interval=1",
            c = connections.clamp(1, crate::aria2::MAX_CONNECTIONS)
        ))
        // yt-dlp chooses the output name (and the container, after muxing),
        // so ask it to report the final path; nothing else knows it.
        .arg("--exec")
        .arg(format!("after_move:printf '{FILE_TAG}%s\\n' {{}}"))
        // pre_process fires right after extraction, so the title is available
        // before the download starts. --print would be the neater tool but it
        // implies --simulate at this stage, which would download nothing.
        .arg("--exec")
        .arg(format!("pre_process:printf '{NAME_TAG}%%s\\n' %(title)q"))
        .arg("--progress-template")
        .arg(format!(
            "download:{TAG}%(progress.downloaded_bytes)s|%(progress.total_bytes)s|\
             %(progress.total_bytes_estimate)s|%(progress.speed)s"
        ));

    apply_cookies(&mut cmd, cookies_from);
    apply_js_runtimes(&mut cmd);
    for arg in extra {
        cmd.arg(arg);
    }

    // Hand back the extraction the picker just did, when it is still warm.
    let reused_info = fresh_info_json(url);
    if let Some(path) = &reused_info {
        log::info!("reusing the probe's extraction for {url}");
        cmd.arg("--load-info-json").arg(path);
    }

    for h in headers {
        // Cookies and Referer are what make member-only or hotlink-protected
        // media resolvable at all.
        cmd.arg("--add-header").arg(h.to_arg());
    }

    // With `--load-info-json` the URL would be extracted a second time and
    // both results downloaded, so it is one or the other.
    if reused_info.is_none() {
        cmd.arg(url);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("spawning yt-dlp")?;
    let stdout = child
        .stdout
        .take()
        .context("yt-dlp stdout was not captured")?;

    let last_error = Arc::new(Mutex::new(None));
    if let Some(stderr) = child.stderr.take() {
        let sink = last_error.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("ERROR:") {
                    // Strip yt-dlp's own prefixes so the UI shows the cause.
                    let raw = line
                        .split_once("ERROR:")
                        .map(|(_, rest)| rest.trim())
                        .unwrap_or(line.trim());
                    let msg = if is_js_challenge_failure(raw) {
                        js_challenge_advice(raw)
                    } else {
                        raw.to_string()
                    };
                    log::warn!("yt-dlp: {msg}");
                    *sink.lock().unwrap() = Some(msg);
                }
            }
        });
    }

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        // aria2 prints one summary line per active download; yt-dlp runs it
        // once for the video stream and again for the audio, so the counts are
        // summed across gids to describe the job rather than the segment.
        let mut segments: HashMap<String, Segment> = HashMap::new();
        // While aria2 is reporting, yt-dlp's own line is strictly worse — it
        // arrives once per format, at 100%, and would drag the total backwards
        // when the second format starts from zero.
        let mut aria2_reporting = false;

        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if let Some(seg) = parse_aria2_summary(line) {
                aria2_reporting = true;
                segments.insert(seg.gid.clone(), seg);
                if tx.send(Event::Progress(total_of(&segments))).await.is_err() {
                    break;
                }
                continue;
            }
            if let Some(path) = line.strip_prefix(FILE_TAG) {
                let path = path.trim();
                if !path.is_empty()
                    && tx
                        .send(Event::Finished(std::path::PathBuf::from(path)))
                        .await
                        .is_err()
                {
                    break;
                }
                continue;
            }
            if let Some(name) = parse_already_downloaded(line) {
                if tx.send(Event::Skipped(name.to_string())).await.is_err() {
                    break;
                }
                continue;
            }
            if let Some(title) = line.strip_prefix(NAME_TAG) {
                let title = title.trim();
                if !title.is_empty()
                    && tx.send(Event::Title(title.to_string())).await.is_err()
                {
                    break;
                }
                continue;
            }
            let Some(rest) = line.strip_prefix(TAG) else {
                continue;
            };
            if aria2_reporting {
                continue;
            }
            if let Some(p) = parse_progress(rest) {
                if tx.send(Event::Progress(p)).await.is_err() {
                    break; // receiver gone: the download was cancelled
                }
            }
        }
    });

    Ok(YtDlpHandle { child, last_error })
}

/// The file in `[download] <file> has already been downloaded`.
///
/// yt-dlp treats a target that is already there as a download that is already
/// done: it fetches nothing and exits a success. Said of an intermediate
/// format file that is a resume; said of the final output it means the job
/// produced no bytes at all, which is not something to report as complete.
fn parse_already_downloaded(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("[download] ")?;
    let (name, _) = rest.split_once(" has already been downloaded")?;
    Some(name.trim()).filter(|name| !name.is_empty())
}

/// One download's line in aria2's progress summary.
#[derive(Debug, Clone)]
struct Segment {
    gid: String,
    downloaded: i64,
    total: i64,
    speed: i64,
    connections: i64,
    seen: std::time::Instant,
}

/// Fold every segment aria2 has reported into one figure for the job.
fn total_of(segments: &HashMap<String, Segment>) -> Progress {
    // A segment aria2 has stopped mentioning has finished; its bytes still
    // count, but quoting its last speed would invent throughput that stopped.
    const CURRENT: std::time::Duration = std::time::Duration::from_secs(3);
    let mut progress = Progress { downloaded: 0, total: 0, speed: 0, connections: 0 };
    for seg in segments.values() {
        progress.downloaded += seg.downloaded;
        progress.total += seg.total;
        if seg.seen.elapsed() < CURRENT {
            progress.speed += seg.speed;
            progress.connections += seg.connections;
        }
    }
    if progress.total <= 0 {
        progress.total = -1;
    }
    progress
}

/// Parse `[#8d4a4d 31MiB/114MiB(27%) CN:16 DL:10MiB ETA:8s]`.
///
/// This readout is the only progress there is once an external downloader owns
/// the transfer, so it is worth reading even though aria2 rounds it for human
/// eyes; the exact byte count arrives with the finished file.
fn parse_aria2_summary(line: &str) -> Option<Segment> {
    let body = line.strip_prefix("[#")?.strip_suffix(']')?;
    let mut fields = body.split_whitespace();
    let gid = fields.next()?.to_owned();
    let (mut downloaded, mut total, mut speed, mut connections) = (None, None, 0, 0);
    for field in fields {
        if let Some(rate) = field.strip_prefix("DL:") {
            speed = parse_human_size(rate).unwrap_or(0);
        } else if let Some(n) = field.strip_prefix("CN:") {
            connections = n.parse().unwrap_or(0);
        } else if let Some((done, whole)) = field.split_once('/') {
            // "114MiB(27%)" — the percentage is derivable, so drop it.
            downloaded = parse_human_size(done);
            total = parse_human_size(whole.split('(').next().unwrap_or(whole));
        }
    }
    let total = total?;
    Some(Segment {
        gid,
        downloaded: downloaded?,
        total: if total > 0 { total } else { -1 },
        speed,
        connections,
        seen: std::time::Instant::now(),
    })
}

/// aria2 writes sizes for people: `0B`, `368KiB`, `7.7MiB`, `6.5GiB`.
fn parse_human_size(s: &str) -> Option<i64> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let value: f64 = s[..split].parse().ok()?;
    let scale: f64 = match &s[split..] {
        "" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * scale) as i64)
}

/// `downloaded|total|total_estimate|speed`, where yt-dlp writes "NA" for any
/// field it does not know yet.
fn parse_progress(s: &str) -> Option<Progress> {
    let mut it = s.split('|');
    let field = |v: Option<&str>| -> i64 {
        v.map(str::trim)
            .filter(|x| *x != "NA" && !x.is_empty())
            // Values can arrive as floats ("1234.0"); truncate rather than fail.
            .and_then(|x| x.parse::<f64>().ok())
            .map(|f| f as i64)
            .unwrap_or(-1)
    };
    let downloaded = field(it.next());
    let total = field(it.next());
    let estimate = field(it.next());
    let speed = field(it.next());
    if downloaded < 0 {
        return None;
    }
    Some(Progress {
        downloaded,
        total: if total > 0 { total } else { estimate },
        speed: speed.max(0),
        // yt-dlp says nothing about connections; aria2's own readout does.
        connections: 0,
    })
}

/// Is this failure worth retrying?
///
/// A missing format or a private video will fail identically five times in a
/// row; only transient network trouble deserves another attempt.
pub fn is_permanent_error(message: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "requested format is not available",
        "video unavailable",
        "private video",
        "members-only",
        "is not a valid url",
        "unsupported url",
        "no video formats found",
        "sign in to confirm",
        "this video is available to this channel's members",
        "removed by the uploader",
        "account associated with this video has been terminated",
        "video has been removed",
        "not available in your country",
        "geo restricted",
        "age-restricted",
        // A missing JavaScript runtime will not appear part-way through a
        // retry loop, and YouTube answers identically every time.
        "the page needs to be reloaded",
        "player challenge could not be solved",
    ];
    let lower = message.to_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

/// Hosts where the page is a player rather than a file, so yt-dlp is the right
/// tool even though the URL looks ordinary.
pub fn looks_like_streaming_site(url: &str) -> bool {
    const HOSTS: &[&str] = &[
        "youtube.com", "youtu.be", "vimeo.com", "dailymotion.com", "twitch.tv",
        "twitter.com", "x.com", "reddit.com", "tiktok.com", "instagram.com",
        "facebook.com", "soundcloud.com", "bandcamp.com", "bilibili.com",
        "odysee.com", "rumble.com", "nebula.tv", "ted.com",
    ];
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_start_matches("www.").to_lowercase();
    HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/* ---------------------------------------------------------------------- *
 * Unit tests
 *
 * The aria2 readout is parsed rather than structured data, and it is the only
 * progress a yt-dlp download reports, so its shapes are pinned down here.
 * ---------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_summary_line() {
        let seg = parse_aria2_summary("[#8d4a4d 31MiB/114MiB(27%) CN:16 DL:10MiB ETA:8s]")
            .expect("a well-formed summary line");
        assert_eq!(seg.gid, "8d4a4d");
        assert_eq!(seg.downloaded, 31 * 1024 * 1024);
        assert_eq!(seg.total, 114 * 1024 * 1024);
        assert_eq!(seg.speed, 10 * 1024 * 1024);
        assert_eq!(seg.connections, 16);
    }

    #[test]
    fn reads_a_line_without_percentage_or_eta() {
        // aria2 omits both until it knows the size.
        let seg = parse_aria2_summary("[#685787 0B/0B CN:1 DL:0B]").expect("a bare line");
        assert_eq!(seg.downloaded, 0);
        assert_eq!(seg.total, -1, "an unknown size must not read as zero");
        assert_eq!(seg.speed, 0);
        assert_eq!(seg.connections, 1);
    }

    #[test]
    fn ignores_everything_that_is_not_a_summary_line() {
        for line in [
            "[download] Destination: video.mp4",
            "FILE: /home/u/Downloads/video.mp4.part",
            "===============================",
            "@LDM@1|2|3|4",
            "[#deadbe",
        ] {
            assert!(parse_aria2_summary(line).is_none(), "parsed {line:?}");
        }
    }

    #[test]
    fn scales_the_units_aria2_prints() {
        assert_eq!(parse_human_size("0B"), Some(0));
        assert_eq!(parse_human_size("368KiB"), Some(376_832));
        assert_eq!(parse_human_size("7.7MiB"), Some(8_074_035));
        assert_eq!(parse_human_size("6.5GiB"), Some(6_979_321_856));
        assert_eq!(parse_human_size("nonsense"), None);
    }

    #[test]
    fn sums_segments_so_video_plus_audio_reads_as_one_job() {
        let mut segments = HashMap::new();
        segments.insert(
            "aaa".to_string(),
            Segment { gid: "aaa".into(), downloaded: 100, total: 100, speed: 0,
                      connections: 0, seen: std::time::Instant::now() },
        );
        segments.insert(
            "bbb".to_string(),
            Segment { gid: "bbb".into(), downloaded: 20, total: 60, speed: 7,
                      connections: 16, seen: std::time::Instant::now() },
        );
        let p = total_of(&segments);
        assert_eq!(p.downloaded, 120);
        assert_eq!(p.total, 160);
        assert_eq!(p.speed, 7, "only the segment still moving contributes speed");
        assert_eq!(p.connections, 16, "and only it contributes connections");
    }

    #[test]
    fn reads_the_file_yt_dlp_refused_to_fetch_again() {
        assert_eq!(
            parse_already_downloaded("[download] Hymns.webm has already been downloaded"),
            Some("Hymns.webm"),
        );
        // The merge-time wording, and a name with spaces in it.
        assert_eq!(
            parse_already_downloaded(
                "[download] My video.f251.webm has already been downloaded and merged"
            ),
            Some("My video.f251.webm"),
        );
    }

    #[test]
    fn ordinary_download_chatter_is_not_a_skip() {
        for line in [
            "[download] Destination: Hymns.webm",
            "[download] 100% of 136.05MiB in 00:00:11 at 11.50MiB/s",
            "[Merger] Merging formats into \"Hymns.webm\"",
            "[download]  has already been downloaded",
        ] {
            assert!(parse_already_downloaded(line).is_none(), "parsed {line:?}");
        }
    }

    #[test]
    fn a_stalled_segment_stops_counting_towards_speed() {
        let mut segments = HashMap::new();
        segments.insert(
            "old".to_string(),
            Segment {
                gid: "old".into(),
                downloaded: 500,
                total: 500,
                speed: 999,
                connections: 16,
                seen: std::time::Instant::now() - std::time::Duration::from_secs(30),
            },
        );
        let p = total_of(&segments);
        assert_eq!(p.downloaded, 500, "its bytes still count");
        assert_eq!(p.speed, 0, "its throughput does not");
        assert_eq!(p.connections, 0, "nor its connections");
    }
}
