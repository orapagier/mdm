//! Where MDM's files live, resolved through each platform's own conventions:
//! XDG base directories on Linux, `%APPDATA%`/`%LOCALAPPDATA%` on Windows.
//!
//! Everything MDM writes lands in a standard location so the app is trivially
//! removable and survives reinstalls of the browser extension.

use std::path::PathBuf;

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;

    pub fn home() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    }

    pub fn xdg(var: &str, fallback: &str) -> PathBuf {
        match std::env::var_os(var) {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => home().join(fallback),
        }
    }

    pub fn config_dir() -> PathBuf {
        xdg("XDG_CONFIG_HOME", ".config").join("mdm")
    }

    pub fn data_dir() -> PathBuf {
        xdg("XDG_DATA_HOME", ".local/share").join("mdm")
    }

    pub fn cache_dir() -> PathBuf {
        xdg("XDG_CACHE_HOME", ".cache").join("mdm")
    }

    /// Where the IPC socket lives. Falls back to /tmp when the session has no
    /// runtime dir (headless, cron, some containers).
    pub fn runtime_dir() -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v).join("mdm"),
            // getuid has no libc-free equivalent in std; this is the whole
            // reason the dependency exists.
            _ => std::env::temp_dir().join(format!("mdm-{}", unsafe { libc::getuid() })),
        }
    }

    /// The Unix socket the extension's native host connects to.
    pub fn socket_path() -> PathBuf {
        runtime_dir().join("mdm.sock")
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

    /// Create every directory MDM writes to, and lock the socket directory
    /// down: anything that can connect to it can queue downloads as this user.
    pub fn ensure_dirs() -> std::io::Result<()> {
        for d in [config_dir(), data_dir(), cache_dir(), runtime_dir()] {
            std::fs::create_dir_all(&d)?;
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(runtime_dir(), std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::path::PathBuf;

    fn sub(dir: Option<PathBuf>, name: &str) -> PathBuf {
        dir.unwrap_or_else(std::env::temp_dir).join(name)
    }

    /// `%APPDATA%\mdm` — settings.toml. Windows has no XDG_CONFIG_HOME
    /// equivalent separate from roaming app data.
    pub fn config_dir() -> PathBuf {
        sub(dirs::config_dir(), "mdm")
    }

    /// `%APPDATA%\mdm` — the SQLite database. Windows
    /// draws no distinction between "config" and "data" the way XDG does, so
    /// both resolve to the same roaming-appdata tree.
    pub fn data_dir() -> PathBuf {
        sub(dirs::data_dir(), "mdm")
    }

    /// `%LOCALAPPDATA%\mdm` — logs and scratch space.
    pub fn cache_dir() -> PathBuf {
        sub(dirs::cache_dir(), "mdm")
    }

    /// Ephemeral scratch space (in-memory blob stashes, yt-dlp info-json
    /// cache). Windows draws no line between this and `cache_dir` the way
    /// `XDG_RUNTIME_DIR` does on Linux, so the two are the same directory
    /// here; callers create their own subdirectories under it as needed.
    pub fn runtime_dir() -> PathBuf {
        cache_dir()
    }

    /// There is no filesystem-backed runtime directory on Windows: the IPC
    /// transport is a named pipe, which lives in its own kernel object
    /// namespace rather than under any directory that needs creating.
    pub fn pipe_name() -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        format!(r"\\.\pipe\mdm-{user}")
    }

    /// The named pipe path the extension's native host connects to. Kept as a
    /// `PathBuf` for parity with the Unix socket path — Windows APIs
    /// (`CreateFileW`, and tokio's named-pipe server) both accept a pipe path
    /// exactly like any other path.
    pub fn socket_path() -> PathBuf {
        PathBuf::from(pipe_name())
    }

    /// `~\Downloads`.
    pub fn default_download_dir() -> PathBuf {
        dirs::download_dir().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Downloads")
        })
    }

    /// Create every directory MDM writes to. Nothing analogous to the Unix
    /// socket directory needs creating: a named pipe is not a file.
    pub fn ensure_dirs() -> std::io::Result<()> {
        for d in [config_dir(), data_dir(), cache_dir()] {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub use unix::{
    config_dir, data_dir, cache_dir, default_download_dir, ensure_dirs, runtime_dir, socket_path,
};

#[cfg(windows)]
pub use windows_impl::{
    config_dir, data_dir, cache_dir, default_download_dir, ensure_dirs, runtime_dir, socket_path,
};

pub fn db_path() -> PathBuf {
    data_dir().join("mdm.db")
}
pub fn config_path() -> PathBuf {
    config_dir().join("settings.toml")
}
