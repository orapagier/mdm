//! The download engine: queueing, dispatch, progress tracking and scheduling.

use crate::aria2::{AddOptions, Aria2, GlobalStat};
use crate::categories;
use crate::model::{Download, Job, Queue, Settings, Status};
use crate::store::Store;
use crate::supervisor::Supervisor;
use crate::{now, ytdlp};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// How often the engine reconciles with aria2. Fast enough that the progress
/// bar looks continuous, slow enough that polling costs nothing measurable.
const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// Everything the UI needs for one repaint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub downloads: Vec<Download>,
    pub global_speed: i64,
    pub active: i64,
    pub queued: i64,
    pub aria2_ok: bool,
}

/// Live state for a yt-dlp download, which bypasses aria2's RPC entirely.
struct YtState {
    downloaded: i64,
    total: i64,
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
    /// Connections aria2 reports for this job, so a streamed download can say
    /// what it is really doing instead of claiming a single connection.
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
    aria2: Arc<Aria2>,
    supervisor: Mutex<Option<Supervisor>>,
    events: broadcast::Sender<Snapshot>,
    /// Downloads the scheduler has parked because their queue window is shut.
    scheduler_held: Mutex<HashSet<i64>>,
    retries: Mutex<HashMap<i64, u8>>,
    ytdlp_jobs: Mutex<HashMap<i64, YtState>>,
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
        let supervisor = Supervisor::start(&settings).await?;
        let aria2 = supervisor.client.clone();
        let (events, _) = broadcast::channel(32);

        let engine = Arc::new(Self {
            store,
            settings: RwLock::new(settings),
            aria2,
            supervisor: Mutex::new(Some(supervisor)),
            events,
            scheduler_held: Mutex::new(HashSet::new()),
            retries: Mutex::new(HashMap::new()),
            ytdlp_jobs: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            me: RwLock::new(Weak::new()),
        });
        *engine.me.write().unwrap() = Arc::downgrade(&engine);

        engine.apply_global_options().await?;

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
        // Clicking the same link twice must not start a rival download. aria2
        // would resolve the filename collision by writing "file.1.iso", which
        // is how you end up with one finished copy and one abandoned stub.
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

