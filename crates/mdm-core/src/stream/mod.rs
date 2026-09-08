//! Segmented streams, downloaded and remuxed in this process.
//!
//! HLS and DASH are the one thing a plain HTTP downloader cannot do: the URL
//! names a *manifest*, and the video is behind it as hundreds or thousands of
//! separate files, sometimes with the picture and the sound in two parallel
//! sets. Until now that meant handing the job to yt-dlp, which in turn meant
//! ffmpeg to put the two halves back together.
//!
//! This module does both natively. It is deliberately *format*-driven rather
//! than site-driven: it understands manifests, not websites, so it needs no
//! per-site extractor and does not rot when a site is redesigned. What it
//! cannot do is find a manifest that a page hides behind its own JavaScript —
//! that is exactly the work yt-dlp exists for, and those URLs still go there.
//!
//! The pipeline:
//!
//!   1. Fetch the manifest and parse it ([`hls`], [`dash`]).
//!   2. Choose a rendition — the best picture, plus the sound that goes with
//!      it when they are served apart.
//!   3. Fetch every segment, a window at a time, appending each track to one
//!      file in playback order.
//!   4. Rebuild the result as a single MP4 ([`mp4::remux`]), demuxing first if
//!      the segments were transport stream rather than fragmented MP4 ([`ts`]).

pub mod dash;
pub mod hls;
pub mod mkv;
pub mod mp4;
pub mod ts;

use crate::fetch::{Event, Progress, Spec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use mp4::Kind;

/// Where a stream's half-finished tracks live, beside the eventual output.
const WORK_SUFFIX: &str = ".mdmstream";

/// How many segments to have in flight at once.
///
/// Segments are small and numerous, so the win here is hiding round trips
/// rather than splitting one file. Beyond a handful the gain flattens and the
/// risk of being rate-limited does not.
const WINDOW: usize = 8;

/// How many times one segment is retried before the download gives up.
const SEGMENT_RETRIES: u32 = 4;

/// Whether this URL is worth handing to the stream downloader.
///
/// Kept deliberately narrow. A guess that says yes wrongly costs a manifest
/// fetch and a fall back to yt-dlp; the real decision is made by looking at
/// what comes back, in [`download`].
pub fn looks_like_manifest(url: &str, mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if matches!(
        mime.as_str(),
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
            | "application/dash+xml"
    ) {
        return true;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u") || path.ends_with(".mpd")
}

/// One piece to fetch, from either manifest flavour.
#[derive(Debug, Clone)]
struct Part {
    url: String,
    /// `(offset, length)`, for the byte-range playlists that serve a whole
    /// rendition as slices of one file.
    range: Option<(u64, u64)>,
}

impl From<&hls::Segment> for Part {
    fn from(s: &hls::Segment) -> Self {
        Part {
            url: s.uri.clone(),
            range: s.range,
        }
    }
}

impl From<&dash::Part> for Part {
    fn from(p: &dash::Part) -> Self {
        Part {
            url: p.url.clone(),
            range: p.range,
        }
    }
}

/// One track to fetch: its init segment, if any, then its media segments.
struct Plan {
    kind: Kind,
    init: Option<Part>,
    parts: Vec<Part>,
    /// Only used to estimate the finished size before anything is fetched.
    bandwidth: u64,
}

/// What the manifest turned out to describe.
struct Streams {
    tracks: Vec<Plan>,
    /// Total media length in seconds, where the manifest said.
    duration: f64,
    label: String,
}

/// Resume bookkeeping: how many segments of each track are already on disk,
/// and how long that made the file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Keyed by track index, in the same order the plan built them.
    done: Vec<usize>,
    bytes: Vec<u64>,
    /// The manifest this state belongs to. A stream re-published under the
    /// same name is a different stream, and resuming into it would splice two
    /// videos together.
    source: String,
}

/* ------------------------------------------------------------------ *
 * Manifest handling
 * ------------------------------------------------------------------ */

async fn text(client: &reqwest::Client, url: &str) -> Result<(String, String)> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered {}", response.status());
    }
    // The address after redirects, because every relative URI in the manifest
    // resolves against wherever it actually came from.
    let landed = response.url().to_string();
    let body = response.text().await.context("reading the manifest")?;
    Ok((body, landed))
}

