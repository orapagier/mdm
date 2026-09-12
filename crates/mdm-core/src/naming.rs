//! What a download is called on disk.
//!
//! A leaf: no module in the crate is reachable from here. The name a download
//! takes is decided before anything is fetched and again when the server, an
//! extractor or the user offers a better one, so both the engine and the
//! fetcher need these — and neither needs the other to get them.

use crate::now;

/// Does any of `names` already belong to this stem?
///
/// Everything yt-dlp derives from a stem begins with `stem.` — `stem.webm`,
/// the `stem.f251.webm.part` of a download under way, the `stem.temp.mp4` of
/// one being muxed — and a download that has not written a byte yet holds the
/// bare stem. Any of them means the name is spoken for.
pub fn stem_taken(names: &[String], stem: &str) -> bool {
    names.iter().any(|name| {
        name == stem
            || name
                .strip_prefix(stem)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// `("archive", ".zip")`, or `("plain", "")`. The dot travels with the
/// extension, so a name without one needs no special case when they are
/// joined back together.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        // A leading dot makes a hidden file, not an extension.
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// The first of `file.iso`, `file_2.iso`, `file_3.iso` … that `taken` does not
/// claim.
///
/// The number goes before the extension, where it belongs: a `.iso` that
/// becomes `.iso_2` stops being an ISO as far as everything else is concerned.
pub fn unique_filename(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = split_extension(name);
    let free = unique_name(stem, |candidate| taken(&format!("{candidate}{ext}")));
    format!("{free}{ext}")
}

/// The first of `name`, `name_2`, `name_3` … that `taken` does not claim.
///
/// What counts as taken differs by caller — a file on disk, a name another
/// download has reserved, or both — so it is asked rather than assumed.
pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    // Bounded: a predicate that answers yes to everything must not be allowed
    // to spin, and a timestamp is unique enough to end the argument.
    (2..1000)
        .map(|n| format!("{name}_{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| format!("{name}_{}", now()))
}

/// Make a name from a server, a page title or the user safe to write to disk.
///
/// The last path segment only: a name like `../../.bashrc` would otherwise
/// write outside the folder the user chose.
///
/// The characters Windows refuses — `< > : " | ? *` — go with it, along with
/// trailing dots and spaces (which Windows silently drops when it creates the
/// file, leaving a name we would never find again) and the DOS device names it
/// still reserves. A video titled "To Rescue a Sinner Like Me | Quennie
/// Benabaye (Cover)" is otherwise refused outright, before a byte is fetched.
/// The same rules apply on every platform rather than behind a `cfg`: a folder
/// is shared, synced and moved, and a name only one system can hold is a name
/// the download cannot keep.
pub fn sanitize(name: String) -> String {
    /// Names MS-DOS gave to devices, which Windows will not let a file take —
    /// with or without an extension, in any case.
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5",
        "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5",
        "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    /// Long enough for any real title, short enough to leave room for the
    /// `.f251.webm.part` an extractor hangs off the stem.
    const LIMIT: usize = 200;

    let base = name.rsplit(['/', '\\']).next().unwrap_or("").to_string();
    let mut out: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    out = out
        .trim()
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' '])
        .to_string();
    if out.is_empty() {
        return "download".into();
    }
    if RESERVED.contains(&split_extension(&out).0.to_ascii_lowercase().as_str()) {
        out = format!("_{out}");
    }
    if out.len() > LIMIT {
        // Cut the stem, not the extension: a name that loses its `.mp4` is
        // filed under the wrong category and opens with the wrong program.
        // Cutting on a byte is not enough either — a title is as likely to be
        // Japanese as English, and half a character is not a name at all.
        let (stem, ext) = split_extension(&out);
        let ext = if ext.len() <= 20 { ext } else { "" };
        let mut cut = LIMIT.saturating_sub(ext.len()).min(stem.len());
        while cut > 0 && !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        out = format!("{}{ext}", stem[..cut].trim_end_matches(['.', ' ']));
        if out.is_empty() || out.starts_with('.') {
            return "download".into();
        }
    }
    out
}

pub fn filename_from_url(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|s| s.filter(|p| !p.is_empty()).next_back())
                .map(percent_decode)
        })
        .filter(|s| !s.is_empty())
        .map(|name| strip_page_extension(&name))
        .unwrap_or_else(|| "download".into())
}

/// Drop the extension when the last path segment names a *script* rather than
/// a file.
///
/// `view_video.php` is the address of a player page, and carrying its `.php`
/// into the download gives a row that claims an extension it will never have,
/// files itself under "Other" by that extension, and shows the user a name
/// that was never going to be the name. The real one arrives when the
/// extractor reports it; until then `view_video` is the honest half of what we
/// know.
fn strip_page_extension(name: &str) -> String {
    const PAGE_EXTENSIONS: &[&str] = &[
        "php", "php3", "php4", "php5", "asp", "aspx", "jsp", "jspx", "cgi",
        "pl", "do", "action", "html", "htm", "xhtml", "shtml",
    ];
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && PAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) =>
        {
            stem.to_string()
        }
        _ => name.to_string(),
    }
}

pub(crate) fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}
