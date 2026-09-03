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
const SUPERSEDED_FORMATS: &[&str] = &["bestvideo*+bestaudio/best"];

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
