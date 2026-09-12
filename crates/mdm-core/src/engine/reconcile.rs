//! Reading back what the downloaders are doing, and deciding what a finished
//! or failed one means.

use super::*;

impl Engine {
    pub(super) async fn tick(&self) -> Result<()> {
        self.reconcile_ytdlp().await?;
        self.reconcile_fetch().await?;
        self.run_scheduler().await?;
        self.broadcast().await;
        Ok(())
    }

    /// Reap finished fetcher jobs and mirror their progress into the store.
    pub(super) async fn reconcile_fetch(&self) -> Result<()> {
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

    /// Reap finished yt-dlp children and mirror their progress into the store.
    pub(super) async fn reconcile_ytdlp(&self) -> Result<()> {
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
    pub(super) fn refile(&self, d: &mut Download) -> Result<()> {
        let Some(wanted) = rules::refile_to(d, &self.settings()) else {
            return Ok(());
        };
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

    pub(super) async fn on_complete(&self, d: &Download) -> Result<()> {
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

    /// Carry out what `rules::on_failure` decided.
    ///
    /// Every branch below is an *effect*: a store write, a settings update, a
    /// directory removed, a notification. What should happen was settled
    /// before any of them, by a function with no database and no runtime, and
    /// is tested there.
    pub(super) async fn on_failure(&self, d: &Download, message: &str) -> Result<()> {
        let settings = self.settings();
        let attempts = {
            let mut r = self.retries.lock().unwrap();
            let n = r.entry(d.id).or_insert(0);
            *n += 1;
            *n
        };
        let plan = rules::on_failure(d, &settings, message, attempts, ytdlp::available());

        if let Some(site) = &plan.stop_capturing {
            self.stop_capturing(site).await;
        }

        if plan.switch_to_ytdlp {
            log::info!(
                "#{}: {} — handing it to yt-dlp",
                d.id,
                if plan.was_page {
                    "that URL is a page, not a file".to_string()
                } else {
                    format!(
                        "the built-in stream downloader could not take this ({})",
                        plan.message
                    )
                }
            );
            self.store.set_use_ytdlp(d.id, true)?;
        }

        if plan.disable_native {
            log::info!(
                "#{}: the server refused MDM's own fetch ({}) — the retry will use yt-dlp itself",
                d.id,
                plan.message
            );
            self.store.set_no_native(d.id)?;
            // The native attempt is abandoned, so its scratch directory serves
            // no purpose but clutter in the user's download folder. The muxer
            // normally clears it on success only, so the refusal's leftover
            // has to be retired here.
            let dir = Path::new(&d.directory);
            for stem in &plan.scratch_stems {
                let work = dir.join(format!("{stem}.mdmstream"));
                if work.is_dir() {
                    match std::fs::remove_dir_all(&work) {
                        Ok(()) => {
                            log::info!("#{}: cleared the abandoned native scratch {work:?}", d.id)
                        }
                        Err(e) => log::warn!("#{}: could not clear {work:?}: {e}", d.id),
                    }
                    break;
                }
            }
        }

        if plan.permanent {
            log::warn!("#{} failed permanently: {}", d.id, plan.message);
        }

        if plan.forget_extraction {
            ytdlp::forget_info(&d.url);
        }
        if plan.refresh_extractor {
            // It runs in the background: the retry has its own wait, and a fix
            // that arrives during the attempt after this one is still a fix
            // nobody had to be told about.
            //
            // Not while one is running: on Windows a program that is executing
            // cannot be replaced, and there is no hurry — tomorrow's check, or
            // the next failure, comes round soon enough. Runtime state, so it
            // is asked here rather than planned for.
            let idle = self.ytdlp_jobs.lock().unwrap().is_empty();
            if idle {
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

        match plan.next {
            Next::Retry { after, attempt } => {
                log::warn!(
                    "#{} failed ({}); retry {attempt}/{} in {}s",
                    d.id,
                    plan.message,
                    settings.retry_limit,
                    after.as_secs()
                );
                self.retry_after
                    .lock()
                    .unwrap()
                    .insert(d.id, std::time::Instant::now() + after);

                // The reason is kept on the row rather than cleared. A queued
                // row that silently sits there for a minute looks stuck;
                // saying which attempt failed and that another is coming is
                // the difference between waiting and giving up on the app.
                let waiting = format!(
                    "{} — trying again in {}s ({attempt} of {})",
                    plan.message,
                    after.as_secs(),
                    settings.retry_limit
                );
                self.store.set_status(d.id, Status::Queued, Some(&waiting))?;
                self.scheduler_held.lock().unwrap().remove(&d.id);
            }
            Next::Fail => {
                self.retry_after.lock().unwrap().remove(&d.id);
                self.store
                    .set_status(d.id, Status::Failed, Some(&plan.message))?;
                self.live.lock().unwrap().remove(&d.id);
                if settings.notify {
                    notify(
                        "Download failed",
                        &format!("{}: {}", d.filename, plan.message),
                    );
                }
            }
        }
        Ok(())
    }
}
