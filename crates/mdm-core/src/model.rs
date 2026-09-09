//! Types shared by the engine, the IPC layer and the UI.

use serde::{Deserialize, Serialize};

/// A single HTTP header captured by the extension and replayed on the download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    /// Rendered as `Name: value` when the request is built.
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
    /// `Link: rel=duplicate`). Dealt across connections alongside `url` so one file can
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
    /// Whether yt-dlp belongs between this URL and the downloader.
    ///
    /// Three answers rather than two. `None` is "no opinion", and only then
    /// does the engine guess from the host. A caller that has already looked
    /// says so outright: the format picker asks yt-dlp about every page it can
    /// find before it gives up and offers the file the player is using, and
    /// its `Some(false)` is a finding rather than an unset default.
    #[serde(default)]
    pub use_ytdlp: Option<bool>,
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

impl Job {
    /// Has the browser already made this request?
    ///
    /// Only the capture nets watch a response go past; by the time one of them
    /// hands a job over, the address has been asked and answered once. That is
    /// what makes a single-use link unusable to MDM — its one answer is spent —
    /// and it is the whole basis for refusing those.
    ///
    /// The other sources have spent nothing. A right-clicked link, a link
    /// picked out of a page, an address pasted into the window: nothing has
    /// requested any of them, so MDM's request is the first and the file is
    /// there to be had. Refusing those was refusing the one case on such a host
    /// that works.
    ///
    /// Unknown sources count as spent, so a net added later is refused until
    /// somebody decides otherwise rather than quietly cancelling a browser
    /// download that cannot be replaced.
    ///
    /// * `menu` — right-click ▸ Download with MDM, on a link nothing has followed
    /// * `cli`  — an address on the command line, or an `mdm:` link
    /// * `ui`   — pasted into the window
    /// * `preempt` — the request MDM makes *instead of* the browser's
    pub fn spends_the_link(&self) -> bool {
        !matches!(self.source.as_str(), "menu" | "cli" | "ui" | "preempt")
    }
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

/// A download as the app tracks it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: i64,
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

/// One server's login, as the user typed it.
///
/// Kept in `settings.toml` in the clear, which is worth being plain about: the
/// file is under the user's own profile and readable only by them, but anything
/// with that user's rights can read it. It is the same bargain every
/// `.netrc`-shaped file makes. Nothing here is sent anywhere except to the host
/// it names, over the scheme that host was reached by.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Credential {
    /// The host it applies to — `files.example.com`. Matched exactly, and on
    /// the *final* URL, so a redirect to another host does not carry the
    /// password with it.
    pub host: String,
    pub username: String,
    pub password: String,
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
    /// The most connections the governor may open to one server: a ceiling,
    /// not a target. It measures what this server and this link actually
    /// reward and settles on the fewest connections that go that fast, which
    /// is commonly well below this number.
    pub connections: u8,
    /// Smallest piece worth handing to a separate connection.
    pub min_split_size: String,
    /// Downloads running at once.
    pub max_concurrent: u8,
    /// Global cap in bytes/sec; 0 means unlimited.
    pub max_speed: u64,
    /// Per-download cap in bytes/sec; 0 means unlimited.
    pub max_speed_per_download: u64,
    pub retry_limit: u8,
    /// How to reach the internet.
    ///
    /// Empty means "the way everything else on this machine does": the system
    /// proxy settings, which is what a browser follows and what an office
    /// network configures centrally. A URL — `http://host:3128`,
    /// `socks5://host:1080`, either with `user:password@` in front of the host
    /// — overrides that. The word `off` ignores the system settings and goes
    /// direct, which is the only way to say "no proxy" on a machine that
    /// configures one.
    pub proxy: String,
    /// Logins for servers that ask for one, matched by host.
    ///
    /// A download captured from the browser already carries the browser's own
    /// `Authorization` header, so those need nothing here; this is for the
    /// links typed, pasted or queued by hand, which arrive with no session
    /// behind them.
    pub credentials: Vec<Credential>,
    /// Watch the clipboard for URLs and offer to download them.
    pub clipboard_watch: bool,
    /// Verify SHA-256 after completion.
    pub checksum: bool,
    pub notify: bool,
    pub start_minimized: bool,
    pub ytdlp_format: String,
    /// Browser yt-dlp should lift cookies from, in its `--cookies-from-browser`
    /// syntax (e.g. "firefox", "firefox:/path/to/profile", "chromium").
    /// Empty disables it.
    pub ytdlp_cookies_from: String,
    /// Extra flags for yt-dlp, e.g.
    /// `["--extractor-args", "youtube:player_client=web_safari"]`.
    /// YouTube's extraction changes often; this avoids needing a rebuild.
    pub ytdlp_extra_args: Vec<String>,
    /// Whether MDM keeps its own copy of yt-dlp current.
    ///
    /// Only ever its own copy — one installed by a package manager is that
    /// package manager's to update. Worth a switch because a pinned version
    /// is a real thing to want: a site can break in a new release as easily
    /// as it is fixed by one.
    pub ytdlp_auto_update: bool,
    /// Hosts that hand out an address good for exactly one request, so
    /// captures from them are left with the browser.
    ///
    /// Learned rather than configured: a capture arrives only after the
    /// browser has already asked for the file, so a one-time address is spent
    /// by the time MDM could fetch it and the second request gets the landing
    /// page. The first download from such a host is lost whatever we do; this
    /// is how the second one is not. Written by the engine when it recognises
    /// that failure, and listed in Settings so a host that landed here by
    /// accident can be taken out again.
    ///
    /// Matched like the extension's own site list: the host itself, or any
    /// subdomain of it.
    pub single_use_hosts: Vec<String>,
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
            min_split_size: "1M".into(),
            max_concurrent: 4,
            max_speed: 0,
            max_speed_per_download: 0,
            retry_limit: 5,
            proxy: String::new(),
            credentials: Vec::new(),
            clipboard_watch: false,
            checksum: false,
            notify: true,
            start_minimized: false,
            // Best picture, with one exception: a codec the desktop cannot
            // decode. TikTok offers the same video twice, 1080p in HEVC and
            // 720p in H.264, and prefers the HEVC — which is patent-encumbered
            // and so ships with no decoder on Fedora and most other distros.
            // The download succeeded, weighed the right seven megabytes and
            // played as a black screen with sound, which is indistinguishable
            // from "it only downloaded the audio". Fewer pixels that play beat
            // more that do not. Sites offering nothing else are unaffected:
            // the last branch is the old expression, so HEVC is still taken
            // where it is the only thing on offer.
            //
            // The branches are ordered so that both halves come from one
            // container family wherever that is possible: two MP4-family
            // streams are merged by `stream::mp4`, two WebM ones by
            // `stream::mkv`, and either way the whole download is MDM's own
            // work with no ffmpeg anywhere. A mix of the two would mean
            // translating a codec description from one container's way of
            // writing it to the other's, which is understanding rather than
            // copying — so it is avoided here instead, and only the last
            // branches, for a site that offers nothing better, fall back to
            // yt-dlp doing the merge.
            //
            // Picture quality is not traded for this: YouTube offers AV1 in
            // MP4 up to 2160p, so the first branch is not a lower ceiling
            // than the third.
            ytdlp_format: "bestvideo*[vcodec!*=hev][vcodec!*=h265][ext=mp4]+bestaudio[ext=m4a]/\
                           bestvideo*[vcodec!*=hev][vcodec!*=h265][ext=webm]+bestaudio[ext=webm]/\
                           bestvideo*[vcodec!*=hev][vcodec!*=h265]+bestaudio/\
                           best[vcodec!*=hev][vcodec!*=h265]/\
                           bestvideo*+bestaudio/best"
                .into(),
            // YouTube now refuses anonymous extraction on many videos with
            // "Sign in to confirm you're not a bot". The user is already
            // signed in in Firefox, which is where the request came from.
            ytdlp_cookies_from: "firefox".into(),
            ytdlp_extra_args: Vec::new(),
            ytdlp_auto_update: true,
            single_use_hosts: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::Job;

