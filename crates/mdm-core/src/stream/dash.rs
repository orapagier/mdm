//! DASH manifests (MPD), parsed into a flat list of downloadable renditions.
//!
//! An MPD is XML, and unlike an HLS playlist it is worth reading with a real
//! parser. The shape that matters is narrow: a Period holds AdaptationSets
//! (one for picture, one for sound, usually), each holding Representations
//! that are the same content at different qualities. What varies wildly is how
//! a Representation says where its segments are, and there are three ways:
//!
//!   * `SegmentTemplate`, which builds URLs from a pattern — with a
//!     `SegmentTimeline` listing each segment's duration, or with a fixed
//!     duration and a count implied by the presentation's length.
//!   * `SegmentList`, which simply names them.
//!   * `SegmentBase`, which means the whole Representation is one ordinary
//!     file and no segmentation is involved at all.
//!
//! All three are handled, because which one a site uses is not something the
//! user picked.

use anyhow::{bail, Context, Result};
use quick_xml::events::attributes::Attribute;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use url::Url;

use super::mp4::Kind;

/// One piece to fetch: a whole URL, or a slice of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub url: String,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
pub struct Representation {
    pub id: String,
    pub kind: Kind,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: String,
    pub init: Option<Part>,
    pub segments: Vec<Part>,
}

/* ----------------------------- a small DOM ---------------------------- */

/// A generic XML element.
///
/// The MPD is walked twice — once for inherited attributes, once for segment
/// information — and doing that over a tree is far less error-prone than
/// threading the state through a stream of events by hand.
#[derive(Debug, Default, Clone)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn kids<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    fn kid(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
}

/// An attribute's value with entity references resolved.
///
/// `Implicit1_0` matches what the deprecated `unescape_value` did, and is what
/// every manifest in the wild is written to: newline normalisation is the only
/// thing 1.1 changes, and no MPD depends on it.
fn value(attr: &Attribute<'_>) -> String {
    attr.normalized_value(XmlVersion::Implicit1_0)
        .unwrap_or_default()
        .into_owned()
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    // Namespace prefixes vary between packagers and carry nothing we need.
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn parse_xml(text: &str) -> Result<Node> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<Node> = vec![Node::default()];

    loop {
        match reader.read_event().context("reading the manifest")? {
            Event::Start(e) => {
                let mut node = Node {
                    name: local(e.name().as_ref()),
                    ..Default::default()
                };
                for attr in e.attributes().flatten() {
                    node.attrs.push((
                        local(attr.key.as_ref()),
                        value(&attr),
                    ));
                }
                stack.push(node);
            }
            Event::Empty(e) => {
                let mut node = Node {
                    name: local(e.name().as_ref()),
                    ..Default::default()
                };
                for attr in e.attributes().flatten() {
                    node.attrs.push((
                        local(attr.key.as_ref()),
                        value(&attr),
                    ));
                }
                stack
                    .last_mut()
                    .expect("the root is never popped")
                    .children
                    .push(node);
            }
            Event::Text(e) => {
                // `xml10_content` is 0.41's name for what used to be
                // `unescape`: entity references resolved, XML 1.0 rules.
                if let Ok(t) = e.xml10_content() {
                    stack
                        .last_mut()
                        .expect("the root is never popped")
                        .text
                        .push_str(t.as_ref());
                }
            }
            Event::End(_) => {
                if stack.len() > 1 {
                    let node = stack.pop().expect("checked above");
                    stack
                        .last_mut()
                        .expect("the root is never popped")
                        .children
                        .push(node);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    stack
        .pop()
        .and_then(|root| root.children.into_iter().next())
        .context("the manifest had no root element")
}

/* ------------------------------- helpers ------------------------------ */

/// ISO 8601 durations, in the one shape MPDs use: `PT1H2M3.5S`.
fn duration_seconds(spec: &str) -> Option<f64> {
    let rest = spec.trim().strip_prefix('P')?;
    // Only the time part matters; a manifest measured in days is not a thing
    // anyone streams.
    let time = rest.split_once('T').map(|(_, t)| t).unwrap_or(rest);
    let mut total = 0.0f64;
    let mut number = String::new();
    for c in time.chars() {
        match c {
            '0'..='9' | '.' => number.push(c),
            'H' => {
                total += number.parse::<f64>().ok()? * 3600.0;
                number.clear();
            }
            'M' => {
                total += number.parse::<f64>().ok()? * 60.0;
                number.clear();
            }
            'S' => {
                total += number.parse::<f64>().ok()?;
                number.clear();
            }
            _ => number.clear(),
        }
    }
    Some(total)
}

/// Fill in `$Number$`, `$Time$`, `$RepresentationID$` and `$Bandwidth$`,
/// honouring the `%0Nd` padding a template may ask for.
fn expand(template: &str, id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push('$');
            rest = after;
            continue;
        };
        let token = &after[..end];
        rest = &after[end + 1..];
        if token.is_empty() {
            // "$$" is an escaped dollar sign.
            out.push('$');
            continue;
        }
        let (name, format) = match token.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (token, None),
        };
        let value = match name {
            "Number" => number.to_string(),
            "Time" => time.to_string(),
            "RepresentationID" => id.to_string(),
            "Bandwidth" => bandwidth.to_string(),
            _ => {
                out.push('$');
                out.push_str(token);
                out.push('$');
                continue;
            }
        };
        match format {
            // e.g. "05d": pad to five digits with zeroes.
            Some(f) if name != "RepresentationID" => {
                let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
                let width: usize = digits.trim_start_matches('0').parse().unwrap_or_else(|_| {
                    digits.parse().unwrap_or(0)
                });
                out.push_str(&format!("{value:0>width$}"));
            }
            _ => out.push_str(&value),
        }
    }
    out.push_str(rest);
    out
}

fn resolve(base: &Url, uri: &str) -> String {
    base.join(uri)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| uri.to_string())
}

