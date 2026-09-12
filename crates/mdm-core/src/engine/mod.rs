//! The download engine: queueing, dispatch, progress tracking and scheduling.
//!
//! `Engine` itself lives here, along with its lifecycle and the snapshot the
//! UI repaints from. Its work is split across sibling files, each holding one
//! `impl Engine` block:
//!
//!   * [`submit`] — taking a job in: name, folder, and what gives way for it
//!   * [`dispatch`] — handing a row to the downloader that suits it
//!   * [`reconcile`] — reading back what the downloaders did, and what it means
//!   * [`control`] — pause, resume, retry, remove, re-target
//!   * [`schedule`] — queue windows
//!
//! What any of them *decides* — as opposed to carries out — belongs in
//! [`crate::rules`], which has no store, no filesystem and no runtime, and is
//! where those decisions are tested.

use crate::categories;
use crate::model::{Download, Header, Job, Queue, Settings, Status};
use crate::naming::{percent_decode, sanitize, stem_taken, unique_filename, unique_name};
use crate::rules::{self, Next};
use crate::store::Store;

use crate::{fetch, now, ytdlp};
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

mod control;
mod dispatch;
mod reconcile;
mod schedule;
mod submit;

/// How often the engine reconciles with its downloaders. Fast enough that the
/// progress bar looks continuous, slow enough that polling costs nothing
/// measurable.
const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// Everything the UI needs for one repaint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub downloads: Vec<Download>,
    pub global_speed: i64,
    pub active: i64,
    pub queued: i64,
}

/// Holds a row's dispatch claim, and gives it back however the dispatch
/// ends — including the paths that return early or fail, which is most of
/// them. A claim left behind would park that row for the life of the
/// process, so it is not left to a `remove` at the end of a function.
struct Claim<'a> {
    set: &'a Mutex<HashSet<i64>>,
    id: i64,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

/// Which of the in-process downloaders a job goes to.
///
/// All three end in the same place — one file, reported through one
/// `fetch::Event` channel, reaped by one loop — so the choice is made once,
/// here, and nothing downstream has to know which it was.
enum Fetcher {
    /// One address, one file, as many connections as the server allows.
    Direct,
    /// A manifest: hundreds of segments, remuxed once they are all in.
    Segmented,
    /// Streams an extractor resolved for us — whole files, fetched by the
    /// ordinary downloader and then merged into one MP4 in this process.
    /// This is the path that needs no ffmpeg.
    Merged(Vec<crate::stream::Stream>),
    /// A connection already open, because the address will not answer twice.
    /// Boxed because a response is large next to the other variants and this
    /// is the rare one. See [`Engine::preempt`].
    Held(Box<(reqwest::Response, fetch::Probe)>),
}

/// Deliberately shaped like [`YtState`]: both are engines the poll loop has to
/// watch rather than ask, so they are reaped the same way.
struct FetchState {
    downloaded: i64,
    total: i64,
    speed: i64,
    connections: i64,
    /// Where it actually landed, known only once the partial file is renamed.
    output: Option<PathBuf>,
    /// Raised to wind every connection down. The partial file and its state
    /// stay put, which is what makes the next start a resume.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Set when *we* stopped it, so the reaper reads the end as a pause rather
    /// than a failure worth retrying.
    stopped_by_us: bool,
    task: tokio::task::JoinHandle<Result<PathBuf>>,
}

/// What a yt-dlp job has fetched, and what it weighs.
///
/// Its own number is per *stream*: yt-dlp fetches the video and then the
/// audio, counting each of them from zero, so what it reports walks back to
/// the start when the second one begins. Taken at face value the bar drops to
/// nothing and climbs a second time, which reads as the download having
/// started over. So the streams already done are banked and the report is
/// added to that.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    /// Bytes fetched by streams that have already finished.
    base: i64,
    /// What the stream now running has fetched, on yt-dlp's own count.
    stream: i64,
    /// The two together: what the row shows.
    downloaded: i64,
    /// What the job weighs. Seeded with the picker's figure when there was
    /// one, since that already weighed every stream.
    total: i64,
}

