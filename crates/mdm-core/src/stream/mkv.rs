//! Matroska (WebM): enough to read the streams a site serves and write one file.
//!
//! The sibling of [`super::mp4`], and here for the same reason. A site that
//! serves picture and sound apart hands back two files, and putting them
//! together is a container rebuild rather than a transcode. The MP4 family
//! covers most of what YouTube offers, but VP9 and Opus arrive in WebM — which
//! is Matroska — and until this existed a merge of those had to be given away
//! to ffmpeg.
//!
//! The same discipline applies as next door: nothing here decodes anything. A
//! track is copied by taking its `TrackEntry` children *verbatim* — the
//! `CodecID`, the `CodecPrivate`, the `Video` and `Audio` blocks, Opus's
//! `CodecDelay` and `SeekPreRoll` — and rewriting only the number it is filed
//! under. Whatever codec those describe, we never have to understand it.
//!
//! Matroska is a tree of EBML elements: an id, a length, and either a payload
//! or more elements. What follows is that idea, and a list of the ids needed.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::mp4::Kind;

/* ------------------------------------------------------------------ *
 * Element ids
 *
 * Written as they appear on the wire, marker bits included, because that is
 * how they are compared while reading and how they are emitted while writing.
 * ------------------------------------------------------------------ */

const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;

const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2AD7_B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
const DISCARD_PADDING: u32 = 0x75A2;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CUE_RELATIVE_POSITION: u32 = 0xF0;
const SEEK_HEAD: u32 = 0x114D_9B74;
const TAGS: u32 = 0x1254_C367;
const CHAPTERS: u32 = 0x1043_A770;
const ATTACHMENTS: u32 = 0x1941_A469;

/// Every id that may appear directly inside a Segment.
///
/// Needed for one case, and a real one: a Cluster of *unknown* length, which
/// anything writing as it records has to use. Its end is wherever the next
/// top-level element starts.
const TOP_LEVEL: &[u32] = &[SEEK_HEAD, INFO, TRACKS, CLUSTER, CUES, TAGS, CHAPTERS, ATTACHMENTS];

/// The output's timestamp scale: one millisecond, in nanoseconds.
///
/// What every WebM in the wild uses, and coarse enough that a block's
/// timestamp — sixteen signed bits, relative to its cluster — still spans half
/// a minute.
const SCALE_NS: u64 = 1_000_000;

/// How much of a cluster is enough. Both bounds matter: the time keeps a
/// block's relative timestamp inside its sixteen bits, the size keeps a player
/// from reading megabytes before it can show anything.
const CLUSTER_MS: i64 = 2_000;
const CLUSTER_BYTES: usize = 4 << 20;

/* ------------------------------------------------------------------ *
 * Reading
 * ------------------------------------------------------------------ */

/// One track of an input file, ready to be written into an output.
#[derive(Debug, Clone)]
pub struct Track {
    pub kind: Kind,
    /// What this track is called in the file it came from, which is how its
    /// own blocks name it.
    number: u64,
    /// The `TrackEntry` children, verbatim, less the two the output rewrites.
    /// Copying these is what keeps this codec-agnostic.
    fields: Vec<u8>,
}

/// One frame: where its bytes are, and when it is played.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Index into its source's `tracks`.
    pub track: usize,
    /// Absolute, in nanoseconds, so files written at different scales can be
    /// merged without either of them being rescaled first.
    pub timestamp_ns: i64,
    pub offset: u64,
    pub size: u32,
    pub keyframe: bool,
    /// Nanoseconds of sound at the end of this frame that were only ever
    /// encoder padding, and must not be played.
    ///
    /// Opus encodes in fixed-length frames, so the last one of a track runs
    /// past where the recording actually stopped, and the container is what
    /// says by how much. Dropped, the file ends with a few milliseconds of
    /// padding — inaudible, and still a difference between what went in and
    /// what came out, which is not a difference a copy is allowed to make.
    pub discard_ns: i64,
}

/// A file to take tracks and frames from.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub tracks: Vec<Track>,
    pub frames: Vec<Frame>,
}

/// Whether a file is Matroska, by the four bytes that say so.
pub fn is_matroska(path: &Path) -> bool {
    let mut head = [0u8; 4];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .map(|()| u32::from_be_bytes(head) == EBML)
        .unwrap_or(false)
}

/// A reader that knows where it is, which is most of what EBML asks for.
struct Cursor<R> {
    inner: R,
    at: u64,
}

impl<R: Read> Cursor<R> {
    fn byte(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.inner.read_exact(&mut b)?;
        self.at += 1;
        Ok(b[0])
    }