/// `"start-end"`, as `SegmentBase` and `Initialization` write byte ranges.
fn range(spec: &str) -> Option<(u64, u64)> {
    let (start, end) = spec.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    (end >= start).then_some((start, end - start + 1))
}

/// Push one level of `<BaseURL>` onto the address inherited from above.
fn descend(base: &Url, node: &Node) -> Url {
    match node.kid("BaseURL") {
        Some(b) if !b.text.trim().is_empty() => {
            base.join(b.text.trim()).unwrap_or_else(|_| base.clone())
        }
        _ => base.clone(),
    }
}

/* -------------------------------- parsing ----------------------------- */

pub fn parse(text: &str, manifest_url: &str) -> Result<Vec<Representation>> {
    let root = parse_xml(text)?;
    if root.name != "MPD" {
        bail!("not a DASH manifest (root element is <{}>)", root.name);
    }
    if root
        .attr("type")
        .is_some_and(|t| t.eq_ignore_ascii_case("dynamic"))
    {
        bail!("this is a live stream, which has no end to download to");
    }

    let manifest = Url::parse(manifest_url).context("the manifest URL")?;
    let mpd_base = descend(&manifest, &root);
    let total = root
        .attr("mediaPresentationDuration")
        .and_then(duration_seconds)
        .unwrap_or(0.0);

    let mut out = Vec::new();
    for period in root.kids("Period") {
        let period_base = descend(&mpd_base, period);
        let period_duration = period
            .attr("duration")
            .and_then(duration_seconds)
            .unwrap_or(total);

        for set in period.kids("AdaptationSet") {
            let set_base = descend(&period_base, set);
            for rep in set.kids("Representation") {
                // Both levels are consulted for everything: packagers put the
                // same fact at either level more or less at random.
                let mime = rep
                    .attr("mimeType")
                    .or_else(|| set.attr("mimeType"))
                    .or_else(|| rep.attr("contentType"))
                    .or_else(|| set.attr("contentType"))
                    .unwrap_or("");
                let kind = if mime.starts_with("video") {
                    Kind::Video
                } else if mime.starts_with("audio") {
                    Kind::Audio
                } else {
                    // Subtitles and image-based trick-play tracks.
                    continue;
                };

                let rep_base = descend(&set_base, rep);
                let id = rep.attr("id").unwrap_or("").to_string();
                let bandwidth = rep
                    .attr("bandwidth")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0);

                let (init, segments) =
                    match locate(rep, set, &rep_base, &id, bandwidth, period_duration) {
                        Ok(found) => found,
                        Err(e) => {
                            log::debug!("skipping representation {id}: {e:#}");
                            continue;
                        }
                    };
                if segments.is_empty() {
                    continue;
                }

                out.push(Representation {
                    id,
                    kind,
                    bandwidth,
                    width: rep
                        .attr("width")
                        .or_else(|| set.attr("width"))
                        .and_then(|w| w.parse().ok()),
                    height: rep
                        .attr("height")
                        .or_else(|| set.attr("height"))
                        .and_then(|h| h.parse().ok()),
                    codecs: rep
                        .attr("codecs")
                        .or_else(|| set.attr("codecs"))
                        .unwrap_or("")
                        .to_string(),
                    init,
                    segments,
                });
            }
        }
    }

    if out.is_empty() {
        bail!("the manifest offered no audio or video to download");
    }
    Ok(out)
}