/// Pick the best rendition a master playlist offers, and the audio to go with it.
async fn plan_hls(client: &reqwest::Client, body: &str, url: &str) -> Result<Streams> {
    if hls::is_master(body) {
        let master = hls::parse_master(body, url)?;
        let best = master
            .variants
            .iter()
            .max_by_key(|v| (v.height.unwrap_or(0) as u64, v.bandwidth))
            .context("the master playlist offered no rendition")?;

        let (video_body, video_url) = text(client, &best.uri).await?;
        let video = hls::parse_media(&video_body, &video_url)?;
        if video.live {
            bail!("this is a live stream, which has no end to download to");
        }
        let duration: f64 = video.segments.iter().map(|s| s.duration).sum();

        let mut tracks = vec![Plan {
            kind: Kind::Video,
            init: video.init.as_ref().map(Part::from),
            parts: video.segments.iter().map(Part::from).collect(),
            bandwidth: best.bandwidth,
        }];

        // A rendition that names an audio group carries no sound of its own.
        if let Some(group) = &best.audio_group {
            let pick = master
                .media
                .iter()
                .filter(|m| m.kind.eq_ignore_ascii_case("AUDIO") && &m.group == group)
                .find(|m| m.default)
                .or_else(|| {
                    master
                        .media
                        .iter()
                        .find(|m| m.kind.eq_ignore_ascii_case("AUDIO") && &m.group == group)
                });
            if let Some(uri) = pick.and_then(|m| m.uri.as_ref()) {
                let (audio_body, audio_url) = text(client, uri).await?;
                let audio = hls::parse_media(&audio_body, &audio_url)?;
                tracks.push(Plan {
                    kind: Kind::Audio,
                    init: audio.init.as_ref().map(Part::from),
                    parts: audio.segments.iter().map(Part::from).collect(),
                    bandwidth: 128_000,
                });
            }
        }

        let label = match (best.width, best.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => format!("{} kbps", best.bandwidth / 1000),
        };
        return Ok(Streams {
            tracks,
            duration,
            label,
        });
    }

    // A media playlist straight away: one track, sound and picture together.
    let media = hls::parse_media(body, url)?;
    if media.live {
        bail!("this is a live stream, which has no end to download to");
    }
    let duration = media.segments.iter().map(|s| s.duration).sum();
    Ok(Streams {
        tracks: vec![Plan {
            kind: Kind::Video,
            init: media.init.as_ref().map(Part::from),
            parts: media.segments.iter().map(Part::from).collect(),
            bandwidth: 0,
        }],
        duration,
        label: "stream".into(),
    })
}

fn plan_dash(body: &str, url: &str) -> Result<Streams> {
    let reps = dash::parse(body, url)?;
    let video = reps
        .iter()
        .filter(|r| r.kind == Kind::Video)
        .max_by_key(|r| (r.height.unwrap_or(0) as u64, r.bandwidth));
    let audio = reps
        .iter()
        .filter(|r| r.kind == Kind::Audio)
        .max_by_key(|r| r.bandwidth);

    let mut tracks = Vec::new();
    if let Some(v) = video {
        tracks.push(Plan {
            kind: Kind::Video,
            init: v.init.as_ref().map(Part::from),
            parts: v.segments.iter().map(Part::from).collect(),
            bandwidth: v.bandwidth,
        });
    }
    if let Some(a) = audio {
        tracks.push(Plan {
            kind: Kind::Audio,
            init: a.init.as_ref().map(Part::from),
            parts: a.segments.iter().map(Part::from).collect(),
            bandwidth: a.bandwidth,
        });
    }
    if tracks.is_empty() {
        bail!("the manifest offered no audio or video to download");
    }

    let label = video
        .and_then(|v| Some(format!("{}x{}", v.width?, v.height?)))
        .unwrap_or_else(|| "audio".into());
    Ok(Streams {
        tracks,
        // DASH states its duration on the manifest, which `dash::parse`
        // already used to count segments; recovering it here would mean
        // parsing twice, and the estimate below copes without it.
        duration: 0.0,
        label,
    })
}

