//! The copy of yt-dlp MDM keeps for itself, and how it stays current.
//!
//! yt-dlp is the one dependency that *must* move. It is where the per-site
//! knowledge lives, sites change it under everyone weekly, and a yt-dlp a few
//! months old does not fail politely — it reports "the page needs to be
//! reloaded", or drops every format, and the app looks broken for a reason
//! that has nothing to do with the app.
//!
//! Telling the user to run a package manager was the old answer, and it is a
//! bad one: it arrives as a sentence under a failed download, long after the
//! moment they wanted a video.
//!
//! So MDM keeps its own copy, under its own data directory, and updates that.
//! The distinction matters more than it looks:
//!
//!   * A copy installed by apt, dnf or winget is *not ours*. Writing to it
//!     goes behind the package manager's back, and yt-dlp refuses to update
//!     itself when it can tell it was installed that way. A yt-dlp found on
//!     PATH is therefore used and never touched.
//!   * A copy MDM downloaded is ours outright, and updating it is honest
//!     housekeeping rather than meddling.
//!
//! Both the first install and every update go through MDM's own fetcher
//! rather than through yt-dlp's `--update-to`. That is not a matter of taste:
//! yt-dlp's updater uses its own network stack, which on the machine this was
//! written for answers "Unable to fetch update spec: Connection to github.com
//! timed out" while the fetcher next door pulls the same release in seven
//! seconds. That fetcher has a resolver with its own budget, its own retries
//! and HTTP/1.1, and having gone to the trouble of writing it there is no
//! reason to reach GitHub any other way.

use crate::which::which;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long a copy is trusted before it is worth asking about again.
///
/// yt-dlp releases roughly weekly and fixes urgent breakage sooner; a day is
/// often enough to catch that while asking GitHub once per machine per day.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long fetching a release may take before it is abandoned.
///
/// Generous — it is some twenty megabytes — but bounded, because a hung
/// updater must never become a stuck app.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// The yt-dlp release asset for this platform, and what to call it here.
///
/// Named per architecture rather than per operating system, because the
/// release is a *binary*: handing an ARM machine the x86-64 build produces a
/// file that downloads perfectly and cannot be executed, which is a worse
/// failure than not downloading it at all. Every combination this project is
/// built for has its own line, and anything else stops the build with a
/// sentence saying so rather than compiling into that failure.
///
/// Linux gets the static build, which runs on distributions whose Python is
/// too old for the source one — the same distributions whose packaged yt-dlp
/// is too old to be useful.
#[cfg(all(windows, target_arch = "x86_64"))]
const ASSET: (&str, &str) = ("yt-dlp.exe", "yt-dlp.exe");
#[cfg(all(windows, target_arch = "x86"))]
const ASSET: (&str, &str) = ("yt-dlp_x86.exe", "yt-dlp.exe");
#[cfg(all(windows, target_arch = "aarch64"))]
const ASSET: (&str, &str) = ("yt-dlp_arm64.exe", "yt-dlp.exe");
#[cfg(target_os = "macos")]
const ASSET: (&str, &str) = ("yt-dlp_macos", "yt-dlp");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: (&str, &str) = ("yt-dlp_linux", "yt-dlp");
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET: (&str, &str) = ("yt-dlp_linux_aarch64", "yt-dlp");

#[cfg(not(any(
    all(windows, any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")),
    target_os = "macos",
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
)))]
compile_error!(
    "no yt-dlp release is published for this platform, so MDM cannot fetch one \
     for it. Install yt-dlp yourself and MDM will use it: everything here reads \
     PATH as a fallback."
);

/// Where the release for this platform is served from.
///
/// GitHub's `latest/download` redirects to whatever the newest release is, so
/// there is no version to hard-code and nothing to keep up to date here.
const RELEASE_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";

/// Where MDM keeps the programs it owns.
pub fn bin_dir() -> PathBuf {
    crate::paths::data_dir().join("bin")
}

/// MDM's own copy of yt-dlp, whether or not it exists yet.
pub fn ytdlp_path() -> PathBuf {
    bin_dir().join(ASSET.1)
}

