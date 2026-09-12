//! What the user asks of a download that already exists: pause, resume,
//! retry, remove, re-target, and the settings those answer to.

use super::*;

impl Engine {
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
        rules::single_use_match(url, &self.settings().single_use_hosts)
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
    pub(super) async fn stop_capturing(&self, host: &str) {
        if host.is_empty() {
            return;
        }
        let host = host.to_ascii_lowercase();
        let mut settings = self.settings();
        if settings.single_use_hosts.iter().any(|h| rules::host_matches(&host, h)) {
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
}