/* ------------------------------------------------------------------ *
 * Downloading
 * ------------------------------------------------------------------ */

async fn fetch_part(
    client: &reqwest::Client,
    part: &Part,
    stop: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut last = None;
    for attempt in 0..SEGMENT_RETRIES {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped");
        }
        if attempt > 0 {
            // Brief, and growing: a CDN that just refused one segment of a
            // thousand is usually a moment from being fine again.
            tokio::time::sleep(Duration::from_millis(300 * (1 << attempt.min(4)))).await;
        }
        let mut request = client.get(&part.url);
        if let Some((offset, length)) = part.range {
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", offset, offset + length - 1),
            );
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                match response.bytes().await {
                    Ok(bytes) => return Ok(bytes.to_vec()),
                    Err(e) => last = Some(anyhow::anyhow!("{e}")),
                }
            }
            Ok(response) => last = Some(anyhow::anyhow!("answered {}", response.status())),
            Err(e) => last = Some(anyhow::anyhow!("{e}")),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("unreachable")))
        .with_context(|| format!("fetching segment {}", part.url))
}

/// Fetch one track's parts in order, appending to `path`.
///
/// A window of requests is in flight at once, but they are *written* strictly
/// in order: a stream spliced together out of order is not a video.
#[allow(clippy::too_many_arguments)]
async fn fetch_track(
    client: &reqwest::Client,
    plan: &Plan,
    path: &Path,
    skip: usize,
    downloaded: &mut u64,
    total: Option<u64>,
    tx: &mpsc::Sender<Event>,
    stop: &Arc<AtomicBool>,
    started: Instant,
    // `+ Send` is load-bearing: this future is handed to `tokio::spawn`, and a
    // bare `dyn FnMut` held across an await makes the whole future unspawnable.
    on_progress: &mut (dyn FnMut(usize, u64) + Send),
) -> Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;

    // The init segment is part of the file's identity rather than of its
    // progress, and it is only written on a fresh start.
    if skip == 0 {
        if let Some(init) = &plan.init {
            let bytes = fetch_part(client, init, stop).await?;
            file.write_all(&bytes)?;
            *downloaded += bytes.len() as u64;
        }
    }

    let mut index = skip;
    while index < plan.parts.len() {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped");
        }
        let end = (index + WINDOW).min(plan.parts.len());
        let mut window = Vec::with_capacity(end - index);
        for part in &plan.parts[index..end] {
            let client = client.clone();
            let part = part.clone();
            let stop = stop.clone();
            window.push(tokio::spawn(
                async move { fetch_part(&client, &part, &stop).await },
            ));
        }
        for handle in window {
            let bytes = handle.await.context("a segment task failed")??;
            file.write_all(&bytes)?;
            *downloaded += bytes.len() as u64;
            index += 1;
            let written = file.metadata().map(|m| m.len()).unwrap_or(0);
            on_progress(index, written);
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let _ = tx
                .send(Event::Progress(Progress {
                    downloaded: *downloaded,
                    total,
                    speed: (*downloaded as f64 / elapsed) as u64,
                    connections: WINDOW as u64,
                }))
                .await;
        }
    }
    file.flush()?;
    Ok(())
}

/// Whether a track file is fragmented MP4 rather than transport stream.
fn is_fragmented(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 12];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    matches!(&head[4..8], b"ftyp" | b"styp" | b"moof" | b"moov")
}

/* ---------------------------------------------------------------------- *
 * Merging streams that are already whole files
 * ---------------------------------------------------------------------- */

/// One stream of a download that arrives as plain files rather than segments.
#[derive(Debug, Clone)]
pub struct Stream {
    pub url: String,
    /// What the CDN expects on a request for it, headers and all.
    pub headers: Vec<crate::model::Header>,
    /// What it weighs, where whoever resolved it said so.
    pub size: Option<u64>,
    /// The container it arrives in, as a file extension. Only consulted
    /// when there is nothing to merge and the stream *is* the download, so
    /// that a lone WebM is not saved under a name claiming to be MP4.
    pub ext: String,
}

