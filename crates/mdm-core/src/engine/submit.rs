//! Taking a job in: where it lands, what it is called, and what has to
//! give way for it.

use super::*;

impl Engine {
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
    pub(super) fn resolve_destination(
        &self,
        job: &Job,
        settings: &Settings,
        use_ytdlp: bool,
    ) -> Result<(PathBuf, String, &'static str)> {
        let rules::Destination { dir: base, filename, category } =
            rules::destination(job, settings, use_ytdlp);

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
    pub(super) fn free_filename(&self, dir: &Path, name: &str, except: i64) -> Result<String> {
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
    pub(super) fn free_stem(&self, dir: &Path, stem: &str, except: i64) -> Result<String> {
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
}