    /// Built the way one actually arrives — out of the extension's JSON —
    /// rather than by naming fields, so a job the nets could really send is
    /// what gets asked.
    fn from(source: &str) -> Job {
        serde_json::from_value(serde_json::json!({
            "url": "https://filekeeper.net/get/abc",
            "source": source,
        }))
        .expect("a job with a url and a source is a valid job")
    }

    /// The nets watch a response go past, so by the time they hand a job over
    /// the address has been asked and answered. Those are the ones a
    /// single-use host has to be refused for.
    #[test]
    fn a_capture_has_already_spent_the_link() {
        for source in ["webRequest", "downloads", "blob", "data"] {
            assert!(from(source).spends_the_link(), "{source} was treated as unspent");
        }
    }

    /// Nothing has requested a right-clicked link, so MDM's request is the
    /// first one and the file is there to be had. Refusing these was refusing
    /// the one thing that works on such a host.
    #[test]
    fn a_link_nothing_has_followed_is_still_good() {
        for source in ["menu", "cli", "ui", "preempt"] {
            assert!(!from(source).spends_the_link(), "{source} was treated as spent");
        }
    }

    /// A net added later is refused until somebody decides otherwise: the cost
    /// of guessing wrong one way is a download that could have been faster,
    /// and the other way is a download that is gone.
    #[test]
    fn an_unknown_source_is_assumed_to_have_asked() {
        assert!(from("").spends_the_link());
        assert!(from("something-added-in-2027").spends_the_link());
    }
}
