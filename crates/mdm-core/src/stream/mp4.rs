//! MP4 boxes: enough to read a fragmented stream and write a plain file.
//!
//! This is the piece that replaces `ffmpeg -c copy` for the one job MDM
//! actually asks of it. A DASH or HLS stream arrives as a fragmented MP4 — an
//! init segment carrying the track description, then many `moof`/`mdat` pairs
//! carrying the samples — and often as *two* of them, a picture track and a
//! sound track fetched separately. Turning that into one file everything can
//! play is a remux: the encoded samples are copied untouched and only the
//! container around them is rebuilt.
//!
//! Nothing here decodes or re-encodes anything, which is what makes it
//! tractable. In particular the `stsd` sample entry — `avc1` with its `avcC`,
//! `mp4a` with its `esds`, `hvc1` with its `hvcC` — is lifted out of the init
//! segment and written into the output byte for byte. Whatever codec it
//! describes, we never have to understand it.
//!
//! The output is a plain progressive MP4 with `moov` in front of `mdat`
//! ("faststart"), because a file that plays before it has finished copying is
//! worth the extra measuring pass it costs.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/* ------------------------------------------------------------------ *
 * Reading
 * ------------------------------------------------------------------ */

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

/// Walks the boxes laid out end to end in one buffer.
///
/// Every container box in MP4 has the same shape, so one iterator serves for
/// `moov`, `trak`, `stbl` and the rest. A malformed length ends the walk
/// rather than panicking: these files come off the open internet.
struct Atoms<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Atoms<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + 8 > self.buf.len() {
            return None;
        }
        let size32 = be32(self.buf, self.pos) as u64;
        let typ = [
            self.buf[self.pos + 4],
            self.buf[self.pos + 5],
            self.buf[self.pos + 6],
            self.buf[self.pos + 7],
        ];
        // 1 means the real size is the 64-bit field that follows the type;
        // 0 means "to the end of the enclosing box".
        let (size, header) = match size32 {
            1 => {
                if self.pos + 16 > self.buf.len() {
                    return None;
                }
                (be64(self.buf, self.pos + 8), 16usize)
            }
            0 => ((self.buf.len() - self.pos) as u64, 8),
            n => (n, 8),
        };
        if size < header as u64 || self.pos as u64 + size > self.buf.len() as u64 {
            return None;
        }
        let start = self.pos + header;
        let end = self.pos + size as usize;
        self.pos = end;
        Some((typ, &self.buf[start..end]))
    }
}

fn atoms(buf: &[u8]) -> Atoms<'_> {
    Atoms { buf, pos: 0 }
}

/// The payload of the first child box of this type.
fn find<'a>(buf: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    atoms(buf).find(|(t, _)| t == typ).map(|(_, p)| p)
}

/// Follow a chain of single children, e.g. `minf/stbl/stsd`.
fn path<'a>(buf: &'a [u8], names: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let mut cur = buf;
    for name in names {
        cur = find(cur, name)?;
    }
    Some(cur)
}

/// Whether a track carries picture or sound. Anything else (subtitles, timed
/// metadata) is dropped: the point here is a playable file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

/// One encoded frame, located but not read.
///
/// Sample data stays on disk until the output is written. A two-hour stream
/// has millions of these, and holding the bytes as well as the index would
/// cost gigabytes for no gain.
#[derive(Debug, Clone)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// In the track's own timescale.
    pub duration: u32,
    /// Composition-time offset: how far presentation sits from decode.
    /// Non-zero only where B-frames reorder the stream.
    pub cto: i32,
    /// A sync sample — one a player may seek to and start decoding from.
    pub sync: bool,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: u32,
    pub kind: Kind,
    pub timescale: u32,
    /// Stored as 16.16 fixed point in `tkhd`; kept here as whole pixels.
    pub width: u16,
    pub height: u16,
    /// The `stsd` entry, verbatim from the init segment. Copying it is what
    /// keeps this codec-agnostic.
    pub sample_entry: Vec<u8>,
    pub samples: Vec<Sample>,
    /// How much media to skip at the start, in this track's timescale.
    ///
    /// This is an AAC encoder's priming: the first frame or so of an audio
    /// track is decoder warm-up that was never meant to be heard, and the
    /// input signals that by trimming it in an edit list rather than by
    /// leaving it out. Dropping the edit list on the floor makes the sound
    /// start ~23 ms before the picture — small, constant, and exactly what
    /// "the audio is slightly out of sync" sounds like.
    edit_start: u64,
    /// A genuine gap before this track begins, in seconds. Written as an
    /// "empty edit", and the only way a stream can say its sound starts after
    /// its picture.
    edit_delay: f64,
    /// The first fragment's `tfdt` baseMediaDecodeTime, in this track's
    /// timescale.
    ///
    /// This is where the track's media sits on the presentation timeline, and
    /// two tracks fetched separately do not have to agree on it. Assuming both
    /// start at zero silently discards the difference — which is a fixed A/V
    /// offset in the finished file, and the reason this is read at all.
    media_start: Option<u64>,
    /// `trex` defaults, applied wherever a fragment declines to repeat them.
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
}

impl Track {
    /// Total media time, in the track's own timescale.
    pub fn duration(&self) -> u64 {
        self.samples.iter().map(|s| s.duration as u64).sum()
    }

