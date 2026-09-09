//! The HLS path, against a real stream over the real network.
//!
//! Every other test here is pure logic, which is the right default: a test that
//! needs the internet is a test that fails for reasons that are nothing to do
//! with this code. But the failure these exist for was not a logic failure. The
//! parser was right, the fetcher was right, and Download still came back with a
//! single six-second segment — because what the app was handed was a segment,
//! and nothing downstream is in a position to notice that a video is one slice
//! of the video it should have been.
//!
//! So these run end to end, and they are `#[ignore]`d so that an ordinary
//! `cargo test` never reaches the network:
//!
//!     cargo test -p mdm-core --test hls -- --ignored --nocapture
//!
//! The stream is Mux's published HLS test asset, which exists to be fetched by
//! things like this.

use mdm_core::fetch::{Event, Spec};
use mdm_core::stream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A multi-variant master playlist: several renditions, each its own media
/// playlist, which is the shape a player on a streaming page is given.
const MASTER: &str = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8";

/// Served as `audio/mpegurl` despite carrying video — a real header from a real
/// CDN, and the reason `looks_like_manifest` cannot go by the leading type.
#[test]
fn a_video_master_served_as_audio_is_still_a_manifest() {
    assert!(stream::looks_like_manifest(MASTER, "audio/mpegurl"));
    assert!(stream::looks_like_manifest(MASTER, ""));
    // The case the failure was actually about: a segment is not a manifest, and
    // saying so is what stops one being downloaded as though it were the video.
    assert!(!stream::looks_like_manifest(
        "https://example.com/99rR47vp5TCOI3yKaKyr6MbOH55_027NPX3Qo1",
        "video/mp2t"
    ));
}

/// The whole path: fetch a master, pick a rendition, fetch its segments,
/// remux into one MP4.
#[tokio::test]
#[ignore = "needs the network"]
async fn downloads_a_master_playlist_into_one_mp4() {
    let dir = std::env::temp_dir().join(format!("mdm-hls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut spec = Spec::new(MASTER, &dir);
    spec.filename = Some("mux-test.mp4".into());

    let (tx, mut rx) = mpsc::channel(64);
    let stop = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(stream::download(spec, tx, stop));

    let mut last = 0u64;
    while let Some(event) = rx.recv().await {
        if let Event::Progress(p) = event {
            last = p.downloaded;
        }
    }
    let out = task.await.unwrap().expect("the stream downloader failed");

    let size = std::fs::metadata(&out).unwrap().len();
    println!("{} -> {} bytes ({last} reported)", out.display(), size);

    // The bug this guards: one HLS segment of this asset is a couple of MB, and
    // a "download" that produced one looked like a success everywhere except on
    // the disk. The whole rendition is an order of magnitude larger.
    assert!(
        size > 10 * 1024 * 1024,
        "{} is {size} bytes — that is a segment, not the stream",
        out.display()
    );
    // An MP4, not a raw transport stream: the remux is the half that makes the
    // result playable, and a .ts renamed to .mp4 would pass a size check.
    let head = std::fs::read(&out).unwrap();
    assert_eq!(&head[4..8], b"ftyp", "the output is not an MP4");

    std::fs::remove_dir_all(&dir).ok();
}
