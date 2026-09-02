//! SQLite persistence for downloads and queues.
//!
//! Queries here are small and indexed, so they run directly on the calling
//! task rather than going through `spawn_blocking`; the database is local and
//! every statement touches at most a handful of rows.

use crate::model::{Download, Header, Queue, Status};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS downloads (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    gid             TEXT,
    url             TEXT    NOT NULL,
    filename        TEXT    NOT NULL,
    directory       TEXT    NOT NULL,
    category        TEXT    NOT NULL DEFAULT 'Other',
    status          TEXT    NOT NULL,
    total_bytes     INTEGER NOT NULL DEFAULT -1,
    completed_bytes INTEGER NOT NULL DEFAULT 0,
    mime            TEXT    NOT NULL DEFAULT '',
    referrer        TEXT    NOT NULL DEFAULT '',
    headers         TEXT    NOT NULL DEFAULT '[]',
    error           TEXT,
    sha256          TEXT,
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    queue           TEXT    NOT NULL DEFAULT 'main',
    use_ytdlp       INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_downloads_status  ON downloads(status);
CREATE INDEX IF NOT EXISTS idx_downloads_created ON downloads(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_downloads_gid     ON downloads(gid);

CREATE TABLE IF NOT EXISTS queues (
    name           TEXT PRIMARY KEY,
    enabled        INTEGER NOT NULL DEFAULT 1,
    start_minute   INTEGER,
    stop_minute    INTEGER,
    days           TEXT    NOT NULL DEFAULT '',
    max_concurrent INTEGER NOT NULL DEFAULT 4
);
"#;

/// Add columns introduced after a database was first created.
///
/// The schema uses CREATE TABLE IF NOT EXISTS, so an existing install would
/// otherwise never gain new columns.
fn migrate(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        ("output_name", "ALTER TABLE downloads ADD COLUMN output_name TEXT"),
        ("format_id", "ALTER TABLE downloads ADD COLUMN format_id TEXT"),
        ("mirrors", "ALTER TABLE downloads ADD COLUMN mirrors TEXT NOT NULL DEFAULT '[]'"),
    ] {
        let present = conn
            .prepare(&format!("SELECT {column} FROM downloads LIMIT 1"))
            .is_ok();
        if !present {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

impl Store {
    pub fn open() -> Result<Self> {
        let path = crate::paths::db_path();
        let conn = Connection::open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // WAL keeps the UI's reads from blocking the poll loop's writes.
        //
        // execute_batch rather than pragma_update: `PRAGMA journal_mode` answers
        // with a row, and pragma_update goes through `execute`, which treats a
        // returned row as an error.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )
        .context("setting pragmas")?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        migrate(&conn).context("migrating schema")?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store.ensure_default_queue()?;
        store.reconcile_on_start()?;
        Ok(store)
    }

    fn ensure_default_queue(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO queues (name, enabled, max_concurrent)
             VALUES ('main', 1, 4)",
            [],
        )?;
        Ok(())
    }

    /// Anything recorded as running when we last exited is not running now.
    /// Demote it to queued so the engine re-dispatches it (aria2 resumes from
    /// the partial file, so no bytes are lost).
    ///
    /// Paused is left alone. It is not a stale state but a decision — the
    /// user's "later", or a capture still waiting to be confirmed — and
    /// restarting the app is not consent to start it.
    fn reconcile_on_start(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE downloads
                SET status = 'queued', gid = NULL
              WHERE status = 'active'",
            [],
        )?;
        Ok(())
    }

    /* ---------------------------------------------------------------- */

    pub fn insert(&self, d: &Download) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO downloads
                (gid, url, filename, directory, category, status, total_bytes,
                 completed_bytes, mime, referrer, headers, created_at, queue, use_ytdlp,
                 output_name, format_id, mirrors)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                d.gid,
                d.url,
                d.filename,
                d.directory,
                d.category,
                d.status.as_str(),
                d.total_bytes,
                d.completed_bytes,
                d.mime,
                d.referrer,
                serde_json::to_string(&d.headers)?,
                d.created_at,
                d.queue,
                d.use_ytdlp as i32,
                d.output_name,
                d.format_id,
                serde_json::to_string(&d.mirrors)?,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_gid(&self, id: i64, gid: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE downloads SET gid = ?1 WHERE id = ?2", params![gid, id])?;
        Ok(())
    }

    pub fn set_status(&self, id: i64, status: Status, error: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let finished = status.is_terminal().then(|| crate::now());
        conn.execute(
            "UPDATE downloads SET status = ?1, error = ?2, finished_at = ?3 WHERE id = ?4",
            params![status.as_str(), error, finished, id],
        )?;
        Ok(())
    }

    /// Persist transfer counters. Speed and connection count are deliberately
    /// *not* stored — they are live values, meaningless once a run ends, and
    /// writing them every 700 ms would be needless disk churn.
    pub fn update_progress(
        &self,
        id: i64,
        total: i64,
        completed: i64,
        filename: Option<&str>,
        directory: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE downloads
                SET total_bytes     = ?1,
                    completed_bytes = ?2,
                    filename        = COALESCE(?3, filename),
                    directory       = COALESCE(?4, directory)
              WHERE id = ?5",
            params![total, completed, filename, directory, id],
        )?;
        Ok(())
    }

    /// Point a yt-dlp row at a different output name, which is a stem: the
    /// container is settled by muxing and is not ours to choose.
    pub fn set_output_name(&self, id: i64, output_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE downloads SET output_name = ?1 WHERE id = ?2",
            params![output_name, id],
        )?;
        Ok(())
    }

    /// Rename the row without touching transfer counters.
    pub fn set_filename(&self, id: i64, filename: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE downloads SET filename = ?1 WHERE id = ?2",
            params![filename, id],
        )?;
        Ok(())
    }

    /// yt-dlp picks the container only after muxing, so the category derived
    /// at submit time from a page URL is frequently wrong.
    pub fn set_category(&self, id: i64, category: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE downloads SET category = ?1 WHERE id = ?2",
            params![category, id],
        )?;
        Ok(())
    }

    pub fn set_sha256(&self, id: i64, sum: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE downloads SET sha256 = ?1 WHERE id = ?2",
            params![sum, id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM downloads WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get(&self, id: i64) -> Result<Option<Download>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(SELECT_COLS)?;
        let row = stmt
            .query_row(params![id], row_to_download)
            .optional()?;
        Ok(row)
    }

    pub fn by_gid(&self, gid: &str) -> Result<Option<Download>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&SELECT_ALL.replace("{where}", "gid = ?1"))?;
        let row = stmt.query_row(params![gid], row_to_download).optional()?;
        Ok(row)
    }

    /// Most recent first, capped so the UI never has to render unbounded rows.
    pub fn list(&self, limit: i64) -> Result<Vec<Download>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, gid, url, filename, directory, category, status, total_bytes,
                    completed_bytes, mime, referrer, headers, error, sha256,
                    created_at, finished_at, queue, use_ytdlp, output_name,
                    format_id, mirrors
               FROM downloads
              ORDER BY (status IN ('active','queued','paused')) DESC, created_at DESC
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_download)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn by_status(&self, status: Status) -> Result<Vec<Download>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&SELECT_ALL.replace("{where}", "status = ?1"))?;
        let rows = stmt.query_map(params![status.as_str()], row_to_download)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// An in-flight download of this URL *in this format*, if one exists.
    ///
    /// Terminal rows are excluded deliberately: re-fetching something that
    /// already finished is a legitimate thing to ask for, whereas starting a
    /// second copy of a download still in progress never is.
    ///
    /// The format is part of the identity because two picks from one page are
    /// two different files. Matching on the URL alone would answer a request
    /// for the audio track with the video that is already running.
    pub fn find_unfinished_target(
        &self,
        url: &str,
        format_id: Option<&str>,
    ) -> Result<Option<Download>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, gid, url, filename, directory, category, status, total_bytes,
                    completed_bytes, mime, referrer, headers, error, sha256,
                    created_at, finished_at, queue, use_ytdlp, output_name,
                    format_id, mirrors
               FROM downloads
              WHERE url = ?1 AND format_id IS ?2
                AND status IN ('queued','active','paused')
              ORDER BY created_at ASC
              LIMIT 1",
        )?;
        // `IS` rather than `=`: a direct download has no format, and NULL = NULL
        // is never true, so `=` would fail to recognise its own duplicates.
        Ok(stmt
            .query_row(params![url, format_id], row_to_download)
            .optional()?)
    }

    /// Names that downloads still in flight are relying on.
    ///
    /// A second pick from the same page can be submitted before the first has
    /// written a single byte, so what the folder holds is not on its own a
    /// complete answer to which names are free.
    /// `except` is the row doing the asking, which must not be counted as
    /// competing with itself; no row has id 0, so that asks about all of them.
    pub fn names_in_flight(&self, directory: &str, except: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT filename, output_name FROM downloads
              WHERE directory = ?1 AND id != ?2
                AND status IN ('queued','active','paused')",
        )?;
        let rows = stmt.query_map(params![directory, except], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut names = Vec::new();
        for row in rows {
            let (filename, output_name) = row?;
            names.push(filename);
            names.extend(output_name);
        }
        Ok(names)
    }

    pub fn count_by_status(&self, status: Status) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM downloads WHERE status = ?1",
            params![status.as_str()],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Oldest-first so the queue behaves like a queue.
    pub fn next_queued(&self, queue: &str, limit: i64) -> Result<Vec<Download>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, gid, url, filename, directory, category, status, total_bytes,
                    completed_bytes, mime, referrer, headers, error, sha256,
                    created_at, finished_at, queue, use_ytdlp, output_name,
                    format_id, mirrors
               FROM downloads
              WHERE status = 'queued' AND queue = ?1
              ORDER BY created_at ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![queue, limit], row_to_download)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_finished(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM downloads WHERE status IN ('complete','failed','removed')",
            [],
        )?)
    }

    /* ---------------------------- queues ---------------------------- */

    pub fn queues(&self) -> Result<Vec<Queue>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, enabled, start_minute, stop_minute, days, max_concurrent
               FROM queues ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            let days: String = r.get(4)?;
            Ok(Queue {
                name: r.get(0)?,
                enabled: r.get::<_, i64>(1)? != 0,
                start_minute: r.get::<_, Option<i64>>(2)?.map(|v| v as u16),
                stop_minute: r.get::<_, Option<i64>>(3)?.map(|v| v as u16),
                days: days
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse().ok())
                    .collect(),
                max_concurrent: r.get::<_, i64>(5)? as u8,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn save_queue(&self, q: &Queue) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let days = q
            .days
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(",");
        conn.execute(
            "INSERT INTO queues (name, enabled, start_minute, stop_minute, days, max_concurrent)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(name) DO UPDATE SET
                enabled        = excluded.enabled,
                start_minute   = excluded.start_minute,
                stop_minute    = excluded.stop_minute,
                days           = excluded.days,
                max_concurrent = excluded.max_concurrent",
            params![
                q.name,
                q.enabled as i32,
                q.start_minute.map(|v| v as i64),
                q.stop_minute.map(|v| v as i64),
                days,
                q.max_concurrent as i64,
            ],
        )?;
        Ok(())
    }

    pub fn delete_queue(&self, name: &str) -> Result<()> {
        if name == "main" {
            anyhow::bail!("the main queue cannot be deleted");
        }
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE downloads SET queue = 'main' WHERE queue = ?1", params![name])?;
        conn.execute("DELETE FROM queues WHERE name = ?1", params![name])?;
        Ok(())
    }
}