/// Fetch a set of whole-file streams and rebuild them as one MP4.
///
/// This is the merge ffmpeg was being carried for. A site that serves picture
/// and sound as two adaptive streams — which is every YouTube format above
/// 360p — hands back two fragmented MP4 files, and putting them together is a
/// container rebuild, not a transcode: [`mp4::remux`] copies the samples and
/// their `stsd` entries across untouched. It is the same rebuild [`download`]
/// ends with; only the fetching differs, because these arrive as files rather
/// than as thousands of segments, and so are fetched by the ordinary
/// downloader with all of its connections, resume and throttling.
///
/// The weight of the whole job is known before the first byte, which the
/// segmented path can only estimate — so the bar is scaled correctly from the
/// start and does not step when the second stream begins.
pub async fn merge(
    spec: Spec,
    streams: Vec<Stream>,
    tx: mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
) -> Result<PathBuf> {
    if streams.is_empty() {
        bail!("nothing to merge");
    }
    let base = spec.filename.clone().unwrap_or_else(|| "video".into());
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(&base).to_string();
    let output = spec.dir.join(format!("{stem}.mp4"));
    let work = spec.dir.join(format!("{stem}{WORK_SUFFIX}"));
    std::fs::create_dir_all(&work)
        .with_context(|| format!("creating {}", work.display()))?;

    // Stated rather than estimated: every part weighed itself at extraction.
    let total: Option<u64> = streams.iter().map(|p| p.size).sum();

    let mut files = Vec::new();
    // What the streams already finished came to, so the bar counts the job
    // rather than restarting at each stream — the same reason the yt-dlp path
    // banks its finished streams.
    let mut done: u64 = 0;

    for (i, part) in streams.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped");
        }
        // A stream this already has, whole, from an attempt that was paused
        // or that failed later on. Fetching it again would be the price of
        // every resume: these addresses are signed and a new extraction
        // hands back different ones, so the fetcher cannot recognise its own
        // partial file across attempts — but a *finished* part is finished
        // whatever address it came from, and the weight it was promised is
        // what says so.
        let already = work.join(format!("part{i}.mp4"));
        if let (Some(size), Ok(meta)) = (part.size, std::fs::metadata(&already)) {
            if meta.len() == size {
                log::info!("stream {} of {} is already here", i + 1, streams.len());
                done += size;
                files.push(already);
                let _ = tx
                    .send(Event::Progress(Progress {
                        downloaded: done,
                        total,
                        speed: 0,
                        connections: 0,
                    }))
                    .await;
                continue;
            }
        }

        let mut one = spec.clone();
        one.url = part.url.clone();
        // Mirrors belong to the page's own URL, not to a CDN stream of it.
        one.mirrors.clear();
        one.dir = work.clone();
        one.filename = Some(format!("part{i}.mp4"));
        one.expected_sha256 = None;
        if !part.headers.is_empty() {
            one.headers = part.headers.clone();
        }

        // The streams run one after another, so their progress is folded into
        // one running count here rather than reported as several downloads.
        let (ptx, mut prx) = mpsc::channel(16);
        let out = tx.clone();
        let offset = done;
        let relay = tokio::spawn(async move {
            while let Some(event) = prx.recv().await {
                // Only progress crosses: `Probed` describes one stream and
                // `Done` would announce a part as the finished download.
                let forwarded = match event {
                    Event::Progress(p) => Event::Progress(Progress {
                        downloaded: offset + p.downloaded,
                        total: total.or(p.total.map(|t| offset + t)),
                        speed: p.speed,
                        connections: p.connections,
                    }),
                    Event::Concurrency { connections, speed } => {
                        Event::Concurrency { connections, speed }
                    }
                    _ => continue,
                };
                if out.send(forwarded).await.is_err() {
                    break;
                }
            }
        });

        let path = crate::fetch::download(one, ptx, stop.clone())
            .await
            .with_context(|| format!("fetching stream {} of {}", i + 1, streams.len()))?;
        let _ = relay.await;
        done += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        files.push(path);
    }

    if stop.load(Ordering::Relaxed) {
        bail!("stopped");
    }

    // One part is already the file it was going to be: a progressive format
    // needs no rebuild, and copying it through the remux would be work with
    // nothing to show for it.
    if files.len() == 1 {
        let only = files.remove(0);
        let ext = streams[0].ext.trim().trim_start_matches('.');
        let output = if ext.is_empty() || ext == "mp4" {
            output
        } else {
            spec.dir.join(format!("{stem}.{ext}"))
        };
        if output.exists() {
            std::fs::remove_file(&output).ok();
        }
        std::fs::rename(&only, &output)
            .with_context(|| format!("moving {} into place", only.display()))?;
        let _ = std::fs::remove_dir_all(&work);
        let _ = tx.send(Event::Done(output.clone())).await;
        return Ok(output);
    }

    // Which rebuild this is comes from what actually arrived rather than
    // from what was expected: a file says what it is in its first four
    // bytes, which is a better authority than a container name a site wrote
    // into a JSON document.
    let output = if files.iter().all(|f| mkv::is_matroska(f)) {
        let output = spec.dir.join(format!("{stem}.webm"));
        let sources = files
            .iter()
            .map(|path| mkv::read(path).with_context(|| format!("reading {}", path.display())))
            .collect::<Result<Vec<_>>>()?;
        log::info!("merging {} stream(s) into {}", sources.len(), output.display());
        mkv::remux(&sources, &output).context("rebuilding the streams as one WebM")?;
        output
    } else {
        let mut sources = Vec::new();
        for path in &files {
            let tracks = mp4::read_fragmented(path)
                .with_context(|| format!("reading {}", path.display()))?;
            sources.push(mp4::Source { path: path.clone(), tracks });
        }
        log::info!("merging {} stream(s) into {}", sources.len(), output.display());
        mp4::remux(&mut sources, &output).context("rebuilding the streams as one MP4")?;
        output
    };

    // Only once the output exists, so a failed merge leaves the fetched
    // streams where a retry can pick them up rather than fetching them again.
    let _ = std::fs::remove_dir_all(&work);
    let _ = tx.send(Event::Done(output.clone())).await;
    Ok(output)
}