    /// When the last frame stops being on screen, in the track's timescale.
    ///
    /// Not the same as [`duration`](Self::duration) on a reordered stream:
    /// decode order ends before presentation does, because the final frames
    /// are displayed later than they are decoded. An edit list measured
    /// against the decode total therefore cuts the tail off — two frames, in
    /// the case that found this.
    fn presentation_end(&self) -> u64 {
        let mut dts: i64 = 0;
        let mut end: i64 = 0;
        for s in &self.samples {
            end = end.max(dts + s.cto as i64 + s.duration as i64);
            dts += s.duration as i64;
        }
        end.max(0) as u64
    }

    /// Construct a track from samples that came from somewhere other than a
    /// fragmented MP4 — the transport-stream demuxer, which builds its own
    /// sample entry because a TS carries no container to copy one from.
    pub fn synthetic(
        id: u32,
        kind: Kind,
        timescale: u32,
        width: u16,
        height: u16,
        sample_entry: Vec<u8>,
        samples: Vec<Sample>,
    ) -> Self {
        Track {
            id,
            kind,
            timescale: timescale.max(1),
            width,
            height,
            sample_entry,
            samples,
            // A transport stream carries no edit list; its AAC priming, where
            // it has any, is not signalled and cannot be trimmed. Its samples
            // are built on one timeline already, so there is no start to
            // reconcile either.
            edit_start: 0,
            edit_delay: 0.0,
            media_start: None,
            default_duration: 0,
            default_size: 0,
            default_flags: 0,
        }
    }
}

/// A parsed input and the file its sample offsets point into.
pub struct Source {
    pub path: PathBuf,
    pub tracks: Vec<Track>,
}

/// Read a fragmented MP4 — an init segment followed by its media segments,
/// concatenated — into a sample index.
///
/// Only `moov` and each `moof` are read into memory; `mdat` is skipped over.
/// That keeps the memory cost proportional to the number of samples rather
/// than to the size of the video.
pub fn read_fragmented(file: &Path) -> Result<Vec<Track>> {
    let mut f = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let total = f.metadata()?.len();
    let mut tracks: Vec<Track> = Vec::new();
    let mut pos = 0u64;

    while pos + 8 <= total {
        f.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        if f.read(&mut header[..8])? < 8 {
            break;
        }
        let size32 = be32(&header, 0) as u64;
        let typ = [header[4], header[5], header[6], header[7]];
        let (size, header_len) = match size32 {
            1 => {
                f.read_exact(&mut header[8..16])?;
                (be64(&header, 8), 16u64)
            }
            0 => (total - pos, 8),
            n => (n, 8),
        };
        if size < header_len || pos + size > total {
            break;
        }

        match &typ {
            b"moov" => {
                let mut buf = vec![0u8; (size - header_len) as usize];
                f.read_exact(&mut buf)?;
                tracks = parse_moov(&buf);
            }
            b"moof" => {
                let mut buf = vec![0u8; (size - header_len) as usize];
                f.read_exact(&mut buf)?;
                parse_moof(&buf, pos, &mut tracks);
            }
            // `mdat` is where the bytes are, and it is precisely what we do
            // not want to read. `styp`, `sidx` and the rest carry nothing the
            // output needs.
            _ => {}
        }
        pos += size;
    }

    if tracks.is_empty() {
        bail!("no usable track found — this is not a fragmented MP4");
    }
    tracks.retain(|t| !t.samples.is_empty());
    if tracks.is_empty() {
        bail!("the init segment described tracks, but no fragment carried samples");
    }
    Ok(tracks)
}

fn parse_moov(moov: &[u8]) -> Vec<Track> {
    // Needed to read an edit list: its durations are in movie time, its
    // media_time in track time, and only the first needs converting.
    let movie_timescale = find(moov, b"mvhd")
        .and_then(|mvhd| {
            let at = if mvhd.first().copied().unwrap_or(0) == 1 { 20 } else { 12 };
            (mvhd.len() >= at + 4).then(|| be32(mvhd, at))
        })
        .unwrap_or(1000)
        .max(1);

    let mut tracks = Vec::new();
    for (typ, trak) in atoms(moov) {
        if &typ == b"trak" {
            if let Some(track) = parse_trak(trak, movie_timescale) {
                tracks.push(track);
            }
        }
    }

    // `trex` holds the defaults a fragment may omit. Applied here rather than
    // at each fragment so the per-sample code has one source of truth.
    if let Some(mvex) = find(moov, b"mvex") {
        for (typ, trex) in atoms(mvex) {
            if &typ != b"trex" || trex.len() < 24 {
                continue;
            }
            let id = be32(trex, 4);
            if let Some(t) = tracks.iter_mut().find(|t| t.id == id) {
                t.default_duration = be32(trex, 12);
                t.default_size = be32(trex, 16);
                t.default_flags = be32(trex, 20);
            }
        }
    }
    tracks
}