        let settings = self.settings();
        let use_ytdlp = job.use_ytdlp || ytdlp::looks_like_streaming_site(&job.url);
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
            gid: None,
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
            output_name,
            format_id: job.format_id.clone(),
            mirrors: job.mirrors.clone(),
        };

        let id = self.store.insert(&record)?;
        record.id = id;

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
    /// aria2 is told to continue whatever it finds, so a finished file of the
    /// same name reads as a download that is already done: it reports complete
    /// without fetching a byte. Another copy is what was asked for, so the name
    /// is numbered — except where a control file marks an interrupted download
    /// of exactly this target, the one case where reusing the name is how the
    /// transfer picks up where it left off.
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
                || (path.exists() && !control_file(&path).exists())
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

    /// Hand a download to aria2 (or yt-dlp) and record the resulting handle.
    async fn dispatch(&self, d: &Download, job: &Job) -> Result<()> {
        if d.use_ytdlp {
            return self.dispatch_ytdlp(d, job).await;
        }

        let settings = self.settings();
        let opts = AddOptions {
            dir: d.directory.clone(),
            out: Some(d.filename.clone()),
            headers: d.headers.clone(),
            referer: Some(d.referrer.clone()),
            connections: settings.connections,
            split: settings.split,
            min_split_size: settings.min_split_size.clone(),
            max_speed: settings.max_speed_per_download,
            retry_limit: settings.retry_limit,
            paused: false,
            extra: Vec::new(),
        };

        // The original first, then every mirror the capture found. aria2
        // spreads its connections across the list and abandons the slow ones,
        // so a well-mirrored file arrives faster than any one server can send it.
        let mut sources = Vec::with_capacity(1 + d.mirrors.len());
        sources.push(d.url.clone());
        sources.extend(d.mirrors.iter().cloned());
        if sources.len() > 1 {
            log::info!("#{} has {} sources", d.id, sources.len());
        }

        let gid = self.aria2.add_uri(&sources, &opts).await?;
        self.store.set_gid(d.id, Some(&gid))?;
        log::info!("#{} -> aria2 {gid} ({})", d.id, d.filename);
        Ok(())
    }

    async fn dispatch_ytdlp(&self, d: &Download, job: &Job) -> Result<()> {
        if !ytdlp::available() {
            bail!("yt-dlp is not installed");
        }
        let settings = self.settings();
        let format = job
            .format_id
            .clone()
            .unwrap_or_else(|| settings.ytdlp_format.clone());

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
                downloaded: 0,
                total: d.total_bytes,
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
                            state.downloaded = p.downloaded;
                            if p.total > 0 {
                                // The streams arrive one after another, so what
                                // aria2 has weighed so far is a floor rather
                                // than the job: taking it as the job is what
                                // sends the bar from 99% back to 67% the moment
                                // the audio stream appears behind the video.
                                state.total = state.total.max(p.total);
                            }
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
        self.reconcile_aria2().await?;
        self.reconcile_ytdlp().await?;
        self.run_scheduler().await?;
        self.broadcast().await;
        Ok(())
    }

    /// Pull aria2's view of the world and fold it into our rows.
    async fn reconcile_aria2(&self) -> Result<()> {
        let tasks = match self.aria2.tell_all(200).await {
            Ok(t) => t,
            Err(e) => {
                log::debug!("aria2 poll failed: {e:#}");
                return Ok(());
            }
        };

        let by_gid: HashMap<&str, _> = tasks.iter().map(|t| (t.gid.as_str(), t)).collect();

        for mut d in self.store.list(500)? {
            let Some(gid) = d.gid.clone() else { continue };
            if d.status.is_terminal() {
                continue;
            }
            let Some(task) = by_gid.get(gid.as_str()) else {
                continue;
            };

            // aria2 may rename the file (Content-Disposition, or a collision
            // resolved by auto-file-renaming); adopt whatever it actually wrote.
            let (new_name, new_dir) = match &task.path {
                Some(p) if !p.is_empty() => {
                    let path = Path::new(p);
                    (
                        path.file_name().map(|n| n.to_string_lossy().into_owned()),
                        path.parent().map(|d| d.to_string_lossy().into_owned()),
                    )
                }
                _ => (None, None),
            };

            self.store.update_progress(
                d.id,
                task.total_length,
                task.completed_length,
                new_name.as_deref().filter(|n| *n != d.filename),
                new_dir.as_deref().filter(|p| *p != d.directory),
            )?;

            self.live
                .lock()
                .unwrap()
                .insert(d.id, (task.download_speed, task.connections));

            let held = self.scheduler_held.lock().unwrap().contains(&d.id);
            let reported = Status::from_aria2(&task.status);
            let effective = if held && reported == Status::Paused {
                Status::Queued
            } else {
                reported
            };

            if effective != d.status {
                match effective {
                    Status::Complete => {
                        if let Some(n) = new_name {
                            d.filename = n;
                        }
                        if let Some(p) = new_dir {
                            d.directory = p;
                        }
                        self.on_complete(&d).await?;
                    }
                    Status::Failed => {
                        let msg = task
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "download failed".into());
                        self.on_failure(&d, &msg).await?;
                    }
                    other => self.store.set_status(d.id, other, None)?,
                }
            }
        }
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
                .map(|(id, s)| (*id, s.downloaded, s.total, s.speed, s.connections))
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

    async fn on_failure(&self, d: &Download, message: &str) -> Result<()> {
        let settings = self.settings();
        let attempts = {
            let mut r = self.retries.lock().unwrap();
            let n = r.entry(d.id).or_insert(0);
            *n += 1;
            *n
        };

        // A missing format or a private video fails identically every time;
        // retrying only delays telling the user what actually went wrong.
        let permanent = crate::ytdlp::is_permanent_error(message);
        if permanent {
            log::warn!("#{} failed permanently: {message}", d.id);
        }

        if !permanent && attempts <= settings.retry_limit {
            log::warn!(
                "#{} failed ({message}); retry {attempts}/{}",
                d.id,
                settings.retry_limit
            );
            // Clear the handle so the scheduler re-dispatches from scratch;
            // aria2 resumes from the partial file it already wrote.
            self.store.set_gid(d.id, None)?;
            self.store.set_status(d.id, Status::Queued, None)?;
            self.scheduler_held.lock().unwrap().remove(&d.id);
            return Ok(());
        }

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
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        if let Some(gid) = &d.gid {
            self.aria2.pause(gid).await?;
        }
        if let Some(state) = self.ytdlp_jobs.lock().unwrap().get_mut(&id) {
            // yt-dlp has no pause; stopping is the honest equivalent, and the
            // partial fragments are reused when it restarts. Flag it first, so
            // the reaper reads the exit as intentional rather than as a crash
            // worth retrying.
            state.stopped_by_us = true;
            let _ = state.child.start_kill();
        }
        self.store.set_status(id, Status::Paused, None)?;
        self.scheduler_held.lock().unwrap().remove(&id);
        self.broadcast().await;
        Ok(())
    }

    pub async fn resume(&self, id: i64) -> Result<()> {
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        match &d.gid {
            Some(gid) => {
                self.aria2.unpause(gid).await?;
                self.store.set_status(id, Status::Active, None)?;
            }
            // No aria2 handle: it never started, or a retry cleared it.
            None => {
                let job = job_from(&d);
                self.store.set_status(id, Status::Queued, None)?;
                self.dispatch(&d, &job).await?;
            }
        }
        self.broadcast().await;
        Ok(())
    }

    /// Point a download that has not started yet at a different folder or name.
    ///
    /// Only before the first byte: once aria2 or yt-dlp owns a partial file,
    /// moving the target underneath it would orphan what is already written.
    pub fn set_target(
        &self,
        id: i64,
        directory: Option<&str>,
        filename: Option<&str>,
    ) -> Result<()> {
        let Some(d) = self.store.get(id)? else { return Ok(()) };
        if d.completed_bytes > 0 || d.gid.is_some() {
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
        if let Some(gid) = &d.gid {
            let _ = self.aria2.remove(gid).await;
        }
        self.store.set_gid(id, None)?;
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
        if let Some(gid) = &d.gid {
            let _ = self.aria2.remove(gid).await;
        }
        if let Some(mut state) = self.ytdlp_jobs.lock().unwrap().remove(&id) {
            let _ = state.child.start_kill();
        }
        if delete_file {
            let path = d.full_path();
            let _ = std::fs::remove_file(&path);
            // aria2 leaves a control file beside the target when interrupted.
            // Built by appending, not with_extension: for a name carrying no
            // extension the latter yields "file..aria2" and misses the file.
            let _ = std::fs::remove_file(control_file(&path));
        }
        self.store.delete(id)?;
        self.live.lock().unwrap().remove(&id);
        self.retries.lock().unwrap().remove(&id);
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

    pub async fn update_settings(&self, new: Settings) -> Result<()> {
        let restart_needed = {
            let old = self.settings.read().unwrap();
            old.rpc_port != new.rpc_port
        };
        crate::config::save(&new)?;
        *self.settings.write().unwrap() = new;

        // Everything else applies live. The port is the one option aria2
        // cannot change in place, and tearing the daemon down mid-transfer to
        // honour it would cost more than waiting for the next launch.
        self.apply_global_options().await?;
        self.broadcast().await;
        if restart_needed {
            log::info!("rpc port change takes effect on next launch");
        }
        Ok(())
    }

    /// Push the options aria2 can change without a restart.
    async fn apply_global_options(&self) -> Result<()> {
        let s = self.settings();
        self.aria2
            .set_global(&[
                ("max-overall-download-limit", s.max_speed.to_string()),
                ("max-download-limit", s.max_speed_per_download.to_string()),
                (
                    "max-concurrent-downloads",
                    s.max_concurrent.max(1).to_string(),
                ),
                (
                    "max-connection-per-server",
                    s.connections.clamp(1, crate::aria2::MAX_CONNECTIONS).to_string(),
                ),
                ("split", s.split.max(1).to_string()),
                ("min-split-size", s.min_split_size.clone()),
                ("dir", s.download_dir.clone()),
            ])
            .await
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
                    if d.gid.is_some() {
                        continue; // already with aria2, just waiting its turn
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
                    if let Some(gid) = &d.gid {
                        let _ = self.aria2.pause(gid).await;
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
            aria2_ok: true,
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
        let mut guard = self.supervisor.lock().unwrap().take();
        if let Some(sup) = guard.as_mut() {
            sup.stop().await;
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
        use_ytdlp: d.use_ytdlp,
        // Recorded on the row precisely so a resume, a retry or a scheduled
        // dispatch fetches the quality that was chosen rather than the default.
        format_id: d.format_id.clone(),
        output_name: d.output_name.clone(),
        start_paused: false,
    }
}

/// aria2's control file for a given target: `<path>.aria2`.
pub fn control_file(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".aria2");
    PathBuf::from(name)
}

/// GNOME/KDE/COSMIC all honour `notify-send`; absence of it is not an error.
fn notify(title: &str, body: &str) {
    if crate::supervisor::which("notify-send").is_none() {
        return;
    }
    let _ = std::process::Command::new("notify-send")
        .arg("--app-name=My Download Manager")
        .arg("--icon=ldm")
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

/// Make a server-supplied name safe to use as aria2's `out` option.
///
/// The last path segment only: aria2 resolves `out` relative to `dir` and will
/// happily create parent directories, so a name like `../../.bashrc` would
/// otherwise write outside the download folder.
pub fn sanitize(name: String) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").to_string();
    let mut out: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    out = out.trim().trim_start_matches('.').to_string();
    if out.is_empty() {
        out = "download".into();
    }
    if out.len() > 200 {
        out.truncate(200);
    }
    out
}

pub fn filename_from_url(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|s| s.filter(|p| !p.is_empty()).next_back())
                .map(|s| {
                    percent_decode(s)
                })
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "download".into())
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
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

/// Re-exported so callers can show aria2's aggregate throughput.
pub async fn global_stat(aria2: &Aria2) -> Result<GlobalStat> {
    aria2.global_stat().await
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
        use_ytdlp: false,
        format_id: None,
        output_name: None,
        start_paused: false,
    }
}
