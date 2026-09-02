//! File-type routing, the way IDM sorts downloads into per-category folders.

/// Category name, doubling as the subdirectory under the download root.
pub const CATEGORIES: &[&str] = &[
    "Video",
    "Music",
    "Documents",
    "Compressed",
    "Programs",
    "Images",
    "Other",
];

const VIDEO: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg", "ts", "m2ts",
    "ogv", "3gp", "vob", "rmvb", "divx", "m3u8", "mpd",
];
const MUSIC: &[&str] = &[
    "mp3", "flac", "wav", "aac", "ogg", "oga", "opus", "m4a", "wma", "alac", "aiff", "ape",
    "mid", "midi",
];
const DOCUMENTS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt",
    "epub", "mobi", "azw3", "djvu", "chm", "tex", "csv", "tsv", "md",
];
const COMPRESSED: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "tbz", "xz", "txz", "zst", "lz4", "lzma",
    "cab", "arj", "z", "iso", "img",
];
const PROGRAMS: &[&str] = &[
    "exe", "msi", "deb", "rpm", "apk", "appimage", "flatpak", "snap", "dmg", "pkg", "bin",
    "run", "jar", "whl", "crx", "xpi", "vsix", "so", "dll", "elf",
];
const IMAGES: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif", "svg", "ico", "heic", "avif",
    "raw", "cr2", "nef", "psd", "xcf",
];

/// Pick a category from the filename first, falling back to the MIME type.
///
/// Extension wins because servers routinely send `application/octet-stream`
/// for everything, whereas the name a user sees is almost always accurate.
pub fn categorize(filename: &str, mime: &str) -> &'static str {
    let ext = extension_of(filename);
    if !ext.is_empty() {
        for (name, list) in [
            ("Video", VIDEO),
            ("Music", MUSIC),
            ("Documents", DOCUMENTS),
            ("Compressed", COMPRESSED),
            ("Programs", PROGRAMS),
            ("Images", IMAGES),
        ] {
            if list.contains(&ext.as_str()) {
                return name;
            }
        }
    }

    let mime = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    if mime.starts_with("video/") {
        return "Video";
    }
    if mime.starts_with("audio/") {
        return "Music";
    }
    if mime.starts_with("image/") {
        return "Images";
    }
    if mime.starts_with("text/")
        || mime == "application/pdf"
        || mime.starts_with("application/vnd.openxmlformats-officedocument.")
        || mime.starts_with("application/vnd.oasis.opendocument.")
    {
        return "Documents";
    }
    if matches!(
        mime.as_str(),
        "application/zip"
            | "application/x-tar"
            | "application/gzip"
            | "application/x-7z-compressed"
            | "application/vnd.rar"
            | "application/x-bzip2"
            | "application/x-xz"
    ) {
        return "Compressed";
    }
    if matches!(
        mime.as_str(),
        "application/x-msdownload"
            | "application/vnd.debian.binary-package"
            | "application/x-rpm"
            | "application/vnd.android.package-archive"
            | "application/x-executable"
    ) {
        return "Programs";
    }

    "Other"
}

pub fn extension_of(filename: &str) -> String {
    let name = filename.rsplit('/').next().unwrap_or(filename);
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            let ext = name[i + 1..].to_lowercase();
            if ext.len() <= 12 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                ext
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}