const SELECT_COLS: &str = "SELECT id, gid, url, filename, directory, category, status,
     total_bytes, completed_bytes, mime, referrer, headers, error, sha256,
     created_at, finished_at, queue, use_ytdlp, output_name, format_id, mirrors \
     FROM downloads WHERE id = ?1";

const SELECT_ALL: &str = "SELECT id, gid, url, filename, directory, category, status,
     total_bytes, completed_bytes, mime, referrer, headers, error, sha256,
     created_at, finished_at, queue, use_ytdlp, output_name, format_id, mirrors \
     FROM downloads WHERE {where}";

fn row_to_download(r: &Row<'_>) -> rusqlite::Result<Download> {
    let headers_json: String = r.get(11)?;
    let headers: Vec<Header> = serde_json::from_str(&headers_json).unwrap_or_default();
    let status: String = r.get(6)?;
    Ok(Download {
        id: r.get(0)?,
        gid: r.get(1)?,
        url: r.get(2)?,
        filename: r.get(3)?,
        directory: r.get(4)?,
        category: r.get(5)?,
        status: Status::parse(&status),
        total_bytes: r.get(7)?,
        completed_bytes: r.get(8)?,
        // Live-only fields; refreshed from aria2 on each poll.
        download_speed: 0,
        connections: 0,
        mime: r.get(9)?,
        referrer: r.get(10)?,
        headers,
        error: r.get(12)?,
        sha256: r.get(13)?,
        created_at: r.get(14)?,
        finished_at: r.get(15)?,
        queue: r.get(16)?,
        use_ytdlp: r.get::<_, i64>(17)? != 0,
        output_name: r.get(18)?,
        format_id: r.get(19)?,
        // A row written before this column existed reads as an empty list.
        mirrors: serde_json::from_str(&r.get::<_, String>(20)?).unwrap_or_default(),
    })
}