    fn bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.inner.read_exact(&mut buf)?;
        self.at += n as u64;
        Ok(buf)
    }

    /// An element id, kept exactly as written: its marker bits are part of it.
    fn id(&mut self) -> Result<u32> {
        let first = self.byte()?;
        let len = leading_length(first).context("an element id with no length marker")?;
        let mut id = first as u32;
        for _ in 1..len {
            id = (id << 8) | self.byte()? as u32;
        }
        Ok(id)
    }

    /// A length. `None` is EBML's "unknown", written as all ones.
    fn size(&mut self) -> Result<Option<u64>> {
        let first = self.byte()?;
        let len = leading_length(first).context("a length with no marker")?;
        let mask = value_mask(len);
        let mut value = (first & mask) as u64;
        let mut all_ones = (first & mask) == mask;
        for _ in 1..len {
            let b = self.byte()?;
            value = (value << 8) | b as u64;
            all_ones = all_ones && b == 0xFF;
        }
        Ok((!all_ones).then_some(value))
    }
}

impl<R: Read + Seek> Cursor<R> {
    fn skip(&mut self, n: u64) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        self.inner.seek(SeekFrom::Current(n as i64))?;
        self.at += n;
        Ok(())
    }

    fn rewind_to(&mut self, at: u64) -> Result<()> {
        self.inner.seek(SeekFrom::Start(at))?;
        self.at = at;
        Ok(())
    }
}

/// Which bits of a length's first byte are the length rather than the
/// marker.
///
/// At the widest — eight bytes — the first byte is *all* marker and carries
/// no value at all, which is a shift of eight on a byte and so has to be
/// said outright rather than computed.
fn value_mask(len: usize) -> u8 {
    if len >= 8 { 0 } else { 0xFFu8 >> len }
}

/// How many bytes this first byte says its id or length occupies.
fn leading_length(first: u8) -> Option<usize> {
    (0..8).find(|i| first & (0x80 >> i) != 0).map(|i| i + 1)
}

/// A signed integer element: big-endian two's complement, in as few bytes
/// as it needed.
fn sint(bytes: &[u8]) -> i64 {
    let Some(first) = bytes.first() else { return 0 };
    // Sign-extended by starting from all ones when the top bit is set.
    let mut value: i64 = if first & 0x80 != 0 { -1 } else { 0 };
    for b in bytes {
        value = (value << 8) | *b as i64;
    }
    value
}

/// An unsigned integer element: big-endian, in as few bytes as it needed.
fn uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64)
}

/// Read the tracks and every frame of a Matroska file.
///
/// Frame *bytes* stay where they are: only positions are collected, because a
/// merge copies them straight from one file into another, and holding a
/// gigabyte of video in memory to do that would be absurd.
pub fn read(path: &Path) -> Result<Source> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut cur = Cursor { inner: BufReader::with_capacity(1 << 16, file), at: 0 };

    let mut tracks: Vec<Track> = Vec::new();
    let mut numbers: Vec<u64> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut scale_ns = SCALE_NS;

    while cur.at < len {
        let Ok(id) = cur.id() else { break };
        let Ok(size) = cur.size() else { break };
        let body = cur.at;
        let end = size.map(|s| (body + s).min(len)).unwrap_or(len);

        match id {
            // Descended into rather than skipped: its children are the file.
            SEGMENT => continue,
            INFO => {
                let raw = cur.bytes((end - body) as usize)?;
                if let Some(stated) = child(&raw, TIMESTAMP_SCALE).map(|v| uint(&v)) {
                    if stated > 0 {
                        scale_ns = stated;
                    }
                }
            }
            TRACKS => {
                let raw = cur.bytes((end - body) as usize)?;
                for entry in children(&raw, TRACK_ENTRY) {
                    if let Some(track) = track_from(&entry) {
                        numbers.push(track.number);
                        tracks.push(track);
                    }
                }
            }
            CLUSTER => read_cluster(&mut cur, end, size.is_none(), scale_ns, &numbers, &mut frames)?,
            _ => cur.skip(end.saturating_sub(cur.at))?,
        }
    }

    if tracks.is_empty() {
        bail!("{} names no tracks", path.display());
    }
    if frames.is_empty() {
        bail!("{} holds no frames", path.display());
    }
    frames.sort_by_key(|f| f.timestamp_ns);
    Ok(Source { path: path.to_path_buf(), tracks, frames })
}