/// The copy MDM owns, if it is there.
pub fn owned_ytdlp() -> Option<PathBuf> {
    let path = ytdlp_path();
    path.is_file().then_some(path)
}

/// The yt-dlp this machine should run: ours first, then the system's.
///
/// Ours first because it is the one that can be kept current. A copy on PATH
/// is a fine second — someone who installed yt-dlp deliberately should not
/// find MDM ignoring it — but nothing here will ever write to it.
pub fn ytdlp() -> Option<PathBuf> {
    owned_ytdlp().or_else(|| which("yt-dlp"))
}

/// The file whose modification time is the last time we asked about updates.
///
/// A timestamp rather than a setting, because it is bookkeeping rather than a
/// preference: nobody should find it in a config file and wonder what to set
/// it to. Its mtime carries the whole of the state.
fn stamp_path() -> PathBuf {
    crate::paths::runtime_dir().join("ytdlp-checked")
}

/// Whether enough time has passed to be worth another look.
pub fn check_due() -> bool {
    due_at(std::fs::metadata(stamp_path()).and_then(|m| m.modified()).ok(), SystemTime::now())
}

/// The decision itself, separated from the filesystem so it can be tested.
fn due_at(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        // Never asked: ask.
        None => true,
        // A stamp from the future is a clock that was wrong when it was
        // written, or has been put right since. Either way the honest reading
        // is "we do not know when we last looked", and asking once is cheaper
        // than never asking again until the future catches up.
        Some(then) => now.duration_since(then).map(|since| since >= CHECK_EVERY).unwrap_or(true),
    }
}

fn stamp_now() {
    let path = stamp_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, b"");
}

/// Forget when we last checked, so the next check happens whatever the clock
/// says.
///
/// For the case that matters most: an extraction has just failed in the way a
/// stale yt-dlp fails. Waiting out the rest of the day before looking for the
/// fix that already exists would be the wrong answer to exactly the question
/// the failure asked.
pub fn force_check() {
    let _ = std::fs::remove_file(stamp_path());
}

/// What a version check or update did, for the caller to log or show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do: no copy of ours to update, or it was already current.
    UpToDate,
    /// Ours, and now newer. Carries what it says it is now.
    Updated(String),
    /// It was tried and did not work. Never fatal: the copy that was there
    /// before is still there, and still runs.
    Failed(String),
}

/// Where GitHub redirects "the latest release" to, which names the version.
///
/// One request and no JSON: the tag is in the address the redirect lands on,
/// so this asks nothing of the API and cannot be rate-limited by it.
const LATEST_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest";

/// What the newest release calls itself.
async fn latest_version() -> Result<String> {
    let spec = crate::fetch::Spec::new(LATEST_URL, std::env::temp_dir());
    let client = crate::fetch::stream_client(&spec).await?;
    let response = tokio::time::timeout(Duration::from_secs(30), client.get(LATEST_URL).send())
        .await
        .context("asking GitHub for the latest yt-dlp timed out")?
        .context("asking GitHub for the latest yt-dlp")?;
    if !response.status().is_success() {
        bail!("GitHub answered {}", response.status());
    }
    // ".../releases/tag/2026.08.19" — the last segment is the version.
    let landed = response.url().to_string();
    let tag = landed.rsplit('/').next().unwrap_or_default().trim().to_string();
    if tag.is_empty() || tag.eq_ignore_ascii_case("latest") {
        bail!("GitHub did not say which release is the latest one ({landed})");
    }
    Ok(tag)
}

/// Fetch the release build for this platform into `dir`, and prove it runs.
///
/// Proving it is the reason this is two steps rather than one: a file that
/// will not execute is worse than no file at all, because every download
/// afterwards would report *its* failure rather than this one.
async fn fetch_release(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let url = format!("{RELEASE_URL}/{}", ASSET.0);
    let partial = dir.join(format!("{}.part", ASSET.1));
    let _ = std::fs::remove_file(&partial);

    log::info!("fetching yt-dlp from {url}");
    let mut spec = crate::fetch::Spec::new(url, dir);
    spec.filename = Some(format!("{}.part", ASSET.1));
    // One connection: it is twenty megabytes nobody is waiting on, and a
    // release CDN has no interest in being asked for it eight times at once.
    spec.concurrency = crate::fetch::Concurrency::Fixed(1);

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let landed = tokio::time::timeout(INSTALL_TIMEOUT, crate::fetch::download(spec, tx, stop))
        .await
        .context("downloading yt-dlp timed out")?
        .context("downloading yt-dlp")?;

    make_runnable(&landed)?;
    let version = version_of(&landed).await.context("the downloaded yt-dlp would not run")?;
    log::info!("fetched yt-dlp {version}");
    Ok(landed)
}

