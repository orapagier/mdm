//! Core of My Download Manager: the aria2-backed engine, its store,
//! and the IPC surface the browser extension talks to.

pub mod aria2;
pub mod categories;
pub mod checksum;
pub mod clipboard;
pub mod config;
pub mod distro;
pub mod engine;
pub mod ipc;
pub mod model;
pub mod paths;
pub mod store;
pub mod supervisor;
pub mod ytdlp;

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Human-readable byte count, used by the UI and the CLI alike.
pub fn human_bytes(n: i64) -> String {
    if n < 0 {
        return "unknown".into();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.0} {}", UNITS[i])
    }
}
