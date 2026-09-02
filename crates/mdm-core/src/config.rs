//! Settings persistence as TOML.

use crate::model::Settings;
use crate::paths;
use anyhow::{Context, Result};

pub fn load() -> Settings {
    let path = paths::config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Settings>(&text) {
            Ok(s) => s,
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
