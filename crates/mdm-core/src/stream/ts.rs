//! MPEG-TS demuxing, for the half of HLS that is not fragmented MP4.
//!
//! Older HLS — and a lot of live-derived VOD — ships 188-byte transport
//! stream packets rather than `moof`/`mdat` fragments. There is no container
//! to copy a sample entry out of, so this module has to do the one piece of
//! real codec work in the whole stream path: find the H.264 parameter sets and
//! the AAC header, and build the `avcC` and `esds` that describe them.
//!
//! The samples themselves are still copied, not decoded. What changes is only
//! their framing: H.264 arrives as Annex B start codes and MP4 wants
//! length-prefixed NAL units; AAC arrives with an ADTS header per frame and
//! MP4 wants the header gone.
//!
//! Output is one flat file of sample payloads plus an index into it, which is
//! exactly what [`super::mp4::remux`] consumes.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::mp4::{self, Kind, Sample, Track};

const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// PTS and DTS tick at 90 kHz, which becomes the video track's timescale.
const CLOCK: u32 = 90_000;

/// Every AAC frame is this many samples, which is what makes an audio
/// timescale of "the sample rate" give exact, drift-free durations.
const AAC_FRAME: u32 = 1024;

/* ------------------------------ bit reading ---------------------------- */

/// A bit-level reader over an RBSP, for the exponential-Golomb coding H.264
/// uses throughout its parameter sets.
struct Bits<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Bits { buf, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.buf.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// Unsigned exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        Some((1 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Signed exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        let value = k.div_ceil(2) as i32;
        Some(if k % 2 == 0 { -value } else { value })
    }
}