/// Read `edts/elst` into "skip this much media" and "start this late".
///
/// Two entry shapes matter. An entry with `media_time` of -1 is an *empty*
/// edit — a gap before the track starts. The first entry with a real
/// `media_time` says where in the media the presentation actually begins, and
/// that is what trims an encoder's priming.
fn parse_edits(trak: &[u8], movie_timescale: u32) -> (u64, f64) {
    let Some(elst) = path(trak, [b"edts", b"elst"].as_slice()) else {
        return (0, 0.0);
    };
    if elst.len() < 8 {
        return (0, 0.0);
    }
    let version = elst[0];
    let count = be32(elst, 4) as usize;
    let width = if version == 1 { 20 } else { 12 };
    let mut delay = 0f64;
    let mut at = 8;
    for _ in 0..count {
        if elst.len() < at + width {
            break;
        }
        let (duration, media_time) = if version == 1 {
            (be64(elst, at), be64(elst, at + 8) as i64)
        } else {
            (be32(elst, at) as u64, be32(elst, at + 4) as i32 as i64)
        };
        if media_time < 0 {
            delay += duration as f64 / movie_timescale as f64;
        } else {
            return (media_time as u64, delay);
        }
        at += width;
    }
    (0, delay)
}

fn parse_trak(trak: &[u8], movie_timescale: u32) -> Option<Track> {
    let tkhd = find(trak, b"tkhd")?;
    if tkhd.len() < 24 {
        return None;
    }
    // Version 1 widens the timestamps, moving the track id; width and height
    // are the last eight bytes either way.
    let id = if tkhd.first().copied().unwrap_or(0) == 1 {
        be32(tkhd, 20)
    } else {
        be32(tkhd, 12)
    };
    let dims_at = tkhd.len().checked_sub(8)?;
    let width = (be32(tkhd, dims_at) >> 16) as u16;
    let height = (be32(tkhd, dims_at + 4) >> 16) as u16;

    let mdia = find(trak, b"mdia")?;
    let mdhd = find(mdia, b"mdhd")?;
    let timescale = if mdhd.first().copied().unwrap_or(0) == 1 {
        if mdhd.len() < 24 {
            return None;
        }
        be32(mdhd, 20)
    } else {
        if mdhd.len() < 16 {
            return None;
        }
        be32(mdhd, 12)
    };

    let hdlr = find(mdia, b"hdlr")?;
    let kind = match hdlr.get(8..12)? {
        b"vide" => Kind::Video,
        b"soun" => Kind::Audio,
        // Subtitles and timed metadata are dropped rather than carried: they
        // are not what was asked for, and an unrecognised sample entry copied
        // into the output is a way to make the whole file unplayable.
        _ => return None,
    };

    let stsd = path(mdia, [b"minf", b"stbl", b"stsd"].as_slice())?;
    // Full box: version and flags, then the entry count, then the entries.
    let (etyp, epayload) = atoms(stsd.get(8..)?).next()?;
    // Rebuilt rather than sliced, because the iterator hands back the payload
    // without the header the output has to reproduce.
    let mut sample_entry = Vec::with_capacity(epayload.len() + 8);
    sample_entry.extend_from_slice(&((epayload.len() + 8) as u32).to_be_bytes());
    sample_entry.extend_from_slice(&etyp);
    sample_entry.extend_from_slice(epayload);

    let (edit_start, edit_delay) = parse_edits(trak, movie_timescale);

    Some(Track {
        id,
        kind,
        timescale: timescale.max(1),
        width,
        height,
        sample_entry,
        samples: Vec::new(),
        edit_start,
        edit_delay,
        media_start: None,
        default_duration: 0,
        default_size: 0,
        default_flags: 0,
    })
}