impl Tally {
    /// Fold in one progress report. `total` is the current stream's, or 0
    /// where yt-dlp has not said yet.
    fn advance(&mut self, downloaded: i64, total: i64) {
        // A count that went backwards is the next stream starting, not bytes
        // going missing: only the run of a single stream is monotonic.
        if downloaded < self.stream {
            self.base += self.stream;
        }
        self.stream = downloaded;
        self.downloaded = self.base + self.stream;
        if total > 0 {
            // One stream's weight is a floor for the job rather than the job
            // itself — taking it as the job is what sent the bar from 99%
            // back to 67%. A picker's figure covers the lot and stands.
            self.total = self.total.max(self.base + total);
        }
        self.total = self.total.max(self.downloaded);
    }
}

struct YtState {
    /// What has been fetched of this job, and what it weighs.
    tally: Tally,
    speed: i64,
    child: tokio::process::Child,
    /// Populated by the stderr reader; read when the process exits.
    last_error: std::sync::Arc<Mutex<Option<String>>>,
    /// Where yt-dlp actually wrote the file. It picks the name and, after
    /// muxing, the container, so this cannot be predicted up front.
    output: Option<PathBuf>,
    /// Title reported at extraction time, applied to the row so it stops
    /// showing a bare video id while the download runs.
    title: Option<String>,
    /// Whether that title has already been written to the database.
    title_applied: bool,
    /// Connections the downloader reports for this job. yt-dlp says nothing
    /// about its own, so this stays 0 and the row shows no count rather than
    /// inventing one; it is the fetcher that has a real number to give.
    connections: i64,
    /// Set when *we* killed yt-dlp. It has no pause, so pausing means killing
    /// it — and without this flag the exit reads as a crash, gets retried, and
    /// the download the user just paused starts itself again.
    stopped_by_us: bool,
    /// The last file yt-dlp said was already in place, so did not fetch. An
    /// intermediate format file is an ordinary resume; the job's own output
    /// means nothing was downloaded at all.
    skipped: Option<String>,
}

/// What a reaped yt-dlp process left behind, read off `YtState` before the
/// job is dropped so the store can be updated without holding its lock.
struct Exit {
    id: i64,
    status: Option<std::process::ExitStatus>,
    /// yt-dlp's own last words, when it had any.
    error: Option<String>,
    output: Option<PathBuf>,
    /// Whether the exit was our own doing rather than a failure.
    stopped: bool,
    skipped: Option<String>,
}

pub struct Engine {
    pub store: Arc<Store>,
    settings: RwLock<Settings>,
    events: broadcast::Sender<Snapshot>,
    /// Downloads the scheduler has parked because their queue window is shut.
    scheduler_held: Mutex<HashSet<i64>>,
    /// Rows a dispatch is currently working on.
    ///
    /// Starting a download is not instant — a yt-dlp job asks the extractor
    /// what the format expression names first, which is seconds of work —
    /// and until it finishes the row is in none of the job maps and still
    /// reads as Queued. The scheduler tick lands inside that gap, dispatches
    /// the same row again, and two downloads write the same files: measured
    /// here as one of them renaming the part file while the other was still
    /// fetching into it. So a dispatch claims its row for as long as it
    /// takes, and the claim is what the second attempt trips over.
    claimed: Mutex<HashSet<i64>>,
    retries: Mutex<HashMap<i64, u8>>,
    /// The earliest moment a failed download may be dispatched again.
    ///
    /// Without it a retry is re-queued into the very next tick, so five
    /// attempts against an outage that lasts seconds — a DNS server that
    /// blinks, a link that drops while a laptop changes network — are all
    /// spent inside the outage and the row lands on Failed before the
    /// connection is even back.
    retry_after: Mutex<HashMap<i64, std::time::Instant>>,
    ytdlp_jobs: Mutex<HashMap<i64, YtState>>,
    fetch_jobs: Mutex<HashMap<i64, FetchState>>,
    /// Live speed/connection counts, which are not worth persisting.
    live: Mutex<HashMap<i64, (i64, i64)>>,
    /// Weak handle to ourselves so spawned tasks can reach the engine without
    /// keeping it alive past shutdown. Set once, in `start`.
    me: RwLock<Weak<Engine>>,
}

