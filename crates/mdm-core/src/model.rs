//! Types shared by the engine, the IPC layer and the UI.

use serde::{Deserialize, Serialize};

/// A single HTTP header captured by the extension and replayed by aria2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    /// aria2's `--header` wire format.
    pub fn to_arg(&self) -> String {
        format!("{}: {}", self.name, self.value)
    }
}

/// What the browser extension hands over. Everything except `url` is advisory:
/// the engine re-derives what it can and fills the gaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub url: String,
    /// Other servers the capture found holding this same file (RFC 6249
    /// `Link: rel=duplicate`). Handed to aria2 alongside `url` so one file can
    /// be pulled from several places at once.
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default)]
    pub filename: String,
    /// `-1` when the server sent no Content-Length.
    #[serde(default = "unknown_size")]
    pub size: i64,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub referrer: String,
    #[serde(default)]
    pub cookie_store_id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub source: String,
    /// Set by the UI when the user picks a directory explicitly.
    #[serde(default)]
    pub directory: Option<String>,
    /// Streaming sites go to yt-dlp instead of straight to aria2.
    #[serde(default)]
    pub use_ytdlp: bool,
    #[serde(default)]
    pub format_id: Option<String>,
    /// Name the user picked in the format dialog, without extension.
    /// yt-dlp decides the container after muxing, so only the stem is ours.
    #[serde(default)]
    pub output_name: Option<String>,
    /// Queue the download but leave it paused ("Download Later").
    #[serde(default)]
    pub start_paused: bool,
    /// The file itself, base64, for a download the page assembled in memory.
    ///
    /// A `blob:` URL names an object inside one document and nothing else can
    /// resolve it — so for those the extension reads the bytes in the page and
    /// sends them, and there is no URL left to fetch.
    #[serde(default)]
    pub data: Option<String>,
}

fn unknown_size() -> i64 {
    -1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Held in the queue: either awaiting a free slot or a scheduled window.
    Queued,
    Active,
    Paused,
    Complete,
    Failed,
    Removed,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Complete | Status::Failed | Status::Removed)
    }

    /// Map an aria2 status string onto ours.
    pub fn from_aria2(s: &str) -> Self {
        match s {
            "active" => Status::Active,
            "waiting" => Status::Queued,
            "paused" => Status::Paused,
            "complete" => Status::Complete,
            "error" => Status::Failed,
            "removed" => Status::Removed,
            _ => Status::Queued,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Active => "active",
            Status::Paused => "paused",
            Status::Complete => "complete",
            Status::Failed => "failed",
            Status::Removed => "removed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "active" => Status::Active,
            "paused" => Status::Paused,
            "complete" => Status::Complete,
            "failed" => Status::Failed,
            "removed" => Status::Removed,
            _ => Status::Queued,
        }
    }
}

/// A download as the app tracks it. `gid` is aria2's handle, absent until the
/// job is actually dispatched.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: i64,
    pub gid: Option<String>,
    pub url: String,
    pub filename: String,
    pub directory: String,
    pub category: String,
    pub status: Status,
    pub total_bytes: i64,
    pub completed_bytes: i64,
    pub download_speed: i64,
    pub connections: i64,
    pub mime: String,
    pub referrer: String,
    pub headers: Vec<Header>,
    pub error: Option<String>,
    pub sha256: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    pub finished_at: Option<i64>,
    /// Queue this download belongs to; queues are what the scheduler acts on.
    pub queue: String,
    pub use_ytdlp: bool,
    /// Preserved so a retry re-uses the name the user chose.
    pub output_name: Option<String>,
    /// The format expression the user picked, for the same reason.
    ///
    /// yt-dlp has no pause, so pausing kills it and resuming starts it again;
    /// without this the second run would fall back to the default format and
    /// silently fetch a different quality than the one that was chosen.
    pub format_id: Option<String>,
    /// Mirrors, kept so a resume is as fast as the first attempt. They are
    /// only visible in the response headers of the original request, which no
    /// later attempt gets to see again.
    #[serde(default)]
    pub mirrors: Vec<String>,
}