/// Fold one fragment's samples into the tracks built from `moov`.
///
/// `moof_at` is the fragment's own offset in the file, which is what sample
/// offsets are relative to unless the fragment says otherwise.
fn parse_moof(moof: &[u8], moof_at: u64, tracks: &mut [Track]) {
    for (typ, traf) in atoms(moof) {
        if &typ != b"traf" {
            continue;
        }
        let Some(tfhd) = find(traf, b"tfhd") else {
            continue;
        };
        if tfhd.len() < 8 {
            continue;
        }
        let flags = be32(tfhd, 0) & 0x00ff_ffff;
        let track_id = be32(tfhd, 4);
        let Some(track) = tracks.iter_mut().find(|t| t.id == track_id) else {
            continue;
        };

        // Only the first fragment's is kept: later ones simply track playback
        // and say nothing new about where the track begins.
        if track.media_start.is_none() {
            if let Some(tfdt) = find(traf, b"tfdt") {
                track.media_start = match tfdt.first().copied().unwrap_or(0) {
                    1 if tfdt.len() >= 12 => Some(be64(tfdt, 4)),
                    0 if tfdt.len() >= 8 => Some(be32(tfdt, 4) as u64),
                    _ => None,
                };
            }
        }

        let mut at = 8;
        // The spec's default, and what `default-base-is-moof` asks for too:
        // offsets are measured from the first byte of this `moof`.
        let mut base = moof_at;
        if flags & 0x01 != 0 {
            if tfhd.len() < at + 8 {
                continue;
            }
            base = be64(tfhd, at);
            at += 8;
        }
        if flags & 0x02 != 0 {
            at += 4; // sample-description-index, always 1 for these inputs
        }
        let mut default_duration = track.default_duration;
        if flags & 0x08 != 0 {
            if tfhd.len() < at + 4 {
                continue;
            }
            default_duration = be32(tfhd, at);
            at += 4;
        }
        let mut default_size = track.default_size;
        if flags & 0x10 != 0 {
            if tfhd.len() < at + 4 {
                continue;
            }
            default_size = be32(tfhd, at);
            at += 4;
        }
        let mut default_flags = track.default_flags;
        if flags & 0x20 != 0 {
            if tfhd.len() < at + 4 {
                continue;
            }
            default_flags = be32(tfhd, at);
        }

        for (ttyp, trun) in atoms(traf) {
            if &ttyp != b"trun" || trun.len() < 8 {
                continue;
            }
            let tflags = be32(trun, 0) & 0x00ff_ffff;
            let count = be32(trun, 4) as usize;
            let mut p = 8;
            let mut cursor = base;
            if tflags & 0x0001 != 0 {
                if trun.len() < p + 4 {
                    continue;
                }
                cursor = base.wrapping_add_signed(be32(trun, p) as i32 as i64);
                p += 4;
            }
            let mut first_flags = None;
            if tflags & 0x0004 != 0 {
                if trun.len() < p + 4 {
                    continue;
                }
                first_flags = Some(be32(trun, p));
                p += 4;
            }

            for i in 0..count {
                let mut duration = default_duration;
                let mut size = default_size;
                let mut sflags = if i == 0 {
                    first_flags.unwrap_or(default_flags)
                } else {
                    default_flags
                };
                let mut cto = 0i32;
                if tflags & 0x0100 != 0 {
                    if trun.len() < p + 4 {
                        return;
                    }
                    duration = be32(trun, p);
                    p += 4;
                }
                if tflags & 0x0200 != 0 {
                    if trun.len() < p + 4 {
                        return;
                    }
                    size = be32(trun, p);
                    p += 4;
                }
                if tflags & 0x0400 != 0 {
                    if trun.len() < p + 4 {
                        return;
                    }
                    sflags = be32(trun, p);
                    p += 4;
                }
                if tflags & 0x0800 != 0 {
                    if trun.len() < p + 4 {
                        return;
                    }
                    // Version 1 made this signed; reading it as `i32` either
                    // way is right, because version 0 files never set the top
                    // bit on a legitimate offset.
                    cto = be32(trun, p) as i32;
                    p += 4;
                }
                track.samples.push(Sample {
                    offset: cursor,
                    size,
                    duration,
                    cto,
                    // Bit 16 is `sample_is_non_sync_sample`, so the sense is
                    // inverted: unset means this frame can be seeked to.
                    sync: sflags & 0x0001_0000 == 0,
                });
                cursor += size as u64;
            }
        }
    }
}

/* ------------------------------------------------------------------ *
 * Writing
 * ------------------------------------------------------------------ */

/// The movie header's timescale. Milliseconds: fine enough that no track's
/// duration is visibly wrong, coarse enough never to overflow.
const MOVIE_TIMESCALE: u32 = 1000;

/// How much media time goes into one chunk.
///
/// Chunking is what interleaves the tracks: a second of video, then the second
/// of audio that plays alongside it, and so on. Too fine and the sample tables
/// balloon; too coarse and a player streaming the file has to buffer a long
/// way ahead to find the sound that goes with the picture.
const CHUNK_SECONDS: f64 = 1.0;

fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
    out
}

fn full(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(body.len() + 4);
    inner.push(version);
    inner.extend_from_slice(&flags.to_be_bytes()[1..]);
    inner.extend_from_slice(body);
    bx(typ, &inner)
}

/// The identity matrix `tkhd` and `mvhd` both want, in 16.16 / 2.30 fixed point.
const UNITY_MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x00, 0x00, 0x00,
];

/// One track's samples grouped into a chunk, and where that chunk landed.
struct Chunk {
    track: usize,
    first_sample: usize,
    count: usize,
    /// Media start time, in seconds, used only to order the chunks.
    start: f64,
    offset: u64,
}

/// Shift a track's composition offsets so presentation starts at zero.
///
/// A reordered stream is usually written with all-positive `cto` values, which
/// puts the first frame's presentation one or two frames after the start of
/// the timeline. Left alone that is a small constant lag between picture and
/// sound — exactly what "the audio is slightly out of sync" sounds like.
///
/// There are two ways a file says this, and doing both is a bug rather than
/// belt and braces: an edit list states the offset explicitly, and where the
/// input carried one it is written straight through, so shifting the samples
/// as well would skip that much *real* content. This therefore runs only for
/// tracks with no edit list — a transport stream, or a packager that omitted
/// it. Subtracting a constant preserves sample order, so all that moves is
/// where the timeline begins.
fn zero_composition(track: &mut Track) {
    // Only an input edit list conflicts with this. A start delay does not: it
    // says where the track begins relative to the others, which is a separate
    // question from where its own presentation begins.
    if track.edit_start > 0 {
        return;
    }
    let mut dts: i64 = 0;
    let mut earliest = i64::MAX;
    for s in &track.samples {
        earliest = earliest.min(dts + s.cto as i64);
        dts += s.duration as i64;
    }
    if earliest > 0 && earliest != i64::MAX {
        for s in &mut track.samples {
            s.cto -= earliest as i32;
        }
    }
}

