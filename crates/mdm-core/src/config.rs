//! Settings persistence as TOML.

use crate::model::Settings;
use crate::paths;
use anyhow::{Context, Result};

/// Format selectors that were once the shipped default.
///
/// A saved settings file pins whatever the default was on the day it was
/// written, so fixing a default fixes nothing for anyone who already has one —
/// and the value being fixed here is the one that hands a Fedora desktop an
/// HEVC file it has no decoder for, which plays as sound over a black screen.
/// Only these exact strings are replaced: a selector the user typed is theirs,
/// however much it resembles one of ours.
const SUPERSEDED_FORMATS: &[&str] = &[
    "bestvideo*+bestaudio/best",
    // Kept the sound in the MP4 family, but let the picture be WebM behind
    // it — a mix neither muxer will write, so those merges went to ffmpeg.
    "bestvideo*[vcodec!*=hev][vcodec!*=h265]+bestaudio[ext=m4a]/\
                           bestvideo*[vcodec!*=hev][vcodec!*=h265]+bestaudio/\
                           best[vcodec!*=hev][vcodec!*=h265]/\
                           bestvideo*+bestaudio/best",
    // Right about the picture, but it leaves YouTube's sound in WebM,
    // which sends every merge back through ffmpeg for want of a container
    // this crate can rebuild.
    "bestvideo*[vcodec!*=hev][vcodec!*=h265]+bestaudio/\
                           best[vcodec!*=hev][vcodec!*=h265]/\
                           bestvideo*+bestaudio/best",
];

fn migrate(mut settings: Settings) -> Settings {
    if SUPERSEDED_FORMATS.contains(&settings.ytdlp_format.as_str()) {
        settings.ytdlp_format = Settings::default().ytdlp_format;
        log::info!(
            "video format left at an old default; using {} instead",
            settings.ytdlp_format
        );
    }
    settings
}

pub fn load() -> Settings {
    let path = paths::config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Settings>(&text) {
            Ok(s) => migrate(s),
            Err(e) => {
                // A malformed config must not stop the app from starting; the
                // user would have no way to fix it from inside the UI.
                log::warn!("{} is invalid ({e}); using defaults", path.display());
                Settings::default()
            }
        },
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) -> Result<()> {
    let path = paths::config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(settings).context("serialising settings")?;
    // Write-then-rename so an interrupted save cannot truncate the config.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).context("replacing settings file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_format(format: &str) -> Settings {
        Settings { ytdlp_format: format.to_string(), ..Settings::default() }
    }

    /// Every default this project has shipped has to be recognised as one, or
    /// the person carrying it never gets the new behaviour: they keep the old
    /// selection, keep needing ffmpeg for it, and nothing tells them why.
    #[test]
    fn a_superseded_default_is_replaced_by_the_current_one() {
        for old in SUPERSEDED_FORMATS {
            let migrated = migrate(with_format(old));
            assert_eq!(
                migrated.ytdlp_format,
                Settings::default().ytdlp_format,
                "{old} was left in place"
            );
        }
    }

    /// The exact string the previous release wrote into settings.toml. Spelled
    /// out here as one line, the way it is actually stored, because the entry
    /// in `SUPERSEDED_FORMATS` is written across several with continuations —
    /// and a continuation that lost its indentation, or gained a space, would
    /// still compile and would silently stop matching anybody's saved file.
    #[test]
    fn the_previous_default_is_recognised_exactly_as_it_was_stored() {
        let stored = "bestvideo*[vcodec!*=hev][vcodec!*=h265]+bestaudio/\
best[vcodec!*=hev][vcodec!*=h265]/bestvideo*+bestaudio/best";
        assert!(
            SUPERSEDED_FORMATS.contains(&stored),
            "the format the last release saved is no longer recognised"
        );
        assert_eq!(migrate(with_format(stored)).ytdlp_format, Settings::default().ytdlp_format);
    }

    /// A selector someone typed is theirs, however much it resembles one of
    /// ours. Replacing it would silently download a different quality than the
    /// one they asked for.
    #[test]
    fn an_expression_someone_chose_is_left_alone() {
        let mine = "bestvideo[height<=720]+bestaudio";
        assert_eq!(migrate(with_format(mine)).ytdlp_format, mine);
    }
}
