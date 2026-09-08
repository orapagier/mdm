//! HLS playlists, parsed by hand.
//!
//! The format is line-based and small enough that a crate would be more
//! surface than it saves. Two shapes matter: a *master* playlist, which lists
//! renditions to choose between, and a *media* playlist, which lists the
//! segments of one rendition. A server may hand back either at the same URL,
//! so which one arrived is decided by looking at it.
//!
//! What is deliberately not handled is encryption. `#EXT-X-KEY` with anything
//! but `NONE` is reported rather than ignored, because a decrypted-as-if-plain
//! segment is not a broken download — it is a file that exists, weighs the
//! right amount and is noise from end to end. Those streams stay with yt-dlp.

use anyhow::{bail, Result};
use url::Url;

/// One rendition offered by a master playlist.
#[derive(Debug, Clone)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: String,
    /// The `AUDIO` group this rendition expects its sound to come from, when
    /// the picture is served without it.
    pub audio_group: Option<String>,
}

/// An `#EXT-X-MEDIA` entry: usually the audio that pairs with a video-only
/// rendition, occasionally subtitles, which are ignored.
#[derive(Debug, Clone)]
pub struct Media {
    pub kind: String,
    pub group: String,
    pub name: String,
    pub uri: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub media: Vec<Media>,
}

/// One segment, and the slice of it that belongs to this entry.
#[derive(Debug, Clone)]
pub struct Segment {
    pub uri: String,
    /// `(offset, length)` when `#EXT-X-BYTERANGE` narrowed it to part of a
    /// larger file, which is how a single-file HLS rendition is served.
    pub range: Option<(u64, u64)>,
    pub duration: f64,
}

#[derive(Debug, Clone, Default)]
pub struct MediaPlaylist {
    /// `#EXT-X-MAP`: the init segment an fMP4 rendition needs in front of
    /// everything else. Absent on a transport-stream rendition.
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
    /// True while the playlist has no `#EXT-X-ENDLIST` — a live stream, which
    /// has no end to download to.
    pub live: bool,
}

/// Whether this text lists renditions rather than segments.
pub fn is_master(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("#EXT-X-STREAM-INF"))
}

/// Whether this text is an HLS playlist at all.
pub fn is_playlist(text: &str) -> bool {
    text.trim_start().starts_with("#EXTM3U")
}

/// Split an attribute list — `A=1,B="x,y",C=3` — respecting quotes.
///
/// Naive splitting on commas is wrong precisely where it matters: `CODECS`
/// lists several codecs inside one pair of quotes, and cutting it there loses
/// the audio codec of every rendition.
fn attributes(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_key = true;
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            '=' if in_key && !quoted => in_key = false,
            ',' if !quoted => {
                if !key.is_empty() {
                    out.push((key.trim().to_ascii_uppercase(), value.trim().to_string()));
                }
                key.clear();
                value.clear();
                in_key = true;
            }
            _ if in_key => key.push(c),
            _ => value.push(c),
        }
    }
    if !key.is_empty() {
        out.push((key.trim().to_ascii_uppercase(), value.trim().to_string()));
    }
    out
}

fn attribute(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// Resolve a playlist-relative URI against the playlist's own address.
fn resolve(base: &Url, uri: &str) -> String {
    base.join(uri).map(|u| u.to_string()).unwrap_or_else(|_| uri.to_string())
}

/// `<length>[@<offset>]`. A missing offset continues from the end of the
/// previous range, which is how a byte-range playlist walks one file.
fn byterange(spec: &str, previous_end: u64) -> Option<(u64, u64)> {
    let mut parts = spec.trim().splitn(2, '@');
    let length: u64 = parts.next()?.trim().parse().ok()?;
    let offset = match parts.next() {
        Some(o) => o.trim().parse().ok()?,
        None => previous_end,
    };
    Some((offset, length))
}

pub fn parse_master(text: &str, base: &str) -> Result<Master> {
    let base = Url::parse(base)?;
    let mut master = Master::default();
    let mut pending: Option<Vec<(String, String)>> = None;

    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributes(rest);
            master.media.push(Media {
                kind: attribute(&attrs, "TYPE").unwrap_or_default(),
                group: attribute(&attrs, "GROUP-ID").unwrap_or_default(),
                name: attribute(&attrs, "NAME").unwrap_or_default(),
                uri: attribute(&attrs, "URI").map(|u| resolve(&base, &u)),
                default: attribute(&attrs, "DEFAULT")
                    .is_some_and(|d| d.eq_ignore_ascii_case("YES")),
            });
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A bare line following #EXT-X-STREAM-INF is that rendition's address.
        if let Some(attrs) = pending.take() {
            let resolution = attribute(&attrs, "RESOLUTION").unwrap_or_default();
            let (width, height) = resolution
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None));
            master.variants.push(Variant {
                uri: resolve(&base, line),
                bandwidth: attribute(&attrs, "BANDWIDTH")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0),
                width,
                height,
                codecs: attribute(&attrs, "CODECS").unwrap_or_default(),
                audio_group: attribute(&attrs, "AUDIO").filter(|g| !g.is_empty()),
            });
        }
    }

    if master.variants.is_empty() {
        bail!("the master playlist offered no renditions");
    }
    Ok(master)
}