/// Download yt-dlp for this platform, if MDM has no copy and none is on PATH.
///
/// Deliberately conservative about *when*: a machine that already has yt-dlp
/// keeps using it, so this only ever fills a hole. What it fetches is the
/// official release build, which carries its own Python and the JavaScript
/// components YouTube's challenge needs.
pub async fn install() -> Result<PathBuf> {
    if let Some(existing) = ytdlp() {
        return Ok(existing);
    }
    let target = ytdlp_path();
    let staged = fetch_release(&bin_dir()).await?;
    std::fs::rename(&staged, &target)
        .with_context(|| format!("moving yt-dlp into {}", target.display()))?;
    stamp_now();
    log::info!("installed yt-dlp at {}", target.display());
    Ok(target)
}

/// Update MDM's own copy, if there is one and it is behind.
///
/// Only ever our copy: one from a package manager belongs to that package
/// manager. `force` skips the "is it due yet" question, for the case that has
/// already answered it — an extraction that just failed the way a stale yt-dlp
/// fails.
///
/// Nothing here can leave the machine worse off than it was. The new copy is
/// fetched beside the old one and proved to run before either is touched, and
/// if it cannot be moved into place — Windows will not let a running program
/// be replaced — the old one stays exactly as it was for the next attempt.
pub async fn update(force: bool) -> Outcome {
    let Some(path) = owned_ytdlp() else {
        return Outcome::UpToDate;
    };
    if !force && !check_due() {
        return Outcome::UpToDate;
    }
    // Stamped before the attempt rather than after: a check that fails should
    // not be retried on the next tick a few seconds later, hammering GitHub
    // for as long as the network happens to be down.
    stamp_now();

    let installed = version_of(&path).await.unwrap_or_default();
    let latest = match latest_version().await {
        Ok(latest) => latest,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    if !installed.is_empty() && installed == latest {
        return Outcome::UpToDate;
    }
    log::info!("yt-dlp {installed} is behind {latest}; fetching it");

    let staged = match fetch_release(&bin_dir()).await {
        Ok(staged) => staged,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    if let Err(e) = std::fs::rename(&staged, &path) {
        let _ = std::fs::remove_file(&staged);
        return Outcome::Failed(format!("could not replace {}: {e}", path.display()));
    }
    Outcome::Updated(latest)
}

/* ------------------------------------------------------------------ *
 * The JavaScript runtime
 * ------------------------------------------------------------------ */

/// The QuickJS release asset for this platform, and what to call it here.
///
/// QuickJS rather than Node or Deno, which are what people usually install for
/// this: it does the same job in two megabytes where those take ninety, and
/// nobody wants a JavaScript toolchain installed on their behalf because a
/// download manager needed one for a few milliseconds of arithmetic. It is
/// quickjs-*ng* specifically — the maintained fork — because that is the build
/// yt-dlp recognises, and it says so in as many words: `JS runtimes:
/// quickjs-ng-0.16.2`.
///
/// Per architecture, for the same reason as [`ASSET`] above.
#[cfg(all(windows, target_arch = "x86_64"))]
const JS_ASSET: (&str, &str) = ("qjs-windows-x86_64.exe", "qjs.exe");
#[cfg(all(windows, target_arch = "x86"))]
const JS_ASSET: (&str, &str) = ("qjs-windows-x86.exe", "qjs.exe");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const JS_ASSET: (&str, &str) = ("qjs-darwin-arm64", "qjs");
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const JS_ASSET: (&str, &str) = ("qjs-darwin-x86_64", "qjs");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const JS_ASSET: (&str, &str) = ("qjs-linux-x86_64", "qjs");
#[cfg(all(target_os = "linux", target_arch = "x86"))]
const JS_ASSET: (&str, &str) = ("qjs-linux-x86", "qjs");
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const JS_ASSET: (&str, &str) = ("qjs-linux-aarch64", "qjs");
#[cfg(all(target_os = "linux", target_arch = "arm"))]
const JS_ASSET: (&str, &str) = ("qjs-linux-armv7", "qjs");
#[cfg(all(target_os = "linux", target_arch = "riscv64"))]
const JS_ASSET: (&str, &str) = ("qjs-linux-riscv64", "qjs");

/// Windows on ARM has no QuickJS build published, and is the one platform
/// where this falls back rather than fails: the x86-64 one runs under the
/// emulation those machines ship with, slower than native and far faster than
/// a challenge nobody can solve.
#[cfg(all(windows, target_arch = "aarch64"))]
const JS_ASSET: (&str, &str) = ("qjs-windows-x86_64.exe", "qjs.exe");

/// Where the QuickJS release for this platform is served from.
const JS_RELEASE_URL: &str = "https://github.com/quickjs-ng/quickjs/releases/latest/download";

/// MDM's own copy of QuickJS, whether or not it exists yet.
pub fn quickjs_path() -> PathBuf {
    bin_dir().join(JS_ASSET.1)
}

/// The copy MDM owns, if it is there.
pub fn owned_quickjs() -> Option<PathBuf> {
    let path = quickjs_path();
    path.is_file().then_some(path)
}

/// Fetch QuickJS if this machine has no copy of ours.
///
/// Unlike yt-dlp, a runtime already on PATH is *not* a reason to skip this.
/// yt-dlp picks its own favourite from what is enabled — Deno first, then
/// Node — so a machine with either still uses that; ours is the floor, and it
/// costs two megabytes to know that a machine with neither still works.
pub async fn install_quickjs() -> Result<PathBuf> {
    if let Some(existing) = owned_quickjs() {
        return Ok(existing);
    }
    let dir = bin_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let url = format!("{JS_RELEASE_URL}/{}", JS_ASSET.0);
    let target = quickjs_path();
    let staged = dir.join(format!("{}.part", JS_ASSET.1));
    let _ = std::fs::remove_file(&staged);

    log::info!("fetching QuickJS from {url}");
    let mut spec = crate::fetch::Spec::new(url, &dir);
    spec.filename = Some(format!("{}.part", JS_ASSET.1));
    spec.concurrency = crate::fetch::Concurrency::Fixed(1);

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let landed = tokio::time::timeout(INSTALL_TIMEOUT, crate::fetch::download(spec, tx, stop))
        .await
        .context("downloading QuickJS timed out")?
        .context("downloading QuickJS")?;

    make_runnable(&landed)?;
    // Proved before it is installed, the same way yt-dlp is: a runtime that
    // will not start would be reported by yt-dlp as a challenge it could not
    // solve, which is a long way from the truth.
    let version = js_version(&landed).await.context("the downloaded QuickJS would not run")?;
    std::fs::rename(&landed, &target)
        .with_context(|| format!("moving QuickJS into {}", target.display()))?;
    log::info!("installed QuickJS {version} at {}", target.display());
    Ok(target)
}

/// Ask a copy of QuickJS what it is.
async fn js_version(path: &Path) -> Result<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.stdin(Stdio::null()).output())
        .await
        .context("QuickJS did not answer --version")?
        .context("running qjs --version")?;
    if !out.status.success() {
        bail!("qjs --version exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ask a copy of yt-dlp what it is.
async fn version_of(path: &Path) -> Result<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.stdin(Stdio::null()).output())
        .await
        .context("yt-dlp did not answer --version")?
        .context("running yt-dlp --version")?;
    if !out.status.success() {
        bail!("yt-dlp --version exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// On Unix a downloaded file is not executable until it is said to be.
#[cfg(unix)]
fn make_runnable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("making {} executable", path.display()))
}

#[cfg(windows)]
fn make_runnable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Install if missing, then update if due — the whole of the housekeeping, in
/// the order that makes sense.
///
/// Every failure is swallowed into a log line on purpose. This runs in the
/// background at startup, and nothing it does is worth interrupting anyone
/// over: a machine with no network still opens, still downloads from sites
/// that need no extractor, and tries again tomorrow.
pub async fn maintain(auto_update: bool) {
    // The runtime first: without one, a yt-dlp that is otherwise perfect
    // still cannot read YouTube.
    if let Err(e) = install_quickjs().await {
        log::warn!("could not install QuickJS: {e:#}");
    }
    if ytdlp().is_none() {
        match install().await {
            Ok(path) => log::info!("yt-dlp is at {}", path.display()),
            Err(e) => log::warn!("could not install yt-dlp: {e:#}"),
        }
        return; // freshly downloaded; nothing to update
    }
    if !auto_update {
        return;
    }
    match update(false).await {
        Outcome::Updated(version) => log::info!("yt-dlp updated to {version}"),
        Outcome::UpToDate => {}
        Outcome::Failed(why) => log::warn!("could not update yt-dlp: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_never_looked_at_is_due() {
        assert!(due_at(None, SystemTime::now()));
    }

    #[test]
    fn a_copy_looked_at_a_moment_ago_is_not() {
        let now = SystemTime::now();
        assert!(!due_at(Some(now - Duration::from_secs(60)), now));
        assert!(!due_at(Some(now - (CHECK_EVERY - Duration::from_secs(1))), now));
    }

    #[test]
    fn a_copy_looked_at_yesterday_is_due_again() {
        let now = SystemTime::now();
        assert!(due_at(Some(now - CHECK_EVERY), now));
        assert!(due_at(Some(now - Duration::from_secs(30 * 24 * 60 * 60)), now));
    }

    /// A stamp dated in the future is a clock that was wrong. Reading it as
    /// "recent" would park the check until the future arrived, which on a
    /// machine whose clock jumped a year means never.
    #[test]
    fn a_stamp_from_the_future_does_not_park_the_check_for_ever() {
        let now = SystemTime::now();
        assert!(due_at(Some(now + Duration::from_secs(365 * 24 * 60 * 60)), now));
    }


    /// The runtime is fetched under the name it will be run as, beside yt-dlp.
    /// On Windows that means keeping the extension, without which nothing will
    /// execute it.
    #[test]
    fn the_runtime_is_named_the_way_it_will_be_run() {
        let path = quickjs_path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if cfg!(windows) {
            assert_eq!(name, "qjs.exe");
        } else {
            assert_eq!(name, "qjs");
        }
        assert!(path.starts_with(bin_dir()));
        assert_ne!(path, ytdlp_path(), "two programs, two names");
    }

    /// The asset is named for the platform it will run on, and the one this
    /// binary was built for is the one it must ask GitHub for.
    #[test]
    fn the_asset_matches_the_platform_it_was_built_for() {
        let (asset, _) = JS_ASSET;
        if cfg!(windows) {
            assert!(asset.starts_with("qjs-windows"), "{asset}");
            assert!(asset.ends_with(".exe"), "{asset}");
        } else if cfg!(target_os = "macos") {
            assert!(asset.starts_with("qjs-darwin"), "{asset}");
        } else {
            assert!(asset.starts_with("qjs-linux"), "{asset}");
        }
        // quickjs-ng publishes one asset per architecture under these exact
        // names; a mismatch here is a 404 at the moment someone needs a video.
        assert!(asset.contains(std::env::consts::ARCH) || cfg!(target_arch = "aarch64"), "{asset}");
    }
    /// The installed name is the one every other part of the app looks for,
    /// and on Windows an executable without its extension is not runnable.
    #[test]
    fn the_downloaded_program_is_named_the_way_it_will_be_run() {
        let path = ytdlp_path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if cfg!(windows) {
            assert_eq!(name, "yt-dlp.exe");
        } else {
            assert_eq!(name, "yt-dlp");
        }
        assert!(path.starts_with(bin_dir()));
    }
}