/// Read one cluster's blocks. `open` says its length was left unstated, in
/// which case it runs until something that can only be a top-level element.
fn read_cluster<R: Read + Seek>(
    cur: &mut Cursor<R>,
    end: u64,
    open: bool,
    scale_ns: u64,
    numbers: &[u64],
    frames: &mut Vec<Frame>,
) -> Result<()> {
    let mut cluster_ts: i64 = 0;
    while cur.at < end {
        let before = cur.at;
        let Ok(id) = cur.id() else { break };
        if open && TOP_LEVEL.contains(&id) {
            // Not ours. Hand it back by rewinding onto its id.
            return cur.rewind_to(before);
        }
        let Ok(size) = cur.size() else { break };
        let body = cur.at;
        let stop = size.map(|s| (body + s).min(end)).unwrap_or(end);
        match id {
            TIMESTAMP => {
                let raw = cur.bytes((stop - body) as usize)?;
                cluster_ts = uint(&raw) as i64;
            }
            SIMPLE_BLOCK => {
                let head = block_head(cur, stop)?;
                push_frames(&head, cur.at, stop, cluster_ts, scale_ns, numbers, head.keyframe, frames);
                cur.skip(stop.saturating_sub(cur.at))?;
            }
            // A block that needed something said about it. Which frames it
            // refers to is what says whether it can be decoded on its own —
            // the fact a `SimpleBlock` carries in a flag.
            BLOCK_GROUP => {
                let mut referenced = false;
                let mut discard_ns = 0i64;
                let mut pending: Option<(BlockHead, u64, u64)> = None;
                while cur.at < stop {
                    let Ok(inner) = cur.id() else { break };
                    let Ok(inner_size) = cur.size() else { break };
                    let inner_body = cur.at;
                    let inner_stop = inner_size.map(|s| (inner_body + s).min(stop)).unwrap_or(stop);
                    if inner == BLOCK {
                        let head = block_head(cur, inner_stop)?;
                        pending = Some((head, cur.at, inner_stop));
                    } else if inner == REFERENCE_BLOCK {
                        referenced = true;
                    } else if inner == DISCARD_PADDING {
                        let raw = cur.bytes((inner_stop - cur.at) as usize)?;
                        discard_ns = sint(&raw);
                    }
                    cur.skip(inner_stop.saturating_sub(cur.at))?;
                }
                if let Some((head, at, data_end)) = pending {
                    let first = frames.len();
                    push_frames(&head, at, data_end, cluster_ts, scale_ns, numbers, !referenced, frames);
                    // On the last of the block's frames, since that is the
                    // end the padding is at.
                    if discard_ns != 0 {
                        if let Some(frame) = frames.get_mut(first..).and_then(|f| f.last_mut()) {
                            frame.discard_ns = discard_ns;
                        }
                    }
                }
            }
            _ => cur.skip(stop.saturating_sub(cur.at))?,
        }
    }
    Ok(())
}

/// The fixed part at the front of a block: which track, when, and how the
/// frames behind it are packed.
struct BlockHead {
    track: u64,
    relative: i64,
    keyframe: bool,
    /// Frame lengths, when the block carries more than one.
    lacing: Vec<u32>,
}

/// Read a block's header, leaving the cursor on its first frame.
fn block_head<R: Read + Seek>(cur: &mut Cursor<R>, end: u64) -> Result<BlockHead> {
    let (track, _) = read_vint(cur)?;
    let hi = cur.byte()? as u16;
    let lo = cur.byte()? as u16;
    let relative = (((hi << 8) | lo) as i16) as i64;
    let flags = cur.byte()?;
    let keyframe = flags & 0x80 != 0;

    // Lacing packs several small frames into one block. Nothing MDM fetches
    // does it — YouTube writes one frame per block and says so in
    // `FlagLacing` — but a reader that met one and carried on would glue
    // several frames into one, so all three packings are handled.
    let lacing = match (flags >> 1) & 0x03 {
        0 => Vec::new(),
        packing => {
            let count = cur.byte()? as usize + 1;
            let mut sizes = Vec::with_capacity(count);
            match packing {
                // Fixed: every frame the same size, so none is written down.
                2 => {
                    let each = (end - cur.at) / count as u64;
                    sizes.extend(std::iter::repeat_n(each as u32, count));
                }
                // Xiph: each length as runs of 255 ending in anything less.
                1 => {
                    let mut total = 0u64;
                    for _ in 0..count.saturating_sub(1) {
                        let mut size = 0u32;
                        loop {
                            let b = cur.byte()?;
                            size += b as u32;
                            if b != 0xFF {
                                break;
                            }
                        }
                        total += size as u64;
                        sizes.push(size);
                    }
                    sizes.push((end - cur.at).saturating_sub(total) as u32);
                }
                // EBML: the first length outright, the rest as differences.
                _ => {
                    let (first, _) = read_vint(cur)?;
                    let mut size = first as i64;
                    let mut total = size as u64;
                    sizes.push(size as u32);
                    for _ in 1..count.saturating_sub(1) {
                        let (value, width) = read_vint(cur)?;
                        // Signed, biased so that zero means "unchanged".
                        let bias = (1i64 << (width * 7 - 1)) - 1;
                        size += value as i64 - bias;
                        total += size.max(0) as u64;
                        sizes.push(size.max(0) as u32);
                    }
                    sizes.push((end - cur.at).saturating_sub(total) as u32);
                }
            }
            sizes
        }
    };
    Ok(BlockHead { track, relative, keyframe, lacing })
}