/// Rebuild the inputs as one progressive MP4.
///
/// Every source track becomes an output track, renumbered from 1. Sample bytes
/// are copied from wherever they already are; nothing is decoded, and the
/// output plays exactly what the input encoded.
pub fn remux(sources: &mut [Source], out: &Path) -> Result<()> {
    for source in sources.iter_mut() {
        for track in &mut source.tracks {
            zero_composition(track);
        }
    }

    // Put every track on one timeline. Tracks fetched as separate
    // representations each carry their own `tfdt`, and the differences between
    // them are real: the earliest is the origin, and anything starting later
    // gets an empty edit saying so. Discarding this is how the picture ends up
    // a fixed twenty-odd milliseconds away from the sound.
    let origin = sources
        .iter()
        .flat_map(|s| s.tracks.iter())
        .filter_map(|t| t.media_start.map(|m| m as f64 / t.timescale as f64))
        .fold(f64::INFINITY, f64::min);
    if origin.is_finite() {
        for source in sources.iter_mut() {
            for track in &mut source.tracks {
                let Some(start) = track.media_start else { continue };
                let offset = start as f64 / track.timescale as f64 - origin;
                if offset > 0.0 {
                    track.edit_delay += offset;
                }
            }
        }
    }

    let mut tracks: Vec<(usize, &Track)> = Vec::new();
    for (i, source) in sources.iter().enumerate() {
        for track in &source.tracks {
            tracks.push((i, track));
        }
    }
    if tracks.is_empty() {
        bail!("nothing to remux: no track carried any sample");
    }

    // Chunk every track, then order the chunks by media time so the output is
    // interleaved rather than "all the video, then all the sound".
    let mut chunks: Vec<Chunk> = Vec::new();
    for (index, (_, track)) in tracks.iter().enumerate() {
        let per_chunk = (CHUNK_SECONDS * track.timescale as f64) as u64;
        let mut sample = 0usize;
        let mut elapsed = 0u64;
        while sample < track.samples.len() {
            let start = elapsed;
            let mut count = 0usize;
            let mut span = 0u64;
            while sample + count < track.samples.len()
                && (count == 0 || span < per_chunk.max(1))
            {
                span += track.samples[sample + count].duration as u64;
                count += 1;
            }
            chunks.push(Chunk {
                track: index,
                first_sample: sample,
                count,
                start: start as f64 / track.timescale as f64,
                offset: 0,
            });
            sample += count;
            elapsed += span;
        }
    }
    // Stable, so two chunks starting at the same instant keep track order and
    // the picture is written before the sound that accompanies it.
    chunks.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));

    let payload: u64 = tracks
        .iter()
        .flat_map(|(_, t)| t.samples.iter())
        .map(|s| s.size as u64)
        .sum();
    // Decided before the header is built, because `stco` and `co64` are
    // different sizes and the header's own length feeds back into the offsets
    // it contains. The margin covers the header itself.
    let large = payload + (64 << 20) > u32::MAX as u64;

    let ftyp = bx(
        b"ftyp",
        &[
            b"isom".as_slice(),
            &0x0000_0200u32.to_be_bytes(),
            b"isom",
            b"iso2",
            b"avc1",
            b"mp41",
        ]
        .concat(),
    );

    // Built twice: the first pass exists only to learn how long the header is,
    // because every chunk offset in it is measured from the start of the file
    // and so depends on that length. Nothing about the second pass changes the
    // size — the offset fields are fixed width — so one repeat is enough.
    let probe = build_moov(&tracks, &chunks, large);
    let mdat_header: u64 = if large { 16 } else { 8 };
    let mdat_at = ftyp.len() as u64 + probe.len() as u64;
    let mut cursor = mdat_at + mdat_header;
    for chunk in &mut chunks {
        chunk.offset = cursor;
        let (_, track) = tracks[chunk.track];
        for i in 0..chunk.count {
            cursor += track.samples[chunk.first_sample + i].size as u64;
        }
    }
    let moov = build_moov(&tracks, &chunks, large);
    debug_assert_eq!(moov.len(), probe.len(), "header size must not move");

    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    w.write_all(&ftyp)?;
    w.write_all(&moov)?;
    if large {
        w.write_all(&1u32.to_be_bytes())?;
        w.write_all(b"mdat")?;
        w.write_all(&(payload + 16).to_be_bytes())?;
    } else {
        w.write_all(&((payload + 8) as u32).to_be_bytes())?;
        w.write_all(b"mdat")?;
    }

    // One reader per input, kept open across the whole copy: the chunks
    // alternate between them, and reopening per chunk would cost a syscall
    // storm on a file with tens of thousands of them.
    let mut readers: Vec<File> = sources
        .iter()
        .map(|s| File::open(&s.path).with_context(|| format!("opening {}", s.path.display())))
        .collect::<Result<_>>()?;

    let mut buf = vec![0u8; 1 << 20];
    for chunk in &chunks {
        let (source, track) = tracks[chunk.track];
        let reader = &mut readers[source];
        for i in 0..chunk.count {
            let sample = &track.samples[chunk.first_sample + i];
            reader.seek(SeekFrom::Start(sample.offset))?;
            let mut left = sample.size as usize;
            while left > 0 {
                let want = left.min(buf.len());
                reader.read_exact(&mut buf[..want]).with_context(|| {
                    format!("reading {} bytes of sample data", sample.size)
                })?;
                w.write_all(&buf[..want])?;
                left -= want;
            }
        }
    }
    w.flush().context("flushing the remuxed file")?;
    Ok(())
}