/// Work out where one Representation's bytes are.
fn locate(
    rep: &Node,
    set: &Node,
    base: &Url,
    id: &str,
    bandwidth: u64,
    period_duration: f64,
) -> Result<(Option<Part>, Vec<Part>)> {
    // A template on the Representation wins over one on the AdaptationSet.
    if let Some(t) = rep.kid("SegmentTemplate").or_else(|| set.kid("SegmentTemplate")) {
        return from_template(t, base, id, bandwidth, period_duration);
    }
    if let Some(l) = rep.kid("SegmentList").or_else(|| set.kid("SegmentList")) {
        return Ok(from_list(l, base));
    }
    if let Some(b) = rep.kid("SegmentBase").or_else(|| set.kid("SegmentBase")) {
        // One ordinary file. The init range is only worth carrying because the
        // rest of the pipeline expects an init part to exist.
        let init = b
            .kid("Initialization")
            .and_then(|i| i.attr("range"))
            .and_then(range)
            .map(|r| Part {
                url: base.to_string(),
                range: Some(r),
            });
        return Ok((
            init,
            vec![Part {
                url: base.to_string(),
                range: None,
            }],
        ));
    }
    // No segment information at all: the BaseURL is the file.
    Ok((
        None,
        vec![Part {
            url: base.to_string(),
            range: None,
        }],
    ))
}

fn from_template(
    t: &Node,
    base: &Url,
    id: &str,
    bandwidth: u64,
    period_duration: f64,
) -> Result<(Option<Part>, Vec<Part>)> {
    let timescale: f64 = t
        .attr("timescale")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    let start_number: u64 = t
        .attr("startNumber")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let init = t.attr("initialization").map(|i| Part {
        url: resolve(base, &expand(i, id, bandwidth, 0, 0)),
        range: None,
    });
    let Some(media) = t.attr("media") else {
        bail!("a SegmentTemplate with no media attribute");
    };

    let mut segments = Vec::new();
    if let Some(timeline) = t.kid("SegmentTimeline") {
        // Each <S> is one segment, or `r` more of the same length after it.
        // `t` restates the running time wherever it jumps.
        let mut time = 0u64;
        let mut number = start_number;
        for s in timeline.kids("S") {
            if let Some(t0) = s.attr("t").and_then(|v| v.parse().ok()) {
                time = t0;
            }
            let d: u64 = s.attr("d").and_then(|v| v.parse().ok()).unwrap_or(0);
            // `r` counts *repeats*, so `r="2"` means three segments. A
            // negative value means "until the period ends", which only
            // happens on live manifests, already refused above.
            let repeats: i64 = s.attr("r").and_then(|v| v.parse().ok()).unwrap_or(0);
            for _ in 0..=repeats.max(0) {
                segments.push(Part {
                    url: resolve(base, &expand(media, id, bandwidth, number, time)),
                    range: None,
                });
                time += d;
                number += 1;
            }
        }
    } else {
        // No timeline: fixed-length segments, and the count follows from how
        // long the period is.
        let duration: f64 = t
            .attr("duration")
            .and_then(|d| d.parse().ok())
            .unwrap_or(0.0);
        if duration <= 0.0 || period_duration <= 0.0 {
            bail!("a SegmentTemplate with neither a timeline nor a usable duration");
        }
        let per_segment = duration / timescale;
        let count = (period_duration / per_segment).ceil() as u64;
        for i in 0..count {
            let number = start_number + i;
            segments.push(Part {
                url: resolve(base, &expand(media, id, bandwidth, number, i * duration as u64)),
                range: None,
            });
        }
    }
    Ok((init, segments))
}

