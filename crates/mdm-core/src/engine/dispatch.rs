//! Handing a row to the downloader that suits it — the fetcher, the stream
//! downloader, yt-dlp, or the bytes the page already has.

use super::*;

impl Engine {
    /// Hand a download to the fetcher (or yt-dlp) and record the resulting handle.
    pub(super) async fn dispatch(&self, d: &Download, job: &Job) -> Result<()> {
        // Whoever holds the claim is already starting this one.
        if !self.claimed.lock().unwrap().insert(d.id) {
            log::debug!("#{} is already being started; leaving it to that", d.id);
            return Ok(());
        }
        let _claim = Claim { set: &self.claimed, id: d.id };
        self.dispatch_claimed(d, job).await
    }

    pub(super) async fn dispatch_claimed(&self, d: &Download, job: &Job) -> Result<()> {
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
    pub(super) async fn dispatch_blob(&self, d: &Download, stash: &Path) -> Result<()> {
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
    pub(super) async fn dispatch_fetch(
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

    /// The streams to fetch ourselves, when this job is one we can finish
    /// without yt-dlp downloading anything.
    ///
    /// `None` is the ordinary answer for half the sites in the world and is
    /// not a failure: it means "let yt-dlp do it", which is what happened
    /// before this existed. Nothing here is allowed to fail a download — a
    /// planner that cannot answer simply declines.
    pub(super) async fn native_plan(
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
        if plan.tracks.iter().any(|t| rules::cdn_signs_to_session(&t.url)) {
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

    pub(super) async fn dispatch_ytdlp(&self, d: &Download, job: &Job) -> Result<()> {
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
}