fn build_moov(tracks: &[(usize, &Track)], chunks: &[Chunk], large: bool) -> Vec<u8> {
    let longest = tracks
        .iter()
        .map(|(_, t)| t.duration() as f64 / t.timescale as f64)
        .fold(0.0f64, f64::max);
    let movie_duration = (longest * MOVIE_TIMESCALE as f64) as u32;

    let mut body = Vec::new();
    let mut mvhd = Vec::new();
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification
    mvhd.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    mvhd.extend_from_slice(&movie_duration.to_be_bytes());
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    mvhd.extend_from_slice(&[0u8; 10]); // reserved
    mvhd.extend_from_slice(&UNITY_MATRIX);
    mvhd.extend_from_slice(&[0u8; 24]); // pre_defined
    mvhd.extend_from_slice(&((tracks.len() as u32) + 1).to_be_bytes());
    body.extend_from_slice(&full(b"mvhd", 0, 0, &mvhd));

    for (index, (_, track)) in tracks.iter().enumerate() {
        body.extend_from_slice(&build_trak(
            index,
            track,
            chunks,
            large,
            movie_duration,
        ));
    }
    bx(b"moov", &body)
}

fn build_trak(
    index: usize,
    track: &Track,
    chunks: &[Chunk],
    large: bool,
    movie_duration: u32,
) -> Vec<u8> {
    let track_id = index as u32 + 1;
    let media_duration = track.duration();
    let in_movie = ((media_duration as f64 / track.timescale as f64)
        * MOVIE_TIMESCALE as f64) as u32;

    let mut tkhd = Vec::new();
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // creation
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // modification
    tkhd.extend_from_slice(&track_id.to_be_bytes());
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // reserved
    tkhd.extend_from_slice(&in_movie.min(movie_duration.max(in_movie)).to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 8]); // reserved
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    tkhd.extend_from_slice(
        &if track.kind == Kind::Audio { 0x0100u16 } else { 0 }.to_be_bytes(),
    );
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // reserved
    tkhd.extend_from_slice(&UNITY_MATRIX);
    tkhd.extend_from_slice(&((track.width as u32) << 16).to_be_bytes());
    tkhd.extend_from_slice(&((track.height as u32) << 16).to_be_bytes());
    // Flags 7: enabled, in the movie, in the preview.
    let tkhd = full(b"tkhd", 0, 7, &tkhd);

    // Carry the input's edit list through, rebuilt in this file's movie
    // timescale. Without it an audio track plays its encoder priming — sound
    // that starts before the picture by a frame or so, every time.
    let edts = if track.edit_start > 0 || track.edit_delay > 0.0 {
        let mut entries: Vec<u8> = Vec::new();
        let mut count = 0u32;
        if track.edit_delay > 0.0 {
            let gap = (track.edit_delay * MOVIE_TIMESCALE as f64) as u32;
            entries.extend_from_slice(&gap.to_be_bytes());
            entries.extend_from_slice(&(-1i32).to_be_bytes()); // an empty edit
            entries.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
            count += 1;
        }
        let playable = track.presentation_end().saturating_sub(track.edit_start);
        let span = ((playable as f64 / track.timescale as f64) * MOVIE_TIMESCALE as f64) as u32;
        entries.extend_from_slice(&span.to_be_bytes());
        entries.extend_from_slice(&(track.edit_start.min(i32::MAX as u64) as i32).to_be_bytes());
        entries.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        count += 1;

        let mut body = count.to_be_bytes().to_vec();
        body.extend_from_slice(&entries);
        bx(b"edts", &full(b"elst", 0, 0, &body))
    } else {
        Vec::new()
    };

    let mut mdhd = Vec::new();
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&track.timescale.to_be_bytes());
    mdhd.extend_from_slice(&(media_duration.min(u32::MAX as u64) as u32).to_be_bytes());
    // 'und' packed as three five-bit letters, then a zero quality field.
    mdhd.extend_from_slice(&0x55c4u16.to_be_bytes());
    mdhd.extend_from_slice(&0u16.to_be_bytes());
    let mdhd = full(b"mdhd", 0, 0, &mdhd);

    let (handler, name): (&[u8; 4], &[u8]) = match track.kind {
        Kind::Video => (b"vide", b"VideoHandler\0"),
        Kind::Audio => (b"soun", b"SoundHandler\0"),
    };
    let mut hdlr = Vec::new();
    hdlr.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    hdlr.extend_from_slice(handler);
    hdlr.extend_from_slice(&[0u8; 12]); // reserved
    hdlr.extend_from_slice(name);
    let hdlr = full(b"hdlr", 0, 0, &hdlr);

    let media_header = match track.kind {
        // graphicsmode 0 (copy) and a black opcolor.
        Kind::Video => full(b"vmhd", 0, 1, &[0u8; 8]),
        // balance 0, then reserved.
        Kind::Audio => full(b"smhd", 0, 0, &[0u8; 4]),
    };
    // A self-contained file: the data is in this very file, which is what the
    // "URL is empty" flag on `url ` means.
    let dref = full(b"dref", 0, 0, &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat());
    let dinf = bx(b"dinf", &dref);

    let stbl = build_stbl(index, track, chunks, large);
    let minf = bx(b"minf", &[media_header, dinf, stbl].concat());
    let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
    bx(b"trak", &[tkhd, edts, mdia].concat())
}