/// A variable-length integer, and how many bytes it took.
fn read_vint<R: Read>(cur: &mut Cursor<R>) -> Result<(u64, u32)> {
    let first = cur.byte()?;
    let len = leading_length(first).context("a malformed length")?;
    let mask = value_mask(len);
    let mut value = (first & mask) as u64;
    for _ in 1..len {
        value = (value << 8) | cur.byte()? as u64;
    }
    Ok((value, len as u32))
}

#[allow(clippy::too_many_arguments)]
fn push_frames(
    head: &BlockHead,
    data_at: u64,
    data_end: u64,
    cluster_ts: i64,
    scale_ns: u64,
    numbers: &[u64],
    keyframe: bool,
    frames: &mut Vec<Frame>,
) {
    // A block naming a track the file never described is not something to
    // guess about; there is nowhere to put its bytes.
    let Some(track) = numbers.iter().position(|n| *n == head.track) else {
        return;
    };
    let ts = (cluster_ts + head.relative) * scale_ns as i64;
    if head.lacing.is_empty() {
        frames.push(Frame {
            track,
            timestamp_ns: ts,
            offset: data_at,
            size: (data_end - data_at) as u32,
            keyframe,
            discard_ns: 0,
        });
        return;
    }
    // Laced frames share their block's timestamp; a player spaces them by the
    // codec's own frame length, which is what every other muxer relies on too.
    let mut at = data_at;
    for size in &head.lacing {
        frames.push(Frame { track, timestamp_ns: ts, offset: at, size: *size, keyframe, discard_ns: 0 });
        at += *size as u64;
    }
}

/// Pull one `TrackEntry` apart: what kind of track it is, what it is called,
/// and everything else exactly as it was written.
fn track_from(entry: &[u8]) -> Option<Track> {
    let number = child(entry, TRACK_NUMBER).map(|v| uint(&v))?;
    let kind = match child(entry, TRACK_TYPE).map(|v| uint(&v))? {
        1 => Kind::Video,
        2 => Kind::Audio,
        // Subtitles, buttons, and the other things a merge has no place for.
        _ => return None,
    };
    let mut fields = Vec::with_capacity(entry.len());
    for (id, _, whole) in walk(entry) {
        if id != TRACK_NUMBER && id != TRACK_UID {
            fields.extend_from_slice(whole);
        }
    }
    Some(Track { kind, number, fields })
}

/// The payload of the first `id` directly inside `parent`.
fn child(parent: &[u8], id: u32) -> Option<Vec<u8>> {
    walk(parent).into_iter().find(|(this, _, _)| *this == id).map(|(_, body, _)| body.to_vec())
}

/// Every `id` directly inside `parent`, as payloads.
fn children(parent: &[u8], id: u32) -> Vec<Vec<u8>> {
    walk(parent)
        .into_iter()
        .filter(|(this, _, _)| *this == id)
        .map(|(_, body, _)| body.to_vec())
        .collect()
}

/// The elements directly inside a buffer: id, payload, and the whole element
/// including its header — the last of which is what copying verbatim needs.
fn walk(buf: &[u8]) -> Vec<(u32, &[u8], &[u8])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < buf.len() {
        let Some(id_len) = leading_length(buf[at]) else { break };
        if at + id_len >= buf.len() {
            break;
        }
        let mut id = 0u32;
        for i in 0..id_len {
            id = (id << 8) | buf[at + i] as u32;
        }
        let size_at = at + id_len;
        let Some(size_len) = leading_length(buf[size_at]) else { break };
        if size_at + size_len > buf.len() {
            break;
        }
        let mask = value_mask(size_len);
        let mut size = (buf[size_at] & mask) as u64;
        for i in 1..size_len {
            size = (size << 8) | buf[size_at + i] as u64;
        }
        let body = size_at + size_len;
        let Some(end) = body.checked_add(size as usize).filter(|e| *e <= buf.len()) else {
            break;
        };
        out.push((id, &buf[body..end], &buf[at..end]));
        at = end;
    }
    out
}