impl Download {
    pub fn progress(&self) -> f64 {
        if self.total_bytes <= 0 {
            return 0.0;
        }
        (self.completed_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }

    /// Seconds remaining at the current rate, or `None` when unknowable.
    pub fn eta_secs(&self) -> Option<i64> {
        if self.download_speed <= 0 || self.total_bytes <= 0 {
            return None;
        }
        let left = self.total_bytes - self.completed_bytes;
        if left <= 0 {
            return None;
        }
        Some(left / self.download_speed)
    }

    pub fn full_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.directory).join(&self.filename)
    }
}

/// User-facing configuration, persisted as TOML.
///
/// camelCase rather than the more usual TOML snake_case: this same struct is
/// handed straight to the frontend, and one casing for both surfaces beats
/// maintaining a parallel DTO just to satisfy convention on each side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Root for categorised downloads.
    pub download_dir: String,
    /// Sort finished files into per-type subdirectories.
    pub categorize: bool,
    /// Connections to a single server per download. aria2's RPC caps this at
    /// 16 regardless of what its command line accepts.
    pub connections: u8,
    /// Segments per download; aria2 will not split below `min_split_size`.
    pub split: u8,
    /// Smallest piece aria2 will hand to a separate connection.
    pub min_split_size: String,
    /// Downloads running at once.
    pub max_concurrent: u8,
    /// Global cap in bytes/sec; 0 means unlimited.
    pub max_speed: u64,
    /// Per-download cap in bytes/sec; 0 means unlimited.
    pub max_speed_per_download: u64,
    pub retry_limit: u8,
    /// Watch the clipboard for URLs and offer to download them.
    pub clipboard_watch: bool,
    /// Verify SHA-256 after completion.
    pub checksum: bool,
    pub notify: bool,
    pub start_minimized: bool,
    /// aria2 RPC port. Changing it restarts the daemon.
    pub rpc_port: u16,
    /// Extra flags appended to the aria2 command line, for power users.
    pub aria2_extra_args: Vec<String>,
    pub ytdlp_format: String,
    /// Browser yt-dlp should lift cookies from, in its `--cookies-from-browser`
    /// syntax (e.g. "firefox", "firefox:/path/to/profile", "chromium").
    /// Empty disables it.
    pub ytdlp_cookies_from: String,
    /// Extra flags for yt-dlp, e.g.
    /// `["--extractor-args", "youtube:player_client=web_safari"]`.
    /// YouTube's extraction changes often; this avoids needing a rebuild.
    pub ytdlp_extra_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: crate::paths::default_download_dir()
                .to_string_lossy()
                .into_owned(),
            categorize: true,
            // 16 is the sweet spot: enough to saturate most links, low
            // enough that servers rarely throttle or ban for it.
            connections: 16,
            split: 16,
            min_split_size: "1M".into(),
            max_concurrent: 4,
            max_speed: 0,
            max_speed_per_download: 0,
            retry_limit: 5,
            clipboard_watch: false,
            checksum: false,
            notify: true,
            start_minimized: false,
            // Deliberately not aria2's default 6800: MDM runs its own daemon
            // and must not collide with one the user already has.
            rpc_port: 6810,
            aria2_extra_args: Vec::new(),
            ytdlp_format: "bestvideo*+bestaudio/best".into(),
            // YouTube now refuses anonymous extraction on many videos with
            // "Sign in to confirm you're not a bot". The user is already
            // signed in in Firefox, which is where the request came from.
            ytdlp_cookies_from: "firefox".into(),
            ytdlp_extra_args: Vec::new(),
        }
    }
}

/// A named queue with an optional scheduled window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queue {
    pub name: String,
    pub enabled: bool,
    /// Minutes past midnight, local time.
    pub start_minute: Option<u16>,
    pub stop_minute: Option<u16>,
    /// Days the window applies to; 0 = Monday. Empty means every day.
    pub days: Vec<u8>,
    pub max_concurrent: u8,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            name: "main".into(),
            enabled: true,
            start_minute: None,
            stop_minute: None,
            days: Vec::new(),
            max_concurrent: 4,
        }
    }
}