fn build_stbl(index: usize, track: &Track, chunks: &[Chunk], large: bool) -> Vec<u8> {
    let stsd = full(
        b"stsd",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &track.sample_entry].concat(),
    );

    // stts: runs of equal sample durations. Constant frame rate collapses to
    // a single entry, which is the common case and worth the loop.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for sample in &track.samples {
        match runs.last_mut() {
            Some((count, delta)) if *delta == sample.duration => *count += 1,
            _ => runs.push((1, sample.duration)),
        }
    }
    let mut stts = Vec::with_capacity(runs.len() * 8 + 4);
    stts.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in &runs {
        stts.extend_from_slice(&count.to_be_bytes());
        stts.extend_from_slice(&delta.to_be_bytes());
    }
    let stts = full(b"stts", 0, 0, &stts);

    // stss: only meaningful when some samples are not sync samples. Audio is
    // all-sync, and writing the box anyway makes some players treat every
    // frame as a seek point and scrub badly.
    let stss = if track.samples.iter().all(|s| s.sync) {
        Vec::new()
    } else {
        let syncs: Vec<u32> = track
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.sync)
            .map(|(i, _)| i as u32 + 1)
            .collect();
        let mut body = Vec::with_capacity(syncs.len() * 4 + 4);
        body.extend_from_slice(&(syncs.len() as u32).to_be_bytes());
        for n in syncs {
            body.extend_from_slice(&n.to_be_bytes());
        }
        full(b"stss", 0, 0, &body)
    };

    // ctts: omitted entirely when decode order is presentation order, which is
    // every audio track and any video without B-frames.
    let ctts = if track.samples.iter().all(|s| s.cto == 0) {
        Vec::new()
    } else {
        let mut runs: Vec<(u32, i32)> = Vec::new();
        for sample in &track.samples {
            match runs.last_mut() {
                Some((count, offset)) if *offset == sample.cto => *count += 1,
                _ => runs.push((1, sample.cto)),
            }
        }
        let mut body = Vec::with_capacity(runs.len() * 8 + 4);
        body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (count, offset) in &runs {
            body.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&offset.to_be_bytes());
        }
        // Version 1, whose offsets are signed. Version 0 cannot express the
        // negative offsets a closed-GOP stream produces.
        full(b"ctts", 1, 0, &body)
    };

    // stsc and stco walk this track's chunks in the order they were written.
    let mine: Vec<&Chunk> = chunks.iter().filter(|c| c.track == index).collect();
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    for (i, chunk) in mine.iter().enumerate() {
        let count = chunk.count as u32;
        match stsc.last() {
            Some((_, prev)) if *prev == count => {}
            _ => stsc.push((i as u32 + 1, count)),
        }
    }
    let mut stsc_body = Vec::with_capacity(stsc.len() * 12 + 4);
    stsc_body.extend_from_slice(&(stsc.len() as u32).to_be_bytes());
    for (first, per) in &stsc {
        stsc_body.extend_from_slice(&first.to_be_bytes());
        stsc_body.extend_from_slice(&per.to_be_bytes());
        stsc_body.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    }
    let stsc = full(b"stsc", 0, 0, &stsc_body);

    let mut stsz = Vec::with_capacity(track.samples.len() * 4 + 8);
    stsz.extend_from_slice(&0u32.to_be_bytes()); // varying sizes follow
    stsz.extend_from_slice(&(track.samples.len() as u32).to_be_bytes());
    for sample in &track.samples {
        stsz.extend_from_slice(&sample.size.to_be_bytes());
    }
    let stsz = full(b"stsz", 0, 0, &stsz);

    let offsets = if large {
        let mut body = Vec::with_capacity(mine.len() * 8 + 4);
        body.extend_from_slice(&(mine.len() as u32).to_be_bytes());
        for chunk in &mine {
            body.extend_from_slice(&chunk.offset.to_be_bytes());
        }
        full(b"co64", 0, 0, &body)
    } else {
        let mut body = Vec::with_capacity(mine.len() * 4 + 4);
        body.extend_from_slice(&(mine.len() as u32).to_be_bytes());
        for chunk in &mine {
            body.extend_from_slice(&(chunk.offset as u32).to_be_bytes());
        }
        full(b"stco", 0, 0, &body)
    };

    bx(
        b"stbl",
        &[stsd, stts, stss, ctts, stsc, stsz, offsets].concat(),
    )
}

/* ------------------------------------------------------------------ *
 * Helpers shared with the transport-stream demuxer
 * ------------------------------------------------------------------ */

/// Build an `avc1` sample entry around an `avcC` this crate assembled.
///
/// The fragmented path never needs this — it copies the entry the init segment
/// already carries — but a transport stream has no container to copy from, so
/// the entry has to be constructed around the parameter sets found in the
/// elementary stream.
pub fn avc1_entry(width: u16, height: u16, avcc: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    body.extend_from_slice(&[0u8; 16]); // pre_defined / reserved
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi horizontal
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi vertical
    body.extend_from_slice(&0u32.to_be_bytes()); // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    body.extend_from_slice(&[0u8; 32]); // compressor name
    body.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    body.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
    body.extend_from_slice(&bx(b"avcC", avcc));
    bx(b"avc1", &body)
}

