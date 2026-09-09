//! Tests for the pure decision logic: category routing, scheduling windows,
//! filename safety and the credentials a download is sent with.

use mdm_core::categories::{categorize, extension_of};
use mdm_core::engine::{
    authorization_for, filename_from_url, job_from_url, queue_open_at, sanitize, stem_taken,
    strip_userinfo, unique_filename, unique_name, wants_ytdlp,
};
use mdm_core::human_bytes;
use mdm_core::model::{Credential, Queue, Settings, Status};

/* ------------------------------- categories ------------------------------ */

#[test]
fn categorises_by_extension() {
    assert_eq!(categorize("movie.mkv", ""), "Video");
    assert_eq!(categorize("song.flac", ""), "Music");
    assert_eq!(categorize("paper.pdf", ""), "Documents");
    assert_eq!(categorize("source.tar.gz", ""), "Compressed");
    assert_eq!(categorize("app.AppImage", ""), "Programs");
    assert_eq!(categorize("photo.JPEG", ""), "Images");
    assert_eq!(categorize("mystery", ""), "Other");
}

#[test]
fn falls_back_to_mime_when_the_name_is_uninformative() {
    assert_eq!(categorize("download", "video/mp4"), "Video");
    assert_eq!(categorize("download", "audio/ogg"), "Music");
    assert_eq!(categorize("download", "application/pdf"), "Documents");
    assert_eq!(categorize("download", "application/zip"), "Compressed");
    assert_eq!(categorize("download", "application/x-rpm"), "Programs");
    assert_eq!(categorize("download", "image/webp"), "Images");
}

#[test]
fn extension_beats_mime_because_servers_lie() {
    // A server labelling an ISO as text/plain is common; the name is better
    // evidence than the header.
    assert_eq!(categorize("fedora.iso", "text/plain"), "Compressed");
}

#[test]
fn extension_of_rejects_junk() {
    assert_eq!(extension_of("a.zip"), "zip");
    assert_eq!(extension_of("archive.tar.gz"), "gz");
    assert_eq!(extension_of("no-extension"), "");
    assert_eq!(extension_of("trailing."), "");
    assert_eq!(extension_of(".hidden"), "");
    // Query strings are not extensions.
    assert_eq!(extension_of("file.php?a=1"), "");
}

/* -------------------------------- filenames ------------------------------ */

#[test]
fn sanitise_strips_path_components() {
    // A name is joined to the download folder, so anything that survives here
    // could write outside the folder the user chose.
    assert_eq!(sanitize("../../etc/passwd".into()), "passwd");
    assert_eq!(sanitize("/absolute/path.iso".into()), "path.iso");
    assert_eq!(sanitize("a/../b.zip".into()), "b.zip");
}

#[test]
fn sanitise_never_yields_a_traversable_or_hidden_name() {
    for input in [
        "../../etc/passwd",
        "..",
        "...",
        "/",
        ".bashrc",
        "\\\\server\\share\\x.dll",
    ] {
        let out = sanitize(input.to_string());
        assert!(!out.contains('/'), "separator survived in {out:?}");
        assert!(!out.starts_with('.'), "leading dot survived in {out:?}");
        assert!(!out.is_empty(), "empty name from {input:?}");
    }
}

#[test]
fn sanitise_replaces_control_characters() {
    assert_eq!(sanitize("a\nb\tc.zip".into()), "a_b_c.zip");
}

#[test]
fn sanitise_falls_back_when_nothing_usable_remains() {
    assert_eq!(sanitize("".into()), "download");
    assert_eq!(sanitize("...".into()), "download");
}

#[test]
fn sanitise_replaces_the_characters_windows_refuses() {
    // The report that started this: every YouTube title with a pipe in it
    // failed at `CreateFile` with os error 123, before a byte was fetched.
    assert_eq!(
        sanitize("To Rescue a Sinner Like Me | Quennie Benabaye (Cover).mp4".into()),
        "To Rescue a Sinner Like Me _ Quennie Benabaye (Cover).mp4"
    );
    assert_eq!(sanitize("what? <yes> \"x\" *.mp4".into()), "what_ _yes_ _x_ _.mp4");
    // A colon would also name an alternate data stream, not a file.
    assert_eq!(sanitize("9:41".into()), "9_41");
}