impl Engine {
    pub async fn start(settings: Settings) -> Result<Arc<Self>> {
        crate::paths::ensure_dirs().context("creating application directories")?;

        let store = Arc::new(Store::open()?);
        let (events, _) = broadcast::channel(32);

        let engine = Arc::new(Self {
            store,
            settings: RwLock::new(settings),
            events,
            scheduler_held: Mutex::new(HashSet::new()),
            claimed: Mutex::new(HashSet::new()),
            retries: Mutex::new(HashMap::new()),
            retry_after: Mutex::new(HashMap::new()),
            ytdlp_jobs: Mutex::new(HashMap::new()),
            fetch_jobs: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            me: RwLock::new(Weak::new()),
        });
        *engine.me.write().unwrap() = Arc::downgrade(&engine);

        // The extractor, fetched if this machine has none and kept current
        // if it is ours to keep. In the background and never awaited: a
        // machine with no network still opens, still downloads everything
        // that needs no extractor, and tries again tomorrow.
        let auto_update = engine.settings().ytdlp_auto_update;
        tokio::spawn(async move { crate::tools::maintain(auto_update).await });

        let poll = engine.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if let Err(e) = poll.tick().await {
                    log::warn!("engine tick failed: {e:#}");
                }
            }
        });

        Ok(engine)
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Snapshot> {
        self.events.subscribe()
    }

    /// One row, as the store has it. `None` once it has been removed.
    pub fn download(&self, id: i64) -> Option<Download> {
        self.store.get(id).ok().flatten()
    }

    pub fn queues(&self) -> Result<Vec<Queue>> {
        self.store.queues()
    }

    pub fn save_queue(&self, q: &Queue) -> Result<()> {
        self.store.save_queue(q)
    }

    pub fn delete_queue(&self, name: &str) -> Result<()> {
        self.store.delete_queue(name)
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let mut downloads = self.store.list(500)?;
        let live = self.live.lock().unwrap();
        for d in &mut downloads {
            if let Some((speed, conns)) = live.get(&d.id) {
                d.download_speed = *speed;
                d.connections = *conns;
            }
        }
        drop(live);

        let global_speed = downloads
            .iter()
            .filter(|d| d.status == Status::Active)
            .map(|d| d.download_speed)
            .sum();
        let active = downloads.iter().filter(|d| d.status == Status::Active).count() as i64;
        let queued = downloads.iter().filter(|d| d.status == Status::Queued).count() as i64;

        Ok(Snapshot {
            downloads,
            global_speed,
            active,
            queued,
        })
    }

    async fn broadcast(&self) {
        if let Ok(snap) = self.snapshot() {
            // Errors here only mean nobody is listening yet.
            let _ = self.events.send(snap);
        }
    }

    pub async fn shutdown(&self) {
        for (_, mut state) in self.ytdlp_jobs.lock().unwrap().drain() {
            let _ = state.child.start_kill();
        }
        // Stopped rather than aborted: the flag lets each connection finish its
        // current chunk and checkpoint, so closing the app leaves a partial
        // file that resumes instead of one with holes in it.
        for (_, state) in self.fetch_jobs.lock().unwrap().drain() {
            state.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/* ---------------------------------------------------------------------- *
 * Helpers
 * ---------------------------------------------------------------------- */

fn job_from(d: &Download) -> Job {
    Job {
        url: d.url.clone(),
        mirrors: d.mirrors.clone(),
        filename: d.filename.clone(),
        size: d.total_bytes,
        mime: d.mime.clone(),
        headers: d.headers.clone(),
        referrer: d.referrer.clone(),
        cookie_store_id: String::new(),
        reason: "resume".into(),
        source: "engine".into(),
        directory: Some(d.directory.clone()),
        // Settled when the row was created, so a resume re-runs the decision
        // that was made rather than guessing at it again from the URL.
        use_ytdlp: Some(d.use_ytdlp),
        // Recorded on the row precisely so a resume, a retry or a scheduled
        // dispatch fetches the quality that was chosen rather than the default.
        format_id: d.format_id.clone(),
        output_name: d.output_name.clone(),
        start_paused: false,
        // Never re-sent: bytes live in the stash from the moment they arrive,
        // which is what lets a resume find them after a restart.
        data: None,
    }
}

/* ---------------------------------------------------------------------- *
 * Bytes handed over by the page
 * ---------------------------------------------------------------------- */

/// Where a captured blob waits between arriving and being started.
///
/// Under the runtime directory rather than the download folder: until the user
/// presses Start nothing has been agreed to, and a half-offered file must not
/// appear among the ones they chose to keep.
fn blob_stash(id: i64) -> PathBuf {
    crate::paths::runtime_dir().join("blobs").join(format!("{id}.bin"))
}

fn stash_blob(id: i64, bytes: &[u8]) -> Result<()> {
    let path = blob_stash(id);
    let dir = path.parent().expect("stash path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// GNOME/KDE/COSMIC all honour `notify-send`; absence of it is not an error.
fn notify(title: &str, body: &str) {
    if crate::which::which("notify-send").is_none() {
        return;
    }
    let _ = std::process::Command::new("notify-send")
        .arg("--app-name=My Download Manager")
        .arg("--icon=mdm")
        .arg(title)
        .arg(body)
        .spawn();
}

/// Did yt-dlp hand back a file it found rather than one it fetched?
///
/// Only the job's own output counts. yt-dlp says the same thing about an
/// intermediate format file it already has, and there it is the good news:
/// a resumed download picking up where it stopped.
fn reused_existing(output: &Option<PathBuf>, skipped: &Option<String>) -> bool {
    let (Some(output), Some(skipped)) = (output, skipped) else {
        return false;
    };
    Path::new(skipped).file_name() == output.file_name()
}

/* ------------------------------------------------------------------ *
 * HTTP authentication
 * ------------------------------------------------------------------ */

/// `Authorization: Basic …` for a username and password.
pub fn basic_auth(username: &str, password: &str) -> Header {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{username}:{password}"));
    Header { name: "Authorization".into(), value: format!("Basic {encoded}") }
}

/// Move any `user:password@` out of a job's URL and into its headers.
///
/// The credentials are the same either way — Basic authentication is what a
/// server asks for and what a URL of this shape means — but the URL is what
/// gets stored, logged, shown in the list and copied to the clipboard, and a
/// password should not be in any of those.
pub fn strip_userinfo(mut job: Job) -> Job {
    let Ok(mut url) = url::Url::parse(&job.url) else { return job };
    let username = percent_decode(url.username());
    if username.is_empty() {
        return job;
    }
    let password = url.password().map(percent_decode).unwrap_or_default();
    // Both setters fail only on a URL that cannot have a host — `mailto:`,
    // `data:` — which is not something this reaches with a username in it.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    job.url = url.to_string();
    job.headers.retain(|h| !h.name.eq_ignore_ascii_case("authorization"));
    job.headers.push(basic_auth(&username, &password));
    job
}

/// The login configured for this URL's host, if there is one.
///
/// Exact host match, deliberately. A wildcard that let `example.com` speak for
/// `files.example.com` would also let it speak for a host someone else
/// controls under the same suffix, and a password is not a thing to be
/// approximate about.
pub fn authorization_for(url: &str, settings: &Settings) -> Option<Header> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    settings
        .credentials
        .iter()
        .find(|c| !c.host.trim().is_empty() && c.host.trim().eq_ignore_ascii_case(&host))
        .map(|c| basic_auth(&c.username, &c.password))
}

fn local_minute_of_day() -> u16 {
    let now = chrono::Local::now();
    use chrono::Timelike;
    (now.hour() * 60 + now.minute()) as u16
}

/// 0 = Monday, matching `Queue::days`.
fn local_weekday() -> u8 {
    use chrono::Datelike;
    chrono::Local::now().weekday().num_days_from_monday() as u8
}


/// Whether yt-dlp belongs between this job and the fetcher.
///
/// The host is consulted only when nobody has looked. A caller that has
/// already tried to read a page out of this URL holds the better answer, and
/// overruling it is how a working download became a broken one: the format
/// picker exhausts every page it can find, gives up, offers the file the
/// player is using — and that file comes off the site's own CDN, so
/// `v16-webapp.tiktok.com` read as "a TikTok page" and an extractor was put in
/// front of an mp4 the fetcher could simply have taken.
///
/// Where nobody has an opinion the type still settles it before the host does:
/// a response the browser has already called video or audio is the file, and
/// bytes are not a page. A manifest is not caught by that — `.m3u8` arrives as
/// `application/vnd.apple.mpegurl`, and turning one of those into a file is
/// precisely yt-dlp's job.
pub fn wants_ytdlp(job: &Job) -> bool {
    job.use_ytdlp.unwrap_or_else(|| {
        // A manifest is no longer automatically an extractor's problem. HLS
        // and DASH are fetched and remuxed in process, and the only reason to
        // hand one to yt-dlp now is that the native path could not cope —
        // which is decided by trying, not by guessing, and lands here as an
        // explicit `use_ytdlp` on the retry.
        if crate::stream::looks_like_manifest(&job.url, &job.mime) {
            return false;
        }
        !ytdlp::is_media_response(&job.mime) && ytdlp::looks_like_streaming_site(&job.url)
    })
}

/// Convenience for the UI's "add URL" box.
pub fn job_from_url(url: &str) -> Job {
    Job {
        url: url.to_string(),
        mirrors: Vec::new(),
        filename: String::new(),
        size: -1,
        mime: String::new(),
        headers: Vec::new(),
        referrer: String::new(),
        cookie_store_id: String::new(),
        reason: "manual".into(),
        source: "ui".into(),
        directory: None,
        // Nothing has looked at this URL yet; the engine decides.
        use_ytdlp: None,
        format_id: None,
        output_name: None,
        start_paused: false,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::Tally;

    /// A merged YouTube download: a big video stream, then a small audio one,
    /// each counted from zero. What the row shows must only ever climb — the
    /// bar falling back to nothing and setting off again is the complaint this
    /// exists to answer.
    ///
    /// The *fraction* can still step back once, at the moment the audio
    /// announces a weight nothing knew about: bytes are facts, and a job whose
    /// weight is learned a stream at a time genuinely does turn out to be
    /// bigger than it looked. Where the weight is known up front — which is
    /// every download the picker starts — it does not, and the test below says
    /// so.
    #[test]
    fn the_second_stream_does_not_send_the_bar_backwards() {
        let mut tally = Tally::default();
        let mut highest = 0;
        let mut climbs = |t: &Tally| {
            assert!(
                t.downloaded >= highest,
                "progress went backwards: {} after {highest}",
                t.downloaded
            );
            assert!(t.total >= t.downloaded, "a job cannot weigh less than it has fetched");
            highest = t.downloaded;
        };

        for downloaded in [0, 40_000_000, 80_000_000, 100_000_000] {
            tally.advance(downloaded, 100_000_000);
            climbs(&tally);
        }
        for downloaded in [0, 2_000_000, 4_000_000] {
            tally.advance(downloaded, 4_000_000);
            climbs(&tally);
        }

        assert_eq!(tally.downloaded, 104_000_000, "both streams count");
        assert_eq!(tally.total, 104_000_000);
    }

    /// Where the picker weighed the formats up front, the row is scaled to the
    /// whole job from the first byte and one stream's own figure must not
    /// shrink it back down.
    #[test]
    fn a_weight_known_up_front_stands() {
        let mut tally = Tally { total: 104_000_000, ..Tally::default() };
        tally.advance(10_000_000, 100_000_000);
        assert_eq!(tally.total, 104_000_000);
        tally.advance(100_000_000, 100_000_000);
        assert_eq!(tally.total, 104_000_000);
        tally.advance(4_000_000, 4_000_000);
        assert_eq!(tally.downloaded, 104_000_000);
        assert_eq!(tally.total, 104_000_000);
    }

    /// A resumed stream picks up from what is already on disk rather than from
    /// zero, which is not a new stream and must not be banked as one.
    #[test]
    fn a_resume_is_not_a_second_stream() {
        let mut tally = Tally::default();
        tally.advance(30_000_000, 100_000_000);
        tally.advance(60_000_000, 100_000_000);
        assert_eq!(tally.downloaded, 60_000_000);
        assert_eq!(tally.total, 100_000_000);
    }

}