/// Strip the emulation-prevention bytes an Annex B stream inserts.
///
/// A `0x000003` in the payload means `0x0000`; leaving the `03` in place
/// shifts every field after it and yields a plausible-looking but wrong
/// resolution.
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Pull the coded resolution out of an SPS.
///
/// Needed because a transport stream states its dimensions nowhere else, and
/// `tkhd` with a zero width is a file players show as a black rectangle.
fn sps_dimensions(sps: &[u8]) -> Option<(u16, u16)> {
    let data = rbsp(sps);
    // Skip the NAL header byte.
    let mut b = Bits::new(data.get(1..)?);
    let profile = b.bits(8)?;
    b.bits(8)?; // constraint flags and reserved bits
    b.bits(8)?; // level_idc
    b.ue()?; // seq_parameter_set_id

    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = b.ue()?;
        if chroma == 3 {
            b.bit()?; // separate_colour_plane_flag
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.bit()?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            // Scaling lists, which have to be walked to stay in step even
            // though nothing here uses them.
            let count = if chroma == 3 { 12 } else { 8 };
            for i in 0..count {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut next = 8i32;
                    let mut last = 8i32;
                    for _ in 0..size {
                        if next != 0 {
                            let delta = b.se()?;
                            next = (last + delta + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }

    b.ue()?; // log2_max_frame_num_minus4
    let order = b.ue()?;
    if order == 0 {
        b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if order == 1 {
        b.bit()?; // delta_pic_order_always_zero_flag
        b.se()?; // offset_for_non_ref_pic
        b.se()?; // offset_for_top_to_bottom_field
        let cycle = b.ue()?;
        for _ in 0..cycle {
            b.se()?;
        }
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps_in_frame_num_value_allowed_flag

    let width_mbs = b.ue()? + 1;
    let height_map = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?; // mb_adaptive_frame_field_flag
    }
    b.bit()?; // direct_8x8_inference_flag

    let mut width = width_mbs * 16;
    let mut height = (2 - frame_mbs_only) * height_map * 16;
    if b.bit()? == 1 {
        // Cropping is in chroma units for 4:2:0, which is what all of this
        // material is; a 1080p stream is coded as 1088 and cropped down.
        let left = b.ue()?;
        let right = b.ue()?;
        let top = b.ue()?;
        let bottom = b.ue()?;
        width = width.saturating_sub((left + right) * 2);
        height = height.saturating_sub((top + bottom) * 2 * (2 - frame_mbs_only));
    }
    Some((width.min(u16::MAX as u32) as u16, height.min(u16::MAX as u32) as u16))
}

/// Assemble the `avcC` that `avc1` needs, from the parameter sets found in
/// the stream.
fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sps.len() + pps.len() + 16);
    out.push(1); // configurationVersion
    out.push(*sps.get(1).unwrap_or(&0x42)); // AVCProfileIndication
    out.push(*sps.get(2).unwrap_or(&0)); // profile_compatibility
    out.push(*sps.get(3).unwrap_or(&30)); // AVCLevelIndication
    out.push(0xff); // 6 reserved bits, then lengthSizeMinusOne = 3
    out.push(0xe1); // 3 reserved bits, then numOfSequenceParameterSets = 1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPictureParameterSets
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// The sampling frequencies an ADTS header's four-bit index selects.
const ADTS_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/* ------------------------------- demuxing ------------------------------ */

#[derive(Default)]
struct Pending {
    /// The PES payload accumulated so far, before its length is known.
    data: Vec<u8>,
    pts: Option<u64>,
    dts: Option<u64>,
}

#[derive(Default)]
struct VideoOut {
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// `(offset, size, dts, pts, is_sync)`.
    ///
    /// Both timestamps are kept, and the difference between them is the whole
    /// reason: a stream with B-frames is transmitted out of display order, and
    /// `pts - dts` is what puts it back. Dropping it produces a file whose
    /// frame count and duration are exactly right and whose motion stutters.
    /// Durations are filled in afterwards, once the next frame's DTS is known.
    frames: Vec<(u64, u32, u64, u64, bool)>,
}

#[derive(Default)]
struct AudioOut {
    config: Vec<u8>,
    rate: u32,
    channels: u16,
    frames: Vec<(u64, u32)>,
}

/// Demux a transport stream into one flat sample file plus its index.
///
/// `payloads` is written with the video and audio sample bytes in the order
/// they were found; the returned tracks point into it.
pub fn demux(ts: &Path, payloads: &Path) -> Result<Vec<Track>> {
    let mut input = File::open(ts).with_context(|| format!("opening {}", ts.display()))?;
    let out = File::create(payloads)
        .with_context(|| format!("creating {}", payloads.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, out);
    let mut written = 0u64;

    let mut pmt_pid: Option<u16> = None;
    let mut video_pid: Option<u16> = None;
    let mut audio_pid: Option<u16> = None;
    let mut video_pending = Pending::default();
    let mut audio_pending = Pending::default();
    let mut video = VideoOut::default();
    let mut audio = AudioOut::default();

    let mut packet = [0u8; PACKET];
    // Some servers prepend junk; find the first sync byte rather than assuming
    // the stream starts on a packet boundary.
    align(&mut input)?;

    loop {
        match input.read_exact(&mut packet) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading the transport stream"),
        }
        if packet[0] != SYNC {
            // A dropped byte desynchronises everything after it; re-finding
            // the sync byte costs one seek and saves the rest of the file.
            input.seek(SeekFrom::Current(-(PACKET as i64) + 1))?;
            align(&mut input)?;
            continue;
        }

        let pid = (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16;
        let payload_start = packet[1] & 0x40 != 0;
        let adaptation = (packet[3] >> 4) & 0x03;
        let mut at = 4;
        if adaptation & 0x02 != 0 {
            at += 1 + packet[4] as usize;
        }
        if adaptation & 0x01 == 0 || at >= PACKET {
            continue;
        }
        let body = &packet[at..];

        if pid == 0 {
            if let Some(p) = parse_pat(body, payload_start) {
                pmt_pid = Some(p);
            }
            continue;
        }
        if Some(pid) == pmt_pid {
            if let Some((v, a)) = parse_pmt(body, payload_start) {
                video_pid = v;
                audio_pid = a;
            }
            continue;
        }

        let is_video = Some(pid) == video_pid;
        let is_audio = Some(pid) == audio_pid;
        if !is_video && !is_audio {
            continue;
        }
        let pending = if is_video {
            &mut video_pending
        } else {
            &mut audio_pending
        };

        if payload_start {
            // The previous PES for this PID is complete.
            if !pending.data.is_empty() {
                if is_video {
                    flush_video(pending, &mut video, &mut w, &mut written)?;
                } else {
                    flush_audio(pending, &mut audio, &mut w, &mut written)?;
                }
            }
            let (pts, dts, header) = parse_pes(body);
            pending.pts = pts;
            pending.dts = dts;
            pending.data.clear();
            pending.data.extend_from_slice(&body[header.min(body.len())..]);
        } else if !pending.data.is_empty() || pending.pts.is_some() {
            pending.data.extend_from_slice(body);
        }
    }

    if !video_pending.data.is_empty() {
        flush_video(&mut video_pending, &mut video, &mut w, &mut written)?;
    }
    if !audio_pending.data.is_empty() {
        flush_audio(&mut audio_pending, &mut audio, &mut w, &mut written)?;
    }
    w.flush().context("flushing the demuxed samples")?;

    build_tracks(video, audio)
}

fn align(input: &mut File) -> Result<()> {
    let mut byte = [0u8; 1];
    for _ in 0..(PACKET * 8) {
        match input.read_exact(&mut byte) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e).context("scanning for a sync byte"),
        }
        if byte[0] == SYNC {
            input.seek(SeekFrom::Current(-1))?;
            return Ok(());
        }
    }
    bail!("no transport stream sync byte in the first 1504 bytes");
}

/// The PAT names the PID the program map lives on.
fn parse_pat(body: &[u8], payload_start: bool) -> Option<u16> {
    let section = section(body, payload_start)?;
    // 8-byte section header, then (program, pid) pairs, then a 4-byte CRC.
    let entries = section.get(8..section.len().checked_sub(4)?)?;
    for pair in entries.chunks_exact(4) {
        let program = mp4::read_be16(pair, 0);
        let pid = mp4::read_be16(pair, 2) & 0x1fff;
        // Program 0 is the network information table, not a program.
        if program != 0 {
            return Some(pid);
        }
    }
    None
}

/// The PMT names each elementary stream's PID and what codec it carries.
fn parse_pmt(body: &[u8], payload_start: bool) -> Option<(Option<u16>, Option<u16>)> {
    let section = section(body, payload_start)?;
    let info_len = (mp4::read_be16(section, 10) & 0x0fff) as usize;
    let mut at = 12 + info_len;
    let end = section.len().checked_sub(4)?;
    let mut video = None;
    let mut audio = None;
    while at + 5 <= end {
        let stream_type = section[at];
        let pid = mp4::read_be16(section, at + 1) & 0x1fff;
        let es_info = (mp4::read_be16(section, at + 3) & 0x0fff) as usize;
        match stream_type {
            // H.264. HEVC (0x24) is deliberately not claimed: its parameter
            // sets need an `hvcC` this module does not build, and a track
            // written with the wrong description is worse than no track.
            0x1b if video.is_none() => video = Some(pid),
            // AAC in ADTS framing.
            0x0f if audio.is_none() => audio = Some(pid),
            _ => {}
        }
        at += 5 + es_info;
    }
    Some((video, audio))
}

/// Step over the pointer field a section-carrying packet starts with.
fn section(body: &[u8], payload_start: bool) -> Option<&[u8]> {
    if !payload_start {
        return None;
    }
    let pointer = *body.first()? as usize;
    body.get(1 + pointer..)
}

/// Read a PES header: its timestamps, and how long it is.
fn parse_pes(body: &[u8]) -> (Option<u64>, Option<u64>, usize) {
    if body.len() < 9 || body[0] != 0 || body[1] != 0 || body[2] != 1 {
        return (None, None, 0);
    }
    let flags = body[7];
    let header_len = body[8] as usize;
    let mut pts = None;
    let mut dts = None;
    if flags & 0x80 != 0 && body.len() >= 14 {
        pts = Some(timestamp(&body[9..14]));
        if flags & 0x40 != 0 && body.len() >= 19 {
            dts = Some(timestamp(&body[14..19]));
        }
    }
    (pts, dts, 9 + header_len)
}

/// The 33-bit timestamp MPEG scatters across five bytes with marker bits.
fn timestamp(b: &[u8]) -> u64 {
    (((b[0] as u64) >> 1) & 0x07) << 30
        | (b[1] as u64) << 22
        | (((b[2] as u64) >> 1) & 0x7f) << 15
        | (b[3] as u64) << 7
        | ((b[4] as u64) >> 1) & 0x7f
}

/// Walk the Annex B start codes in a buffer, yielding each NAL unit.
fn nal_units(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= buf.len() {
        let three = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        let four = i + 4 <= buf.len()
            && buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1;
        if three || four {
            if let Some(s) = start {
                out.push((s, i));
            }
            i += if four { 4 } else { 3 };
            start = Some(i);
            continue;
        }
        i += 1;
    }
    if let Some(s) = start {
        out.push((s, buf.len()));
    }
    out
}

fn flush_video(
    pending: &mut Pending,
    video: &mut VideoOut,
    w: &mut BufWriter<File>,
    written: &mut u64,
) -> Result<()> {
    let data = std::mem::take(&mut pending.data);
    let offset = *written;
    let mut size = 0u32;
    let mut sync = false;

    for (start, end) in nal_units(&data) {
        let nal = &data[start..end];
        let Some(&first) = nal.first() else { continue };
        match first & 0x1f {
            // Parameter sets go into `avcC`, not into the samples.
            7 => {
                if video.sps.is_empty() {
                    video.sps = nal.to_vec();
                }
                continue;
            }
            8 => {
                if video.pps.is_empty() {
                    video.pps = nal.to_vec();
                }
                continue;
            }
            // Access unit delimiters and filler carry nothing a player needs.
            9 | 12 => continue,
            5 => sync = true,
            _ => {}
        }
        w.write_all(&(nal.len() as u32).to_be_bytes())?;
        w.write_all(nal)?;
        size += 4 + nal.len() as u32;
    }

    *written += size as u64;
    if size > 0 {
        // A PES that gives only one timestamp is telling us the two are equal.
        let pts = pending.pts.or(pending.dts).unwrap_or(0);
        let dts = pending.dts.unwrap_or(pts);
        video.frames.push((offset, size, dts, pts, sync));
    }
    Ok(())
}

fn flush_audio(
    pending: &mut Pending,
    audio: &mut AudioOut,
    w: &mut BufWriter<File>,
    written: &mut u64,
) -> Result<()> {
    let data = std::mem::take(&mut pending.data);
    let mut at = 0usize;
    while at + 7 <= data.len() {
        // ADTS syncword: twelve set bits.
        if data[at] != 0xff || data[at + 1] & 0xf0 != 0xf0 {
            at += 1;
            continue;
        }
        let protection_absent = data[at + 1] & 0x01 != 0;
        let frame_len = (((data[at + 3] as usize) & 0x03) << 11)
            | ((data[at + 4] as usize) << 3)
            | ((data[at + 5] as usize) >> 5);
        if frame_len < 7 || at + frame_len > data.len() {
            break;
        }
        if audio.config.is_empty() {
            let object_type = ((data[at + 2] >> 6) & 0x03) + 1;
            let rate_index = (data[at + 2] >> 2) & 0x0f;
            let channels = (((data[at + 2] & 0x01) << 2) | (data[at + 3] >> 6)) as u16;
            audio.rate = *ADTS_RATES.get(rate_index as usize).unwrap_or(&44100);
            audio.channels = channels.max(1);
            // AudioSpecificConfig: five bits of object type, four of sample
            // rate index, four of channel configuration.
            let asc = ((object_type as u16) << 11)
                | ((rate_index as u16) << 7)
                | (audio.channels << 3);
            audio.config = asc.to_be_bytes().to_vec();
        }
        let header = if protection_absent { 7 } else { 9 };
        if frame_len > header {
            let payload = &data[at + header..at + frame_len];
            w.write_all(payload)?;
            audio.frames.push((*written, payload.len() as u32));
            *written += payload.len() as u64;
        }
        at += frame_len;
    }
    Ok(())
}

fn build_tracks(video: VideoOut, audio: AudioOut) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();

    if !video.frames.is_empty() {
        if video.sps.is_empty() || video.pps.is_empty() {
            bail!("the stream carried H.264 frames but no parameter sets to describe them");
        }
        let (width, height) = sps_dimensions(&video.sps).unwrap_or((0, 0));
        let entry = mp4::avc1_entry(width, height, &avcc(&video.sps, &video.pps));

        // Durations come from the gaps between decode timestamps; the last
        // frame inherits the one before it, having nothing to be measured
        // against.
        let mut samples = Vec::with_capacity(video.frames.len());
        for (i, &(offset, size, dts, pts, sync)) in video.frames.iter().enumerate() {
            let duration = video
                .frames
                .get(i + 1)
                .map(|next| next.2.saturating_sub(dts))
                .unwrap_or(0) as u32;
            samples.push(Sample {
                offset,
                size,
                duration,
                // Signed, and legitimately negative on a reordered stream.
                cto: (pts as i64 - dts as i64) as i32,
                sync,
            });
        }
        let typical = samples
            .iter()
            .map(|s| s.duration)
            .filter(|d| *d > 0)
            .min()
            .unwrap_or(CLOCK / 30);
        if let Some(last) = samples.last_mut() {
            if last.duration == 0 {
                last.duration = typical;
            }
        }
        // A timestamp that wrapped its 33 bits, or a stream spliced from two
        // sources, shows up as one absurd gap. Clamping keeps a single bad
        // frame from stretching the whole timeline.
        let ceiling = typical.saturating_mul(20).max(CLOCK);
        for s in &mut samples {
            if s.duration == 0 || s.duration > ceiling {
                s.duration = typical;
            }
        }

        tracks.push(Track::synthetic(
            1,
            Kind::Video,
            CLOCK,
            width,
            height,
            entry,
            samples,
        ));
    }

    if !audio.frames.is_empty() && !audio.config.is_empty() {
        let entry = mp4::mp4a_entry(audio.channels, audio.rate, &audio.config);
        let samples = audio
            .frames
            .iter()
            .map(|&(offset, size)| Sample {
                offset,
                size,
                duration: AAC_FRAME,
                cto: 0,
                sync: true,
            })
            .collect();
        tracks.push(Track::synthetic(
            2,
            Kind::Audio,
            audio.rate.max(1),
            0,
            0,
            entry,
            samples,
        ));
    }

    if tracks.is_empty() {
        bail!("no H.264 or AAC stream found in the transport stream");
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_reads_the_values_h264_encodes() {
        // 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        // 00 00 03 01 means 00 00 01 — leave the 03 in and every field after
        // it is read one byte late.
        assert_eq!(rbsp(&[0x00, 0x00, 0x03, 0x01]), vec![0x00, 0x00, 0x01]);
        // A 03 that is not preceded by two zeroes is ordinary data.
        assert_eq!(rbsp(&[0x01, 0x03, 0x04]), vec![0x01, 0x03, 0x04]);
    }

    #[test]
    fn start_codes_split_into_nal_units() {
        let buf = [0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x68, 0xbb];
        let nals = nal_units(&buf);
        assert_eq!(nals.len(), 2);
        assert_eq!(&buf[nals[0].0..nals[0].1], &[0x67, 0xaa]);
        assert_eq!(&buf[nals[1].0..nals[1].1], &[0x68, 0xbb]);
    }

    #[test]
    fn a_pes_timestamp_is_reassembled_from_its_marker_bits() {
        // 0 is encoded as the marker bits alone.
        assert_eq!(timestamp(&[0x21, 0x00, 0x01, 0x00, 0x01]), 0);
    }

    #[test]
    fn sps_dimensions_survive_a_real_parameter_set() {
        // A 1280x720 baseline SPS as a browser would emit it.
        let sps = [
            0x67, 0x42, 0xc0, 0x1f, 0xd9, 0x00, 0x50, 0x05, 0xbb, 0x01, 0x6a, 0x02, 0x02,
            0x02, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x8c, 0x18,
            0xcb,
        ];
        let (w, h) = sps_dimensions(&sps).expect("a valid SPS should parse");
        assert_eq!((w, h), (1280, 720));
    }
}