#[test]
fn sanitise_drops_what_windows_would_drop_silently() {
    // Windows creates "clip.mp4" for either of these and then cannot find the
    // name we recorded.
    assert_eq!(sanitize("clip.mp4.".into()), "clip.mp4");
    assert_eq!(sanitize("clip.mp4 ".into()), "clip.mp4");
}

#[test]
fn sanitise_steps_around_the_dos_device_names() {
    assert_eq!(sanitize("nul".into()), "_nul");
    assert_eq!(sanitize("CON.txt".into()), "_CON.txt");
    assert_eq!(sanitize("com9.mp4".into()), "_com9.mp4");
    // Only the whole stem is reserved; "console" is an ordinary name.
    assert_eq!(sanitize("console.log".into()), "console.log");
}

#[test]
fn a_long_name_is_cut_on_a_character_and_keeps_its_extension() {
    let out = sanitize(format!("{}.mp4", "あ".repeat(300)));
    assert!(out.len() <= 200, "still {} bytes: {out}", out.len());
    assert!(out.ends_with(".mp4"), "extension lost: {out}");
    // Cutting mid-character would have panicked on the way here.
    assert!(out.trim_end_matches(".mp4").chars().all(|c| c == 'あ'));
}

#[test]
fn filename_from_url_decodes_and_drops_the_query() {
    assert_eq!(filename_from_url("https://e.com/a/b/file.zip?sig=x"), "file.zip");
    assert_eq!(filename_from_url("https://e.com/my%20file.pdf"), "my file.pdf");
    assert_eq!(filename_from_url("https://e.com/"), "download");
    assert_eq!(filename_from_url("not a url"), "download");
    // A trailing slash must not yield an empty name.
    assert_eq!(filename_from_url("https://e.com/dir/"), "dir");
}

