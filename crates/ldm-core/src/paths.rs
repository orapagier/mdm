//! XDG base directory resolution.
//!
//! Everything LDM writes lands in a standard location so the app is trivially
//! removable and survives reinstalls of the browser extension.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

/// `~/.config/ldm` — settings.toml.
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("ldm")
}

/// `~/.local/share/ldm` — the SQLite database and aria2 session file.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("ldm")
}

/// `~/.cache/ldm` — logs and scratch space.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("ldm")
}

/// Where the IPC socket lives. Falls back to /tmp when the session has no
/// runtime dir (headless, cron, some containers).
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("ldm"),
        _ => std::env::temp_dir().join(format!("ldm-{}", unsafe { libc_getuid() })),
    }
}

/// The Unix socket the extension's native host connects to.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("ldm.sock")
}

pub fn db_path() -> PathBuf {
    data_dir().join("ldm.db")
}

pub fn aria2_session_path() -> PathBuf {
    data_dir().join("aria2.session")
}

pub fn config_path() -> PathBuf {
    config_dir().join("settings.toml")
}

/// `~/Downloads`, honouring an XDG user-dirs override when one is configured.
pub fn default_download_dir() -> PathBuf {
    if let Some(dir) = xdg_user_dir("DOWNLOAD") {
        return dir;
    }
    home().join("Downloads")
}

/// Parse `~/.config/user-dirs.dirs`, which desktop environments write.
fn xdg_user_dir(key: &str) -> Option<PathBuf> {
    let file = xdg("XDG_CONFIG_HOME", ".config").join("user-dirs.dirs");
    let text = std::fs::read_to_string(file).ok()?;
    let needle = format!("XDG_{key}_DIR=");
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(&needle) else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        let expanded = match value.strip_prefix("$HOME/") {
            Some(tail) => home().join(tail),
            None => PathBuf::from(value),
        };
        if !expanded.as_os_str().is_empty() {
            return Some(expanded);
        }
    }
    None
}

// getuid has no libc-free equivalent in std; this is the whole reason the
// dependency exists.
unsafe fn libc_getuid() -> u32 {
    libc::getuid()
}

/// Create every directory LDM writes to. Called once at startup.
pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [config_dir(), data_dir(), cache_dir(), runtime_dir()] {
        std::fs::create_dir_all(&d)?;
    }
    // The socket directory must not be world-accessible: anything that can
    // connect to it can queue downloads as this user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(runtime_dir(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
