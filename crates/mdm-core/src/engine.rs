//! The download engine: queueing, dispatch, progress tracking and scheduling.

use crate::categories;
use crate::model::{Download, Header, Job, Queue, Settings, Status};
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

    /* ------------------------------------------------------------------ *
     * Submission
     * ------------------------------------------------------------------ */

    /// Accept a job from the extension, the UI or the CLI.
    ///
    /// Returns the new row id. Dispatch happens here when the queue window is
    /// open; otherwise the row sits at `Queued` for the scheduler to pick up.
    pub async fn submit(&self, job: Job) -> Result<i64> {
        // A password has no business sitting in the download list, being
        // logged, or being read over someone's shoulder. If the link arrived
        // with one — `https://user:secret@host/file` is how an authenticated
        // link is typed — it moves into the request headers, where every other
        // credential this app handles already lives, and the URL is kept and
        // shown without it.
        let job = strip_userinfo(job);
        // Clicking the same link twice must not start a rival download: two
        // of them racing for one name, and settling it by numbering the
        // second, is how you end up with one finished copy and one abandoned
        // stub.
        //
        // Same *target*, not same page: asking for the audio track of a video
        // that is downloading is asking for another file, and answering it
        // with the one already running would drop the request on the floor.
        let existing = self
            .store
            .find_unfinished_target(&job.url, job.format_id.as_deref())?;
        if let Some(existing) = existing {
            log::info!(
                "#{} already has {} — reusing that row rather than duplicating it",
                existing.id,
                existing.filename
            );
            // Not when the caller asked for it to wait: a second capture of
            // the same URL must not start the one still sitting in the window
            // unconfirmed.
            if existing.status == Status::Paused && !job.start_paused {
                self.resume(existing.id).await?;
            }
            if self.settings().notify {
                notify("Already downloading", &existing.filename);
            }
            return Ok(existing.id);
        }

        // Decoded before anything is recorded: a payload that will not decode
        // must not leave a row behind pointing at bytes that never existed.
        let blob = match &job.data {
            Some(encoded) => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .context("decoding the bytes the page handed over")?,
            ),
            None => None,
        };

        let settings = self.settings();
        // A blob is already the file. Its URL still carries the origin — and a
        // blob from facebook.com reads as a streaming site — so this has to be
        // settled by what arrived, not by where it came from.
        let use_ytdlp = blob.is_none() && wants_ytdlp(&job);
        let (directory, filename, category) =
            self.resolve_destination(&job, &settings, use_ytdlp)?;
        let output_name = match job.output_name.clone() {
            // Sanitised because it reaches yt-dlp as part of an output
            // template, where a name carrying "../" would write outside the
            // folder the user chose.
            Some(name) if use_ytdlp && !name.trim().is_empty() => {
                Some(self.free_stem(&directory, &sanitize(name), 0)?)
            }
            other => other,
        };

        let mut record = Download {
            id: 0,

            url: job.url.clone(),
            filename: filename.clone(),
            directory: directory.to_string_lossy().into_owned(),
            category: category.to_owned(),
            status: Status::Queued,
            total_bytes: job.size,
            completed_bytes: 0,
            download_speed: 0,
            connections: 0,
            mime: job.mime.clone(),
            referrer: job.referrer.clone(),
            headers: job.headers.clone(),
            error: None,
            sha256: None,
            created_at: now(),
            finished_at: None,
            queue: "main".into(),
            use_ytdlp,
            no_native: false,
            output_name,
            format_id: job.format_id.clone(),
            mirrors: job.mirrors.clone(),
        };
        if let Some(bytes) = &blob {
            // The only honest size there is: what the page actually handed over.
            record.total_bytes = bytes.len() as i64;
        }

        let id = self.store.insert(&record)?;
        record.id = id;

        // Park the bytes where the dispatch can find them. They cannot travel
        // on the job: "Download Later" ends the request there and the file has
        // to survive until the user presses Start, which may be a restart away.
        if let Some(bytes) = blob {
            if let Err(e) = stash_blob(id, &bytes) {
                // Nothing can be fetched to make good on this row, so it must
                // not be left behind looking startable.
                let _ = self.store.delete(id);
                return Err(e).context("holding the bytes the page handed over");
            }
        }

        if job.start_paused {
            // "Download Later": recorded and visible, but nothing is fetched
            // until the user presses play.
            self.store.set_status(id, Status::Paused, None)?;
            self.broadcast().await;
            return Ok(id);
        }

        if self.queue_window_open(&record.queue)? {
            if let Err(e) = self.dispatch(&record, &job).await {
                log::error!("dispatch of #{id} failed: {e:#}");
                self.store
                    .set_status(id, Status::Failed, Some(&format!("{e:#}")))?;
            }
        } else {
            self.scheduler_held.lock().unwrap().insert(id);
        }

        self.broadcast().await;
        Ok(id)
    }

    /// Work out where the file lands, creating the directory as a side effect.
    fn resolve_destination(
        &self,
        job: &Job,
        settings: &Settings,
        use_ytdlp: bool,
    ) -> Result<(PathBuf, String, &'static str)> {
        let filename = sanitize(if job.filename.is_empty() {
            filename_from_url(&job.url)
        } else {
            job.filename.clone()
        });
        let mut category = categories::categorize(&filename, &job.mime);
        // A streaming page URL carries no extension — "youtu.be/<id>" has
        // nothing to categorise by — so it would land in "Other". It is a
        // video download by definition; an audio-only pick is re-filed once
        // the real container is known.
        if use_ytdlp && category == "Other" {
            category = "Video";
        }

        // An explicit directory from the UI always wins; auto-categorising a
        // path the user just picked would be surprising.
        let base = match &job.directory {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ if settings.categorize => Path::new(&settings.download_dir).join(category),
            _ => PathBuf::from(&settings.download_dir),
        };

        std::fs::create_dir_all(&base)
            .with_context(|| format!("creating {}", base.display()))?;

        // Only for a direct download is this the name that will appear on
        // disk. A yt-dlp job carries a placeholder here until it reports the
        // name it chose, and its stem is made free separately.
        let filename = if use_ytdlp {
            filename
        } else {
            self.free_filename(&base, &filename, 0)?
        };
        Ok((base, filename, category))
    }

    /// A filename that will not be answered with the file already sitting there.
    ///
    /// A finished file of the same name would read as a download that is
    /// already done, and another copy is what was asked for, so the name gets
    /// numbered instead.
    ///
    /// The exception this used to carry — reuse the name when a control file
    /// marks an interrupted transfer — went with aria2, which wrote straight
    /// to the final name. The fetcher and the stream downloader both work in a
    /// `.mdmdownload` file and only take the real name once they are finished,
    /// so a file sitting at that name is by definition not one of ours in
    /// progress, and existing is enough to make the name taken.
    ///
    /// `except` is the row asking, which must not be counted as competition
    /// with itself.
    fn free_filename(&self, dir: &Path, name: &str, except: i64) -> Result<String> {
        let claimed = self
            .store
            .names_in_flight(&dir.to_string_lossy(), except)?;
        Ok(unique_filename(name, |candidate| {
            let path = dir.join(candidate);
            claimed.iter().any(|held| held == candidate)
                || path.exists()
        }))
    }

    /// A name for a yt-dlp download that nothing else has already taken.
    ///
    /// yt-dlp reads an existing target as a download that is already finished:
    /// it fetches nothing, reports the file that is there and exits happily,
    /// which lands the row "complete" holding bytes it never downloaded. The
    /// audio-only pick after the video of the same page is exactly that — opus
    /// audio and an AV1+opus mux are both `.webm` under one stem.
    ///
    /// The whole stem is what has to be free, not one filename: the container
    /// is settled by muxing, so which extension will follow is not ours to
    /// predict, and `stem.f251.webm.part` is just as much a collision.
    ///
    /// `except` is the row asking, which must not count as competition with
    /// itself; no row has id 0, so that asks about all of them.
    fn free_stem(&self, dir: &Path, stem: &str, except: i64) -> Result<String> {
        let claimed = self.store.names_in_flight(&dir.to_string_lossy(), except)?;
        // Listed once rather than per candidate: the folder can be large, and
        // what it holds cannot change underneath a single decision.
        let present: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();

        Ok(unique_name(stem, |candidate| {
            stem_taken(&claimed, candidate) || stem_taken(&present, candidate)
        }))
    }

    /// Hand a download to the fetcher (or yt-dlp) and record the resulting handle.
    async fn dispatch(&self, d: &Download, job: &Job) -> Result<()> {
        // Whoever holds the claim is already starting this one.
        if !self.claimed.lock().unwrap().insert(d.id) {
            log::debug!("#{} is already being started; leaving it to that", d.id);
            return Ok(());
        }
        let _claim = Claim { set: &self.claimed, id: d.id };
        self.dispatch_claimed(d, job).await
    }

    async fn dispatch_claimed(&self, d: &Download, job: &Job) -> Result<()> {
        // Bytes the extension read out of a page: there is no server to ask,
        // and nothing to segment. Starting one is moving it into place.
        //
        // Keyed off the URL, not off finding a stash file. Row ids are reused
        // once the rows above them are gone, and a leftover stash must never
        // be able to answer for an ordinary download that happens to inherit
        // its number.
        if d.url.starts_with("blob:") {
            let stash = blob_stash(d.id);
            if !stash.is_file() {
                bail!(
                    "the page's copy of this file is gone — it was only ever \
                     held in memory, so ask the browser for it again"
                );
            }
            return self.dispatch_blob(d, &stash).await;
        }
        if d.use_ytdlp {
            return self.dispatch_ytdlp(d, job).await;
        }

        let settings = self.settings();

        // A manifest is not a file. HLS and DASH name hundreds of segments,
        // often with the picture and the sound in separate sets, and the
        // plain fetcher cannot make a video out of that — so this is decided
        // before the backend question, not after it.
        if crate::stream::looks_like_manifest(&d.url, &d.mime) {
            return self.dispatch_fetch(d, &settings, Fetcher::Segmented).await;
        }

        self.dispatch_fetch(d, &settings, Fetcher::Direct).await
    }


    /// Finish a capture whose bytes are already here.
    ///
    /// A rename when the runtime directory and the download folder share a
    /// filesystem, which on a normal desktop they do not — `/run/user` is
    /// tmpfs — so a copy is the usual path and the rename is the free case.
    async fn dispatch_blob(&self, d: &Download, stash: &Path) -> Result<()> {
        let target = d.full_path();
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if std::fs::rename(stash, &target).is_err() {
            std::fs::copy(stash, &target)
                .with_context(|| format!("writing {}", target.display()))?;
            let _ = std::fs::remove_file(stash);
        }

        let size = std::fs::metadata(&target).map(|m| m.len() as i64).unwrap_or(-1);
        self.store.update_progress(d.id, size, size, None, None)?;
        let mut done = d.clone();
        done.total_bytes = size;
        done.completed_bytes = size;
        log::info!("#{} saved from the page: {} ({size} bytes)", d.id, d.filename);
        self.on_complete(&done).await
    }

    /// Hand an ordinary download to the in-process fetcher, or a manifest to
    /// the in-process stream downloader.
    ///
    /// The two share everything after dispatch. A stream reports through the
    /// same `fetch::Event` channel, is held in the same `fetch_jobs` map and is
    /// reaped by the same loop — the only difference is which future is
    /// spawned, because from the outside "download this" is one job either way.
    ///
    /// No gid: there is no daemon holding this one, so the row is tracked in
    /// `fetch_jobs` and reaped by the poll loop the same way a yt-dlp job is.
    async fn dispatch_fetch(
        &self,
        d: &Download,
        settings: &Settings,
        how: Fetcher,
    ) -> Result<()> {
        let mut spec = fetch::Spec::new(d.url.clone(), &d.directory);
        spec.mirrors = d.mirrors.clone();
        spec.filename = Some(d.filename.clone());
        spec.headers = d.headers.clone();
        spec.referrer = Some(d.referrer.clone()).filter(|r| !r.is_empty());
        spec.max_speed = settings.max_speed_per_download;
        spec.retries = settings.retry_limit;
        spec.proxy = settings.proxy.clone();
        // A capture from the browser already carries the session that was
        // logged in; only a link that arrived without one needs looking up.
        if let Some(header) = authorization_for(&d.url, settings) {
            if !spec
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("authorization"))
            {
                spec.headers.push(header);
            }
        }
        // The user's connection setting is a ceiling for the governor rather
        // than a target: it says how many are acceptable, and measurement says
        // how many actually help.
        spec.concurrency = fetch::Concurrency::Auto { max: settings.connections };

        let (tx, mut rx) = mpsc::channel(64);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Named before it is spent: `Held` carries the open response into the
        // task, so `how` cannot be borrowed again for the log line below.
        let label = match &how {
            Fetcher::Segmented => "stream downloader".to_string(),
            Fetcher::Merged(streams) => format!("native merge of {} stream(s)", streams.len()),
            Fetcher::Direct => "built-in fetcher".to_string(),
            Fetcher::Held(_) => "the request the browser was about to make".to_string(),
        };
        let task = match how {
            Fetcher::Segmented => tokio::spawn(crate::stream::download(spec, tx, stop.clone())),
            Fetcher::Merged(streams) => {
                tokio::spawn(crate::stream::merge(spec, streams, tx, stop.clone()))
            }
            Fetcher::Direct => tokio::spawn(fetch::download(spec, tx, stop.clone())),
            Fetcher::Held(held) => {
                let (response, probe) = *held;
                tokio::spawn(fetch::download_open(spec, response, probe, tx, stop.clone()))
            }
        };

        self.fetch_jobs.lock().unwrap().insert(
            d.id,
            FetchState {
                downloaded: 0,
                total: d.total_bytes,
                speed: 0,
                connections: 0,
                output: None,
                stop,
                stopped_by_us: false,
                task,
            },
        );
        self.store.set_status(d.id, Status::Active, None)?;
        log::info!("#{} -> {} ({})", d.id, label, d.filename);

        // Fold the fetcher's events into the same state the poll loop reads.
        let id = d.id;
        let weak = self.me.read().unwrap().clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let Some(engine) = weak.upgrade() else { break };
                let mut jobs = engine.fetch_jobs.lock().unwrap();
                let Some(state) = jobs.get_mut(&id) else { break };
                match event {
                    fetch::Event::Progress(p) => {
                        state.downloaded = p.downloaded as i64;
                        if let Some(total) = p.total {
                            state.total = total as i64;
                        }
                        state.speed = p.speed as i64;
                        state.connections = p.connections as i64;
                    }
                    fetch::Event::Done(path) => state.output = Some(path),
                    fetch::Event::Probed(probe) => {
                        if let Some(total) = probe.size {
                            state.total = total as i64;
                        }
                    }
                    fetch::Event::Concurrency { connections, speed } => {
                        log::info!(
                            "#{id}: settled on {connections} connections at {}/s",
                            crate::human_bytes(speed as i64)
                        );
                    }
                }
            }
        });
        Ok(())
    }

    /// Reap finished fetcher jobs and mirror their progress into the store.
    async fn reconcile_fetch(&self) -> Result<()> {
        // Live counters first, without holding the lock across an await.
        let live: Vec<(i64, i64, i64, i64, i64)> = {
            let jobs = self.fetch_jobs.lock().unwrap();
            jobs.iter()
                .map(|(id, s)| (*id, s.downloaded, s.total, s.speed, s.connections))
                .collect()
        };
        for (id, downloaded, total, speed, connections) in live {
            self.store.update_progress(id, total, downloaded, None, None)?;
            self.live.lock().unwrap().insert(id, (speed, connections));
        }

        // Taken out from under the lock, then awaited: the tasks have already
        // finished, so each await resolves immediately, but holding a mutex
        // across one is how the poll loop would deadlock against the event
        // pump that also wants it.
        let done: Vec<(i64, bool, tokio::task::JoinHandle<Result<PathBuf>>)> = {
            let mut jobs = self.fetch_jobs.lock().unwrap();
            let ids: Vec<i64> = jobs
                .iter()
                .filter(|(_, s)| s.task.is_finished())
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| jobs.remove(&id).map(|s| (id, s.stopped_by_us, s.task)))
                .collect()
        };

        for (id, stopped, task) in done {
            let result = match task.await {
                Ok(result) => result,
                Err(e) if e.is_cancelled() => continue,
                Err(e) => Err(anyhow::anyhow!("the fetcher panicked: {e}")),
            };
            let Some(mut d) = self.store.get(id)? else { continue };
            if stopped {
                // Paused on purpose: the partial file and its state stay, and
                // the row already says Paused.
                self.live.lock().unwrap().remove(&id);
                continue;
            }
            match result {
                Ok(path) => {
                    if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
                        let size = std::fs::metadata(&path)
                            .map(|m| m.len() as i64)
                            .unwrap_or(d.total_bytes);
                        let dir = path
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| d.directory.clone());
                        self.store
                            .update_progress(id, size, size, Some(&name), Some(&dir))?;
                        d.filename = name;
                        d.directory = dir;
                        d.total_bytes = size;
                        d.completed_bytes = size;
                    }
                    self.on_complete(&d).await?;
                }
                Err(e) => self.on_failure(&d, &format!("{e:#}")).await?,
            }
        }
        Ok(())
    }

    /// The streams to fetch ourselves, when this job is one we can finish
    /// without yt-dlp downloading anything.
    ///
    /// `None` is the ordinary answer for half the sites in the world and is
    /// not a failure: it means "let yt-dlp do it", which is what happened
    /// before this existed. Nothing here is allowed to fail a download — a
    /// planner that cannot answer simply declines.
    async fn native_plan(
        &self,
        d: &Download,
        format: &str,
        settings: &Settings,
    ) -> Option<Vec<crate::stream::Stream>> {
        let resolve = |expression: String| {
            let url = d.url.clone();
            let cookies = settings.ytdlp_cookies_from.clone();
            let extra = settings.ytdlp_extra_args.clone();
            async move { ytdlp::plan(&url, &expression, Some(cookies.as_str()), &extra).await }
        };

        let plan = match resolve(format.to_string()).await {
            Ok(plan) => plan,
            Err(e) => {
                // Whatever went wrong here will go wrong again in a moment,
                // in the download, which is the place that reports it.
                log::debug!("#{}: could not resolve the formats ({e:#}); leaving it to yt-dlp", d.id);
                return None;
            }
        };

        let plan = if plan.native() {
            plan
        } else if ytdlp::can_merge() {
            // There is something to fall back on, so fall back: yt-dlp with
            // ffmpeg behind it merges what this cannot, and that selection
            // is the one that was asked for.
            log::info!("#{}: {} — yt-dlp will download it", d.id, plan.describe());
            return None;
        } else {
            // Nothing to fall back *to*. What usually lands a selection here
            // is a picture and a sound from different container families,
            // which neither muxer will write into one file — but a site in
            // that position nearly always offers a matched pair as well, and
            // a matched pair is one MDM can finish alone. Slightly different
            // bytes beat a download that cannot happen.
            match resolve(ytdlp::MATCHED_FAMILIES.to_string()).await {
                Ok(second) if second.native() => {
                    log::info!(
                        "#{}: {} would need an ffmpeg this machine has not got; taking {} instead",
                        d.id,
                        plan.describe(),
                        second.describe()
                    );
                    second
                }
                _ => {
                    log::info!("#{}: {} — yt-dlp will download it", d.id, plan.describe());
                    return None;
                }
            }
        };

        // Some CDNs sign the resolved addresses to the session that asked for
        // them, so a replay from another request is refused before a byte
        // lands. TikTok's rungs come off `*-webapp-prime.tiktok.com` carrying
        // `tk=tt_chain_token` — the challenge cookie — and answer a plain
        // fetch of the URL that just resolved them with 403 every time.
        // Fetching such a plan here would spend a retry (and show the user a
        // failure) on an attempt that can never become a download, so the plan
        // stays with the tool whose request the address was issued to.
        if plan.tracks.iter().any(|t| Self::cdn_signs_to_session(&t.url)) {
            log::info!(
                "#{}: {} — its addresses are session-signed; yt-dlp will download it",
                d.id,
                plan.describe()
            );
            return None;
        }

        // The name is settled here, before a byte lands, rather than
        // whenever yt-dlp gets round to announcing one — so the row stops
        // showing a bare video id immediately, and the file is written
        // under the name it will keep. What the user typed in the picker
        // wins over the site's own title; a yt-dlp row carries that in
        // `output_name`, because for that path the filename column is only
        // ever a placeholder.
        let chosen = d
            .output_name
            .clone()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| plan.title.trim().to_string());
        if !chosen.is_empty() {
            // A lone stream is saved as it arrives; a merge comes out of
            // the muxer in the family it went in as.
            let ext = match plan.tracks.as_slice() {
                [only] if !only.ext.is_empty() => only.ext.clone(),
                many if many.iter().all(|t| t.ext == "webm") => "webm".to_string(),
                _ => "mp4".to_string(),
            };
            let named = sanitize(chosen);
            let named = match named.rsplit_once('.') {
                Some((stem, had)) if had.eq_ignore_ascii_case(&ext) => format!("{stem}.{ext}"),
                _ => format!("{named}.{ext}"),
            };
            // Never over something already there: this path writes a real
            // file at a real name, unlike the yt-dlp one it replaces, so it
            // inherits the same duty not to lose a download to a collision.
            match self.free_filename(Path::new(&d.directory), &named, d.id) {
                Ok(free) => {
                    if let Err(e) = self.store.set_filename(d.id, &free) {
                        log::warn!("#{}: could not name it {free}: {e:#}", d.id);
                    }
                }
                Err(e) => log::warn!("#{}: could not settle on a name: {e:#}", d.id),
            }
        }
        // What the whole job weighs, before a byte of it has arrived.
        if let Some(size) = plan.size() {
            let _ = self.store.update_progress(d.id, size as i64, d.completed_bytes, None, None);
        }

        log::info!(
            "#{}: {} — fetching and merging it here, no ffmpeg needed",
            d.id,
            plan.describe()
        );
        Some(
            plan.tracks
                .into_iter()
                .map(|t| crate::stream::Stream {
                    url: t.url,
                    headers: t.headers,
                    size: t.filesize,
                    ext: t.ext,
                })
                .collect(),
        )
    }

    async fn dispatch_ytdlp(&self, d: &Download, job: &Job) -> Result<()> {
        if !ytdlp::available() {
            bail!(
                "yt-dlp is not installed — install it with: {}",
                crate::distro::install("yt-dlp")
            );
        }
        let settings = self.settings();
        let format = job
            .format_id
            .clone()
            .unwrap_or_else(|| settings.ytdlp_format.clone());

        // A download that was once refused at the fetch is never offered the
        // native path again. `no_native` is set from the first refusal of
        // that shape — see `on_failure` — so the retry goes straight to
        // yt-dlp, which is the only request that carries the session a
        // signed CDN address was issued to.
        if !d.no_native {
            // Ask what that expression actually names before handing the job
            // over. Where every stream it picked is a plain HTTPS file in an
            // MP4-family container, MDM can do the whole download itself: fetch
            // them with its own connections, resume and progress, and rebuild
            // them with its own muxer — which is the entirety of what ffmpeg was
            // being carried for. Anything else, WebM sound or a fragment list or
            // a live stream, stays yt-dlp's to download.
            //
            // Selection itself is left to yt-dlp. A format selector is a small
            // language with years of behaviour behind it, and reimplementing it
            // would be precisely the borrowed rot this arrangement avoids.
            if let Some(streams) = self.native_plan(d, &format, &settings).await {
                // Re-read: the planner has just given the row the video's own
                // title, and the file has to be written under that rather than
                // under whatever the URL's last path segment was.
                let named = self.store.get(d.id)?.unwrap_or_else(|| d.clone());
                return self
                    .dispatch_fetch(&named, &settings, Fetcher::Merged(streams))
                    .await;
            }
        }

        let (tx, mut rx) = mpsc::channel(16);
        let handle = ytdlp::download(
            &d.url,
            Path::new(&d.directory),
            &format,
            settings.connections,
            &d.headers,
            d.output_name.as_deref(),
            Some(settings.ytdlp_cookies_from.as_str()),
            &settings.ytdlp_extra_args,
            tx,
        )
        .await?;

        self.ytdlp_jobs.lock().unwrap().insert(
            d.id,
            YtState {
                // The picker weighs the formats it offers, and that
                // figure is on the row before yt-dlp is started; a
                // job that arrived without one starts at nothing and
                // learns its weight a stream at a time.
                tally: Tally { total: d.total_bytes, ..Tally::default() },
                speed: 0,
                child: handle.child,
                last_error: handle.last_error,
                output: None,
                title: None,
                title_applied: false,
                connections: 0,
                stopped_by_us: false,
                skipped: None,
            },
        );
        self.store.set_status(d.id, Status::Active, None)?;

        // Fold yt-dlp's progress lines into the same state the poll loop reads.
        let id = d.id;
        let weak = self.me.read().unwrap().clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let Some(engine) = weak.upgrade() else { break };
                // Bound rather than left as a temporary: as the block's tail
                // expression the guard would outlive `engine`, which it borrows.
                let mut jobs = engine.ytdlp_jobs.lock().unwrap();
                if let Some(state) = jobs.get_mut(&id) {
                    match event {
                        ytdlp::Event::Progress(p) => {
                            state.tally.advance(p.downloaded, p.total);
                            state.speed = p.speed;
                            state.connections = p.connections;
                        }
                        ytdlp::Event::Finished(path) => state.output = Some(path),
                        ytdlp::Event::Title(title) => state.title = Some(title),
                        ytdlp::Event::Skipped(name) => state.skipped = Some(name),
                    }
                }
                drop(jobs);
            }
        });

        Ok(())
    }

    /* ------------------------------------------------------------------ *
     * Poll loop
     * ------------------------------------------------------------------ */

    async fn tick(&self) -> Result<()> {
        self.reconcile_ytdlp().await?;
        self.reconcile_fetch().await?;
        self.run_scheduler().await?;
        self.broadcast().await;
        Ok(())
    }


    /// Reap finished yt-dlp children and mirror their progress into the store.
    async fn reconcile_ytdlp(&self) -> Result<()> {
        let mut finished: Vec<Exit> = Vec::new();
        {
            let mut jobs = self.ytdlp_jobs.lock().unwrap();
            for (id, state) in jobs.iter_mut() {
                let exit = |status| Exit {
                    id: *id,
                    status,
                    error: state.last_error.lock().unwrap().clone(),
                    output: state.output.clone(),
                    stopped: state.stopped_by_us,
                    skipped: state.skipped.clone(),
                };
                match state.child.try_wait() {
                    Ok(Some(status)) => finished.push(exit(Some(status))),
                    Ok(None) => {}
                    Err(_) => finished.push(exit(None)),
                }
            }
            for exit in &finished {
                jobs.remove(&exit.id);
            }
        }

        // A title that arrived since the last tick replaces the URL-derived
        // placeholder, so the row is recognisable while it downloads.
        let titles: Vec<(i64, String)> = {
            let mut jobs = self.ytdlp_jobs.lock().unwrap();
            jobs.iter_mut()
                .filter(|(_, s)| !s.title_applied && s.title.is_some())
                .map(|(id, s)| {
                    s.title_applied = true;
                    (*id, s.title.clone().unwrap_or_default())
                })
                .collect()
        };
        for (id, title) in titles {
            self.store.set_filename(id, &sanitize(title))?;
        }

        // Mirror live counters without holding the lock across an await.
        let live: Vec<(i64, i64, i64, i64, i64)> = {
            let jobs = self.ytdlp_jobs.lock().unwrap();
            jobs.iter()
                .map(|(id, s)| (*id, s.tally.downloaded, s.tally.total, s.speed, s.connections))
                .collect()
        };
        for (id, downloaded, total, speed, connections) in live {
            self.store.update_progress(id, total, downloaded, None, None)?;
            self.live.lock().unwrap().insert(id, (speed, connections));
        }

        for Exit { id, status, error, output, stopped, skipped } in finished {
            let Some(mut d) = self.store.get(id)? else { continue };
            if stopped {
                // Paused on purpose. The row already says so, and the partial
                // fragments are picked up again when it resumes.
                self.live.lock().unwrap().remove(&id);
                continue;
            }
            match status {
                // yt-dlp reports a file it decided not to re-download exactly
                // as it reports one it has just written, and exits a success
                // either way — which is how an audio-only pick after the video
                // of the same page used to land "complete" holding the video,
                // both being `.webm` under one stem. Another copy is what was
                // asked for, so it is given a number and downloaded, rather
                // than answered with a file it never fetched.
                Some(s) if s.success() && reused_existing(&output, &skipped) => {
                    let existing = skipped.unwrap_or_default();
                    let attempts = {
                        let mut r = self.retries.lock().unwrap();
                        let n = r.entry(id).or_insert(0);
                        *n += 1;
                        *n
                    };
                    // The numbering only ever picks a name nothing holds, so
                    // colliding again means something outside is filling the
                    // folder faster than we can name files in it. Stop rather
                    // than spin.
                    if attempts > 3 {
                        let message = format!(
                            "{existing} keeps getting in the way — nothing was \
                             downloaded. Save it under a different name."
                        );
                        log::warn!("#{id} downloaded nothing: {message}");
                        self.store.set_status(id, Status::Failed, Some(&message))?;
                        self.live.lock().unwrap().remove(&id);
                        continue;
                    }
                    let stem = Path::new(&existing)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| existing.clone());
                    // Its own claim on the name it is being moved off must
                    // not be what stops it moving.
                    let free = self.free_stem(Path::new(&d.directory), &stem, id)?;
                    log::info!("#{id}: {existing} was already there — downloading it again as {free}");
                    self.store.set_output_name(id, &free)?;
                    d.output_name = Some(free);
                    let job = job_from(&d);
                    if let Err(e) = self.dispatch(&d, &job).await {
                        self.on_failure(&d, &format!("{e:#}")).await?;
                    }
                }
                Some(s) if s.success() => {
                    // Adopt yt-dlp's own name, container and byte count. Until
                    // now the row carried a guess derived from the page URL,
                    // which would leave "Open file" pointing at nothing.
                    if let Some(path) = output {
                        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
                        {
                            let dir = path
                                .parent()
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_else(|| d.directory.clone());
                            let size = std::fs::metadata(&path)
                                .map(|m| m.len() as i64)
                                .unwrap_or(d.total_bytes);
                            self.store.update_progress(
                                id,
                                size,
                                size,
                                Some(&name),
                                Some(&dir),
                            )?;
                            let category = categories::categorize(&name, &d.mime);
                            self.store.set_category(id, category)?;
                            d.filename = name;
                            d.directory = dir;
                            d.total_bytes = size;
                            d.completed_bytes = size;
                            d.category = category.to_string();
                            // An audio-only pick was still filed under Video,
                            // since the container is only settled after muxing.
                            self.refile(&mut d)?;
                        }
                    }
                    self.on_complete(&d).await?
                }
                Some(s) => {
                    // Prefer yt-dlp's own words; "exit status 1" helps nobody.
                    let message =
                        error.unwrap_or_else(|| format!("yt-dlp exited with {s}"));
                    self.on_failure(&d, &message).await?
                }
                None => self.on_failure(&d, "yt-dlp could not be reaped").await?,
            }
        }
        Ok(())
    }

    /// Move a finished file into the folder its real type calls for.
    ///
    /// Only ever a rename within the download root, and never over an existing
    /// file — losing someone's download to a name collision is unforgivable.
    fn refile(&self, d: &mut Download) -> Result<()> {
        let settings = self.settings();
        if !settings.categorize {
            return Ok(());
        }
        let wanted = Path::new(&settings.download_dir).join(&d.category);
        if Path::new(&d.directory) == wanted {
            return Ok(());
        }
        let from = d.full_path();
        if !from.is_file() {
            return Ok(());
        }
        let to = wanted.join(&d.filename);
        if to.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&wanted)
            .with_context(|| format!("creating {}", wanted.display()))?;
        match std::fs::rename(&from, &to) {
            Ok(()) => {
                let dir = wanted.to_string_lossy().into_owned();
                self.store
                    .update_progress(d.id, d.total_bytes, d.completed_bytes, None, Some(&dir))?;
                d.directory = dir;
                log::info!("#{} refiled into {}", d.id, d.category);
            }
            // A cross-device move would need a copy; not worth it, the file is
            // already downloaded and usable where it is.
            Err(e) => log::warn!("could not refile #{}: {e}", d.id),
        }
        Ok(())
    }

    async fn on_complete(&self, d: &Download) -> Result<()> {
        self.store.set_status(d.id, Status::Complete, None)?;
        self.live.lock().unwrap().remove(&d.id);
        self.retries.lock().unwrap().remove(&d.id);
        self.retry_after.lock().unwrap().remove(&d.id);
        log::info!("#{} complete: {}", d.id, d.filename);

        let settings = self.settings();
        if settings.checksum {
            let path = d.full_path();
            let id = d.id;
            let store = self.store.clone();
            // Hashing a large file is CPU-bound and must not stall the loop.
            tokio::task::spawn_blocking(move || match crate::checksum::sha256_file(&path) {
                Ok(sum) => {
                    let _ = store.set_sha256(id, &sum);
                }
                Err(e) => log::warn!("checksum for #{id} failed: {e:#}"),
            });
        }
        if settings.notify {
            notify("Download complete", &d.filename);
        }
        Ok(())
    }

    /// Did the download's own fetcher get turned away by the server?
    ///
    /// Matches the status the stream downloader reports when a CDN refuses a
    /// request that reached it, which it writes as `"{url} answered {status}"`
    /// or `"connection {slot} answered {status}"`. `401`, `403` and `429` are
    /// the refusals that mean *this* request is not welcome — the URL is a
    /// "who" problem (signed to a session only the extractor's own request
    /// carries, throttled, or challenged), not a "what" problem a retry of
    /// the same shape would fix. A 404, by contrast, says the address itself
    /// is dead; the retry earns its keep there too, but it is for yt-dlp to
    /// re-find it, and the native path is no surer than it was.
    fn refused_by_server(message: &str) -> bool {
        const REFUSED: &[u16] = &[401, 403, 429];
        message
            .split("answered ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|code| code.parse::<u16>().ok())
            .is_some_and(|status| REFUSED.contains(&status))
    }

    /// Media CDN hosts whose resolved addresses are bound to the session that
    /// asked for them — carrying the challenge cookie only that request has —
    /// so replays from another request never succeed.
    ///
    /// The native path's pre-plan resolves a format to its addresses and then
    /// downloads the address with a fresh, cookie-less connection. That is a
    /// sound plan for an ordinary signed URL (expiry, fingerprint) but not for
    /// a session-bound one: the CDN answers 403 before a byte lands. Rather
    /// than burn a retry on such addresses every run, the plan is declined up
    /// front and the whole download stays with the request that was issued
    /// them. Three-letter insight: `v19-webapp-prime.tiktok.com` today.
    fn cdn_signs_to_session(url: &str) -> bool {
        const SIGNED_CDN_HOSTS: &[&str] = &["tiktok.com", "tiktokcdn.com"];
        let host = host_of(url).trim_start_matches("www.").to_lowercase();
        SIGNED_CDN_HOSTS
            .iter()
            .any(|s| host == *s || host.ends_with(&format!(".{s}")))
    }

    async fn on_failure(&self, d: &Download, message: &str) -> Result<()> {
        let settings = self.settings();
        // Said in the user's terms rather than the tool's. A Python transport
        // traceback in a red strip under a progress bar reads as "the app is
        // broken"; "the DNS lookup failed, check your connection" is the same
        // fact and can be acted on.
        // Classified on what the tool said, phrased in what the user needs:
        // rewriting first would mean deciding "is this worth retrying?" about
        // a sentence we wrote ourselves.
        // Read before the message is rewritten: the marker is in the raw text,
        // and `plain_error` is under no obligation to keep it.
        let was_page = crate::fetch::is_page_response(message);
        let refused = Self::refused_by_server(message);

        // A page found where the browser had *watched a file arrive* is not a
        // page an extractor can read. It is a link that has been spent.
        //
        // File hosts hand out one-time addresses, and the capture sits at
        // `onHeadersReceived` — by the time it can divert anything the server
        // has already committed the file to the browser's request. Cancelling
        // that request and asking again asks for a token the server has
        // finished with, and what comes back is the landing page it hands
        // anyone without one. Confirmed by hand against the download in the
        // bug report: the capture recorded `video/webm`, 2.1 GB, status 200,
        // and the same GET with the same cookies now answers 200 `text/html`.
        //
        // Everything that used to happen next was wrong. yt-dlp was handed a
        // file host it has no extractor for, so five retries went by and the
        // row settled on "No extractor for filekeeper.net" — a true sentence
        // about the wrong tool, blaming the site for a link MDM had spent
        // itself.
        let spent = was_page && crate::fetch::capture_saw_a_file(d.total_bytes, &d.mime);

        let permanent = crate::ytdlp::is_permanent_error(message) || spent;
        let message = &if spent {
            // Remembered, not merely reported. The first download from a host
            // like this is lost however it is handled — the address was spent
            // by the browser's own request, before MDM was told the download
            // existed — so the only thing left to get right is the *next* one,
            // and telling the user to go and edit a list in the extension made
            // that their job. The engine knows the host, and refusing the next
            // capture from it is enough: the extension hands a download it
            // cannot place back to the browser, which still has an unspent
            // click to make the request with.
            let site = host_of(&d.url);
            self.stop_capturing(&site).await;
            let site = if site.is_empty() { "That site".to_string() } else { site };
            format!(
                "{site} served this file once and now answers with a page — that \
                 link was single-use. MDM will ask {site} before the browser does \
                 from now on, so the next link is not spent before it can be \
                 used; download it again."
            )
        } else if was_page {
            // The marker exists for this branch, not for the user.
            "that address is a web page, not a file — trying an extractor".to_string()
        } else {
            crate::ytdlp::plain_error(message)
        };

        // Two ways a URL turns out to be the extractor's job rather than ours,
        // both discovered by trying rather than guessed at up front:
        //
        //   * a manifest the stream downloader will not touch — encrypted,
        //     live, or a codec it cannot describe;
        //   * a URL that answered with a *page*. A site whose player sits at
        //     `view_video.php` is not on any host list we could keep current,
        //     and the response says what the address does not.
        //
        // Either way the row is handed to yt-dlp for the retry that is already
        // about to happen. Once only: the flag is persisted, so the next
        // attempt sees a row that has already moved.
        if !d.use_ytdlp
            && !spent
            && (was_page || crate::stream::looks_like_manifest(&d.url, &d.mime))
            && ytdlp::available()
        {
            log::info!(
                "#{}: {} — handing it to yt-dlp",
                d.id,
                if was_page {
                    "that URL is a page, not a file".to_string()
                } else {
                    format!("the built-in stream downloader could not take this ({message})")
                }
            );
            self.store.set_use_ytdlp(d.id, true)?;
        }

        // A yt-dlp-backed row whose *formats* MDM tried to fetch itself and the
        // server refused — 401, 403, 429 off the CDN. The resolved address is
        // bound to the session (and, on media like TikTok's `*-webapp-prime`
        // rungs, the challenge cookie) that only the extractor's own request
        // carries; a plain connection to the signed URL can never answer. The
        // retry is already about to happen, so `no_native` is the whole fix:
        // persisted, the next attempt skips the native fetch and goes out
        // through yt-dlp, which is the only path that can redeem the address.
        if d.use_ytdlp && !d.no_native && refused {
            log::info!(
                "#{}: the server refused MDM's own fetch ({message}) — the retry will use yt-dlp itself",
                d.id
            );
            self.store.set_no_native(d.id)?;
            // The native attempt is abandoned, so its scratch directory serves
            // no purpose but clutter in the user's download folder. The muxer
            // normally clears it on success only, so the refusal's leftover
            // has to be retired here.
            let dir = Path::new(&d.directory);
            let mut stems: Vec<&str> = Vec::new();
            if let Some(name) = d.output_name.as_deref() {
                if !name.trim().is_empty() {
                    stems.push(name.trim());
                }
            }
            if let Some(stem) = Path::new(&d.filename).file_stem().and_then(|s| s.to_str()) {
                stems.push(stem);
            }
            for stem in stems {
                let work = dir.join(format!("{stem}.mdmstream"));
                if work.is_dir() {
                    match std::fs::remove_dir_all(&work) {
                        Ok(()) => log::info!("#{}: cleared the abandoned native scratch {work:?}", d.id),
                        Err(e) => log::warn!("#{}: could not clear {work:?}: {e}", d.id),
                    }
                    break;
                }
            }
        }
        let attempts = {
            let mut r = self.retries.lock().unwrap();
            let n = r.entry(d.id).or_insert(0);
            *n += 1;
            *n
        };

        // A missing format or a private video fails identically every time;
        // retrying only delays telling the user what actually went wrong.
        if permanent {
            log::warn!("#{} failed permanently: {message}", d.id);
        }

        if !permanent && attempts <= settings.retry_limit {
            let wait = retry_backoff(attempts);
            log::warn!(
                "#{} failed ({message}); retry {attempts}/{} in {}s",
                d.id,
                settings.retry_limit,
                wait.as_secs()
            );
            if d.use_ytdlp {
                // Whatever extraction produced the URLs this attempt just
                // failed on, handing the retry that same cached extraction
                // would spend it on the identical signed URLs rather than
                // fresh ones — the one thing a retry is supposed to change.
                ytdlp::forget_info(&d.url);

                // A failure of this shape is the strongest evidence there
                // is that the extractor has fallen behind the site — better
                // evidence than any clock — so the daily check is brought
                // forward and the retry gets whatever it finds. It runs in
                // the background: the retry has its own wait, and a fix
                // that arrives during the attempt after this one is still a
                // fix nobody had to be told about.
                // Not while one is running: on Windows a program that is
                // executing cannot be replaced, and there is no hurry —
                // tomorrow's check, or the next failure, comes round soon
                // enough.
                let idle = self.ytdlp_jobs.lock().unwrap().is_empty();
                if idle && settings.ytdlp_auto_update && ytdlp::looks_out_of_date(message) {
                    tokio::spawn(async move {
                        crate::tools::force_check();
                        match crate::tools::update(true).await {
                            crate::tools::Outcome::Updated(version) => {
                                log::info!("yt-dlp updated to {version} after a failure that looked like a stale one");
                            }
                            crate::tools::Outcome::UpToDate => {
                                log::info!("yt-dlp is already current; that failure was not staleness");
                            }
                            crate::tools::Outcome::Failed(why) => {
                                log::warn!("could not update yt-dlp: {why}");
                            }
                        }
                    });
                }
            }
            self.retry_after
                .lock()
                .unwrap()
                .insert(d.id, std::time::Instant::now() + wait);

            // The reason is kept on the row rather than cleared. A queued row
            // that silently sits there for a minute looks stuck; saying which
            // attempt failed and that another is coming is the difference
            // between waiting and giving up on the app.
            let waiting = format!(
                "{message} — trying again in {}s ({attempts} of {})",
                wait.as_secs(),
                settings.retry_limit
            );
            self.store
                .set_status(d.id, Status::Queued, Some(&waiting))?;
            self.scheduler_held.lock().unwrap().remove(&d.id);
            return Ok(());
        }
        self.retry_after.lock().unwrap().remove(&d.id);

        self.store.set_status(d.id, Status::Failed, Some(message))?;
        self.live.lock().unwrap().remove(&d.id);
        if settings.notify {
            notify("Download failed", &format!("{}: {message}", d.filename));
        }
        Ok(())
    }

    /* ------------------------------------------------------------------ *
     * Controls
     * ------------------------------------------------------------------ */

    pub async fn pause(&self, id: i64) -> Result<()> {
        if self.store.get(id)?.is_none() {
            return Ok(());
        }
        if let Some(state) = self.ytdlp_jobs.lock().unwrap().get_mut(&id) {
            // yt-dlp has no pause; stopping is the honest equivalent, and the
            // partial fragments are reused when it restarts. Flag it first, so
            // the reaper reads the exit as intentional rather than as a crash
            // worth retrying.
            state.stopped_by_us = true;
            let _ = state.child.start_kill();
        }
        if let Some(state) = self.fetch_jobs.lock().unwrap().get_mut(&id) {
            // The fetcher has a real pause: every connection stops at its next
            // chunk, and the partial file plus its range state stay on disk,
            // so resuming asks only for what is still missing.
            state.stopped_by_us = true;
            state.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.store.set_status(id, Status::Paused, None)?;
        self.scheduler_held.lock().unwrap().remove(&id);
        self.broadcast().await;
        Ok(())
    }

    pub async fn resume(&self, id: i64) -> Result<()> {
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        // Every downloader MDM has left resumes from what is already on disk
        // rather than from a handle held by something else, so resuming is
        // simply dispatching again: the fetcher picks up from its range state,
        // the stream downloader from the segment it had reached, and yt-dlp
        // from its own partial fragments.
        let job = job_from(&d);
        self.store.set_status(id, Status::Queued, None)?;
        self.dispatch(&d, &job).await?;
        self.broadcast().await;
        Ok(())
    }

    /// Point a download that has not started yet at a different folder or name.
    ///
    /// Only before the first byte: once a downloader owns a partial file,
    /// moving the target underneath it would orphan what is already written.
    pub fn set_target(
        &self,
        id: i64,
        directory: Option<&str>,
        filename: Option<&str>,
    ) -> Result<()> {
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        if d.completed_bytes > 0 {
            return Ok(());
        }
        let directory = directory.map(str::trim).filter(|v| !v.is_empty());
        let filename = filename
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| sanitize(v.to_string()));

        if let Some(dir) = directory {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {dir}"))?;
        }

        // Whichever of the two the caller changed, the name has to be free in
        // the folder it is now going to — a capture confirmed under the name of
        // a file already saved there would otherwise be reported complete
        // without downloading anything.
        let wanted = filename.unwrap_or_else(|| d.filename.clone());
        let name = if d.use_ytdlp {
            // Not the name of anything yet: yt-dlp settles that itself.
            wanted
        } else {
            let dir = directory.unwrap_or(&d.directory);
            self.free_filename(Path::new(dir), &wanted, id)?
        };

        self.store.update_progress(
            id,
            d.total_bytes,
            d.completed_bytes,
            Some(&name),
            directory,
        )?;
        Ok(())
    }

    pub async fn retry(&self, id: i64) -> Result<()> {
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        self.retries.lock().unwrap().remove(&id);
        // Asked for by hand, so it goes now — whatever backoff an automatic
        // attempt was still sitting out is not the user's to wait through.
        self.retry_after.lock().unwrap().remove(&id);
        self.store.set_status(id, Status::Queued, None)?;
        let job = job_from(&d);
        let fresh = self.store.get(id)?.unwrap_or(d);
        self.dispatch(&fresh, &job).await?;
        self.broadcast().await;
        Ok(())
    }

    /// Remove a download, optionally deleting whatever was written so far.
    pub async fn remove(&self, id: i64, delete_file: bool) -> Result<()> {
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        if let Some(mut state) = self.ytdlp_jobs.lock().unwrap().remove(&id) {
            let _ = state.child.start_kill();
        }
        if let Some(state) = self.fetch_jobs.lock().unwrap().remove(&id) {
            state.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            state.task.abort();
        }
        // Bytes still waiting on a Start that is never coming.
        let _ = std::fs::remove_file(blob_stash(id));
        if delete_file {
            let path = d.full_path();
            let _ = std::fs::remove_file(&path);
            // The fetcher's partial file and its range state, which live
            // beside the target under a suffix rather than in place of it.
            let mut part = path.clone().into_os_string();
            part.push(fetch::PART_SUFFIX);
            let part = PathBuf::from(part);
            let _ = std::fs::remove_file(&part);
            let mut state = part.into_os_string();
            state.push(".state");
            let _ = std::fs::remove_file(PathBuf::from(state));

        }
        self.store.delete(id)?;
        self.live.lock().unwrap().remove(&id);
        self.retries.lock().unwrap().remove(&id);
        self.retry_after.lock().unwrap().remove(&id);
        self.broadcast().await;
        Ok(())
    }

    pub async fn pause_all(&self) -> Result<()> {
        for d in self.store.by_status(Status::Active)? {
            let _ = self.pause(d.id).await;
        }
        Ok(())
    }

    pub async fn resume_all(&self) -> Result<()> {
        for d in self.store.by_status(Status::Paused)? {
            let _ = self.resume(d.id).await;
        }
        Ok(())
    }

    pub fn clear_finished(&self) -> Result<usize> {
        self.store.clear_finished()
    }

    /* ------------------------------------------------------------------ *
     * Settings
     * ------------------------------------------------------------------ */

    /// Is this URL's host one that has already proved its links are one-shot?
    ///
    /// Asked of an ordinary capture on its way in. By then the browser has
    /// made the request, so the address has already been answered once and
    /// MDM's own would get the landing page; refusing hands the download back
    /// to the browser, which is where it can still succeed. Returns the host
    /// so the refusal can name it.
    ///
    /// A *pre-empted* capture is the opposite case and never asks this: there
    /// the request has not been made yet, and the same list is what says to
    /// make it here rather than let the browser spend it. See [`Self::preempt`].
    pub fn single_use_host(&self, url: &str) -> Option<String> {
        single_use_match(url, &self.settings().single_use_hosts)
    }

    /// Take a download the browser has not asked for yet.
    ///
    /// The ordinary capture watches a response go by and then asks for the
    /// same file again. On a host that spends its links that second request is
    /// the landing page, which is why those hosts end up on
    /// `single_use_hosts` and their downloads are left to the browser. This is
    /// the way back in: the extension holds the request *before* it is sent
    /// and asks here, so the one answer the address has is ours.
    ///
    /// Which means this has to decide, from that single response, something
    /// the extension could not know when it held the request: whether the
    /// address was a download at all. A file host's own pages sit on the same
    /// host as its links, and cancelling a click on one of those would leave
    /// the tab showing nothing. So a page comes back as `Ok(None)` — the
    /// browser is still holding its request and goes ahead with it — and
    /// anything else keeps the connection and becomes a download on it.
    ///
    /// Nothing about this is a guess that could cost the user the file: every
    /// answer other than `Ok(Some)` leaves the browser exactly where it was.
    pub async fn preempt(&self, mut job: Job) -> Result<Option<i64>> {
        let settings = self.settings();
        let mut spec = fetch::Spec::new(job.url.clone(), PathBuf::new());
        spec.headers = job.headers.clone();
        spec.referrer = Some(job.referrer.clone()).filter(|r| !r.is_empty());
        spec.proxy = settings.proxy.clone();
        if let Some(header) = authorization_for(&job.url, &settings) {
            if !spec.headers.iter().any(|h| h.name.eq_ignore_ascii_case("authorization")) {
                spec.headers.push(header);
            }
        }

        let Some((response, probe)) = fetch::open(&spec).await? else {
            log::info!("{} is a page; letting the browser have it", job.url);
            return Ok(None);
        };

        // The extension saw no response — it held the request before one
        // existed — so everything that names and sizes this download comes off
        // the headers in hand.
        if job.filename.trim().is_empty() {
            job.filename = probe.filename.clone();
        }
        if job.size < 0 {
            job.size = probe.size.map(|s| s as i64).unwrap_or(-1);
        }
        if job.mime.trim().is_empty() {
            job.mime = probe.mime.clone();
        }
        // Not a page to extract from, whatever the host looks like: the file
        // is on the end of a connection that is already open, and putting an
        // extractor in front of it would ask the address a second question it
        // has no answer left for.
        job.use_ytdlp = Some(false);
        // The row is made before a byte is written, but it is not left waiting
        // for a Start button the way an ordinary capture is: the connection is
        // open and the server is not going to hold it while somebody decides.
        job.start_paused = true;
        let id = self.submit(job).await?;
        let Some(d) = self.download(id) else {
            bail!("the row for this capture went missing before it could start");
        };
        // `submit` hands back an existing row for a URL already being fetched.
        // Starting a second fetcher on it would write two copies into one
        // file, so the response is dropped instead and the running one stands.
        if d.status == Status::Active {
            log::info!("#{id} is already running; dropping the held connection");
            return Ok(Some(id));
        }
        self.dispatch_fetch(&d, &settings, Fetcher::Held(Box::new((response, probe))))
            .await?;
        Ok(Some(id))
    }

    /// Record a host whose addresses are good for one request.
    ///
    /// What the entry means depends on when it is read. To an ordinary capture
    /// it says "leave this one alone" — the link is already spent. To the
    /// extension it says the opposite: hold the *next* request to this host
    /// before the browser sends it, and let MDM make it. Both readings come
    /// from the same fact, which is why they come from the same list.
    ///
    /// Additive and idempotent, and it goes through `update_settings` so it is
    /// written to disk and reaches the open window like any other change —
    /// this list is shown in Settings, and a host that landed on it wrongly
    /// has to be visible before it can be taken off again.
    async fn stop_capturing(&self, host: &str) {
        if host.is_empty() {
            return;
        }
        let host = host.to_ascii_lowercase();
        let mut settings = self.settings();
        if settings.single_use_hosts.iter().any(|h| host_matches(&host, h)) {
            return;
        }
        log::info!("{host} serves single-use links; captures from it stay with the browser");
        settings.single_use_hosts.push(host);
        if let Err(e) = self.update_settings(settings).await {
            log::warn!("could not record the single-use host: {e:#}");
        }
    }

    pub async fn update_settings(&self, new: Settings) -> Result<()> {
        crate::config::save(&new)?;
        *self.settings.write().unwrap() = new;

        // Everything applies live now: the fetcher and the stream downloader
        // both read the settings at dispatch, so there is nothing to push at a
        // daemon and nothing that has to wait for a restart.
        self.broadcast().await;
        Ok(())
    }


    /* ------------------------------------------------------------------ *
     * Scheduler
     * ------------------------------------------------------------------ */

    /// Is `queue` inside its permitted window right now?
    fn queue_window_open(&self, queue: &str) -> Result<bool> {
        let Some(q) = self.store.queues()?.into_iter().find(|q| q.name == queue) else {
            return Ok(true);
        };
        Ok(queue_open_at(&q, local_minute_of_day(), local_weekday()))
    }

    /// Start or park downloads as scheduled windows open and close.
    async fn run_scheduler(&self) -> Result<()> {
        for q in self.store.queues()? {
            let open = queue_open_at(&q, local_minute_of_day(), local_weekday());

            if open {
                // Dispatch anything parked, up to the queue's own limit.
                let running = self
                    .store
                    .by_status(Status::Active)?
                    .into_iter()
                    .filter(|d| d.queue == q.name)
                    .count();
                let slots = (q.max_concurrent as usize).saturating_sub(running);
                if slots == 0 {
                    continue;
                }
                for d in self.store.next_queued(&q.name, slots as i64)? {

                    // A failed attempt is holding this row back deliberately;
                    // dispatching it now would spend the retry inside the same
                    // outage that just consumed the last one.
                    if self
                        .retry_after
                        .lock()
                        .unwrap()
                        .get(&d.id)
                        .is_some_and(|t| *t > std::time::Instant::now())
                    {
                        continue;
                    }
                    if self.fetch_jobs.lock().unwrap().contains_key(&d.id) {
                        // The fetcher's equivalent of holding a gid. Without
                        // this a row still winding down from a closed window
                        // would be dispatched a second time, and two sets of
                        // connections would write the same file.
                        continue;
                    }
                    let job = job_from(&d);
                    if let Err(e) = self.dispatch(&d, &job).await {
                        log::error!("scheduled dispatch of #{} failed: {e:#}", d.id);
                        self.store
                            .set_status(d.id, Status::Failed, Some(&format!("{e:#}")))?;
                    } else {
                        self.scheduler_held.lock().unwrap().remove(&d.id);
                    }
                }
            } else {
                // Park anything running in this queue until the window reopens.
                for d in self.store.by_status(Status::Active)? {
                    if d.queue != q.name {
                        continue;
                    }
                    // A fetcher job has to actually be told, or the window
                    // closes on paper while sixteen connections carry on
                    // downloading through it.
                    if let Some(state) = self.fetch_jobs.lock().unwrap().get_mut(&d.id) {
                        state.stopped_by_us = true;
                        state.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.store.set_status(d.id, Status::Queued, None)?;
                    self.scheduler_held.lock().unwrap().insert(d.id);
                }
            }
        }
        Ok(())
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

    /* ------------------------------------------------------------------ *
     * State broadcast
     * ------------------------------------------------------------------ */

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

/// Does any of `names` already belong to this stem?
///
/// Everything yt-dlp derives from a stem begins with `stem.` — `stem.webm`,
/// the `stem.f251.webm.part` of a download under way, the `stem.temp.mp4` of
/// one being muxed — and a download that has not written a byte yet holds the
/// bare stem. Any of them means the name is spoken for.
pub fn stem_taken(names: &[String], stem: &str) -> bool {
    names.iter().any(|name| {
        name == stem
            || name
                .strip_prefix(stem)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// `("archive", ".zip")`, or `("plain", "")`. The dot travels with the
/// extension, so a name without one needs no special case when they are
/// joined back together.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        // A leading dot makes a hidden file, not an extension.
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// The first of `file.iso`, `file_2.iso`, `file_3.iso` … that `taken` does not
/// claim.
///
/// The number goes before the extension, where it belongs: a `.iso` that
/// becomes `.iso_2` stops being an ISO as far as everything else is concerned.
pub fn unique_filename(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = split_extension(name);
    let free = unique_name(stem, |candidate| taken(&format!("{candidate}{ext}")));
    format!("{free}{ext}")
}

/// The first of `name`, `name_2`, `name_3` … that `taken` does not claim.
///
/// What counts as taken differs by caller — a file on disk, a name another
/// download has reserved, or both — so it is asked rather than assumed.
pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    // Bounded: a predicate that answers yes to everything must not be allowed
    // to spin, and a timestamp is unique enough to end the argument.
    (2..1000)
        .map(|n| format!("{name}_{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| format!("{name}_{}", now()))
}

/// Make a name from a server, a page title or the user safe to write to disk.
///
/// The last path segment only: a name like `../../.bashrc` would otherwise
/// write outside the folder the user chose.
///
/// The characters Windows refuses — `< > : " | ? *` — go with it, along with
/// trailing dots and spaces (which Windows silently drops when it creates the
/// file, leaving a name we would never find again) and the DOS device names it
/// still reserves. A video titled "To Rescue a Sinner Like Me | Quennie
/// Benabaye (Cover)" is otherwise refused outright, before a byte is fetched.
/// The same rules apply on every platform rather than behind a `cfg`: a folder
/// is shared, synced and moved, and a name only one system can hold is a name
/// the download cannot keep.
pub fn sanitize(name: String) -> String {
    /// Names MS-DOS gave to devices, which Windows will not let a file take —
    /// with or without an extension, in any case.
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5",
        "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5",
        "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    /// Long enough for any real title, short enough to leave room for the
    /// `.f251.webm.part` an extractor hangs off the stem.
    const LIMIT: usize = 200;

    let base = name.rsplit(['/', '\\']).next().unwrap_or("").to_string();
    let mut out: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    out = out
        .trim()
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' '])
        .to_string();
    if out.is_empty() {
        return "download".into();
    }
    if RESERVED.contains(&split_extension(&out).0.to_ascii_lowercase().as_str()) {
        out = format!("_{out}");
    }
    if out.len() > LIMIT {
        // Cut the stem, not the extension: a name that loses its `.mp4` is
        // filed under the wrong category and opens with the wrong program.
        // Cutting on a byte is not enough either — a title is as likely to be
        // Japanese as English, and half a character is not a name at all.
        let (stem, ext) = split_extension(&out);
        let ext = if ext.len() <= 20 { ext } else { "" };
        let mut cut = LIMIT.saturating_sub(ext.len()).min(stem.len());
        while cut > 0 && !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        out = format!("{}{ext}", stem[..cut].trim_end_matches(['.', ' ']));
        if out.is_empty() || out.starts_with('.') {
            return "download".into();
        }
    }
    out
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

/// The host part of a URL, lowercased, or empty where there is not one.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// Which listed host this URL falls under, if any.
///
/// Free of the engine so the decision can be tested as the decision it is: a
/// URL and a list in, a host or nothing out.
fn single_use_match(url: &str, hosts: &[String]) -> Option<String> {
    let host = host_of(url);
    if host.is_empty() {
        return None;
    }
    hosts
        .iter()
        .any(|pattern| host_matches(&host, pattern))
        .then_some(host)
}

/// Does `host` fall under `pattern` — the host itself, or a subdomain of it?
///
/// Deliberately the same rule as the extension's own site list, down to the
/// leading `*.` being optional, because the two lists are read as one thing by
/// anyone editing them: "sites MDM leaves alone".
fn host_matches(host: &str, pattern: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("*.").to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    host == pattern || host.ends_with(&format!(".{pattern}"))
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

pub fn filename_from_url(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|s| s.filter(|p| !p.is_empty()).next_back())
                .map(percent_decode)
        })
        .filter(|s| !s.is_empty())
        .map(|name| strip_page_extension(&name))
        .unwrap_or_else(|| "download".into())
}

/// Drop the extension when the last path segment names a *script* rather than
/// a file.
///
/// `view_video.php` is the address of a player page, and carrying its `.php`
/// into the download gives a row that claims an extension it will never have,
/// files itself under "Other" by that extension, and shows the user a name
/// that was never going to be the name. The real one arrives when the
/// extractor reports it; until then `view_video` is the honest half of what we
/// know.
fn strip_page_extension(name: &str) -> String {
    const PAGE_EXTENSIONS: &[&str] = &[
        "php", "php3", "php4", "php5", "asp", "aspx", "jsp", "jspx", "cgi",
        "pl", "do", "action", "html", "htm", "xhtml", "shtml",
    ];
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && PAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) =>
        {
            stem.to_string()
        }
        _ => name.to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// How long to wait before attempt number `attempt` is dispatched again.
///
/// The outages a retry exists for are measured in seconds to minutes — a DNS
/// server that stops answering, a wifi handover, a laptop waking up — so the
/// schedule has to span that. Back-to-back retries span nothing: five of them
/// against a resolver that is down finish faster than it takes to notice, and
/// the download fails over a network that came back a moment later. Capped so
/// a long outage still leaves the last attempt near enough to the failure to
/// be recognisable as part of it.
fn retry_backoff(attempt: u8) -> std::time::Duration {
    const SCHEDULE: &[u64] = &[2, 5, 15, 30, 60];
    let idx = usize::from(attempt.saturating_sub(1)).min(SCHEDULE.len() - 1);
    std::time::Duration::from_secs(SCHEDULE[idx])
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

/// A window may wrap past midnight (e.g. 23:00–06:00), which is exactly the
/// case an off-peak download schedule needs.
pub fn queue_open_at(q: &Queue, minute: u16, weekday: u8) -> bool {
    if !q.enabled {
        return false;
    }
    if !q.days.is_empty() && !q.days.contains(&weekday) {
        return false;
    }
    match (q.start_minute, q.stop_minute) {
        (Some(start), Some(stop)) if start <= stop => minute >= start && minute < stop,
        (Some(start), Some(stop)) => minute >= start || minute < stop,
        _ => true,
    }
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
    use super::{single_use_match, Engine, Tally};

    fn hosts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// The whole point of the list: the download after the one that was lost.
    /// A capture from a host already caught spending its own links is refused
    /// before it can cancel the browser's copy, and the refusal names the host
    /// so the row can say which one.
    #[test]
    fn a_host_that_spent_a_link_is_recognised_next_time() {
        let listed = hosts(&["filekeeper.net"]);
        assert_eq!(
            single_use_match("https://filekeeper.net/download", &listed).as_deref(),
            Some("filekeeper.net")
        );
        // File hosts hand the file itself to a numbered edge node, and it is
        // the same site handing out the same one-time addresses.
        assert_eq!(
            single_use_match("https://dl3.filekeeper.net/get/abc", &listed).as_deref(),
            Some("dl3.filekeeper.net")
        );
    }

    /// Everything else still goes to MDM. A list that quietly grew to mean
    /// "stop capturing" would be a worse bug than the one it fixes.
    #[test]
    fn nothing_else_is_left_to_the_browser() {
        let listed = hosts(&["filekeeper.net"]);
        for url in [
            "https://example.com/big.iso",
            // A suffix is not a subdomain: this host is somebody else's.
            "https://notfilekeeper.net/download",
            // Nor is the name appearing somewhere in the path.
            "https://cdn.example.com/filekeeper.net/file.zip",
        ] {
            assert_eq!(single_use_match(url, &listed), None, "{url} was refused");
        }
        assert_eq!(single_use_match("https://filekeeper.net/x", &[]), None);
    }

    /// Written by the engine in lower case, but this list is shown in Settings
    /// and typed into by hand, and a `*.` in front of it is how the same thing
    /// is spelled in the extension's own site list.
    #[test]
    fn a_host_typed_by_hand_is_read_as_written() {
        for pattern in ["FileKeeper.NET", " filekeeper.net ", "*.filekeeper.net"] {
            assert_eq!(
                single_use_match("https://filekeeper.net/download", &hosts(&[pattern])).as_deref(),
                Some("filekeeper.net"),
                "{pattern} matched nothing"
            );
        }
        // An empty entry is a stray comma, not a wildcard.
        assert_eq!(single_use_match("https://example.com/f", &hosts(&["", "  "])), None);
    }

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

    /// Session-signed media hosts must never be fetched by a fresh connection.
    #[test]
    fn session_signed_cdn_hosts_are_recognised() {
        for url in [
            "https://v19-webapp-prime.tiktok.com/video/tos/alisg/ok8Q9Afi2wSa1Smq4EAiI8A4BJIbioCOB2urcV/?tk=tt_chain_token",
            "https://v16-webapp-prime.tiktok.com/video/tos/abcd.mp4",
            "https://example.tiktokcdn.com/video/tos/xyz.mp4",
        ] {
            assert!(
                Engine::cdn_signs_to_session(url),
                "{url} should be left to the extractor's own request"
            );
        }
        // A suffix is not a subdomain, and ordinary CDNs are unaffected.
        for url in [
            "https://notliketok.com/video/1.mp4",
            "https://cdn.example.com/tiktok.com/embed.mp4",
            "https://media.vimeo.com/video/1.mp4",
        ] {
            assert!(
                !Engine::cdn_signs_to_session(url),
                "{url} should stay on the native path"
            );
        }
    }
}
