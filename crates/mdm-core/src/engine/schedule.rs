//! Queue windows: starting what may run now and parking what may not.

use super::*;

impl Engine {
    /// Is `queue` inside its permitted window right now?
    pub(super) fn queue_window_open(&self, queue: &str) -> Result<bool> {
        let Some(q) = self.store.queues()?.into_iter().find(|q| q.name == queue) else {
            return Ok(true);
        };
        Ok(rules::queue_open_at(&q, local_minute_of_day(), local_weekday()))
    }

    /// Start or park downloads as scheduled windows open and close.
    pub(super) async fn run_scheduler(&self) -> Result<()> {
        for q in self.store.queues()? {
            let open = rules::queue_open_at(&q, local_minute_of_day(), local_weekday());

            if open {
                // Dispatch anything parked, up to the queue's own limit.
                let running = self
                    .store
                    .by_status(Status::Active)?
                    .into_iter()
                    .filter(|d| d.queue == q.name)
                    .count();
                let slots = rules::free_slots(&q, running);
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
}