/// Fetch and rebuild a segmented stream. The returned path is the finished file.
pub async fn download(
    spec: Spec,
    tx: mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
) -> Result<PathBuf> {
    let started = Instant::now();
    let client = crate::fetch::stream_client(&spec).await?;

    let (body, landed) = text(&client, &spec.url).await?;
    let streams = if hls::is_playlist(&body) {
        plan_hls(&client, &body, &landed).await?
    } else if body.trim_start().starts_with('<') {
        plan_dash(&body, &landed)?
    } else {
        bail!("this URL is not an HLS or DASH manifest");
    };

    // Named for what it will be, not for what it is: the container is settled
    // by the remux, and every path this takes ends in MP4.
    let base = spec
        .filename
        .clone()
        .unwrap_or_else(|| "stream".into());
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(&base).to_string();
    let output = spec.dir.join(format!("{stem}.mp4"));
    let work = spec.dir.join(format!("{stem}{WORK_SUFFIX}"));
    std::fs::create_dir_all(&work)
        .with_context(|| format!("creating {}", work.display()))?;

    log::info!(
        "{}: {} track(s) at {}, {} segments",
        spec.url,
        streams.tracks.len(),
        streams.label,
        streams.tracks.iter().map(|t| t.parts.len()).sum::<usize>()
    );

    // An estimate, and said to be one: the true size is not known until every
    // segment has arrived, and a progress bar that never moves is worse than
    // one that is approximately right.
    let total = if streams.duration > 0.0 {
        let bits: u64 = streams.tracks.iter().map(|t| t.bandwidth).sum();
        (bits > 0).then(|| (bits as f64 * streams.duration / 8.0) as u64)
    } else {
        None
    };

    let state_path = work.join("state.json");
    let mut state: State = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|s: &State| s.source == spec.url)
        .unwrap_or_default();
    state.done.resize(streams.tracks.len(), 0);
    state.bytes.resize(streams.tracks.len(), 0);
    state.source = spec.url.clone();

    let mut downloaded: u64 = state.bytes.iter().sum();
    let mut track_files = Vec::new();

    for (i, plan) in streams.tracks.iter().enumerate() {
        let name = match plan.kind {
            Kind::Video => "video.bin",
            Kind::Audio => "audio.bin",
        };
        let path = work.join(name);
        track_files.push(path.clone());

        // Trim back to exactly what the state vouches for. A file longer than
        // that ends mid-segment, and appending to it would splice a partial
        // segment into the middle of the stream.
        if state.done[i] > 0 && path.exists() {
            let file = std::fs::OpenOptions::new().write(true).open(&path)?;
            file.set_len(state.bytes[i])?;
        } else {
            let _ = std::fs::remove_file(&path);
            state.done[i] = 0;
            state.bytes[i] = 0;
        }

        let skip = state.done[i].min(plan.parts.len());
        if skip > 0 {
            log::info!("resuming track {i} at segment {skip}/{}", plan.parts.len());
        }
        let mut save = |done: usize, bytes: u64| {
            state.done[i] = done;
            state.bytes[i] = bytes;
            if let Ok(text) = serde_json::to_string(&state) {
                let _ = std::fs::write(&state_path, text);
            }
        };
        fetch_track(
            &client,
            plan,
            &path,
            skip,
            &mut downloaded,
            total,
            &tx,
            &stop,
            started,
            &mut save,
        )
        .await?;
    }

    if stop.load(Ordering::Relaxed) {
        bail!("stopped");
    }

    // Rebuild. Transport stream has to be demuxed into samples first; a
    // fragmented MP4 already is samples, and only needs indexing.
    // Mutable because the remux normalises composition offsets in place.
    let mut sources = Vec::new();
    let mut demuxed = Vec::new();
    for path in &track_files {
        if is_fragmented(path) {
            let tracks = mp4::read_fragmented(path)
                .with_context(|| format!("reading {}", path.display()))?;
            sources.push(mp4::Source {
                path: path.clone(),
                tracks,
            });
        } else {
            let payloads = path.with_extension("es");
            let tracks = ts::demux(path, &payloads)
                .with_context(|| format!("demuxing {}", path.display()))?;
            demuxed.push(payloads.clone());
            sources.push(mp4::Source {
                path: payloads,
                tracks,
            });
        }
    }

    log::info!("remuxing {} track file(s) into {}", sources.len(), output.display());
    mp4::remux(&mut sources, &output).context("rebuilding the stream as MP4")?;

    // Only once the output exists: a failed remux leaves everything in place
    // so the next attempt resumes rather than starting over.
    let _ = std::fs::remove_dir_all(&work);
    for path in demuxed {
        let _ = std::fs::remove_file(path);
    }

    let _ = tx.send(Event::Done(output.clone())).await;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_are_recognised_by_type_or_by_extension() {
        assert!(looks_like_manifest("https://x.test/a.m3u8", ""));
        assert!(looks_like_manifest("https://x.test/a.mpd?token=1", ""));
        assert!(looks_like_manifest(
            "https://x.test/playlist",
            "application/vnd.apple.mpegurl; charset=utf-8"
        ));
        assert!(looks_like_manifest("https://x.test/m", "application/dash+xml"));
    }

    #[test]
    fn an_ordinary_file_is_not_a_manifest() {
        // The query string carrying ".m3u8" must not be enough: this is an
        // mp4, and routing it through the stream path would fetch a manifest
        // that is not there.
        assert!(!looks_like_manifest("https://x.test/v.mp4?from=a.m3u8", "video/mp4"));
        assert!(!looks_like_manifest("https://x.test/v.mp4", "video/mp4"));
    }
}
