//! Lifecycle management for the bundled aria2c daemon.
//!
//! MDM runs its *own* aria2 instance on a private port with a random secret
//! rather than attaching to whatever the user might already have running. That
//! keeps our global options (speed caps, concurrency) from stomping on theirs.

use crate::aria2::Aria2;
use crate::model::Settings;
use crate::paths;
use anyhow::{bail, Context, Result};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};

pub struct Supervisor {
    child: Option<Child>,
    pub client: Arc<Aria2>,
    pub port: u16,
    secret: String,
}

/// 128 bits of entropy from the OS, hex-encoded. Anything that can read this
/// token can queue downloads as the user, so it must not be predictable.
fn random_secret() -> Result<String> {
    let mut buf = [0u8; 16];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut buf)
        .context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

impl Supervisor {
    /// Start aria2c and block until its RPC endpoint answers.
    pub async fn start(settings: &Settings) -> Result<Self> {
        if which("aria2c").is_none() {
            bail!(
                "aria2c was not found on PATH — install it with: {}",
                crate::distro::install("aria2")
            );
        }

        // aria2 fails with a bare exit code and nothing on stderr when it
        // cannot bind, so probe first and say something the user can act on.
        //
        // Not a single check: on restart the previous instance's aria2 is
        // still winding down (--stop-with-process polls, so it lingers a
        // second or two), and refusing to start then would make every restart
        // fail exactly once.
        if !wait_for_free_port(settings.rpc_port, Duration::from_secs(10)).await {
            bail!(
                "port {} is still in use after 10s — another MDM or aria2 daemon is \
                 running. Close it, or change the RPC port in Settings.",
                settings.rpc_port
            );
        }

        let secret = random_secret()?;
        let port = settings.rpc_port;
        let session = paths::aria2_session_path();
        let server_stats = paths::data_dir().join("server-stats");
        let log = paths::cache_dir().join("aria2.log");

        let mut cmd = Command::new("aria2c");
        cmd.arg("--enable-rpc")
            // Loopback only. Combined with the secret this keeps the daemon
            // off the network entirely.
            .arg("--rpc-listen-all=false")
            .arg(format!("--rpc-listen-port={port}"))
            .arg(format!("--rpc-secret={secret}"))
            .arg(format!("--dir={}", settings.download_dir))
            .arg(format!(
                "--max-concurrent-downloads={}",
                settings.max_concurrent.max(1)
            ))
            .arg(format!(
                "--max-connection-per-server={}",
                settings.connections.clamp(1, crate::aria2::MAX_CONNECTIONS)
            ))
            .arg(format!("--split={}", settings.split.max(1)))
            .arg(format!("--min-split-size={}", settings.min_split_size))
            .arg("--continue=true")
            .arg("--file-allocation=falloc")
            .arg("--check-certificate=true")
            // Pick the fastest of a file's sources for the first connections
            // and keep the rest in reserve, rather than working down the list
            // in order. Only bites when a download has mirrors.
            .arg("--uri-selector=adaptive")
            // Which servers were fast last time, remembered across restarts —
            // this is what `adaptive` selects on, and it is worthless if it
            // starts from nothing every launch.
            .arg(format!("--server-stat-of={}", server_stats.display()))
            .arg(format!("--server-stat-if={}", server_stats.display()))
            .arg("--server-stat-timeout=86400")
            // A .metalink/.meta4 is a list of mirrors and checksums, not a file
            // anybody wants on disk. Following it in memory turns one into a
            // multi-source download; `false` downloaded the index instead.
            .arg("--follow-metalink=mem")
            // Written in larger, offset-ordered units instead of sixteen
            // interleaved streams of small writes.
            .arg("--disk-cache=64M")
            // Fewer parallel downloads on a link too slow to feed them, which
            // finishes the first ones sooner. Bounded by max-concurrent above.
            .arg("--optimize-concurrent-downloads=true")
            .arg("--auto-file-renaming=true")
            .arg("--conditional-get=true")
            .arg("--content-disposition-default-utf8=true")
            // Persist unfinished work so a crash or restart resumes rather
            // than losing multi-gigabyte transfers.
            .arg(format!("--save-session={}", session.display()))
            .arg("--save-session-interval=30")
            .arg("--auto-save-interval=20")
            // Deliberately NOT --force-save: that keeps the .aria2 control
            // file beside every *finished* download (it exists for BitTorrent
            // seeding), leaving clutter next to each completed file.
            // If the app dies, aria2 must not linger holding the port.
            .arg(format!("--stop-with-process={}", std::process::id()))
            .arg(format!("--log={}", log.display()))
            .arg("--log-level=warn")
            .arg("--console-log-level=warn")
            .arg("--summary-interval=0");

        if session.exists() {
            cmd.arg(format!("--input-file={}", session.display()));
        }
        if settings.max_speed > 0 {
            cmd.arg(format!("--max-overall-download-limit={}", settings.max_speed));
        }
        for extra in &settings.aria2_extra_args {
            cmd.arg(extra);
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().context("spawning aria2c")?;
        let client = Arc::new(Aria2::new(port, secret.clone()));

        let mut sup = Self {
            child: Some(child),
            client,
            port,
            secret,
        };
        sup.wait_ready().await?;
        Ok(sup)
    }

    /// Poll getVersion until the daemon answers, or give up after ~5 s.
    async fn wait_ready(&mut self) -> Result<()> {
        for attempt in 0..50 {
            // A daemon that already exited will never answer; surface why.
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(exit)) = child.try_wait() {
                    let stderr = match child.stderr.take() {
                        Some(mut s) => {
                            use tokio::io::AsyncReadExt;
                            let mut buf = String::new();
                            let _ = s.read_to_string(&mut buf).await;
                            buf
                        }
                        None => String::new(),
                    };
                    bail!(
                        "aria2c exited immediately ({exit}). {}",
                        stderr.trim().lines().last().unwrap_or("no output")
                    );
                }
            }
            if let Ok(v) = self.client.version().await {
                log::info!("aria2 {v} ready on port {}", self.port);
                return Ok(());
            }
            // Back off slightly: the first few attempts race process startup.
            tokio::time::sleep(Duration::from_millis(if attempt < 10 { 50 } else { 150 }))
                .await;
        }
        bail!("aria2c did not answer on port {} within 5s", self.port)
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Ask aria2 to save its session and exit cleanly, then reap the process.
    pub async fn stop(&mut self) {
        let _ = self.client.save_session().await;
        let _ = self.client.shutdown().await;
        if let Some(child) = self.child.as_mut() {
            // Give it a moment to flush the session file before SIGKILL.
            let deadline = tokio::time::sleep(Duration::from_millis(1500));
            tokio::pin!(deadline);
            tokio::select! {
                _ = child.wait() => {}
                _ = &mut deadline => {
                    let _ = child.start_kill();
                }
            }
        }
        self.child = None;
    }
}

/// Can we bind the port? If not, something else already owns it.
fn port_in_use(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// Poll until the port frees up, or the deadline passes.
async fn wait_for_free_port(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !port_in_use(port) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Minimal `which`, so the crate does not need a dependency for one lookup.
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                p.metadata()
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                p.is_file()
            }
        })
}