#[test]
fn a_stem_is_taken_by_anything_derived_from_it() {
    let present: Vec<String> = [
        "Hymns.webm",              // the muxed video already saved
        "Sermon.f251.webm.part",   // a download still running
        "Talk",                    // a row that has yet to write a byte
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // yt-dlp settles the container itself, so an audio-only pick would land on
    // `Hymns.webm` too — opus and an AV1+opus mux are both webm.
    assert!(stem_taken(&present, "Hymns"));
    assert!(stem_taken(&present, "Sermon"));
    assert!(stem_taken(&present, "Talk"));

    // A stem that merely shares a prefix is a different name.
    assert!(!stem_taken(&present, "Hymn"));
    assert!(!stem_taken(&present, "Hymns_audio"));
    assert!(!stem_taken(&present, "Talks"));
}

#[test]
fn a_free_name_is_left_exactly_as_it_is() {
    assert_eq!(unique_name("Hymns", |_| false), "Hymns");
}

#[test]
fn a_taken_name_is_counted_upwards() {
    // What an audio-only pick runs into: the video of the same page is already
    // saved under this stem, and yt-dlp would call the job finished rather
    // than fetch anything.
    let taken = ["Hymns", "Hymns_2"];
    assert_eq!(unique_name("Hymns", |name| taken.contains(&name)), "Hymns_3");
}

#[test]
fn a_predicate_that_never_yields_still_terminates() {
    let name = unique_name("Hymns", |_| true);
    assert!(name.starts_with("Hymns_"), "{name} kept the stem");
    assert_ne!(name, "Hymns_2", "and did not settle on a name it was told was taken");
}

#[test]
fn a_filename_is_numbered_before_its_extension() {
    // Asking for a second copy of something already saved is a fair thing to
    // ask; what must not happen is being answered with the file already there.
    let taken = ["fedora.iso", "fedora_2.iso"];
    assert_eq!(
        unique_filename("fedora.iso", |name| taken.contains(&name)),
        "fedora_3.iso",
        "the number belongs before the extension, or it stops being an ISO"
    );
    assert_eq!(unique_filename("fedora.iso", |_| false), "fedora.iso");
}

#[test]
fn numbering_copes_with_names_that_carry_no_extension() {
    assert_eq!(unique_filename("README", |name| name == "README"), "README_2");
    // A leading dot is a hidden file, not an extension to number in front of.
    assert_eq!(unique_filename(".bashrc", |name| name == ".bashrc"), ".bashrc_2");
}

/* -------------------------------- scheduler ------------------------------ */

fn window(start: u16, stop: u16) -> Queue {
    Queue {
        start_minute: Some(start),
        stop_minute: Some(stop),
        ..Queue::default()
    }
}

#[test]
fn a_queue_without_a_window_is_always_open() {
    assert!(queue_open_at(&Queue::default(), 0, 0));
    assert!(queue_open_at(&Queue::default(), 1439, 6));
}

#[test]
fn a_disabled_queue_is_never_open() {
    let q = Queue { enabled: false, ..Queue::default() };
    assert!(!queue_open_at(&q, 720, 2));
}

#[test]
fn same_day_window() {
    let q = window(9 * 60, 17 * 60); // 09:00-17:00
    assert!(!queue_open_at(&q, 8 * 60 + 59, 0));
    assert!(queue_open_at(&q, 9 * 60, 0));
    assert!(queue_open_at(&q, 16 * 60 + 59, 0));
    // The stop minute is exclusive, so 17:00 is already shut.
    assert!(!queue_open_at(&q, 17 * 60, 0));
}

#[test]
fn window_wrapping_past_midnight() {
    // The off-peak case: 23:00-06:00.
    let q = window(23 * 60, 6 * 60);
    assert!(queue_open_at(&q, 23 * 60, 0));
    assert!(queue_open_at(&q, 2 * 60, 0));
    assert!(queue_open_at(&q, 5 * 60 + 59, 0));
    assert!(!queue_open_at(&q, 6 * 60, 0));
    assert!(!queue_open_at(&q, 12 * 60, 0));
}

#[test]
fn day_restrictions_apply() {
    let mut q = window(9 * 60, 17 * 60);
    q.days = vec![5, 6]; // weekend only, 0 = Monday
    assert!(!queue_open_at(&q, 12 * 60, 0));
    assert!(queue_open_at(&q, 12 * 60, 5));
    assert!(queue_open_at(&q, 12 * 60, 6));
}

/* ------------------------------ yt-dlp or not ---------------------------- */

/// A media URL as the format picker hands one over: it has already asked
/// yt-dlp about every page it could find and been told no.
fn settled_media(url: &str) -> mdm_core::model::Job {
    let mut job = job_from_url(url);
    job.use_ytdlp = Some(false);
    job
}

#[test]
fn a_streaming_page_nobody_has_looked_at_goes_to_ytdlp() {
    assert!(wants_ytdlp(&job_from_url("https://www.tiktok.com/@someone/video/7123456789012345678")));
    assert!(wants_ytdlp(&job_from_url("https://youtu.be/dQw4w9WgXcQ")));
    assert!(!wants_ytdlp(&job_from_url("https://example.org/debian.iso")));
}

#[test]
fn a_caller_that_has_already_looked_is_not_overruled() {
    // The failure this is here for: the picker resolved nothing on TikTok and
    // offered the file the player was using, which lives on TikTok's own CDN.
    // Guessing from the host put yt-dlp in front of an mp4, and the download
    // died, in the downloader of the day, as "exited with code 16".
    let cdn = "https://v16-webapp.tiktok.com/ad2adb4e/6a9b818c/video/tos/alisg/tos-alisg-pv-0037/02ea";
    assert!(wants_ytdlp(&job_from_url(cdn)), "the host alone still reads as TikTok");
    assert!(!wants_ytdlp(&settled_media(cdn)), "an answer given outright must stand");
}

#[test]
fn a_page_asked_for_explicitly_goes_to_ytdlp_wherever_it_lives() {
    let mut job = job_from_url("https://example.org/watch/12345");
    job.use_ytdlp = Some(true);
    assert!(wants_ytdlp(&job));
}

#[test]
fn a_response_already_typed_as_media_is_not_a_page() {
    // The sniffer's route, where nobody states a preference but the server
    // has already said what the bytes are.
    let mut job = job_from_url("https://v16-webapp.tiktok.com/x/video/tos/alisg/y");
    job.mime = "video/mp4".into();
    assert!(!wants_ytdlp(&job));

    // A manifest is a description of a stream rather than the stream itself,
    // and it used to go to yt-dlp for that reason. It no longer does: the
    // stream downloader fetches the segments and remuxes them in process, so
    // an extractor is only involved if that fails and the retry says so.
    let mut manifest = job_from_url("https://www.tiktok.com/x/playlist.m3u8");
    manifest.mime = "application/vnd.apple.mpegurl".into();
    assert!(
        !wants_ytdlp(&manifest),
        "a manifest is downloaded natively now"
    );

    // Explicitly asking still wins, which is what the failure fallback and the
    // format picker both rely on.
    manifest.use_ytdlp = Some(true);
    assert!(wants_ytdlp(&manifest));
}

#[test]
fn a_streaming_page_is_still_not_a_manifest() {
    // The distinction the routing turns on: a watch page has no segments to
    // fetch and must still reach yt-dlp, even now that manifests do not.
    let page = job_from_url("https://www.tiktok.com/@someone/video/12345");
    assert!(wants_ytdlp(&page));
}

#[test]
fn status_round_trips_through_its_database_form() {
    for s in [
        Status::Queued,
        Status::Active,
        Status::Paused,
        Status::Complete,
        Status::Failed,
        Status::Removed,
    ] {
        assert_eq!(Status::parse(s.as_str()), s);
    }
}

#[test]
fn human_bytes_reads_sensibly() {
    assert_eq!(human_bytes(-1), "unknown");
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(1024), "1.0 KB");
    assert_eq!(human_bytes(1536), "1.5 KB");
    assert_eq!(human_bytes(20 * 1024 * 1024), "20 MB");
}