fn from_list(l: &Node, base: &Url) -> (Option<Part>, Vec<Part>) {
    let init = l.kid("Initialization").map(|i| Part {
        url: i
            .attr("sourceURL")
            .map(|u| resolve(base, u))
            .unwrap_or_else(|| base.to_string()),
        range: i.attr("range").and_then(range),
    });
    let segments = l
        .kids("SegmentURL")
        .map(|s| Part {
            url: s
                .attr("media")
                .map(|u| resolve(base, u))
                .unwrap_or_else(|| base.to_string()),
            range: s.attr("mediaRange").and_then(range),
        })
        .collect();
    (init, segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_durations_come_out_in_seconds() {
        assert_eq!(duration_seconds("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(duration_seconds("PT30S"), Some(30.0));
        assert_eq!(duration_seconds("PT0S"), Some(0.0));
    }

    #[test]
    fn templates_expand_with_padding() {
        assert_eq!(expand("s-$Number$.m4s", "v1", 0, 7, 0), "s-7.m4s");
        assert_eq!(expand("s-$Number%05d$.m4s", "v1", 0, 7, 0), "s-00007.m4s");
        assert_eq!(expand("$RepresentationID$/i.mp4", "v1", 0, 0, 0), "v1/i.mp4");
        assert_eq!(expand("t-$Time$.m4s", "v1", 0, 0, 900), "t-900.m4s");
        // An escaped dollar must survive, and an unknown token must be left
        // alone rather than silently swallowed.
        assert_eq!(expand("a$$b", "v1", 0, 0, 0), "a$b");
        assert_eq!(expand("$Nope$.m4s", "v1", 0, 0, 0), "$Nope$.m4s");
    }

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v0" bandwidth="800000" width="1280" height="720" codecs="avc1.4d401f">
        <SegmentTemplate timescale="1000" duration="5000" startNumber="1"
                         initialization="init-$RepresentationID$.mp4"
                         media="seg-$RepresentationID$-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4">
      <Representation id="a0" bandwidth="128000" codecs="mp4a.40.2">
        <SegmentTemplate timescale="1000" initialization="init-a.mp4" media="seg-a-$Number$.m4s">
          <SegmentTimeline>
            <S t="0" d="5000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn a_manifest_yields_one_representation_per_track() {
        let reps = parse(MPD, "https://example.test/dash/manifest.mpd").unwrap();
        assert_eq!(reps.len(), 2);

        let video = &reps[0];
        assert_eq!(video.kind, Kind::Video);
        assert_eq!(video.height, Some(720));
        assert_eq!(
            video.init.as_ref().unwrap().url,
            "https://example.test/dash/init-v0.mp4"
        );
        // 10 seconds of presentation at 5 seconds a segment.
        assert_eq!(video.segments.len(), 2);
        assert_eq!(video.segments[1].url, "https://example.test/dash/seg-v0-2.m4s");
    }

    #[test]
    fn a_timeline_repeat_counts_the_extra_segments_not_the_total() {
        // r="1" is two segments, not one. Reading it as a total silently drops
        // the second half of every stream packaged this way.
        let reps = parse(MPD, "https://example.test/dash/manifest.mpd").unwrap();
        let audio = reps.iter().find(|r| r.kind == Kind::Audio).unwrap();
        assert_eq!(audio.segments.len(), 2);
    }

    #[test]
    fn a_live_manifest_is_refused() {
        let live = MPD.replace(r#"type="static""#, r#"type="dynamic""#);
        let err = parse(&live, "https://example.test/dash/manifest.mpd").unwrap_err();
        assert!(err.to_string().contains("live"), "got: {err}");
    }

    #[test]
    fn a_base_url_element_redirects_the_segments() {
        let mpd = MPD.replace(
            "<Period>",
            "<Period><BaseURL>https://cdn.example.test/x/</BaseURL>",
        );
        let reps = parse(&mpd, "https://example.test/dash/manifest.mpd").unwrap();
        assert_eq!(
            reps[0].segments[0].url,
            "https://cdn.example.test/x/seg-v0-1.m4s"
        );
    }
}
