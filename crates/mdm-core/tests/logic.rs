//! Tests for the pure decision logic: category routing, scheduling windows,
//! filename safety and the options handed to aria2.

use mdm_core::aria2::{AddOptions, MAX_CONNECTIONS};
use mdm_core::categories::{categorize, extension_of};
use mdm_core::engine::{
    filename_from_url, queue_open_at, sanitize, stem_taken, unique_filename, unique_name,
};
use mdm_core::human_bytes;
use mdm_core::model::{Header, Queue, Status};

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
    // aria2 resolves `out` against `dir` and creates parents, so anything that
    // survives here could write outside the download folder.
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

/* ------------------------------ aria2 options ---------------------------- */

fn options() -> AddOptions {
    AddOptions {
        dir: "/tmp/dl".into(),
        out: Some("file.iso".into()),
        headers: vec![
            Header { name: "Cookie".into(), value: "session=abc".into() },
            Header { name: "User-Agent".into(), value: "Mozilla/5.0".into() },
        ],
        referer: Some("https://example.com/page".into()),
        connections: 16,
        split: 16,
        min_split_size: "1M".into(),
        max_speed: 0,
        retry_limit: 5,
        paused: false,
        extra: Vec::new(),
    }
}

#[test]
fn headers_are_rendered_in_aria2_wire_format() {
    let json = options().to_json();
    let headers = json["header"].as_array().expect("header array");
    assert_eq!(headers[0], "Cookie: session=abc");
    assert_eq!(headers[1], "User-Agent: Mozilla/5.0");
}

#[test]
fn connections_are_clamped_to_what_the_rpc_accepts() {
    // aria2's RPC rejects >16 for this option in both addUri and
    // changeGlobalOption, even though its command line takes 128 and --help
    // advertises "1-*". Exceeding it fails every download, so pin the value.
    assert_eq!(MAX_CONNECTIONS, 16);

    let mut o = options();
    o.connections = 200;
    assert_eq!(o.to_json()["max-connection-per-server"], "16");

    o.connections = 0;
    // Zero is not a valid aria2 value; it must come back as at least one.
    assert_eq!(o.to_json()["max-connection-per-server"], "1");
}

#[test]
fn resume_is_requested_and_silent_truncation_is_refused() {
    let json = options().to_json();
    assert_eq!(json["continue"], "true");
    // always-resume=false lets aria2 fail loudly on a server that ignores
    // Range, rather than quietly producing a short file.
    assert_eq!(json["always-resume"], "false");
    assert_eq!(json["allow-overwrite"], "false");
    assert_eq!(json["auto-file-renaming"], "true");
}

#[test]
fn speed_limit_is_omitted_when_unlimited() {
    assert!(options().to_json().get("max-download-limit").is_none());
    let mut o = options();
    o.max_speed = 512 * 1024;
    assert_eq!(o.to_json()["max-download-limit"], "524288");
}

#[test]
fn pause_flag_is_only_present_when_set() {
    assert!(options().to_json().get("pause").is_none());
    let mut o = options();
    o.paused = true;
    assert_eq!(o.to_json()["pause"], "true");
}

#[test]
fn an_empty_referer_is_not_sent() {
    let mut o = options();
    o.referer = Some(String::new());
    assert!(o.to_json().get("referer").is_none());
}

/* --------------------------------- misc ---------------------------------- */

#[test]
fn aria2_status_strings_map_onto_ours() {
    assert_eq!(Status::from_aria2("active"), Status::Active);
    assert_eq!(Status::from_aria2("waiting"), Status::Queued);
    assert_eq!(Status::from_aria2("paused"), Status::Paused);
    assert_eq!(Status::from_aria2("complete"), Status::Complete);
    assert_eq!(Status::from_aria2("error"), Status::Failed);
    assert_eq!(Status::from_aria2("removed"), Status::Removed);
    // Anything unrecognised must not look finished.
    assert_eq!(Status::from_aria2("something-new"), Status::Queued);
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
fn control_file_appends_rather_than_replacing_the_extension() {
    use mdm_core::engine::control_file;
    use std::path::Path;

    assert_eq!(
        control_file(Path::new("/d/debian.iso")),
        Path::new("/d/debian.iso.aria2")
    );
    // The bug this guards: with_extension would give "/d/archive.tar..aria2"
    // for a dotted name and "/d/noext..aria2" for one with no extension.
    assert_eq!(
        control_file(Path::new("/d/archive.tar.gz")),
        Path::new("/d/archive.tar.gz.aria2")
    );
    assert_eq!(
        control_file(Path::new("/d/noext")),
        Path::new("/d/noext.aria2")
    );
}