/// Build an `mp4a` sample entry for AAC, with the `esds` that describes it.
pub fn mp4a_entry(channels: u16, sample_rate: u32, config: &[u8]) -> Vec<u8> {
    // esds is a chain of length-prefixed descriptors. The lengths are short
    // enough here to fit the one-byte form.
    let mut dec_specific = vec![0x05, config.len() as u8];
    dec_specific.extend_from_slice(config);

    let mut dec_config = vec![
        0x04,
        (13 + dec_specific.len()) as u8,
        0x40, // MPEG-4 audio
        0x15, // audio stream
        0, 0, 0, // buffer size
        0, 0, 0, 0, // max bitrate
        0, 0, 0, 0, // average bitrate
    ];
    dec_config.extend_from_slice(&dec_specific);

    let mut es = vec![
        0x03,
        (3 + dec_config.len() + 3) as u8,
        0,
        0, // ES_ID
        0, // flags
    ];
    es.extend_from_slice(&dec_config);
    es.extend_from_slice(&[0x06, 0x01, 0x02]); // SL descriptor

    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    body.extend_from_slice(&[0u8; 8]); // reserved
    body.extend_from_slice(&channels.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes()); // sample size
    body.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    body.extend_from_slice(&0u16.to_be_bytes()); // reserved
    // 16.16 fixed point, and so unable to carry 96 kHz. Sample rates above
    // 65535 are written as 0 here; the real rate is in the `esds` config,
    // which is where a decoder looks anyway.
    let rate16 = if sample_rate > u16::MAX as u32 { 0 } else { sample_rate };
    body.extend_from_slice(&(rate16 << 16).to_be_bytes());
    body.extend_from_slice(&full(b"esds", 0, 0, &es));
    bx(b"mp4a", &body)
}

/// Read a big-endian `u16` out of a slice, for the demuxer's benefit.
pub(crate) fn read_be16(b: &[u8], o: usize) -> u16 {
    be16(b, o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_walk_and_nest() {
        let inner = bx(b"mdhd", &[1, 2, 3, 4]);
        let outer = bx(b"mdia", &inner);
        let (typ, payload) = atoms(&outer).next().unwrap();
        assert_eq!(&typ, b"mdia");
        assert_eq!(find(payload, b"mdhd").unwrap(), &[1, 2, 3, 4]);
    }

    #[test]
    fn a_truncated_length_ends_the_walk_instead_of_panicking() {
        // A box claiming to be longer than the buffer holding it is exactly
        // what a half-downloaded segment looks like.
        let mut buf = bx(b"moov", &[0u8; 16]);
        buf[3] = 0xff;
        assert_eq!(atoms(&buf).count(), 0);
    }

    fn sample(duration: u32, cto: i32) -> Sample {
        Sample { offset: 0, size: 1, duration, cto, sync: true }
    }

    fn track_with(samples: Vec<Sample>) -> Track {
        Track::synthetic(1, Kind::Video, 1000, 16, 16, Vec::new(), samples)
    }

    #[test]
    fn presentation_end_outlasts_decode_on_a_reordered_stream() {
        // Three frames of 10, the last displayed 20 later than it is decoded.
        // Decode ends at 30; presentation does not end until 50. Measuring an
        // edit list against the former is what cut two frames off the end.
        let t = track_with(vec![sample(10, 0), sample(10, 0), sample(10, 20)]);
        assert_eq!(t.duration(), 30);
        assert_eq!(t.presentation_end(), 50);
    }

    #[test]
    fn composition_offsets_are_shifted_so_display_starts_at_zero() {
        let mut t = track_with(vec![sample(10, 20), sample(10, 50), sample(10, 20)]);
        zero_composition(&mut t);
        // The smallest presentation time was 20; every offset drops by it, and
        // the gaps between them are untouched.
        assert_eq!(
            t.samples.iter().map(|s| s.cto).collect::<Vec<_>>(),
            vec![0, 30, 0]
        );
    }

    #[test]
    fn an_input_edit_list_suppresses_the_shift() {
        // Doing both is a double correction: the edit list already states the
        // offset, so shifting the samples too would skip real content.
        let mut t = track_with(vec![sample(10, 20), sample(10, 20)]);
        t.edit_start = 20;
        zero_composition(&mut t);
        assert_eq!(t.samples[0].cto, 20, "the samples must be left alone");
    }

    #[test]
    fn an_empty_edit_is_read_as_a_delay_and_not_as_a_trim() {
        // ffmpeg writes this pair constantly: an empty edit for the gap, then
        // the real one. Reading the -1 as a media offset yields nonsense.
        let mut elst = 2u32.to_be_bytes().to_vec();
        elst.extend_from_slice(&500u32.to_be_bytes()); // half a second, movie time
        elst.extend_from_slice(&(-1i32).to_be_bytes()); // empty
        elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        elst.extend_from_slice(&9000u32.to_be_bytes());
        elst.extend_from_slice(&1024i32.to_be_bytes()); // the real media start
        elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        let trak = bx(b"edts", &full(b"elst", 0, 0, &elst));

        let (start, delay) = parse_edits(&trak, 1000);
        assert_eq!(start, 1024, "priming to skip, in media time");
        assert!((delay - 0.5).abs() < 1e-9, "gap in seconds, got {delay}");
    }

    #[test]
    fn a_track_without_an_edit_list_reports_neither() {
        assert_eq!(parse_edits(&bx(b"tkhd", &[0u8; 4]), 1000), (0, 0.0));
    }

    #[test]
    fn a_full_box_carries_its_version_and_flags() {
        let b = full(b"tkhd", 0, 7, &[]);
        assert_eq!(&b[4..8], b"tkhd");
        assert_eq!(b[8], 0, "version");
        assert_eq!(&b[9..12], &[0, 0, 7], "flags");
    }
}