pub fn parse_media(text: &str, base: &str) -> Result<MediaPlaylist> {
    let base = Url::parse(base)?;
    let mut playlist = MediaPlaylist {
        live: true,
        ..Default::default()
    };
    let mut duration = 0.0f64;
    let mut range: Option<(u64, u64)> = None;
    let mut previous_end = 0u64;

    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest
                .split(',')
                .next()
                .and_then(|d| d.trim().parse().ok())
                .unwrap_or(0.0);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = byterange(rest, previous_end);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(rest);
            let Some(uri) = attribute(&attrs, "URI") else {
                continue;
            };
            playlist.init = Some(Segment {
                uri: resolve(&base, &uri),
                range: attribute(&attrs, "BYTERANGE").and_then(|r| byterange(&r, 0)),
                duration: 0.0,
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributes(rest);
            let method = attribute(&attrs, "METHOD").unwrap_or_default();
            if !method.is_empty() && !method.eq_ignore_ascii_case("NONE") {
                // Deliberately fatal. Copying the ciphertext through would
                // produce a file of exactly the right size that plays as
                // nothing at all, which is the worst way to fail.
                bail!(
                    "this stream is encrypted ({method}), which the built-in \
                     downloader cannot decrypt"
                );
            }
            continue;
        }
        if line == "#EXT-X-ENDLIST" {
            playlist.live = false;
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((offset, length)) = range {
            previous_end = offset + length;
        }
        playlist.segments.push(Segment {
            uri: resolve(&base, line),
            range: range.take(),
            duration,
        });
        duration = 0.0;
    }

    if playlist.segments.is_empty() {
        bail!("the playlist listed no segments");
    }
    Ok(playlist)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"aac\"\n\
        v720/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028\"\n\
        v1080/index.m3u8\n";

    #[test]
    fn a_master_playlist_is_told_from_a_media_one() {
        assert!(is_master(MASTER));
        assert!(!is_master("#EXTM3U\n#EXTINF:4.0,\nseg0.ts\n"));
    }

    #[test]
    fn renditions_carry_their_resolution_and_audio_group() {
        let m = parse_master(MASTER, "https://example.test/hls/master.m3u8").unwrap();
        assert_eq!(m.variants.len(), 2);
        assert_eq!(m.variants[1].height, Some(1080));
        assert_eq!(
            m.variants[0].uri,
            "https://example.test/hls/v720/index.m3u8",
            "relative URIs resolve against the playlist"
        );
        assert_eq!(m.variants[0].audio_group.as_deref(), Some("aac"));
        assert_eq!(m.media[0].uri.as_deref(), Some("https://example.test/hls/audio/en.m3u8"));
    }

    #[test]
    fn a_quoted_codec_list_survives_attribute_splitting() {
        // The comma inside CODECS is the whole point: split naively and this
        // rendition loses its audio codec, and with it the knowledge that it
        // needs no separate sound track.
        let m = parse_master(MASTER, "https://example.test/hls/master.m3u8").unwrap();
        assert_eq!(m.variants[0].codecs, "avc1.64001f,mp4a.40.2");
    }

    #[test]
    fn segments_and_the_init_map_are_collected() {
        let text = "#EXTM3U\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n\
            #EXTINF:4.000,\n\
            seg1.m4s\n\
            #EXTINF:3.500,\n\
            seg2.m4s\n\
            #EXT-X-ENDLIST\n";
        let p = parse_media(text, "https://example.test/hls/index.m3u8").unwrap();
        assert_eq!(p.init.unwrap().uri, "https://example.test/hls/init.mp4");
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[1].duration, 3.5);
        assert!(!p.live, "EXT-X-ENDLIST means the stream has an end");
    }

    #[test]
    fn a_byterange_playlist_walks_one_file() {
        // The second range omits its offset, which means "straight after the
        // previous one". Getting this wrong re-downloads the same bytes.
        let text = "#EXTM3U\n\
            #EXTINF:4.000,\n\
            #EXT-X-BYTERANGE:1000@0\n\
            all.ts\n\
            #EXTINF:4.000,\n\
            #EXT-X-BYTERANGE:2000\n\
            all.ts\n\
            #EXT-X-ENDLIST\n";
        let p = parse_media(text, "https://example.test/hls/index.m3u8").unwrap();
        assert_eq!(p.segments[0].range, Some((0, 1000)));
        assert_eq!(p.segments[1].range, Some((1000, 2000)));
    }

    #[test]
    fn an_encrypted_playlist_is_refused_rather_than_mangled() {
        let text = "#EXTM3U\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
            #EXTINF:4.000,\n\
            seg1.ts\n\
            #EXT-X-ENDLIST\n";
        let err = parse_media(text, "https://example.test/hls/index.m3u8").unwrap_err();
        assert!(err.to_string().contains("encrypted"), "got: {err}");
    }

    #[test]
    fn a_key_of_none_is_not_encryption() {
        let text = "#EXTM3U\n\
            #EXT-X-KEY:METHOD=NONE\n\
            #EXTINF:4.000,\n\
            seg1.ts\n\
            #EXT-X-ENDLIST\n";
        assert!(parse_media(text, "https://example.test/hls/index.m3u8").is_ok());
    }
}
