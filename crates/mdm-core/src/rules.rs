//! What to do about a download that failed, decided without doing any of it.
//!
//! A failure is eleven questions at once — was that a page or a file, is the
//! link spent, has the server refused us by name, is this worth trying again,
//! and what does the user get told — and the answers interleave with store
//! writes, a settings update, a directory removal and a desktop notification.
//! Interleaved, none of them can be tested without a database, a filesystem
//! and a tokio runtime, which is why the branch that blacklisted a host on one
//! unlucky page response went in unnoticed.
//!
//! So the questions are answered here and executed by the engine. Nothing in
//! this module touches the store, the filesystem, the network or the clock;
//! the one fact it cannot work out for itself — whether yt-dlp is installed —
//! is handed in.

use crate::model::{Download, Job, Queue, Settings};
use std::path::{Path, PathBuf};
use std::time::Duration;

/* ------------------------------------------------------------------ *
 * Hosts
 * ------------------------------------------------------------------ */

/// The host part of a URL, lowercased, or empty where there is not one.
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

/// Which listed host this URL falls under, if any.
///
/// Free of the engine so the decision can be tested as the decision it is: a
/// URL and a list in, a host or nothing out.
pub fn single_use_match(url: &str, hosts: &[String]) -> Option<String> {
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
pub fn host_matches(host: &str, pattern: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("*.").to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    host == pattern || host.ends_with(&format!(".{pattern}"))
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
pub fn cdn_signs_to_session(url: &str) -> bool {
    const SIGNED_CDN_HOSTS: &[&str] = &["tiktok.com", "tiktokcdn.com"];
    let host = host_of(url).trim_start_matches("www.").to_lowercase();
    SIGNED_CDN_HOSTS
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

/* ------------------------------------------------------------------ *
 * Where a download lands
 * ------------------------------------------------------------------ */

/// The name, folder and category a job should get, before anything is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub dir: PathBuf,
    pub filename: String,
    pub category: &'static str,
}

/// Work out where a file lands. Creating the folder, and making the name free
/// of what is already in it, are the engine's to do afterwards.
pub fn destination(job: &Job, settings: &Settings, use_ytdlp: bool) -> Destination {
    let filename = crate::naming::sanitize(if job.filename.is_empty() {
        crate::naming::filename_from_url(&job.url)
    } else {
        job.filename.clone()
    });
    let mut category = crate::categories::categorize(&filename, &job.mime);
    // A streaming page URL carries no extension — "youtu.be/<id>" has nothing
    // to categorise by — so it would land in "Other". It is a video download
    // by definition; an audio-only pick is re-filed once the real container is
    // known.
    if use_ytdlp && category == "Other" {
        category = "Video";
    }

    // An explicit directory from the UI always wins; auto-categorising a path
    // the user just picked would be surprising.
    let dir = match &job.directory {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ if settings.categorize => Path::new(&settings.download_dir).join(category),
        _ => PathBuf::from(&settings.download_dir),
    };

    Destination { dir, filename, category }
}

/// The folder a finished download belongs in, if that is not where it already
/// is. `None` leaves it alone.
///
/// Only ever a rename within the download root. Whether the file is really
/// there, and whether something already holds the name, are questions for the
/// filesystem and are asked by the caller — losing someone's download to a
/// name collision is unforgivable.
pub fn refile_to(d: &Download, settings: &Settings) -> Option<PathBuf> {
    if !settings.categorize {
        return None;
    }
    let wanted = Path::new(&settings.download_dir).join(&d.category);
    (Path::new(&d.directory) != wanted).then_some(wanted)
}

/* ------------------------------------------------------------------ *
 * Scheduling windows
 * ------------------------------------------------------------------ */

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

/// How many more of this queue's downloads may run, given how many already are.
pub fn free_slots(q: &Queue, running: usize) -> usize {
    usize::from(q.max_concurrent).saturating_sub(running)
}

/* ------------------------------------------------------------------ *
 * Classifying a failure
 * ------------------------------------------------------------------ */

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
pub fn refused_by_server(message: &str) -> bool {
    const REFUSED: &[u16] = &[401, 403, 429];
    message
        .split("answered ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|status| REFUSED.contains(&status))
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
pub fn retry_backoff(attempt: u8) -> Duration {
    const SCHEDULE: &[u64] = &[2, 5, 15, 30, 60];
    let idx = usize::from(attempt.saturating_sub(1)).min(SCHEDULE.len() - 1);
    Duration::from_secs(SCHEDULE[idx])
}

/* ------------------------------------------------------------------ *
 * The plan
 * ------------------------------------------------------------------ */

/// Whether this download gets another attempt, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Back to `Queued`, dispatched again after `after`.
    Retry { after: Duration, attempt: u8 },
    /// Nothing left to try; the row settles on `Failed`.
    Fail,
}

/// Everything a failure calls for, in the order the engine carries it out.
///
/// Not an enum: these are not alternatives. One failure can name a single-use
/// host, hand the row to yt-dlp *and* schedule a retry, and the bug worth
/// avoiding is exactly the one where making them exclusive drops two of the
/// three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// What the user is told, in the user's terms rather than the tool's. A
    /// Python transport traceback in a red strip under a progress bar reads as
    /// "the app is broken"; "the DNS lookup failed, check your connection" is
    /// the same fact and can be acted on.
    pub message: String,
    /// The address answered with a page where a file was asked for.
    pub was_page: bool,
    /// Failing identically every time: retrying only delays telling the user
    /// what actually went wrong.
    pub permanent: bool,
    /// Add this host to the single-use list, so the *next* download from it is
    /// asked for by MDM before the browser spends the link.
    pub stop_capturing: Option<String>,
    /// Persist `use_ytdlp`, so the retry that is already coming goes out
    /// through the extractor instead of the fetcher that just failed.
    pub switch_to_ytdlp: bool,
    /// Persist `no_native`: the server refused MDM's own fetch of a resolved
    /// format, and only the extractor's own request can redeem the address.
    pub disable_native: bool,
    /// Stems whose `.mdmstream` scratch directory the abandoned native attempt
    /// left behind, most specific first. The engine removes the first that is
    /// really there; the muxer clears these on success only.
    pub scratch_stems: Vec<String>,
    /// Drop the cached extraction. Handing the retry the same one would spend
    /// it on the identical signed URLs rather than fresh ones — the one thing
    /// a retry is supposed to change.
    pub forget_extraction: bool,
    /// A failure of this shape is the strongest evidence there is that the
    /// extractor has fallen behind the site — better evidence than any clock —
    /// so the daily check is brought forward.
    pub refresh_extractor: bool,
    pub next: Next,
}

/// Decide what a failed download calls for.
///
/// `attempts` counts this failure, so the first one arrives as 1.
/// `ytdlp_available` is the caller's: whether the binary is installed is not
/// something a pure function gets to find out.
pub fn on_failure(
    d: &Download,
    settings: &Settings,
    message: &str,
    attempts: u8,
    ytdlp_available: bool,
) -> Plan {
    // Classified on what the tool said, phrased in what the user needs:
    // rewriting first would mean deciding "is this worth retrying?" about a
    // sentence we wrote ourselves. Read before the message is rewritten, too —
    // the marker is in the raw text, and `plain_error` is under no obligation
    // to keep it.
    let was_page = crate::fetch::is_page_response(message);
    let refused = refused_by_server(message);

    // A page found where the browser had *watched a file arrive* is not a page
    // an extractor can read. It is a link that has been spent.
    //
    // File hosts hand out one-time addresses, and the capture sits at
    // `onHeadersReceived` — by the time it can divert anything the server has
    // already committed the file to the browser's request. Cancelling that
    // request and asking again asks for a token the server has finished with,
    // and what comes back is the landing page it hands anyone without one.
    // Confirmed by hand against the download in the bug report: the capture
    // recorded `video/webm`, 2.1 GB, status 200, and the same GET with the
    // same cookies now answers 200 `text/html`.
    //
    // Everything that used to happen next was wrong. yt-dlp was handed a file
    // host it has no extractor for, so five retries went by and the row
    // settled on "No extractor for filekeeper.net" — a true sentence about the
    // wrong tool, blaming the site for a link MDM had spent itself.
    let spent = was_page && crate::fetch::capture_saw_a_file(d.total_bytes, &d.mime);
    let permanent = crate::ytdlp::is_permanent_error(message) || spent;

    let site = host_of(&d.url);
    let text = if spent {
        // Remembered, not merely reported. The first download from a host like
        // this is lost however it is handled — the address was spent by the
        // browser's own request, before MDM was told the download existed — so
        // the only thing left to get right is the *next* one, and telling the
        // user to go and edit a list in the extension made that their job.
        let named = if site.is_empty() { "That site" } else { &site };
        format!(
            "{named} served this file once and now answers with a page — that \
             link was single-use. MDM will ask {named} before the browser does \
             from now on, so the next link is not spent before it can be \
             used; download it again."
        )
    } else if was_page {
        // The marker exists for the branch above and the one below, not for
        // the user.
        "that address is a web page, not a file — trying an extractor".to_string()
    } else {
        crate::ytdlp::plain_error(message)
    };

    // Two ways a URL turns out to be the extractor's job rather than ours,
    // both discovered by trying rather than guessed at up front:
    //
    //   * a manifest the stream downloader will not touch — encrypted, live,
    //     or a codec it cannot describe;
    //   * a URL that answered with a *page*. A site whose player sits at
    //     `view_video.php` is not on any host list we could keep current, and
    //     the response says what the address does not.
    //
    // Once only: the flag is persisted, so the next attempt sees a row that
    // has already moved. Never for a spent link — that address is not a page
    // an extractor can do anything with.
    let switch_to_ytdlp = !d.use_ytdlp
        && !spent
        && (was_page || crate::stream::looks_like_manifest(&d.url, &d.mime))
        && ytdlp_available;

    let disable_native = d.use_ytdlp && !d.no_native && refused;

    let next = if !permanent && attempts <= settings.retry_limit {
        Next::Retry { after: retry_backoff(attempts), attempt: attempts }
    } else {
        Next::Fail
    };
    let retrying = matches!(next, Next::Retry { .. });

    Plan {
        message: text,
        was_page,
        permanent,
        stop_capturing: spent.then_some(site),
        switch_to_ytdlp,
        disable_native,
        scratch_stems: if disable_native { scratch_stems(d) } else { Vec::new() },
        forget_extraction: retrying && d.use_ytdlp,
        refresh_extractor: retrying
            && d.use_ytdlp
            && settings.ytdlp_auto_update
            && crate::ytdlp::looks_out_of_date(message),
        next,
    }
}

/// The stems a native attempt could have opened a `.mdmstream` under, in the
/// order they are worth looking for. Pure: which of them exists is the
/// engine's to find out.
fn scratch_stems(d: &Download) -> Vec<String> {
    let mut stems = Vec::new();
    if let Some(name) = d.output_name.as_deref() {
        if !name.trim().is_empty() {
            stems.push(name.trim().to_string());
        }
    }
    if let Some(stem) = Path::new(&d.filename).file_stem().and_then(|s| s.to_str()) {
        stems.push(stem.to_string());
    }
    stems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::PAGE_MARKER;
    use crate::model::Status;

    /// A row as the engine holds one. Only the handful of fields a failure is
    /// judged on are ever set by a test; the rest are here because `Download`
    /// is what the store returns.
    fn download(url: &str) -> Download {
        Download {
            id: 1,
            url: url.to_string(),
            filename: "clip.mp4".into(),
            directory: "/downloads".into(),
            category: "Video".into(),
            status: Status::Active,
            total_bytes: 0,
            completed_bytes: 0,
            download_speed: 0,
            connections: 1,
            mime: String::new(),
            referrer: String::new(),
            headers: Vec::new(),
            error: None,
            sha256: None,
            created_at: 0,
            finished_at: None,
            queue: "Main".into(),
            use_ytdlp: false,
            no_native: false,
            output_name: None,
            format_id: None,
            mirrors: Vec::new(),
        }
    }

    /// What the fetcher writes when an address answered with a web page.
    fn page_failure() -> String {
        format!("{PAGE_MARKER}: the server answered with a web page")
    }

    fn settings() -> Settings {
        Settings::default()
    }

    /// A job as the extension sends one. Only `url` is ever required of it.
    fn job(url: &str) -> Job {
        Job {
            url: url.to_string(),
            mirrors: Vec::new(),
            filename: String::new(),
            size: -1,
            mime: String::new(),
            headers: Vec::new(),
            referrer: String::new(),
            cookie_store_id: String::new(),
            reason: String::new(),
            source: String::new(),
            directory: None,
            use_ytdlp: None,
            format_id: None,
            output_name: None,
            start_paused: false,
            data: None,
        }
    }

    fn queue(max_concurrent: u8) -> Queue {
        Queue {
            name: "Main".into(),
            enabled: true,
            start_minute: None,
            stop_minute: None,
            days: Vec::new(),
            max_concurrent,
        }
    }

    /* ------------------------------ destination ----------------------------- */

    /// A folder the user picked in the dialog is not second-guessed, however
    /// the file would otherwise have been categorised.
    #[test]
    fn a_folder_the_user_picked_wins() {
        let mut j = job("https://example.com/movie.mkv");
        j.directory = Some("/elsewhere/keep".into());
        let d = destination(&j, &settings(), false);
        assert_eq!(d.dir, PathBuf::from("/elsewhere/keep"));
        assert_eq!(d.category, "Video", "the category is still recorded");

        // An empty string is not a choice.
        j.directory = Some(String::new());
        assert_ne!(destination(&j, &settings(), false).dir, PathBuf::from(""));
    }

    /// A streaming page URL carries no extension to categorise by, so it would
    /// land in "Other" — but it is a video download by definition.
    #[test]
    fn a_stream_page_is_filed_as_video_rather_than_other() {
        let j = job("https://youtu.be/dQw4w9WgXcQ");
        assert_eq!(destination(&j, &settings(), false).category, "Other");
        assert_eq!(destination(&j, &settings(), true).category, "Video");

        // Only where there was nothing better: a known type keeps its own.
        let mut audio = job("https://example.com/song.flac");
        audio.mime = "audio/flac".into();
        assert_eq!(destination(&audio, &settings(), true).category, "Music");
    }

    /// With categorising off, everything lands in the download root.
    #[test]
    fn categorising_off_puts_everything_in_the_root() {
        let mut s = settings();
        s.download_dir = "/downloads".into();
        s.categorize = false;
        let d = destination(&job("https://example.com/movie.mkv"), &s, false);
        assert_eq!(d.dir, PathBuf::from("/downloads"));

        s.categorize = true;
        let d = destination(&job("https://example.com/movie.mkv"), &s, false);
        assert_eq!(d.dir, PathBuf::from("/downloads/Video"));
    }

    /// The name is made safe here, before anything is created with it.
    #[test]
    fn the_name_is_safe_before_a_folder_is_made_for_it() {
        let mut j = job("https://example.com/x");
        j.filename = "../../.bashrc".into();
        assert_eq!(destination(&j, &settings(), false).filename, "bashrc");
    }

    /* -------------------------------- refiling ------------------------------ */

    /// A download whose real type only became clear once it finished moves to
    /// the folder that type calls for — and only then.
    #[test]
    fn a_download_moves_only_when_it_is_in_the_wrong_folder() {
        let mut s = settings();
        s.download_dir = "/downloads".into();
        s.categorize = true;

        let mut d = download("https://example.com/x");
        d.category = "Music".into();
        d.directory = "/downloads/Video".into();
        assert_eq!(refile_to(&d, &s), Some(PathBuf::from("/downloads/Music")));

        // Already where it belongs.
        d.directory = "/downloads/Music".into();
        assert_eq!(refile_to(&d, &s), None);

        // And never when the user has categorising turned off.
        d.directory = "/downloads/Video".into();
        s.categorize = false;
        assert_eq!(refile_to(&d, &s), None);
    }

    /* ------------------------------- scheduling ----------------------------- */

    /// A queue already at its limit opens no slots, and one that is somehow
    /// over its limit must not wrap around into a very large number.
    #[test]
    fn a_full_queue_opens_no_slots() {
        assert_eq!(free_slots(&queue(4), 0), 4);
        assert_eq!(free_slots(&queue(4), 3), 1);
        assert_eq!(free_slots(&queue(4), 4), 0);
        assert_eq!(free_slots(&queue(4), 9), 0);
    }

    /* ------------------------------ spent links ----------------------------- */

    /// The bug this module was pulled out for. The browser watched 2.1 GB of
    /// `video/webm` arrive, MDM cancelled that request and asked again, and
    /// the one-time address answered with the host's landing page. That is not
    /// a page an extractor can read — it is a link MDM spent itself — so the
    /// host is remembered and yt-dlp is never handed a file host it has no
    /// extractor for.
    #[test]
    fn a_link_the_capture_spent_is_learned_rather_than_blamed() {
        let mut d = download("https://filekeeper.net/download/abc");
        d.total_bytes = 2_100_000_000;
        d.mime = "video/webm".into();

        let plan = on_failure(&d, &settings(), &page_failure(), 1, true);

        assert_eq!(plan.stop_capturing.as_deref(), Some("filekeeper.net"));
        assert!(!plan.switch_to_ytdlp, "a file host has no extractor to hand it to");
        assert!(plan.permanent, "the address is spent; asking again spends nothing");
        assert_eq!(plan.next, Next::Fail);
        assert!(
            plan.message.contains("single-use") && plan.message.contains("filekeeper.net"),
            "the user is told which site and why: {}",
            plan.message
        );
    }

    /// The other half of that bug, and the one that would be worse: a page
    /// response where the capture never saw a file is an ordinary player page.
    /// Blacklisting the host on one unlucky response would quietly stop MDM
    /// capturing a site that works.
    #[test]
    fn an_ordinary_page_response_does_not_blacklist_the_host() {
        for (bytes, mime) in [(0, ""), (0, "text/html"), (5_000, "text/html; charset=utf-8")] {
            let mut d = download("https://tube.example.com/view_video.php?id=9");
            d.total_bytes = bytes;
            d.mime = mime.into();

            let plan = on_failure(&d, &settings(), &page_failure(), 1, true);

            assert_eq!(plan.stop_capturing, None, "{bytes}/{mime} was read as spent");
            assert!(plan.switch_to_ytdlp, "{bytes}/{mime} should go to the extractor");
            assert!(!plan.permanent);
            assert!(matches!(plan.next, Next::Retry { .. }));
        }
    }

    /// Nothing is handed to an extractor that is not installed.
    #[test]
    fn a_page_stays_put_when_there_is_no_extractor() {
        let d = download("https://tube.example.com/view_video.php?id=9");
        let plan = on_failure(&d, &settings(), &page_failure(), 1, false);
        assert!(!plan.switch_to_ytdlp);
    }

    /// A manifest the built-in stream downloader will not touch is the other
    /// way a row turns out to be the extractor's job.
    #[test]
    fn a_manifest_it_cannot_take_goes_to_the_extractor() {
        let d = download("https://cdn.example.com/master.m3u8");
        let plan = on_failure(&d, &settings(), "encrypted segments", 1, true);
        assert!(plan.switch_to_ytdlp);

        // Once only: a row already on yt-dlp is not moved again.
        let mut moved = download("https://cdn.example.com/master.m3u8");
        moved.use_ytdlp = true;
        assert!(!on_failure(&moved, &settings(), "encrypted segments", 1, true).switch_to_ytdlp);
    }

    /* ------------------------------- refusals ------------------------------- */

    /// 401, 403 and 429 off the CDN say *this request* is not welcome. The
    /// address is signed to the extractor's own session, so the retry has to
    /// go out through yt-dlp itself.
    #[test]
    fn a_refusal_moves_a_ytdlp_row_off_the_native_path() {
        for status in [401, 403, 429] {
            let mut d = download("https://v19-webapp-prime.tiktok.com/video/tos/x");
            d.use_ytdlp = true;
            let message = format!("connection 2 answered {status}");

            let plan = on_failure(&d, &settings(), &message, 1, true);

            assert!(plan.disable_native, "{status} should abandon the native fetch");
            assert_eq!(plan.scratch_stems, vec!["clip".to_string()]);
        }
    }

    /// A 404 says the address is dead, not that we are unwelcome. yt-dlp has
    /// to re-find it either way, and the native path is no worse than it was.
    #[test]
    fn a_dead_address_is_not_a_refusal() {
        let mut d = download("https://cdn.example.com/v.mp4");
        d.use_ytdlp = true;
        assert!(!on_failure(&d, &settings(), "https://cdn.example.com/v.mp4 answered 404", 1, true).disable_native);

        // Nor is a status inside some other sentence.
        assert!(!refused_by_server("403 forbidden reading the config file"));
    }

    /// Set from the first refusal, and only then: the flag is persisted, so a
    /// row that already carries it has nothing to write.
    #[test]
    fn the_native_path_is_only_abandoned_once() {
        let mut d = download("https://cdn.example.com/v.mp4");
        d.use_ytdlp = true;
        d.no_native = true;
        assert!(!on_failure(&d, &settings(), "connection 1 answered 403", 1, true).disable_native);

        // And never for a row that was never on yt-dlp's turf.
        let plain = download("https://cdn.example.com/v.mp4");
        assert!(!on_failure(&plain, &settings(), "connection 1 answered 403", 1, true).disable_native);
    }

    /// The name the user chose is tried before the one on disk: a muxing run
    /// opens its scratch under the output name, and the two differ whenever
    /// the user renamed the row.
    #[test]
    fn the_chosen_name_is_the_first_scratch_looked_for() {
        let mut d = download("https://cdn.example.com/v.mp4");
        d.use_ytdlp = true;
        d.output_name = Some("  Holiday  ".into());
        let plan = on_failure(&d, &settings(), "connection 1 answered 403", 1, true);
        assert_eq!(plan.scratch_stems, vec!["Holiday".to_string(), "clip".to_string()]);

        // A blank one is not a name.
        d.output_name = Some("   ".into());
        let plan = on_failure(&d, &settings(), "connection 1 answered 403", 1, true);
        assert_eq!(plan.scratch_stems, vec!["clip".to_string()]);
    }

    /// Nothing to clear unless the native attempt was actually abandoned.
    #[test]
    fn no_scratch_is_cleared_without_a_refusal() {
        let d = download("https://cdn.example.com/v.mp4");
        assert!(on_failure(&d, &settings(), "connection reset", 1, true).scratch_stems.is_empty());
    }

    /* -------------------------------- retries ------------------------------- */

    /// Five attempts by default, then the row settles. The count arrives
    /// having already counted this failure, so the limit is inclusive.
    #[test]
    fn retries_run_out_at_the_limit() {
        let d = download("https://example.com/big.iso");
        let s = settings();
        assert_eq!(s.retry_limit, 5);

        for attempt in 1..=5 {
            let plan = on_failure(&d, &s, "connection reset", attempt, true);
            assert!(
                matches!(plan.next, Next::Retry { attempt: a, .. } if a == attempt),
                "attempt {attempt} should still be retried"
            );
            assert!(plan.message.contains("connection reset") || !plan.message.is_empty());
        }
        assert_eq!(on_failure(&d, &s, "connection reset", 6, true).next, Next::Fail);
    }

    /// A missing format or a private video fails identically every time.
    #[test]
    fn a_permanent_failure_is_not_retried() {
        let d = download("https://youtube.com/watch?v=x");
        for message in ["Private video", "Requested format is not available", "Video unavailable"] {
            let plan = on_failure(&d, &settings(), message, 1, true);
            assert!(plan.permanent, "{message} should be permanent");
            assert_eq!(plan.next, Next::Fail, "{message} should not be retried");
        }
    }

    /// The schedule has to span the outages a retry exists for — seconds to
    /// minutes — and hold there rather than growing without bound.
    #[test]
    fn the_backoff_spans_an_outage_and_then_holds() {
        let secs: Vec<u64> = (1..=7).map(|n| retry_backoff(n).as_secs()).collect();
        assert_eq!(secs, vec![2, 5, 15, 30, 60, 60, 60]);
        assert_eq!(retry_backoff(0).as_secs(), 2, "a zeroth attempt must not underflow");
    }

    /* ------------------------------- extractor ------------------------------ */

    /// Whatever extraction produced the URLs this attempt failed on, handing
    /// the retry that same cached one would spend it on the identical signed
    /// URLs — the one thing a retry is supposed to change.
    #[test]
    fn a_retry_of_a_ytdlp_row_drops_the_cached_extraction() {
        let mut d = download("https://youtube.com/watch?v=x");
        d.use_ytdlp = true;
        assert!(on_failure(&d, &settings(), "connection reset", 1, true).forget_extraction);

        // Nothing is cached for a row the fetcher owns, and nothing is
        // forgotten for a row that is not being tried again.
        let plain = download("https://example.com/big.iso");
        assert!(!on_failure(&plain, &settings(), "connection reset", 1, true).forget_extraction);
        assert!(!on_failure(&d, &settings(), "Private video", 1, true).forget_extraction);
    }

    /// A failure that reads as staleness brings the daily check forward —
    /// better evidence than any clock. Only where the user allows updates,
    /// and only where another attempt is actually coming.
    #[test]
    fn a_stale_looking_failure_brings_the_check_forward() {
        let mut d = download("https://youtube.com/watch?v=x");
        d.use_ytdlp = true;
        let stale = "ERROR: unable to extract player response";

        assert!(on_failure(&d, &settings(), stale, 1, true).refresh_extractor);

        let mut off = settings();
        off.ytdlp_auto_update = false;
        assert!(!on_failure(&d, &off, stale, 1, true).refresh_extractor);

        assert!(
            !on_failure(&d, &settings(), stale, 99, true).refresh_extractor,
            "a row that has stopped retrying has nothing to bring forward"
        );
        assert!(!on_failure(&d, &settings(), "connection reset", 1, true).refresh_extractor);
    }

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

    /// Session-signed media hosts must never be fetched by a fresh connection.
    #[test]
    fn session_signed_cdn_hosts_are_recognised() {
        for url in [
            "https://v19-webapp-prime.tiktok.com/video/tos/alisg/ok8Q9Afi2wSa1Smq4EAiI8A4BJIbioCOB2urcV/?tk=tt_chain_token",
            "https://v16-webapp-prime.tiktok.com/video/tos/abcd.mp4",
            "https://example.tiktokcdn.com/video/tos/xyz.mp4",
        ] {
            assert!(
                cdn_signs_to_session(url),
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
                !cdn_signs_to_session(url),
                "{url} should stay on the native path"
            );
        }
    }
}