/* ------------------------------------------------------------------ *
 * Writing
 * ------------------------------------------------------------------ */

/// An element id, as its bytes.
fn id_bytes(id: u32) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let lead = bytes.iter().position(|b| *b != 0).unwrap_or(3);
    bytes[lead..].to_vec()
}

/// A length, in the fewest bytes that will hold it.
fn vint(value: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // One bit of each byte is the marker, so a length of `len` bytes
        // carries `7 * len` bits — and the all-ones value is reserved for
        // "unknown", so it cannot be used for a real length.
        let capacity = (1u64 << (7 * len)) - 1;
        if value < capacity {
            let mut out = value.to_be_bytes()[(8 - len as usize)..].to_vec();
            out[0] |= 0x80 >> (len - 1);
            return out;
        }
    }
    vec![0xFF]
}

/// A length written in exactly eight bytes, so it can be filled in later.
fn vint_fixed(value: u64) -> [u8; 8] {
    let mut out = value.to_be_bytes();
    out[0] |= 0x01; // the marker for an eight-byte length
    out
}

/// An unsigned integer, in the fewest bytes that will hold it.
fn uint_bytes(value: u64) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let bytes = value.to_be_bytes();
    let lead = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    bytes[lead..].to_vec()
}

/// A complete element: id, length, payload.
fn element(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = id_bytes(id);
    out.extend_from_slice(&vint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

fn uint_element(id: u32, value: u64) -> Vec<u8> {
    element(id, &uint_bytes(value))
}

/// A signed integer, in the fewest bytes that keep its sign.
fn sint_bytes(value: i64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    // Drop leading bytes that only repeat the sign, but never the one that
    // carries it: trimming 0xFF off -1 would leave nothing, and trimming it
    // off -256 would read back as positive.
    let padding = if value < 0 { 0xFF } else { 0x00 };
    let mut start = 0;
    while start < 7 && bytes[start] == padding && (bytes[start + 1] & 0x80 == padding & 0x80) {
        start += 1;
    }
    bytes[start..].to_vec()
}

fn sint_element(id: u32, value: i64) -> Vec<u8> {
    element(id, &sint_bytes(value))
}

fn float_element(id: u32, value: f64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

fn string_element(id: u32, value: &str) -> Vec<u8> {
    element(id, value.as_bytes())
}

/// Where one frame ended up, so the index at the end can point at it.
struct Cue {
    time: u64,
    track: u64,
    /// Where the frame's cluster starts, measured from the start of the
    /// segment's contents.
    cluster_at: u64,
    /// Where the frame's own element starts inside that cluster. Counted
    /// from the cluster's *contents*, which begin with its timestamp — so
    /// this is only complete once the cluster has been written and the
    /// length of that timestamp is known.
    within: u64,
}

/// Rebuild the inputs as one WebM file.
///
/// Every source track becomes an output track, renumbered from 1, and the
/// frames of all of them are interleaved by timestamp into clusters. Sample
/// bytes are copied from wherever they already are; nothing is decoded, and
/// the output plays exactly what the input encoded.
pub fn remux(sources: &[Source], out: &Path) -> Result<()> {
    if sources.is_empty() {
        bail!("nothing to merge");
    }

    // Output track numbers, handed out across every source in order. `at`
    // indexes this by (source, track) so a frame can find its new number.
    let mut numbering: Vec<Vec<u64>> = Vec::new();
    let mut next = 1u64;
    let mut track_entries = Vec::new();
    for source in sources {
        let mut mine = Vec::new();
        for track in &source.tracks {
            let mut payload = uint_element(TRACK_NUMBER, next);
            payload.extend_from_slice(&uint_element(TRACK_UID, next));
            payload.extend_from_slice(&track.fields);
            track_entries.push(element(TRACK_ENTRY, &payload));
            mine.push(next);
            next += 1;
        }
        numbering.push(mine);
    }

    // Everything is on one timeline. Tracks fetched as separate streams each
    // start from their own zero, and a stream that genuinely begins later
    // should keep that distance — so the earliest frame anywhere is the
    // origin, and nothing is shifted relative to anything else.
    let origin = sources
        .iter()
        .flat_map(|s| s.frames.first())
        .map(|f| f.timestamp_ns)
        .min()
        .unwrap_or(0);
    let last = sources
        .iter()
        .flat_map(|s| s.frames.last())
        .map(|f| f.timestamp_ns)
        .max()
        .unwrap_or(origin);
    let duration_ms = ((last - origin) as f64 / SCALE_NS as f64).max(0.0);

    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);

    // The header says what dialect this is. DocType "webm" rather than
    // "matroska": the file only ever holds what a WebM may hold, since its
    // tracks came out of WebM files, and the narrower claim is the one every
    // browser will play.
    let mut header = Vec::new();
    header.extend_from_slice(&uint_element(EBML_VERSION, 1));
    header.extend_from_slice(&uint_element(EBML_READ_VERSION, 1));
    header.extend_from_slice(&uint_element(EBML_MAX_ID_LENGTH, 4));
    header.extend_from_slice(&uint_element(EBML_MAX_SIZE_LENGTH, 8));
    header.extend_from_slice(&string_element(DOC_TYPE, "webm"));
    header.extend_from_slice(&uint_element(DOC_TYPE_VERSION, 4));
    header.extend_from_slice(&uint_element(DOC_TYPE_READ_VERSION, 2));
    writer.write_all(&element(EBML, &header))?;

    // The Segment's length is not known until its contents have been written,
    // so it is written at full width now and filled in at the end.
    writer.write_all(&id_bytes(SEGMENT))?;
    let segment_size_at = writer.stream_position()?;
    writer.write_all(&vint_fixed(0))?;
    let segment_at = writer.stream_position()?;

    let mut info = uint_element(TIMESTAMP_SCALE, SCALE_NS);
    info.extend_from_slice(&float_element(DURATION, duration_ms));
    info.extend_from_slice(&string_element(MUXING_APP, "MDM"));
    info.extend_from_slice(&string_element(WRITING_APP, "MDM"));
    writer.write_all(&element(INFO, &info))?;
    writer.write_all(&element(TRACKS, &track_entries.concat()))?;

    // One reader per source, kept open: the frames of two files are written
    // interleaved, so both are read from throughout.
    let mut readers: Vec<BufReader<File>> = sources
        .iter()
        .map(|s| {
            File::open(&s.path)
                .map(|f| BufReader::with_capacity(1 << 16, f))
                .with_context(|| format!("opening {}", s.path.display()))
        })
        .collect::<Result<_>>()?;

    // A merge is a merge of already-sorted lists, so each source only needs a
    // finger on where it has got to.
    let mut heads: Vec<usize> = vec![0; sources.len()];
    let mut cues: Vec<Cue> = Vec::new();
    let mut cluster: Vec<u8> = Vec::new();
    let mut cluster_ms: i64 = 0;
    let mut cluster_cues: Vec<Cue> = Vec::new();
    let mut copy = vec![0u8; 1 << 16];

    loop {
        // Whichever source has the next frame by time. Ties go to the lower
        // index, which puts video before its audio at the same instant.
        let Some(pick) = (0..sources.len())
            .filter(|i| heads[*i] < sources[*i].frames.len())
            .min_by_key(|i| sources[*i].frames[heads[*i]].timestamp_ns)
        else {
            break;
        };
        let frame = &sources[pick].frames[heads[pick]];
        heads[pick] += 1;

        let ms = (frame.timestamp_ns - origin) / SCALE_NS as i64;
        // A new cluster when this one has run long enough, grown big enough,
        // or when the frame could no longer name its own time within it. The
        // last is not a preference: a block's timestamp is sixteen signed
        // bits relative to its cluster, and there is no writing a frame that
        // sits outside that.
        let too_far = ms - cluster_ms > CLUSTER_MS || ms < cluster_ms;
        let keyframe_break = frame.keyframe && sources[pick].tracks[frame.track].kind == Kind::Video;
        if !cluster.is_empty() && (too_far || cluster.len() > CLUSTER_BYTES || keyframe_break) {
            let (at, before_blocks) = flush_cluster(&mut writer, segment_at, cluster_ms, &cluster)?;
            for mut cue in cluster_cues.drain(..) {
                cue.cluster_at = at;
                cue.within += before_blocks;
                cues.push(cue);
            }
            cluster.clear();
        }
        if cluster.is_empty() {
            cluster_ms = ms;
        }

        let track_number = numbering[pick][frame.track];
        let relative = (ms - cluster_ms) as i16;
        let mut block = vint(track_number);
        block.extend_from_slice(&relative.to_be_bytes());
        // The keyframe flag lives in a `SimpleBlock`; inside a `BlockGroup`
        // the same fact is told by leaving out a `ReferenceBlock`.
        block.push(if frame.keyframe { 0x80 } else { 0x00 });
        let mut payload = Vec::with_capacity(block.len() + frame.size as usize);
        payload.extend_from_slice(&block);

        let reader = &mut readers[pick];
        reader.seek(SeekFrom::Start(frame.offset))?;
        let mut left = frame.size as usize;
        while left > 0 {
            let want = left.min(copy.len());
            reader.read_exact(&mut copy[..want]).with_context(|| {
                format!("reading {} bytes at {} of {}", want, frame.offset, sources[pick].path.display())
            })?;
            payload.extend_from_slice(&copy[..want]);
            left -= want;
        }

        let element_at = cluster.len();
        if frame.discard_ns == 0 {
            cluster.extend_from_slice(&element(SIMPLE_BLOCK, &payload));
        } else {
            // Padding at the end of a frame is not something a
            // `SimpleBlock` can carry, so this one is wrapped in the older
            // form that can. It is the last frame of a track, once.
            let mut group = element(BLOCK, &payload);
            group.extend_from_slice(&sint_element(DISCARD_PADDING, frame.discard_ns));
            if !frame.keyframe {
                // Nothing may claim to be decodable alone that is not, and
                // in a group the claim is made by silence.
                group.extend_from_slice(&sint_element(REFERENCE_BLOCK, 0));
            }
            cluster.extend_from_slice(&element(BLOCK_GROUP, &group));
        }

        // Only keyframes are worth an index entry, because they are the only
        // places a player can start decoding from.
        if frame.keyframe {
            cluster_cues.push(Cue {
                time: ms.max(0) as u64,
                track: track_number,
                cluster_at: 0, // filled in when the cluster lands
                within: element_at as u64,
            });
        }
    }

    if !cluster.is_empty() {
        let (at, before_blocks) = flush_cluster(&mut writer, segment_at, cluster_ms, &cluster)?;
        for mut cue in cluster_cues.drain(..) {
            cue.cluster_at = at;
            cue.within += before_blocks;
            cues.push(cue);
        }
    }

    // The index, written last because only now is it known. A player looking
    // for it reads the top-level elements of the segment, which is exactly
    // what it does to find the clusters anyway.
    if !cues.is_empty() {
        let mut points = Vec::new();
        for cue in &cues {
            let mut positions = uint_element(CUE_TRACK, cue.track);
            positions.extend_from_slice(&uint_element(CUE_CLUSTER_POSITION, cue.cluster_at));
            positions.extend_from_slice(&uint_element(CUE_RELATIVE_POSITION, cue.within));
            let mut point = uint_element(CUE_TIME, cue.time);
            point.extend_from_slice(&element(CUE_TRACK_POSITIONS, &positions));
            points.extend_from_slice(&element(CUE_POINT, &point));
        }
        writer.write_all(&element(CUES, &points))?;
    }

    // Now the Segment's length is known: go back and say it.
    let end = writer.stream_position()?;
    writer.flush()?;
    let mut file = writer.into_inner().context("finishing the output file")?;
    file.seek(SeekFrom::Start(segment_size_at))?;
    file.write_all(&vint_fixed(end - segment_at))?;
    file.flush()?;
    Ok(())
}

/// Write one cluster, and answer both of the things its cues need: where it
/// landed, measured from the start of the segment's contents, and how much
/// of it comes before the first block.
fn flush_cluster(
    writer: &mut BufWriter<File>,
    segment_at: u64,
    timestamp_ms: i64,
    blocks: &[u8],
) -> Result<(u64, u64)> {
    let at = writer.stream_position()?;
    let timestamp = uint_element(TIMESTAMP, timestamp_ms.max(0) as u64);
    let before_blocks = timestamp.len() as u64;
    let mut payload = timestamp;
    payload.extend_from_slice(blocks);
    writer.write_all(&element(CLUSTER, &payload))?;
    Ok((at - segment_at, before_blocks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_round_trip_through_the_marker_bits() {
        // The marker steals a bit from the first byte, so 127 does not fit in
        // one byte the way it would anywhere else: it is the reserved
        // "unknown" value and has to spill into two.
        assert_eq!(vint(0), vec![0x80]);
        assert_eq!(vint(1), vec![0x81]);
        assert_eq!(vint(126), vec![0xFE]);
        assert_eq!(vint(127), vec![0x40, 0x7F]);
        assert_eq!(vint(0x3FFE), vec![0x7F, 0xFE]);
    }

    /// Every length this writes has to be readable by the reader beside it,
    /// which is the only agreement that actually matters.
    #[test]
    fn what_is_written_is_what_is_read() {
        for value in [0u64, 1, 63, 126, 127, 1000, 65_535, 1 << 20, (1 << 35) - 2] {
            let encoded = vint(value);
            let mut cur = Cursor { inner: std::io::Cursor::new(encoded.clone()), at: 0 };
            let (back, width) = read_vint(&mut cur).unwrap();
            assert_eq!(back, value, "{value} came back as {back}");
            assert_eq!(width as usize, encoded.len());
        }
    }

    #[test]
    fn a_fixed_width_length_is_still_a_length() {
        let bytes = vint_fixed(1234);
        let mut cur = Cursor { inner: std::io::Cursor::new(bytes.to_vec()), at: 0 };
        assert_eq!(cur.size().unwrap(), Some(1234));
    }

    #[test]
    fn an_unknown_length_is_told_apart_from_a_long_one() {
        // All ones is "unknown"; one bit short of it is a real, large length.
        let mut unknown = Cursor { inner: std::io::Cursor::new(vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), at: 0 };
        assert_eq!(unknown.size().unwrap(), None);
        let mut known = Cursor { inner: std::io::Cursor::new(vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]), at: 0 };
        assert_eq!(known.size().unwrap(), Some((1u64 << 56) - 2));
    }

    #[test]
    fn ids_keep_their_marker_bits() {
        assert_eq!(id_bytes(SIMPLE_BLOCK), vec![0xA3]);
        assert_eq!(id_bytes(TIMESTAMP_SCALE), vec![0x2A, 0xD7, 0xB1]);
        assert_eq!(id_bytes(SEGMENT), vec![0x18, 0x53, 0x80, 0x67]);
    }

    #[test]
    fn elements_nest_the_way_they_are_read_back() {
        let inner = uint_element(TRACK_NUMBER, 7);
        let outer = element(TRACK_ENTRY, &inner);
        let found = walk(&outer);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, TRACK_ENTRY);
        assert_eq!(child(found[0].1, TRACK_NUMBER).map(|v| uint(&v)), Some(7));
    }

    /// A track is copied by keeping everything except what names it, so a
    /// codec's own description survives without being understood.
    #[test]
    fn a_track_keeps_everything_but_its_number() {
        let mut entry = uint_element(TRACK_NUMBER, 3);
        entry.extend_from_slice(&uint_element(TRACK_UID, 999));
        entry.extend_from_slice(&uint_element(TRACK_TYPE, 2));
        entry.extend_from_slice(&string_element(0x86, "A_OPUS"));
        entry.extend_from_slice(&element(0x63A2, &[1, 2, 3, 4]));
        entry.extend_from_slice(&uint_element(0x56AA, 6_500_000));

        let track = track_from(&entry).expect("an audio track");
        assert_eq!(track.number, 3);
        assert_eq!(track.kind, Kind::Audio);
        assert!(child(&track.fields, TRACK_NUMBER).is_none(), "the number is rewritten");
        assert!(child(&track.fields, TRACK_UID).is_none(), "so is the uid");
        assert_eq!(child(&track.fields, 0x86), Some(b"A_OPUS".to_vec()));
        assert_eq!(child(&track.fields, 0x63A2), Some(vec![1, 2, 3, 4]));
        assert_eq!(
            child(&track.fields, 0x56AA).map(|v| uint(&v)),
            Some(6_500_000),
            "Opus's pre-skip has to survive, or the sound starts in the wrong place"
        );
    }

    /// Discard padding is negative as often as not, and a sign that did
    /// not survive the round trip would trim the wrong end of a track.
    #[test]
    fn signed_values_keep_their_sign_and_their_width() {
        for value in [0i64, 1, -1, 127, -128, 255, -256, 6_500_000, -6_500_000, i32::MIN as i64] {
            let bytes = sint_bytes(value);
            assert_eq!(sint(&bytes), value, "{value} came back as {}", sint(&bytes));
            assert!(bytes.len() <= 8);
        }
        // The shortest form, not merely a correct one.
        assert_eq!(sint_bytes(-1), vec![0xFF]);
        assert_eq!(sint_bytes(0), vec![0x00]);
        assert_eq!(sint_bytes(127), vec![0x7F]);
        assert_eq!(sint_bytes(128), vec![0x00, 0x80]);
    }

    #[test]
    fn a_track_that_is_neither_picture_nor_sound_is_not_carried() {
        let mut entry = uint_element(TRACK_NUMBER, 1);
        entry.extend_from_slice(&uint_element(TRACK_TYPE, 17)); // subtitles
        assert!(track_from(&entry).is_none());
    }
}
