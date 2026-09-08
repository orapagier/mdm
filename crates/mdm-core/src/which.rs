//! Finding a program on PATH, without a dependency for one lookup.
//!
//! This used to live in the aria2 supervisor, which was the first thing that
//! needed it. The supervisor is gone; the lookup is not, because yt-dlp,
//! ffmpeg, the clipboard helpers and the notifier are all optional tools that
//! have to be *checked for* rather than assumed.

/// Whether a program exists on PATH, and where.
#[cfg(unix)]
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|p| {
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Windows resolves a bare command through `%PATHEXT%` (`.EXE`, `.CMD`, …) the
/// same way `cmd.exe` does, so this has to try each suffix itself — every
/// caller passes the bare name, never `yt-dlp.exe`.
#[cfg(windows)]
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    // A caller-supplied extension (rare, but not impossible) is tried as-is
    // rather than re-suffixed.
    let exts: Vec<String> = if std::path::Path::new(program).extension().is_some() {
        vec![String::new()]
    } else {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(str::to_owned)
            .collect()
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