/* ------------------------------ control files ---------------------------- */

#[test]
fn a_transport_failure_is_said_in_words_a_user_can_act_on() {
    use mdm_core::ytdlp::plain_error;

    // Verbatim from a download that failed while the resolver was down: this
    // reached the UI as a clipped Python traceback under the progress bar.
    let raw = "ERROR: [youtube] 9w0y22_5nTU: Unable to download API page: \
               HTTPSConnection(host='www.youtube.com', port=443): Failed to \
               resolve 'www.youtube.com' ([Errno 11001] getaddrinfo failed) \
               (caused by TransportError(\"…\"))";
    let said = plain_error(raw);
    assert!(said.contains("www.youtube.com"), "{said}");
    assert!(said.contains("DNS"), "{said}");
    assert!(!said.contains("TransportError"), "{said}");

    // A failure we have nothing better to say about keeps yt-dlp's own words.
    let unknown = "ERROR: [youtube] something nobody has seen before";
    assert_eq!(plain_error(unknown), unknown);
}

#[test]
fn a_dropped_connection_is_transient_and_a_private_video_is_not() {
    use mdm_core::ytdlp::is_permanent_error;

    // The classification a rewritten message must not disturb: the DNS
    // failure above has to stay retryable, or the backoff never runs.
    assert!(!is_permanent_error("getaddrinfo failed"));
    assert!(!is_permanent_error("Connection refused"));
    assert!(is_permanent_error("ERROR: [youtube] xyz: Private video"));
}

/* --------------------------- pages are not files ------------------------- */

#[test]
fn a_page_response_is_recognised_from_its_failure() {
    // The engine matches on the marker rather than on the sentence, so the
    // wording stays free to change.
    let msg = format!(
        "{}: the server answered with a web page rather than a file (text/html)",
        mdm_core::fetch::PAGE_MARKER
    );
    assert!(mdm_core::fetch::is_page_response(&msg));
    assert!(!mdm_core::fetch::is_page_response("connection reset by peer"));
}

#[test]
fn a_player_page_does_not_become_a_php_download() {
    // A player page saved 1.5 MB of HTML under a name claiming to be a PHP
    // script. The extension is dropped so the row does not advertise one it
    // will never have — the extractor supplies the real name later.
    assert_eq!(
        filename_from_url("https://site.test/view_video.php?viewkey=abc123"),
        "view_video"
    );
    assert_eq!(filename_from_url("https://site.test/watch.aspx?v=1"), "watch");

    // A real file keeps its extension, including one that merely looks odd.
    assert_eq!(filename_from_url("https://site.test/debian.iso"), "debian.iso");
    assert_eq!(filename_from_url("https://site.test/a/archive.tar.gz"), "archive.tar.gz");
    // A dotfile-shaped name has no stem to keep, so it is left alone.
    assert_eq!(filename_from_url("https://site.test/.htm"), ".htm");
}

/* ------------------------------ request shape ---------------------------- */

#[test]
fn a_job_without_headers_still_names_itself() {
    // The bug this pins down: a URL added by hand carried no User-Agent, and
    // the sites that refuse an anonymous client do it by never answering — a
    // fifteen-second "connect" failure against a server that is up.
    let spec = mdm_core::fetch::Spec::new("https://example.test/f.bin", "/tmp");
    let headers = mdm_core::fetch::request_headers(&spec);
    assert!(
        headers.contains_key("user-agent"),
        "a bare job must still send a User-Agent"
    );
}

#[test]
fn a_captured_user_agent_is_never_overwritten() {
    // The browser's own header is the better answer, and replacing it is how a
    // working captured download would start failing.
    let mut spec = mdm_core::fetch::Spec::new("https://example.test/f.bin", "/tmp");
    spec.headers = vec![mdm_core::model::Header {
        name: "User-Agent".into(),
        value: "Firefox/from-the-browser".into(),
    }];
    let headers = mdm_core::fetch::request_headers(&spec);
    assert_eq!(
        headers.get("user-agent").unwrap(),
        "Firefox/from-the-browser"
    );
}

/* ------------------------------ credentials ------------------------------ */

fn header<'a>(job: &'a mdm_core::model::Job, name: &str) -> Option<&'a str> {
    job.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

#[test]
fn a_password_in_a_link_moves_out_of_the_url() {
    let job = strip_userinfo(job_from_url("https://user:s3cret@files.test/big.iso"));
    // What gets stored, logged and shown carries no password.
    assert_eq!(job.url, "https://files.test/big.iso");
    // dXNlcjpzM2NyZXQ= is "user:s3cret".
    assert_eq!(header(&job, "authorization"), Some("Basic dXNlcjpzM2NyZXQ="));
}

#[test]
fn a_username_with_no_password_still_authenticates() {
    // A token pasted as the username half is a real shape: "token@host".
    let job = strip_userinfo(job_from_url("https://tok3n@files.test/big.iso"));
    assert_eq!(job.url, "https://files.test/big.iso");
    assert_eq!(header(&job, "authorization"), Some("Basic dG9rM246"));
}

#[test]
fn a_link_without_credentials_is_left_exactly_as_it_was() {
    let job = strip_userinfo(job_from_url("https://files.test/big.iso?a=b#c"));
    assert_eq!(job.url, "https://files.test/big.iso?a=b#c");
    assert_eq!(header(&job, "authorization"), None);
}

#[test]
fn percent_encoded_credentials_are_decoded_before_they_are_sent() {
    // An "@" or ":" in a password has to be escaped to fit in a URL at all.
    let job = strip_userinfo(job_from_url("https://a%40b.test:p%3Aw@files.test/x.iso"));
    // YUBiLnRlc3Q6cDp3 is "a@b.test:p:w".
    assert_eq!(header(&job, "authorization"), Some("Basic YUBiLnRlc3Q6cDp3"));
}

#[test]
fn a_configured_login_is_found_by_host_and_only_by_host() {
    let mut settings = Settings::default();
    settings.credentials = vec![Credential {
        host: "Files.Test".into(),
        username: "user".into(),
        password: "s3cret".into(),
    }];

    // Case folds, as hostnames do.
    let found = authorization_for("https://files.test/big.iso", &settings);
    assert_eq!(found.map(|h| h.value), Some("Basic dXNlcjpzM2NyZXQ=".into()));

    // A neighbouring host under the same suffix is a different host, and
    // somebody else may own it.
    assert!(authorization_for("https://evil.test/big.iso", &settings).is_none());
    assert!(authorization_for("https://sub.files.test/big.iso", &settings).is_none());
    assert!(authorization_for("not a url", &settings).is_none());
}

#[test]
fn a_blank_host_matches_nothing_rather_than_everything() {
    let mut settings = Settings::default();
    settings.credentials = vec![Credential {
        host: "  ".into(),
        username: "user".into(),
        password: "s3cret".into(),
    }];
    assert!(authorization_for("https://files.test/big.iso", &settings).is_none());
}
