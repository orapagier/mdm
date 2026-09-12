//! In-process segmented HTTP downloader.
//!
//! This replaces the half of aria2 MDM actually uses — parallel ranged GETs
//! against one file — with something in this process, and adds the piece no
//! external downloader can do for us: choosing the connection count by
//! measuring the file being downloaded rather than by asking the user to
//! guess.
//!
//! The parts that make a segmented downloader worth having:
//!
//!   * **Work stealing.** A static split into N equal parts finishes N-1
//!     connections early and then waits on whichever segment drew the slowest
//!     route. An idle connection instead takes half of whatever has the most
//!     left ([`Work::steal`]), which keeps every socket busy to the end.
//!   * **Adaptive concurrency.** More connections help only while the *server*
//!     is the limit. Once the link is saturated they split the same pipe and
//!     add handshakes, and enough of them will get an IP throttled or banned.
//!     [`Governor`] climbs until throughput stops improving and settles there,
//!     re-probing occasionally because a link's capacity is not a constant.
//!   * **Resume that survives a crash.** Bytes land in a `.mdmdownload` file
//!     beside a small state file recording exactly which ranges are complete,
//!     so a restart continues instead of starting over, and a half-finished
//!     download never wears the final name.
//!
//! What it deliberately does not do is HTTP/2: see [`client`].

use crate::model::Header;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most connections this will open to one file under any settings.
///
/// Higher than aria2's 16 because the governor decides the real number by
/// measurement — this is only the ceiling it may climb to, and a server that
/// rewards 32 connections exists. A server that punishes them is detected and
/// backed away from long before this matters.
pub const MAX_CONNECTIONS: u8 = 32;

/// Below this, splitting costs more in round trips than it saves: a new
/// connection has to handshake and slow-start before its first useful byte.
pub const MIN_SPLIT: u64 = 1 << 20; // 1 MiB

/// How much body to hold before handing it to the OS.
///
/// Also how far the on-disk cursor trails what has been received, which bounds
/// how much a steal decided from a stale cursor can duplicate.
const WRITE_BUFFER: usize = 64 * 1024;

/// What to say we are when the caller has not said.
///
/// A capture from the browser carries Firefox's own `User-Agent`, which is why
/// captured downloads have always worked. Anything else — a URL typed into
/// "Add URL", a job from the command line, a link from the clipboard — used to
/// arrive with no `User-Agent` at all, and a plain HTTP client that names
/// itself nothing is a shape a good deal of the web now refuses.
///
/// It does not refuse *politely*. The site this was found on accepts the
/// connection, completes the TCP handshake, and then never answers — so it
/// surfaces as `tcp connect error: deadline has elapsed` fifteen seconds
/// later, which reads as an unreachable server rather than a rejected request.
/// The same URL with this header set answers 200 in about a second.
///
/// A real browser's string rather than our own name, deliberately: the point
/// is to be served the file the browser would have been served.
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:143.0) Gecko/20100101 Firefox/143.0";

/// Suffix for the partial file. A download in progress must not be mistakable
/// for a finished one — by the user, or by the engine's own "is it already
/// there?" check.
pub const PART_SUFFIX: &str = ".mdmdownload";

/// How many connections to use, and who decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// Exactly this many, whatever the measurements say. What a user who has
    /// found the number that works for their link asks for.
    Fixed(u8),
    /// Start small, climb while it helps, settle where it stops helping.
    Auto { max: u8 },
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency::Auto { max: MAX_CONNECTIONS }
    }
}

/// What to fetch, and how hard to try.
#[derive(Debug, Clone)]
pub struct Spec {
    pub url: String,
    /// Other servers holding the same bytes (RFC 6249 `Link: rel=duplicate`).
    /// Connections are dealt round-robin across them, so a slow mirror costs
    /// one connection rather than the download.
    pub mirrors: Vec<String>,
    pub dir: PathBuf,
    /// Name to save as. `None` asks the server, then the URL.
    pub filename: Option<String>,
    pub headers: Vec<Header>,
    pub referrer: Option<String>,
    pub concurrency: Concurrency,
    pub min_split: u64,
    /// Bytes per second across every connection. 0 is unlimited.
    pub max_speed: u64,
    pub retries: u8,
    /// How to reach the server: empty follows the system proxy settings, `off`
    /// ignores them, anything else is a proxy URL. See `Settings::proxy`.
    pub proxy: String,
    /// Checked after the last byte lands, before the file takes its real name.
    pub expected_sha256: Option<String>,
}

impl Spec {
    pub fn new(url: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            mirrors: Vec::new(),
            dir: dir.into(),
            filename: None,
            headers: Vec::new(),
            referrer: None,
            concurrency: Concurrency::default(),
            min_split: MIN_SPLIT,
            max_speed: 0,
            retries: 5,
            proxy: String::new(),
            expected_sha256: None,
        }
    }
}

/// What the server said when asked for the first byte.
#[derive(Debug, Clone)]
pub struct Probe {
    /// Where the redirects landed. Segments ask this directly, so a redirect
    /// chain is walked once rather than once per connection.
    pub url: String,
    pub size: Option<u64>,
    pub resumable: bool,
    pub filename: String,
    pub mime: String,
    /// ETag or Last-Modified — whatever lets a resume notice the file changed.
    pub validator: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed: u64,
    pub connections: u64,
}

#[derive(Debug, Clone)]
pub enum Event {
    Probed(Probe),
    Progress(Progress),
    /// The governor changed its mind about how many connections to run, with
    /// the throughput that convinced it. Surfaced because "why is this using
    /// three connections?" deserves an answer better than "it decided to".
    Concurrency { connections: u8, speed: u64 },
    Done(PathBuf),
}

/* ------------------------------------------------------------------ *
 * Work: who owns which bytes
 * ------------------------------------------------------------------ */

/// One connection's current range.
///
/// `cursor` only rises and `end` only falls, so a worker reads both without a
/// lock and still knows it never writes outside what it owns.
#[derive(Debug)]
struct Claim {
    cursor: AtomicU64,
    end: AtomicU64,
    /// Set by the governor to wind this connection down: the worker finishes
    /// its buffer, hands what is left back, and exits.
    retire: AtomicBool,
    live: AtomicBool,
}

impl Claim {
    fn idle() -> Self {
        Self {
            cursor: AtomicU64::new(0),
            end: AtomicU64::new(0),
            retire: AtomicBool::new(false),
            live: AtomicBool::new(false),
        }
    }

    fn remaining(&self) -> u64 {
        self.end
            .load(Ordering::Acquire)
            .saturating_sub(self.cursor.load(Ordering::Acquire))
    }
}

/// The outstanding work, and the bookkeeping that says what is already done.
struct Work {
    claims: Vec<Claim>,
    /// Ranges nobody is fetching: what a resume found missing, plus whatever
    /// retiring or failed connections handed back.
    pool: Mutex<VecDeque<(u64, u64)>>,
    /// Ranges written to disk, merged. This is what the state file persists
    /// and what a resume subtracts from the whole.
    done: Mutex<Vec<(u64, u64)>>,
    split: Mutex<()>,
}

impl Work {
    fn new(pool: VecDeque<(u64, u64)>, done: Vec<(u64, u64)>) -> Self {
        Self {
            claims: (0..MAX_CONNECTIONS).map(|_| Claim::idle()).collect(),
            pool: Mutex::new(pool),
            done: Mutex::new(done),
            split: Mutex::new(()),
        }
    }

    /// Find this worker something to do: an unclaimed range if there is one,
    /// otherwise half of whoever has the furthest to go.
    fn next(&self, min_split: u64) -> Option<(u64, u64)> {
        if let Some(range) = self.pool.lock().unwrap().pop_front() {
            return Some(range);
        }
        self.steal(min_split)
    }

    /// Take half of the segment with the most bytes left.
    ///
    /// The victim learns of it by reading its own `end`, which this lowers; it
    /// stops at the new boundary. Anything it had buffered past that point is
    /// dropped rather than written, so the two never disagree about a byte.
    fn steal(&self, min_split: u64) -> Option<(u64, u64)> {
        let _guard = self.split.lock().unwrap();
        let victim = self
            .claims
            .iter()
            .filter(|c| c.live.load(Ordering::Acquire))
            .max_by_key(|c| c.remaining())?;

        let cursor = victim.cursor.load(Ordering::Acquire);
        let end = victim.end.load(Ordering::Acquire);
        // Re-read under the lock: the victim has been running since max_by_key
        // and may have finished what made it worth splitting.
        let remaining = end.saturating_sub(cursor);
        if remaining < min_split * 2 {
            return None;
        }
        let mid = cursor + remaining / 2;
        victim.end.store(mid, Ordering::Release);
        Some((mid, end))
    }

    /// Is there anything for another connection to do?
    ///
    /// Asked without taking it, which `next` cannot be used for: reading it as
    /// a test threw the range away, and the download then sat at zero bytes
    /// forever because the only work there was had been consumed by the check
    /// for whether work existed.
    fn has_work(&self, min_split: u64) -> bool {
        if !self.pool.lock().unwrap().is_empty() {
            return true;
        }
        self.claims
            .iter()
            .filter(|c| c.live.load(Ordering::Acquire))
            .any(|c| c.remaining() >= min_split * 2)
    }

    fn give_back(&self, range: (u64, u64)) {
        if range.0 < range.1 {
            self.pool.lock().unwrap().push_back(range);
        }
    }

    fn record_done(&self, start: u64, end: u64) {
        let mut done = self.done.lock().unwrap();
        done.push((start, end));
        merge(&mut done);
    }

    fn completed_bytes(&self) -> u64 {
        self.done.lock().unwrap().iter().map(|(a, b)| b - a).sum()
    }
}

/// Collapse touching or overlapping intervals, so the state file stays small
/// and a resume's arithmetic is over a handful of ranges rather than thousands.
fn merge(ranges: &mut Vec<(u64, u64)>) {
    if ranges.len() < 2 {
        return;
    }
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges.iter() {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    *ranges = out;
}

/// What is left of `[0, size)` once the finished ranges are taken out.
fn missing(done: &[(u64, u64)], size: u64) -> VecDeque<(u64, u64)> {
    let mut gaps = VecDeque::new();
    let mut at = 0u64;
    for &(start, end) in done {
        if start > at {
            gaps.push_back((at, start));
        }
        at = at.max(end);
    }
    if at < size {
        gaps.push_back((at, size));
    }
    gaps
}

/// Cut the outstanding work into enough pieces for the connections allowed,
/// so a fresh download starts parallel instead of climbing from one range.
fn plan(gaps: VecDeque<(u64, u64)>, pieces: usize, min_split: u64) -> VecDeque<(u64, u64)> {
    let mut work: Vec<(u64, u64)> = gaps.into_iter().collect();
    while work.len() < pieces {
        // Always split the largest: splitting anything else leaves the
        // straggler that segmenting exists to avoid.
        let (index, &(start, end)) = match work
            .iter()
            .enumerate()
            .max_by_key(|(_, (start, end))| end - start)
        {
            Some(largest) => largest,
            None => break,
        };
        if end - start < min_split * 2 {
            break;
        }
        let mid = start + (end - start) / 2;
        work[index] = (start, mid);
        work.push((mid, end));
    }
    work.sort_unstable();
    work.into()
}

/* ------------------------------------------------------------------ *
 * Per-host politeness
 * ------------------------------------------------------------------ */

/// What has been learned about how many connections each host tolerates.
///
/// The governor optimises one download in isolation, which is the wrong scope
/// for a shared resource: four downloads from the same small server, each
/// having independently concluded that sixteen connections are wonderful, is
/// sixty-four connections arriving at a machine that may only allow a handful
/// — and the server's answer to that is a rate-limit or a block on the whole
/// address, which costs every download rather than the greedy one.
///
/// So the ceiling is per host and shared across downloads, and a host that
/// pushes back lowers it for everything currently talking to it.
struct HostPolicy {
    /// Connections in flight to each host, across every download.
    permits: Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
    /// Ceilings learned from 429s and 503s. Not persisted: a limit hit an hour
    /// ago may have been a bad afternoon rather than a policy, and starting
    /// each session willing to find out again is the friendlier error.
    learned: Mutex<std::collections::HashMap<String, u8>>,
}

fn host_policy() -> &'static HostPolicy {
    static POLICY: std::sync::OnceLock<HostPolicy> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| HostPolicy {
        permits: Mutex::new(std::collections::HashMap::new()),
        learned: Mutex::new(std::collections::HashMap::new()),
    })
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The gate every connection to `host` passes through, so the total across
/// downloads stays inside what that host has shown it will take.
fn host_permits(host: &str) -> Arc<tokio::sync::Semaphore> {
    let mut permits = host_policy().permits.lock().unwrap();
    permits
        .entry(host.to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS as usize)))
        .clone()
}

/// This host's current ceiling, which starts at the maximum and only falls.
fn host_limit(host: &str) -> u8 {
    *host_policy()
        .learned
        .lock()
        .unwrap()
        .get(host)
        .unwrap_or(&MAX_CONNECTIONS)
}

/// Record that a host asked for fewer connections, halving what we will ask of
/// it until the process restarts.
///
/// Halving rather than decrementing: a server answering 429 is not saying "one
/// fewer", and walking down one at a time means several more rounds of being
/// told off before arriving somewhere acceptable.
fn note_pushback(host: &str) {
    let mut learned = host_policy().learned.lock().unwrap();
    let entry = learned.entry(host.to_owned()).or_insert(MAX_CONNECTIONS);
    *entry = (*entry / 2).max(1);
    log::info!("{host} asked for fewer connections; holding it to {entry}");
}

/* ------------------------------------------------------------------ *
 * Disk space
 * ------------------------------------------------------------------ */

/// Refuse a download that cannot possibly fit, before it writes anything.
///
/// Finding out at 98% costs the whole transfer and leaves a partial file that
/// looks like a resume worth continuing. The check is deliberately advisory:
/// if the free space cannot be read, the download proceeds, because failing a
/// download over a failed statfs would be worse than the case it prevents.
fn check_space(dir: &Path, needed: u64) -> Result<()> {
    let Some(free) = free_space(dir) else { return Ok(()) };
    // A margin, because a filesystem at absolute zero is a different and worse
    // problem for everything else running on the machine.
    const MARGIN: u64 = 64 << 20;
    if needed + MARGIN > free {
        bail!(
            "not enough room in {}: the file needs {} and {} is free",
            dir.display(),
            crate::human_bytes(needed as i64),
            crate::human_bytes(free as i64),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the
    // call, and `stat` is only read after statvfs reports success.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
}

#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; the two output
    // pointers are optional and passed as null, which the API documents.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

/* ------------------------------------------------------------------ *
 * Resume
 * ------------------------------------------------------------------ */

/// Written beside the partial file, so a resume knows what it already has.
#[derive(Debug, Serialize, Deserialize)]
struct State {
    url: String,
    size: u64,
    /// ETag or Last-Modified. A resume against a file that changed underneath
    /// would splice two different files together, which is worse than starting
    /// over, so a mismatch discards the partial rather than continuing it.
    validator: Option<String>,
    done: Vec<(u64, u64)>,
}

fn state_path(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".state");
    PathBuf::from(name)
}

fn load_state(part: &Path, probe: &Probe) -> Option<Vec<(u64, u64)>> {
    let raw = std::fs::read(state_path(part)).ok()?;
    let state: State = serde_json::from_slice(&raw).ok()?;
    let size = probe.size?;
    if state.size != size {
        log::info!("the partial file is for a different length; starting over");
        return None;
    }
    // A validator that both sides state and that disagrees is the one case
    // where continuing is definitely wrong. If either side is silent there is
    // nothing to check and length is all the assurance available.
    match (&state.validator, &probe.validator) {
        (Some(before), Some(now)) if before != now => {
            log::info!("the file changed on the server since this download started; starting over");
            None
        }
        _ => {
            let have: u64 = state.done.iter().map(|(a, b)| b - a).sum();
            if have > 0 {
                log::info!("resuming: {have} of {size} bytes already here");
            }
            Some(state.done)
        }
    }
}

fn save_state(part: &Path, probe: &Probe, done: &[(u64, u64)]) {
    let Some(size) = probe.size else { return };
    let state = State {
        url: probe.url.clone(),
        size,
        validator: probe.validator.clone(),
        done: done.to_vec(),
    };
    if let Ok(json) = serde_json::to_vec(&state) {
        // Written whole then renamed: a state file caught half-written by a
        // crash would be unparseable, and the resume it exists to enable would
        // be the thing it prevented.
        let tmp = state_path(part).with_extension("state.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, state_path(part));
        }
    }
}

/* ------------------------------------------------------------------ *
 * Rate limiting
 * ------------------------------------------------------------------ */

/// A token bucket shared by every connection, so the cap is on the download
/// rather than on each of its parts.
struct Limiter {
    rate: u64,
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    fn new(rate: u64) -> Self {
        Self { rate, state: Mutex::new((rate as f64, Instant::now())) }
    }

    /// How long to wait before spending `n` bytes.
    fn take(&self, n: u64) -> Duration {
        let mut state = self.state.lock().unwrap();
        let (ref mut tokens, ref mut last) = *state;
        let now = Instant::now();
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * self.rate as f64)
            .min(self.rate as f64);
        *last = now;
        *tokens -= n as f64;
        if *tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-*tokens / self.rate as f64)
        }
    }
}

/* ------------------------------------------------------------------ *
 * Adaptive concurrency
 * ------------------------------------------------------------------ */

/// Decides how many connections to run, by trying and measuring.
///
/// The rule every fixed setting gets wrong: connections help while the
/// *server* is rationing per connection, and stop helping the moment the link
/// is full. Sixteen connections on a saturated line is the same bytes through
/// the same pipe plus fifteen more handshakes, and on a server that counts
/// them it is a rate-limit or a ban.
///
/// So this climbs. Add a connection, measure a window, keep it if throughput
/// rose by more than the noise floor, and stop when it does not. The noise
/// floor is deliberately generous because a fluctuating link moves on its own,
/// and reading its drift as a verdict on the last change is how a governor
/// ends up oscillating.
struct Governor {
    target: AtomicUsize,
    /// Highest count that has ever been tried without hurting; the climb does
    /// not go back above a level that already disappointed.
    ceiling: AtomicUsize,
}

/// How long each measurement runs. Long enough for a new connection to finish
/// slow-start and contribute honestly, short enough that the climb converges
/// while there is still a download left to benefit: one window is spent on the
/// baseline before any decision, and a ten-megabyte file on a domestic link is
/// over in twenty seconds.
const WINDOW: Duration = Duration::from_secs(3);

/// The improvement a new connection has to show to be kept. Below this it is
/// indistinguishable from the link wandering.
const WORTHWHILE: f64 = 1.12;

/// A drop this large says the last change actively hurt — the server throttling
/// the extra socket, or the link thrashing — rather than the link drifting.
const HARMFUL: f64 = 0.85;

/// How often a settled download re-tests whether more connections would help
/// now. Conditions change; a number chosen in the first ten seconds should not
/// bind for an hour.
const REPROBE: Duration = Duration::from_secs(45);

/// How much of the settled throughput a connection has to be worth to be kept
/// when the governor tries taking it away again.
///
/// Deliberately strict — 5% is inside the noise this same code calls weather
/// when it is climbing, so a trim only sticks when removing the connection
/// changed essentially nothing. The asymmetry is the point: keeping a socket
/// that does nothing costs the server a little, while dropping one that was
/// carrying bytes costs the user their download speed.
const REDUNDANT: f64 = 0.95;

/// What the governor is doing with the window it is currently measuring.
enum Phase {
    /// Adding connections while each doubling still pays for itself.
    Climbing,
    /// Taking them away again to find the *fewest* that go this fast.
    ///
    /// `reference` is the throughput the climb ended on and every trim is
    /// judged against it, rather than against the step above — comparing each
    /// step to the last would let five windows of "only 4% slower" add up to a
    /// third of the speed. `good` is the smallest count that has held it.
    Trimming { reference: f64, good: usize },
    /// Neither, until [`REPROBE`] says to look again.
    Settled(Instant),
}

/// Ask `excess` of the running connections to stop once they have finished the
/// piece in their hands. What they have not fetched goes back to the pool.
fn wind_down(work: &Work, excess: usize) {
    for claim in work
        .claims
        .iter()
        .filter(|c| c.live.load(Ordering::Acquire))
        .take(excess)
    {
        claim.retire.store(true, Ordering::Release);
    }
}

/// Stop adjusting, and tell the UI what the number ended up being.
async fn settle(tx: &tokio::sync::mpsc::Sender<Event>, at: usize, speed: f64) -> Phase {
    let _ = tx
        .send(Event::Concurrency { connections: at as u8, speed: speed as u64 })
        .await;
    Phase::Settled(Instant::now())
}

/// Watch throughput, adjust the connection count, and say why.
///
/// Two movements, not one. The climb finds a count that is fast; the trim then
/// finds the *smallest* count that is just as fast. Without the second, a
/// server that is equally happy with one connection and with four is served
/// four for the life of the download, purely because four is where the search
/// began — fast, but rude, and on a server that counts connections the
/// difference between fine and rate-limited.
///
/// The climb starts at four rather than one on purpose. Every step costs a
/// measurement window, so starting at one and doubling spends three windows
/// reaching the range most servers actually reward; starting in that range and
/// trimming downwards reaches the same answer sooner, and a short download is
/// over before a slower search has decided anything.
async fn govern(
    shared: Arc<Shared>,
    governor: Arc<Governor>,
    max: usize,
    tx: tokio::sync::mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
) {
    let work = &shared.work;
    let counters = &shared.counters;
    let mut previous: Option<f64> = None;
    let mut phase = Phase::Climbing;

    loop {
        let before = counters.received.load(Ordering::Relaxed);
        let started = Instant::now();
        tokio::time::sleep(WINDOW).await;
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let moved = counters.received.load(Ordering::Relaxed).saturating_sub(before);
        let speed = moved as f64 / started.elapsed().as_secs_f64();

        // A window with no connections running measures nothing about the
        // connection count; the download is simply finishing.
        if counters.active.load(Ordering::Relaxed) == 0 {
            return;
        }

        let current = governor.target.load(Ordering::Relaxed);
        log::debug!(
            "governor: {current} connections moved {:.0} KB/s ({} live)",
            speed / 1024.0,
            counters.active.load(Ordering::Relaxed)
        );
        let Some(last) = previous else {
            previous = Some(speed);
            continue;
        };
        previous = Some(speed);

        phase = match phase {
            Phase::Climbing => {
                if speed > last * WORTHWHILE && current < max {
                    // It helped: double rather than add one. Stepping up
                    // singly needs a window per connection — a short download
                    // ends before the climb arrives anywhere — and, worse, one
                    // extra socket moves throughput less than a fluctuating
                    // link does on its own, so the measurement cannot tell the
                    // change from the weather. Doubling reaches the useful
                    // range in a couple of windows and makes a difference big
                    // enough to read.
                    let next = (current * 2).min(max);
                    governor.target.store(next, Ordering::Relaxed);
                    let _ = tx
                        .send(Event::Concurrency { connections: next as u8, speed: speed as u64 })
                        .await;
                    Phase::Climbing
                } else {
                    let held = if speed < last * HARMFUL && current > 1 {
                        // It hurt. Go back to what was working, and do not
                        // come above this again for this download.
                        let back = (current / 2).max(1);
                        governor.target.store(back, Ordering::Relaxed);
                        governor.ceiling.store(back, Ordering::Relaxed);
                        wind_down(work, current - back);
                        back
                    } else {
                        current
                    };
                    // Now find out how few of these are earning their place.
                    if held > 1 {
                        governor.target.store(held / 2, Ordering::Relaxed);
                        wind_down(work, held - held / 2);
                        Phase::Trimming { reference: speed.max(last), good: held }
                    } else {
                        settle(&tx, held, speed).await
                    }
                }
            }
            Phase::Trimming { reference, good } => {
                if speed >= reference * REDUNDANT {
                    // Nothing was lost by dropping them, so they were not
                    // doing anything. Keep going down while that holds.
                    if current > 1 {
                        governor.target.store(current / 2, Ordering::Relaxed);
                        wind_down(work, current - current / 2);
                        Phase::Trimming { reference, good: current }
                    } else {
                        settle(&tx, current, speed).await
                    }
                } else {
                    // That one was carrying bytes. Put the connections back
                    // and stop here; `conduct` spawns them again by itself.
                    governor.target.store(good, Ordering::Relaxed);
                    settle(&tx, good, speed).await
                }
            }
            Phase::Settled(at) if at.elapsed() >= REPROBE && current < max => {
                // The link is not what it was a minute ago. Try double and
                // see; if it does not help, this settles again straight away.
                governor.target.store((current * 2).min(max), Ordering::Relaxed);
                Phase::Climbing
            }
            settled => settled,
        };
    }
}
/* ------------------------------------------------------------------ *
 * The download itself
 * ------------------------------------------------------------------ */

struct Counters {
    /// Bytes off the wire. Counted on arrival rather than at the write, so the
    /// readout does not freeze while a segment's tail sits in a buffer.
    received: AtomicU64,
    active: AtomicU64,
}

struct Shared {
    work: Work,
    counters: Counters,
    stop: Arc<AtomicBool>,
    file: std::fs::File,
    limiter: Option<Limiter>,
    /// Set when a server answers 429 or 503: it is asking for fewer
    /// connections, and that is not a measurement to be argued with.
    pushback: AtomicBool,
}

/// The same client the segmented fetcher uses, for the stream downloader.
///
/// Shared rather than rebuilt so a stream inherits the request shape a capture
/// arrived with — the headers, the referrer, the HTTP/1.1 decision. A CDN that
/// serves a manifest to the browser and refuses it to us is nearly always
/// answering the headers, not the address.
pub async fn stream_client(spec: &Spec) -> Result<reqwest::Client> {
    client(spec).await
}

/// The headers one of our requests will actually carry.
///
/// Split out of the client so it can be checked directly: "did this job send a
/// User-Agent?" is a question worth a test, and building a whole client to ask
/// it is not.
pub fn request_headers(spec: &Spec) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for h in &spec.headers {
        let name = reqwest::header::HeaderName::from_bytes(h.name.as_bytes());
        let value = reqwest::header::HeaderValue::from_str(&h.value);
        if let (Ok(name), Ok(value)) = (name, value) {
            headers.insert(name, value);
        }
    }
    if let Some(referrer) = spec.referrer.as_deref().filter(|r| !r.is_empty()) {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(referrer) {
            headers.insert(reqwest::header::REFERER, value);
        }
    }
    // Only when the capture did not bring one. A browser's own header is a
    // better answer than ours, and overwriting it would be the one way to make
    // a working captured download stop working.
    headers
        .entry(reqwest::header::USER_AGENT)
        .or_insert_with(|| reqwest::header::HeaderValue::from_static(DEFAULT_USER_AGENT));
    headers
}

async fn client(spec: &Spec) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().default_headers(request_headers(spec));

    // Ordinary web PKI verification, with one repair reqwest's own setup does
    // not make: a server that sends its leaf certificate and forgets the
    // intermediate above it has its chain completed from the address the leaf
    // names, rather than failing a download the browser completes. See `tls`.
    // Where the configuration cannot be built, reqwest's default stands.
    #[cfg(unix)]
    if let Some(config) = crate::tls::client_config() {
        builder = builder.use_preconfigured_tls(config);
    }

    // Left alone, reqwest already follows this machine's proxy settings, which
    // is what the browser the download was captured from does — so the default
    // needs no setting at all, and an office network that configures one
    // centrally works without anybody being asked a question.
    //
    // `off` therefore has to exist as a word: on a machine with a system proxy
    // there is otherwise no way to spell "go direct", because an empty box
    // means "whatever the system says" rather than "nothing".
    match spec.proxy.trim() {
        "" => {}
        "off" | "none" | "direct" => builder = builder.no_proxy(),
        url => match reqwest::Proxy::all(url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            // Not fatal: a mistyped proxy should fail as "this download could
            // not reach the server", with the reason in the log, rather than
            // taking every download down with a startup error.
            Err(e) => log::warn!("proxy {url:?} is not a usable URL ({e}); going direct"),
        },
    }

    // Resolve up front, with the lookup's own budget, and hand the answer to
    // reqwest. Two things follow: `connect_timeout` below now measures only
    // the TCP handshake, so a slow resolver can no longer be reported as an
    // unreachable server; and the addresses arrive already filtered to the
    // ones this machine can actually route to.
    //
    // Best-effort. A failure here is left for the request itself to hit, so a
    // host we could not pre-resolve still gets its ordinary attempt and its
    // ordinary error rather than a different one from this layer.
    if let Ok(url) = url::Url::parse(&spec.url) {
        if let Some(host) = url.host_str() {
            let port = url.port_or_known_default().unwrap_or(443);
            match crate::resolve::addresses(host, port).await {
                Ok(addrs) => builder = builder.resolve_to_addrs(host, addrs.as_slice()),
                Err(e) => log::debug!("pre-resolving {host} failed ({e:#}); \
                                       letting the request try for itself"),
            }
        }
    }

    builder
        // HTTP/1.1, deliberately. Segmenting is fast because N connections are
        // N independent TCP flows, each with its own congestion window — that
        // is the whole reason it beats one stream on a long or lossy link.
        // HTTP/2 would fold them back onto a single connection as multiplexed
        // streams sharing one window: the same bytes, the same head-of-line
        // blocking, and more bookkeeping to arrive there. aria2 is HTTP/1.1
        // for this reason too.
        .http1_only()
        .pool_max_idle_per_host(MAX_CONNECTIONS as usize)
        .redirect(reqwest::redirect::Policy::limited(10))
        // The handshake alone, now that the lookup has its own budget. A TCP
        // connection that has not completed in fifteen seconds is not one that
        // is about to.
        .connect_timeout(Duration::from_secs(15))
        // No total timeout: a large file on a slow link is not a stuck one.
        // A connection that stops delivering is caught here instead, and the
        // retry loop re-requests only the part it still owes.
        .read_timeout(Duration::from_secs(30))
        .build()
        .context("building the HTTP client")
}

/// Marks the "this was a page" failure so the engine can recognise it without
/// matching on prose. Deliberately terse and unlocalised.
pub const PAGE_MARKER: &str = "not-a-file";

/// Whether the engine should read a failure as "that URL was a page".
pub fn is_page_response(message: &str) -> bool {
    message.contains(PAGE_MARKER)
}

/// Did the browser watch this address hand over the file itself?
///
/// The capture records what the *response* said, not what the address looks
/// like: a byte count, and a content type that is not a page. Both present
/// means this URL really did serve a file once, with the browser watching it
/// happen — which is a different thing from an address that has always been a
/// page, and needs a different answer when a later request finds HTML there.
///
/// Nothing in a manually pasted URL can satisfy this: no request has been made
/// yet, so there is no length and no type to have recorded.
pub fn capture_saw_a_file(total_bytes: i64, mime: &str) -> bool {
    total_bytes > 0 && !mime.trim().is_empty() && !is_html(mime)
}

fn is_html(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    mime == "text/html" || mime == "application/xhtml+xml"
}

/// Someone deliberately saving a web page is not this mistake, and the name
/// they are saving it under is what says so.
fn looks_like_html_filename(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    name.ends_with(".html") || name.ends_with(".htm") || name.ends_with(".xhtml")
}

/// Ask for one byte, and learn everything from the answer.
///
/// A ranged GET rather than a HEAD: HEAD is the textbook probe, but plenty of
/// servers answer it with a 405 or with headers that disagree with what a GET
/// returns. Asking for `bytes=0-0` gets the facts out of the same request path
/// the download will use — the final URL, the length (from Content-Range), the
/// name, and decisively whether ranges work: a 206 says yes, a 200 says this
/// file arrives in one stream however politely we ask.
pub async fn probe(client: &reqwest::Client, url: &str) -> Result<Probe> {
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .send()
        .await
        .with_context(|| format!("probing {url}"))?;

    let status = response.status();
    if !status.is_success() {
        bail!("{url} answered {status}");
    }

    let final_url = response.url().to_string();
    let headers = response.headers();
    let resumable = status == reqwest::StatusCode::PARTIAL_CONTENT;

    // Content-Range carries the whole size ("bytes 0-0/12345"). Content-Length
    // on a 206 is the length of the *part* — one byte here, and reading it as
    // the size would make every download one byte long.
    let size = if resumable {
        headers
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next().map(str::to_owned))
            .filter(|total| total != "*")
            .and_then(|total| total.parse::<u64>().ok())
    } else {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };

    let mime = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let validator = headers
        .get(reqwest::header::ETAG)
        .or_else(|| headers.get(reqwest::header::LAST_MODIFIED))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let filename = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_disposition)
        .unwrap_or_else(|| crate::naming::filename_from_url(&final_url));

    Ok(Probe { url: final_url, size, resumable, filename, mime, validator })
}

/// The name out of `attachment; filename="thing.iso"`, preferring RFC 5987's
/// `filename*` when both are offered — only that one can carry a name that is
/// not Latin-1.
fn filename_from_disposition(value: &str) -> Option<String> {
    let extended = value.split(';').find_map(|part| {
        let rest = part.trim().strip_prefix("filename*=")?;
        // `UTF-8''Na%C3%AFve.iso` — charset and language, then the name.
        let encoded = rest.rsplit('\'').next()?;
        Some(
            percent_encoding::percent_decode_str(encoded)
                .decode_utf8_lossy()
                .into_owned(),
        )
    });
    if let Some(name) = extended.filter(|n| !n.is_empty()) {
        return Some(crate::naming::sanitize(name));
    }
    let plain = value.split(';').find_map(|part| {
        let rest = part.trim().strip_prefix("filename=")?;
        Some(rest.trim_matches('"').to_owned())
    })?;
    Some(crate::naming::sanitize(plain)).filter(|n| !n.is_empty())
}

/// Whether an address answers with a web page rather than a file.
///
/// The same one-byte probe a download opens with, asked *before* anything
/// is offered rather than after a download has already failed. A player
/// behind a file-shaped URL — `view_video.php`, `watch`, `embed/…` — is a
/// quality to choose, not a file to save, and the difference is worth
/// knowing while there is still a window to ask the question in.
///
/// `name` is the name the file would be saved under: someone deliberately
/// saving a page is not this mistake, and their own name for it says so.
pub async fn is_page(
    url: &str,
    name: &str,
    headers: &[Header],
    referrer: Option<&str>,
) -> Result<bool> {
    let mut spec = Spec::new(url, PathBuf::new());
    // Asked with what the browser was carrying: a site that answers a bare
    // request with a login wall is a page of a different kind, and would be
    // offered as neither a file nor a video.
    spec.headers = headers.to_vec();
    spec.referrer = referrer.map(str::to_owned).filter(|r| !r.is_empty());
    let client = client(&spec).await?;
    let probe = probe(&client, url).await?;
    Ok(is_html(&probe.mime) && !looks_like_html_filename(name))
}

/// Make the one request an address may be good for, and keep what it opened.
///
/// Some file hosts hand out a link that answers exactly once: serve it, and
/// every later request for it gets the landing page instead. The ordinary
/// capture cannot work on those, because it only learns a download exists once
/// the browser has already spent the link asking for it — which is why the
/// browser gets those downloads and MDM records the host and stays out of the
/// way. `Engine::preempt` is the other side of that: the request is held
/// before the browser sends it and made from here instead, so the one answer
/// the address has left comes to us.
///
/// One request, and not a byte more. No `Range` probe, because that is a
/// request; no second connection, for the same reason. The whole of what a
/// probe would have told us is read off these headers.
///
/// `Ok(None)` is "that address is a page". The browser is still holding its
/// request and can have it after all: a page is not spent by being read, so
/// nothing has been lost by looking.
pub async fn open(spec: &Spec) -> Result<Option<(reqwest::Response, Probe)>> {
    let client = client(spec).await?;
    let response = client
        .get(&spec.url)
        .send()
        .await
        .with_context(|| format!("requesting {}", spec.url))?;

    let status = response.status();
    if !status.is_success() {
        bail!("{} answered {status}", spec.url);
    }

    let final_url = response.url().to_string();
    let headers = response.headers();
    let mime = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let size = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let disposition = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let validator = headers
        .get(reqwest::header::ETAG)
        .or_else(|| headers.get(reqwest::header::LAST_MODIFIED))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let filename = disposition
        .as_deref()
        .and_then(filename_from_disposition)
        .unwrap_or_else(|| crate::naming::filename_from_url(&final_url));

    if answered_with_a_page(&mime, disposition.as_deref()) {
        return Ok(None);
    }

    // `resumable` stays false whatever the server would have allowed: this is
    // the one request there is, so there is nothing to resume into and nothing
    // to split.
    let probe = Probe { url: final_url, size, resumable: false, filename, mime, validator };
    Ok(Some((response, probe)))
}

/// Is this response the site's own page rather than the file behind it?
///
/// The question [`open`] has to answer from one set of headers, and the only
/// judgement in that whole function — so it is here, where it can be tested
/// as the judgement it is.
///
/// "Save this" outranks the type. A server that sent `Content-Disposition:
/// attachment` has said what its response is for, and a fair number of file
/// hosts label the bytes `text/html` on the way out anyway; refusing on the
/// type alone would hand exactly those downloads back to the browser, which is
/// the one thing pre-emption exists to stop.
fn answered_with_a_page(mime: &str, disposition: Option<&str>) -> bool {
    let attachment = disposition
        .map(|d| d.trim_start().to_ascii_lowercase().starts_with("attachment"))
        .unwrap_or(false);
    !attachment && is_html(mime)
}

/// Write out a file whose connection is already open — see [`open`].
///
/// The segmented path in [`download`] and everything it rests on assumes an
/// address that answers as often as it is asked. This one cannot: there is a
/// single response, it is already in hand, and when it ends the download is
/// over one way or the other. So no state file is written and no resume is
/// offered — a link that is spent cannot be picked up again, and pretending
/// otherwise would only turn a lost download into a stuck one.
pub async fn download_open(
    spec: Spec,
    response: reqwest::Response,
    probe: Probe,
    tx: tokio::sync::mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
) -> Result<PathBuf> {
    let _ = tx.send(Event::Probed(probe.clone())).await;

    let name = spec
        .filename
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| probe.filename.clone());

    std::fs::create_dir_all(&spec.dir)
        .with_context(|| format!("creating {}", spec.dir.display()))?;
    if let Some(size) = probe.size {
        check_space(&spec.dir, size)?;
    }
    let target = spec.dir.join(crate::naming::sanitize(name));
    let part = PathBuf::from({
        let mut p = target.as_os_str().to_owned();
        p.push(PART_SUFFIX);
        p
    });

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&part)
        .with_context(|| format!("opening {}", part.display()))?;
    // Nothing is resumed here, so whatever an earlier attempt left is not ours
    // to write around.
    file.set_len(0).ok();

    let shared = Arc::new(Shared {
        work: Work::new(VecDeque::new(), Vec::new()),
        counters: Counters {
            received: AtomicU64::new(0),
            // One, and it is already talking to the server.
            active: AtomicU64::new(1),
        },
        stop: stop.clone(),
        file,
        limiter: (spec.max_speed > 0).then(|| Limiter::new(spec.max_speed)),
        pushback: AtomicBool::new(false),
    });

    // The claim `conduct` would have dealt out, dealt by hand: the whole file,
    // to the one connection there is. `u64::MAX` is the same open-ended end a
    // range-less response gets in [`download`] — the stream is finished when
    // it stops, not at a boundary anybody knew in advance.
    let claim = &shared.work.claims[0];
    claim.cursor.store(0, Ordering::Release);
    claim.end.store(u64::MAX, Ordering::Release);
    claim.live.store(true, Ordering::Release);

    let reporter = tokio::spawn(report(
        shared.clone(),
        tx.clone(),
        probe.size,
        0,
        part.clone(),
        // The size is kept for the progress bar and taken off the copy the
        // checkpoint is written from, which is how `save_state` is told there
        // is nothing to write: a state file here would describe a resume that
        // cannot happen, and would be read by one that then asks the spent
        // address for the rest of the file.
        Probe { size: None, ..probe.clone() },
        stop.clone(),
    ));
    let outcome = drain(&shared, response, 0, false).await;
    reporter.abort();
    claim.live.store(false, Ordering::Release);
    outcome?;

    if stop.load(Ordering::Relaxed) {
        // Said plainly rather than left as a partial file with a state beside
        // it: pausing this is losing it, because the address will not answer a
        // second time.
        bail!("stopped — this link answers once, so there is nothing to resume");
    }

    shared.file.sync_all().ok();
    if let Some(expected) = &spec.expected_sha256 {
        verify(&part, expected)?;
    }
    std::fs::rename(&part, &target)
        .with_context(|| format!("renaming {} into place", part.display()))?;
    let _ = std::fs::remove_file(state_path(&part));

    let received = shared.counters.received.load(Ordering::Relaxed);
    let _ = tx
        .send(Event::Progress(Progress {
            downloaded: received,
            total: probe.size,
            speed: 0,
            connections: 0,
        }))
        .await;
    let _ = tx.send(Event::Done(target.clone())).await;
    Ok(target)
}

/// Fetch `spec` into its directory, reporting as it goes.
///
/// `stop` ends every connection at the next chunk boundary and leaves the
/// partial file and its state beside it, which is what a later call resumes
/// from.
pub async fn download(
    spec: Spec,
    tx: tokio::sync::mpsc::Sender<Event>,
    stop: Arc<AtomicBool>,
) -> Result<PathBuf> {
    let client = client(&spec).await?;
    let probe = probe(&client, &spec.url).await?;
    let _ = tx.send(Event::Probed(probe.clone())).await;

    let name = spec
        .filename
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| probe.filename.clone());

    // A page is not a file. Sites whose player lives behind a normal-looking
    // URL — `view_video.php`, `watch`, `embed/…` — answer with HTML, and
    // saving that produces a download that completes, weighs a megabyte and a
    // half, and cannot be opened. It is worse than a failure, because nothing
    // about it looks wrong until you try to play it.
    //
    // Deliberately not a host list. Any site can serve a player page, and a
    // list of the ones we happen to know is a thing that is permanently out of
    // date; what the response *is* answers the question for all of them. The
    // error is phrased for [`is_page_response`], which turns this into a
    // second attempt through the extractor.
    if is_html(&probe.mime) && !looks_like_html_filename(&name) {
        bail!(
            "{PAGE_MARKER}: the server answered with a web page rather than a \
             file ({})",
            probe.mime
        );
    }
    std::fs::create_dir_all(&spec.dir)
        .with_context(|| format!("creating {}", spec.dir.display()))?;
    let target = spec.dir.join(crate::naming::sanitize(name));
    let part = PathBuf::from({
        let mut p = target.as_os_str().to_owned();
        p.push(PART_SUFFIX);
        p
    });

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&part)
        .with_context(|| format!("opening {}", part.display()))?;

    // Only a server that states a length and honours ranges can be split. A
    // chunked or range-less response is one stream by definition, and asking
    // for more connections would fetch the whole file several times over.
    let size = probe.size.unwrap_or(0);
    let splittable = probe.resumable && size > spec.min_split;

    let resumed = if splittable { load_state(&part, &probe).unwrap_or_default() } else { Vec::new() };
    if resumed.is_empty() {
        // Nothing to keep: start the file clean rather than writing over
        // whatever a previous, unrelated attempt left.
        file.set_len(0).ok();
    }
    if splittable {
        file.set_len(size).ok();
    }

    if splittable {
        // Checked against what still has to be fetched, not the whole file: a
        // resume needs room for the remainder, and the part already on disk is
        // occupying its own space already.
        let outstanding = size.saturating_sub(resumed.iter().map(|(a, b)| b - a).sum::<u64>());
        check_space(&spec.dir, outstanding)?;
    }

    let host = host_of(&probe.url);
    let max = match spec.concurrency {
        Concurrency::Fixed(n) => n.clamp(1, MAX_CONNECTIONS) as usize,
        // Whatever this host has already shown it tolerates outranks the
        // ceiling asked for here.
        Concurrency::Auto { max } => max.clamp(1, host_limit(&host)) as usize,
    };
    // However many connections are allowed, a file with three splits' worth of
    // bytes in it cannot use thirty.
    let ceiling = if splittable {
        max.min(size.div_ceil(spec.min_split).max(1) as usize)
    } else {
        1
    };
    let start_at = match spec.concurrency {
        Concurrency::Fixed(_) => ceiling,
        // Auto starts low and earns its way up: opening thirty sockets to find
        // out that one was enough is exactly the rudeness this avoids.
        Concurrency::Auto { .. } => ceiling.min(4),
    };

    let gaps = if splittable {
        missing(&resumed, size)
    } else {
        VecDeque::from(vec![(0, u64::MAX)])
    };
    let work = Work::new(plan(gaps, ceiling, spec.min_split), resumed);

    let shared = Arc::new(Shared {
        work,
        counters: Counters {
            received: AtomicU64::new(0),
            active: AtomicU64::new(0),
        },
        stop: stop.clone(),
        file,
        limiter: (spec.max_speed > 0).then(|| Limiter::new(spec.max_speed)),
        pushback: AtomicBool::new(false),
    });
    let already = shared.work.completed_bytes();

    let governor = Arc::new(Governor {
        target: AtomicUsize::new(start_at),
        ceiling: AtomicUsize::new(ceiling),
    });

    // One URL per connection, cycling the mirrors: N connections to one host
    // become N spread over however many hosts have the file.
    let mut sources = vec![probe.url.clone()];
    sources.extend(spec.mirrors.iter().cloned());
    let sources = Arc::new(sources);

    let reporter = tokio::spawn(report(
        shared.clone(),
        tx.clone(),
        probe.size,
        already,
        part.clone(),
        probe.clone(),
        stop.clone(),
    ));
    let governing = matches!(spec.concurrency, Concurrency::Auto { .. }) && splittable;
    let conductor = tokio::spawn(conduct(
        shared.clone(),
        governor.clone(),
        client.clone(),
        sources.clone(),
        spec.clone(),
        splittable,
    ));
    let watching = governing.then(|| {
        tokio::spawn(govern(
            shared.clone(),
            governor.clone(),
            ceiling,
            tx.clone(),
            stop.clone(),
        ))
    });

    let outcome = conductor.await;
    reporter.abort();
    if let Some(w) = watching {
        w.abort();
    }
    outcome.context("the download supervisor stopped unexpectedly")??;

    if stop.load(Ordering::Relaxed) {
        save_state(&part, &probe, &shared.work.done.lock().unwrap());
        bail!("stopped");
    }

    shared.file.sync_all().ok();
    if let Some(expected) = &spec.expected_sha256 {
        verify(&part, expected)?;
    }

    // Only now does it get the real name: a file under its final name is a
    // finished file, to the user and to the engine's own collision checks.
    std::fs::rename(&part, &target)
        .with_context(|| format!("renaming {} into place", part.display()))?;
    let _ = std::fs::remove_file(state_path(&part));

    let _ = tx
        .send(Event::Progress(Progress {
            downloaded: already + shared.counters.received.load(Ordering::Relaxed),
            total: probe.size,
            speed: 0,
            connections: 0,
        }))
        .await;
    let _ = tx.send(Event::Done(target.clone())).await;
    Ok(target)
}

/// Keep as many connections running as the governor currently wants.
async fn conduct(
    shared: Arc<Shared>,
    governor: Arc<Governor>,
    client: reqwest::Client,
    sources: Arc<Vec<String>>,
    spec: Spec,
    splittable: bool,
) -> Result<()> {
    let mut running: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let mut next_slot = 0usize;
    let mut failure: Option<anyhow::Error> = None;

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        // A server asking for fewer connections outranks any measurement.
        if shared.pushback.swap(false, Ordering::Relaxed) {
            let target = governor.target.load(Ordering::Relaxed);
            if target > 1 {
                governor.target.store(target - 1, Ordering::Relaxed);
                governor.ceiling.store(target - 1, Ordering::Relaxed);
            }
        }

        running.retain(|task| !task.is_finished());
        let want = governor
            .target
            .load(Ordering::Relaxed)
            .min(governor.ceiling.load(Ordering::Relaxed))
            .max(1);

        // Spawn up to the target, but only while there is work a new
        // connection could actually take — asking without taking, so the
        // question does not consume the answer.
        let mut scanned = 0;
        while running.len() < want && shared.work.has_work(spec.min_split) {
            let slot = next_slot % MAX_CONNECTIONS as usize;
            next_slot += 1;
            scanned += 1;
            if shared.work.claims[slot].live.load(Ordering::Acquire) {
                // Every slot is busy: nothing to add until one frees up.
                if scanned > MAX_CONNECTIONS as usize {
                    break;
                }
                continue;
            }
            let shared = shared.clone();
            let client = client.clone();
            let url = sources[slot % sources.len()].clone();
            let spec = spec.clone();
            running.push(tokio::spawn(async move {
                work(shared, client, url, slot, spec, splittable).await
            }));
        }

        // No connections running and nothing left to give one: done. Reached
        // only after the spawn loop above has had its chance, so "nothing to
        // do" means finished rather than momentarily idle.
        if running.is_empty() {
            break;
        }

        // Reap anything that finished, keeping the first real error.
        let mut still = Vec::with_capacity(running.len());
        for task in running.drain(..) {
            if task.is_finished() {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        failure.get_or_insert(e);
                    }
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => {
                        failure.get_or_insert(anyhow::anyhow!("a connection panicked: {e}"));
                    }
                }
            } else {
                still.push(task);
            }
        }
        running = still;
        if failure.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for task in running {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                failure.get_or_insert(e);
            }
            Err(e) if e.is_cancelled() => {}
            Err(e) => {
                failure.get_or_insert(anyhow::anyhow!("a connection panicked: {e}"));
            }
        }
    }

    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// One connection: take work, fetch it, take more, until there is none or the
/// governor asks this one to wind down.
async fn work(
    shared: Arc<Shared>,
    client: reqwest::Client,
    url: String,
    slot: usize,
    spec: Spec,
    splittable: bool,
) -> Result<()> {
    let claim = &shared.work.claims[slot];
    claim.retire.store(false, Ordering::Release);
    claim.live.store(true, Ordering::Release);
    let result = run(&shared, &client, &url, slot, &spec, splittable).await;
    claim.live.store(false, Ordering::Release);
    // Whatever this connection still owed goes back for someone else.
    let left = (
        claim.cursor.load(Ordering::Acquire),
        claim.end.load(Ordering::Acquire),
    );
    shared.work.give_back(left);
    claim.cursor.store(0, Ordering::Release);
    claim.end.store(0, Ordering::Release);
    result
}

async fn run(
    shared: &Arc<Shared>,
    client: &reqwest::Client,
    url: &str,
    slot: usize,
    spec: &Spec,
    splittable: bool,
) -> Result<()> {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let claim = &shared.work.claims[slot];
        if claim.retire.load(Ordering::Acquire) {
            return Ok(());
        }

        if claim.remaining() == 0 {
            match shared.work.next(spec.min_split) {
                Some((start, end)) => {
                    claim.cursor.store(start, Ordering::Release);
                    claim.end.store(end, Ordering::Release);
                }
                None => return Ok(()),
            }
        }

        let mut attempt = 0;
        loop {
            match segment(shared, client, url, slot, splittable).await {
                Ok(()) => break,
                Err(e) if attempt < spec.retries && !shared.stop.load(Ordering::Relaxed) => {
                    attempt += 1;
                    // Backing off rather than hammering: a connection dropped
                    // mid-file is usually the far end shedding load, and an
                    // instant retry is what it is shedding.
                    let wait = Duration::from_millis(250 * u64::from(attempt).pow(2));
                    log::debug!("connection {slot} failed ({e:#}); retry {attempt} in {wait:?}");
                    tokio::time::sleep(wait.min(Duration::from_secs(10))).await;
                }
                Err(e) => return Err(e),
            }
        }

        if !splittable {
            // A stream with no length to divide is finished when it ends, and
            // its range has to be closed to say so. The sentinel end this
            // started with is not a real boundary: left as it was, the leftover
            // `(cursor, u64::MAX)` went back to the pool as unfinished work,
            // the next connection fetched the same file again from the top,
            // and a 2 MB download grew to 843 MB of the same bytes repeated.
            claim
                .end
                .store(claim.cursor.load(Ordering::Acquire), Ordering::Release);
            return Ok(());
        }
    }
}

/// Fetch one claim's range, stopping early if it is stolen from or retired.
async fn segment(
    shared: &Arc<Shared>,
    client: &reqwest::Client,
    url: &str,
    slot: usize,
    splittable: bool,
) -> Result<()> {
    let claim = &shared.work.claims[slot];
    let start = claim.cursor.load(Ordering::Acquire);
    let end = claim.end.load(Ordering::Acquire);
    if splittable && start >= end {
        return Ok(());
    }

    let mut request = client.get(url);
    if splittable {
        request = request.header(
            reqwest::header::RANGE,
            format!("bytes={start}-{}", end - 1),
        );
    }

    // Queued behind however many connections this host already has open, from
    // this download and every other one.
    let _permit = host_slot(&host_of(url)).await;

    let response = request.send().await.context("requesting a segment")?;
    let status = response.status();

    // 429 and 503 are the server saying "fewer, please" in the only vocabulary
    // it has. Treating that as a transient error and retrying at the same
    // concurrency is how a download turns into a ban.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        shared.pushback.store(true, Ordering::Relaxed);
        // Recorded against the host, not just this download: every other
        // download talking to it is part of what it is complaining about.
        note_pushback(&host_of(url));
        bail!("{status} — backing off");
    }
    if !status.is_success() {
        bail!("connection {slot} answered {status}");
    }
    // A server that forgot it was asked for a range would restart the whole
    // file into the middle of ours.
    if splittable && status != reqwest::StatusCode::PARTIAL_CONTENT {
        bail!("connection {slot}: the server ignored the range request ({status})");
    }

    shared.counters.active.fetch_add(1, Ordering::Relaxed);
    let result = drain(shared, response, slot, splittable).await;
    shared.counters.active.fetch_sub(1, Ordering::Relaxed);
    result
}

/// Wait for this host to have room for another connection.
///
/// Held for the life of the request, so the count reflects sockets actually
/// talking to the host rather than downloads that intend to.
async fn host_slot(host: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
    host_permits(host).acquire_owned().await.ok()
}

/// Copy the body to its offsets, honouring the rate limit, the stop flag and
/// whatever the governor decided while this was running.
async fn drain(
    shared: &Arc<Shared>,
    response: reqwest::Response,
    slot: usize,
    splittable: bool,
) -> Result<()> {
    let claim = &shared.work.claims[slot];
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::with_capacity(WRITE_BUFFER);
    let mut offset = claim.cursor.load(Ordering::Acquire);
    let segment_start = offset;

    while let Some(chunk) = stream.next().await {
        if shared.stop.load(Ordering::Relaxed) || claim.retire.load(Ordering::Acquire) {
            flush(shared, &mut buffer, &mut offset, claim, segment_start)?;
            return Ok(());
        }
        let chunk = chunk.context("reading the response body")?;
        if chunk.is_empty() {
            continue;
        }

        // The range may have shrunk while this chunk was in flight — that is a
        // steal handing our tail to an idle connection. Everything past the new
        // boundary belongs to them now, so drop it rather than write it twice.
        let limit = claim.end.load(Ordering::Acquire);
        let room = if splittable {
            limit.saturating_sub(offset + buffer.len() as u64)
        } else {
            u64::MAX
        };
        if room == 0 {
            break;
        }
        let take = (chunk.len() as u64).min(room) as usize;
        buffer.extend_from_slice(&chunk[..take]);
        // Counted here rather than at the write: the tail of a segment sits in
        // this buffer until it fills or the stream ends, and counting at the
        // write made a download that was moving report 0 B/s and a frozen
        // percentage before jumping to 100%.
        shared.counters.received.fetch_add(take as u64, Ordering::Relaxed);

        if let Some(limiter) = &shared.limiter {
            let wait = limiter.take(take as u64);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }

        if buffer.len() >= WRITE_BUFFER {
            flush(shared, &mut buffer, &mut offset, claim, segment_start)?;
        }
        if take < chunk.len() {
            break; // stolen from mid-chunk
        }
    }

    flush(shared, &mut buffer, &mut offset, claim, segment_start)?;
    Ok(())
}

/// Write what has accumulated at the offset it belongs to.
///
/// Positioned writes, so every connection shares one file handle without a
/// seek race and without part-files to concatenate at the end.
fn flush(
    shared: &Shared,
    buffer: &mut Vec<u8>,
    offset: &mut u64,
    claim: &Claim,
    segment_start: u64,
) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    write_at(&shared.file, buffer, *offset)?;
    let from = *offset;
    *offset += buffer.len() as u64;
    claim.cursor.store(*offset, Ordering::Release);
    // Recorded as done only once it is actually on disk — this is what the
    // state file persists, and a resume that trusted buffered bytes would skip
    // ranges that were never written.
    let _ = segment_start;
    shared.work.record_done(from, *offset);
    buffer.clear();
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, buffer: &[u8], offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buffer, offset).context("writing to the file")
}

#[cfg(windows)]
fn write_at(file: &std::fs::File, buffer: &[u8], offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    // Windows has no write_all_at; seek_write is the positioned write and, like
    // write(2), may take less than it was given.
    let mut written = 0;
    while written < buffer.len() {
        let n = file
            .seek_write(&buffer[written..], offset + written as u64)
            .context("writing to the file")?;
        if n == 0 {
            bail!("the file stopped accepting writes");
        }
        written += n;
    }
    Ok(())
}

/// Hash the finished file and compare, before it takes the name that says it
/// is good.
fn verify(part: &Path, expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(part).context("reopening the file to verify it")?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("reading the file to verify it")?;
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        bail!("checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Sample the counters on a timer, so the UI gets a steady readout rather than
/// one update per chunk from every connection at once — and checkpoint the
/// resume state while we are here.
#[allow(clippy::too_many_arguments)]
async fn report(
    shared: Arc<Shared>,
    tx: tokio::sync::mpsc::Sender<Event>,
    total: Option<u64>,
    already: u64,
    part: PathBuf,
    probe: Probe,
    stop: Arc<AtomicBool>,
) {
    const TICK: Duration = Duration::from_millis(500);
    /// Often enough that a crash costs seconds of work, rarely enough that it
    /// is not writing a state file every tick.
    const CHECKPOINT: Duration = Duration::from_secs(5);

    let mut last = (0u64, Instant::now());
    let mut checkpointed = Instant::now();
    loop {
        tokio::time::sleep(TICK).await;
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let received = shared.counters.received.load(Ordering::Relaxed);
        let now = Instant::now();
        let elapsed = now.duration_since(last.1).as_secs_f64();
        let speed = if elapsed > 0.0 {
            ((received.saturating_sub(last.0)) as f64 / elapsed) as u64
        } else {
            0
        };
        last = (received, now);

        if checkpointed.elapsed() >= CHECKPOINT {
            save_state(&part, &probe, &shared.work.done.lock().unwrap());
            checkpointed = now;
        }

        // Clamped: a steal can hand out a range whose head the victim had
        // already buffered, and both count it. Reporting 101% would be a worse
        // lie than the duplicate byte is a cost.
        let downloaded = (already + received).min(total.unwrap_or(u64::MAX));
        let progress = Progress {
            downloaded,
            total,
            speed,
            connections: shared.counters.active.load(Ordering::Relaxed),
        };
        if tx.send(Event::Progress(progress)).await.is_err() {
            return; // nobody is listening any more
        }
    }
}

/// Does this response describe something worth splitting at all?
pub fn worth_splitting(size: Option<u64>, resumable: bool, min_split: u64) -> bool {
    resumable && size.is_some_and(|s| s > min_split)
}

/* ---------------------------------------------------------------------- *
 * Unit tests
 *
 * The parts worth pinning down are the ones with arithmetic in them: who owns
 * which byte after a split, what a resume concludes is still missing, the
 * header parsing that decides how big a file is and what it is called, and the
 * token bucket.
 * ---------------------------------------------------------------------- */

#[cfg(test)]
mod tests {

    /// The download from the bug report. The browser watched `/download` hand
    /// over 2.1 GB of `video/webm`; the same GET, with the same cookies,
    /// answers with a 151 KB landing page. That is a spent one-time link, and
    /// the evidence for it is the capture's own record of the first response —
    /// which is what separates it from an address that was always a page.
    #[test]
    fn a_page_where_a_file_was_watched_arriving_is_a_spent_link() {
        assert!(super::capture_saw_a_file(2_204_580_989, "video/webm"));
        assert!(super::capture_saw_a_file(4096, "application/octet-stream"));

        // A pasted URL: nothing has requested it, so there is nothing recorded
        // and no reason to claim the site spent anything.
        assert!(!super::capture_saw_a_file(-1, ""));
        assert!(!super::capture_saw_a_file(-1, "video/mp4"));
        assert!(!super::capture_saw_a_file(1_000, ""));

        // A player page that was always a page. The extractor is the right
        // next step for this one, and it must stay reachable.
        assert!(!super::capture_saw_a_file(15_000, "text/html"));
        assert!(!super::capture_saw_a_file(15_000, "text/html; charset=UTF-8"));
    }

    /// The one judgement `open` makes, and the reason pre-emption is safe: a
    /// click that turns out to be a page has to come straight back to the
    /// browser, which is still holding the request.
    #[test]
    fn a_pre_empted_request_tells_a_page_from_a_download() {
        // The landing page of the very file host this exists for. Cancelling
        // this navigation would leave the tab showing nothing.
        assert!(super::answered_with_a_page("text/html; charset=UTF-8", None));
        assert!(super::answered_with_a_page("application/xhtml+xml", None));

        // Anything that is not a page is a download to be taken.
        assert!(!super::answered_with_a_page("video/webm", None));
        assert!(!super::answered_with_a_page("application/octet-stream", None));
        assert!(!super::answered_with_a_page("", None));

        // A file host that labels its bytes text/html and says "attachment"
        // anyway. The disposition is the deliberate statement of the two, and
        // reading only the type would hand this one back to the browser.
        assert!(!super::answered_with_a_page(
            "text/html",
            Some("attachment; filename=\"Moana.mkv\"")
        ));
        assert!(!super::answered_with_a_page("text/html", Some("  ATTACHMENT")));

        // `inline` is not that statement: it is how a server says "show this".
        assert!(super::answered_with_a_page("text/html", Some("inline")));
    }

    use super::*;

    fn work_with(claims: Vec<(u64, u64)>) -> Work {
        let work = Work::new(VecDeque::new(), Vec::new());
        for (i, (cursor, end)) in claims.into_iter().enumerate() {
            work.claims[i].cursor.store(cursor, Ordering::Release);
            work.claims[i].end.store(end, Ordering::Release);
            work.claims[i].live.store(true, Ordering::Release);
        }
        work
    }

    #[test]
    fn a_finished_connection_takes_half_of_the_slowest_one() {
        // Three connections: two nearly done, one with 8 MiB to go. The idle
        // one must be sent to the straggler rather than to whoever is first in
        // the list — that difference is the whole point of stealing.
        let work = work_with(vec![(90, 100), (0, 8 << 20), (95, 100)]);
        let (start, end) = work.steal(MIN_SPLIT).expect("a segment worth splitting");
        assert_eq!(start, 4 << 20, "the split lands halfway through the remainder");
        assert_eq!(end, 8 << 20);
        assert_eq!(
            work.claims[1].end.load(Ordering::Acquire),
            4 << 20,
            "the victim's range has to shrink, or both fetch the same bytes"
        );
    }

    #[test]
    fn nothing_is_split_below_the_threshold() {
        // Two connections' worth of round trips to save half a megabyte is a
        // loss, and splitting toward zero-length ranges would spin.
        let work = work_with(vec![(0, MIN_SPLIT), (0, 512)]);
        assert!(work.steal(MIN_SPLIT).is_none());
        let done = work_with(vec![(100, 100)]);
        assert!(done.steal(MIN_SPLIT).is_none(), "a finished segment has nothing to give");
    }

    #[test]
    fn work_handed_back_by_a_retiring_connection_is_picked_up() {
        // The governor winding a connection down must not lose the bytes it
        // still owed; they go back in the pool and the next connection to ask
        // gets them before anything is stolen.
        let work = work_with(vec![(0, 8 << 20)]);
        work.give_back((5 << 20, 6 << 20));
        assert_eq!(work.next(MIN_SPLIT), Some((5 << 20, 6 << 20)));
    }

    #[test]
    fn asking_whether_there_is_work_does_not_take_it() {
        // The bug this pins down cost a download every byte it had to fetch:
        // the supervisor used `next()` to decide whether to start another
        // connection, which *pops* a range, and the range it popped to answer
        // the question was dropped on the floor. A one-piece download then had
        // nothing left, every worker exited immediately, and the transfer sat
        // at zero bytes with the supervisor respawning workers forever.
        let work = Work::new(VecDeque::from(vec![(0, 8 << 20)]), Vec::new());
        assert!(work.has_work(MIN_SPLIT));
        assert!(work.has_work(MIN_SPLIT), "asking twice must not empty the pool");
        assert_eq!(work.next(MIN_SPLIT), Some((0, 8 << 20)), "the work is still there");
        assert!(!work.has_work(MIN_SPLIT), "and now it is genuinely gone");
    }

    #[test]
    fn a_live_connection_with_plenty_left_counts_as_available_work() {
        // An empty pool is not an idle download: a new connection can still
        // take half of whatever is running, and stopping early because the
        // pool was empty would leave the last segments single-threaded.
        let work = work_with(vec![(0, 8 << 20)]);
        assert!(work.pool.lock().unwrap().is_empty());
        assert!(work.has_work(MIN_SPLIT), "a big live claim is stealable work");

        // Once what is left is too small to split, there is nothing to add.
        let nearly = work_with(vec![(0, MIN_SPLIT)]);
        assert!(!nearly.has_work(MIN_SPLIT));
    }

    #[test]
    fn a_stream_with_no_length_is_never_handed_back_as_unfinished() {
        // A server that ignores Range answers 200, and the claim covering it
        // starts with an open-ended sentinel because there is no length to
        // divide. When the stream ends the claim has to close, or the leftover
        // `(cursor, u64::MAX)` looks like work nobody has done: the next
        // connection re-fetched the whole file and appended it, turning a 2 MB
        // download into 843 MB of the same bytes over and over.
        let work = work_with(vec![(0, u64::MAX)]);
        let claim = &work.claims[0];

        // What the worker does when the body ends.
        claim.end.store(claim.cursor.load(Ordering::Acquire), Ordering::Release);
        assert_eq!(claim.remaining(), 0, "a finished stream still looks unfinished");

        work.give_back((
            claim.cursor.load(Ordering::Acquire),
            claim.end.load(Ordering::Acquire),
        ));
        assert!(
            work.pool.lock().unwrap().is_empty(),
            "an empty range went back into the pool and would be fetched again"
        );
    }

    #[test]
    fn a_resume_asks_only_for_what_is_missing() {
        // Two finished ranges with a hole between them, and a tail: exactly the
        // shape a download interrupted across several connections leaves.
        let done = vec![(0, 1000), (2000, 3000)];
        let gaps: Vec<_> = missing(&done, 5000).into();
        assert_eq!(gaps, vec![(1000, 2000), (3000, 5000)]);

        // Nothing done yet is the whole file, and everything done is nothing.
        assert_eq!(Vec::from(missing(&[], 500)), vec![(0, 500)]);
        assert!(Vec::from(missing(&[(0, 500)], 500)).is_empty());
    }

    #[test]
    fn finished_ranges_are_merged_as_they_are_recorded() {
        // Every flush records a range; without merging, an hour of downloading
        // would leave a state file with a hundred thousand entries in it.
        let mut ranges = vec![(0, 100), (100, 200), (500, 600), (150, 300)];
        merge(&mut ranges);
        assert_eq!(ranges, vec![(0, 300), (500, 600)]);
    }

    #[test]
    fn the_opening_plan_splits_the_largest_piece_each_time() {
        // A resume with one big gap and one small one, asked for four pieces:
        // the big gap is what gets cut up, because splitting the small one
        // leaves the big one as a straggler.
        let gaps = VecDeque::from(vec![(0, 2 << 20), (10 << 20, 26 << 20)]);
        let plan = plan(gaps, 4, MIN_SPLIT);
        assert_eq!(plan.len(), 4);
        let total: u64 = plan.iter().map(|(a, b)| b - a).sum();
        assert_eq!(total, (2 << 20) + (16 << 20), "no bytes invented or lost");
        assert!(
            plan.iter().all(|(a, b)| b > a),
            "an empty range would spin a connection: {plan:?}"
        );
    }

    #[test]
    fn a_plan_never_splits_past_what_is_worth_it() {
        // Asked for thirty-two pieces of a file that only has room for two.
        let plan = plan(VecDeque::from(vec![(0, 3 << 20)]), 32, MIN_SPLIT);
        assert!(plan.len() <= 3, "split below the useful size: {plan:?}");
        let total: u64 = plan.iter().map(|(a, b)| b - a).sum();
        assert_eq!(total, 3 << 20);
    }

    #[test]
    fn the_size_comes_from_content_range_not_content_length() {
        // The trap in probing with `Range: bytes=0-0`: Content-Length is 1,
        // because that is what the body holds.
        let header = "bytes 0-0/4294967296";
        let total: u64 = header.rsplit('/').next().unwrap().parse().unwrap();
        assert_eq!(total, 4 << 30);
    }

    #[test]
    fn a_filename_survives_the_shapes_servers_send_it_in() {
        assert_eq!(
            filename_from_disposition(r#"attachment; filename="ubuntu 24.04.iso""#).as_deref(),
            Some("ubuntu 24.04.iso")
        );
        // RFC 5987 wins where both are present: it is the only one that can
        // carry a name outside Latin-1, and servers send the plain one as a
        // mangled fallback for clients that cannot read this.
        assert_eq!(
            filename_from_disposition(
                "attachment; filename=\"Naive.iso\"; filename*=UTF-8''Na%C3%AFve.iso"
            )
            .as_deref(),
            Some("Naïve.iso")
        );
        assert_eq!(filename_from_disposition("inline").as_deref(), None);
    }

    #[test]
    fn a_path_in_a_filename_stays_in_the_download_folder() {
        // Content-Disposition is the server's to choose, so it is not trusted:
        // `../` would otherwise write outside the folder the user picked. The
        // leading dot goes too — a server should not get to drop a hidden file
        // into someone's downloads either.
        let name = filename_from_disposition(r#"attachment; filename="../../.bashrc""#).unwrap();
        assert!(!name.contains(".."), "traversal survived sanitising: {name}");
        assert!(!name.starts_with('.'), "a server named a hidden file: {name}");
        assert_eq!(name, "bashrc");
        let windows =
            filename_from_disposition(r#"attachment; filename="..\..\evil.exe""#).unwrap();
        assert_eq!(windows, "evil.exe");
    }

    #[test]
    fn the_bucket_hands_out_a_second_of_bytes_per_second() {
        let limiter = Limiter::new(1000);
        // The bucket starts full, so the first burst is free.
        assert_eq!(limiter.take(1000), Duration::ZERO);
        let wait = limiter.take(1000);
        assert!(
            wait > Duration::from_millis(900) && wait <= Duration::from_secs(1),
            "expected about a second of backpressure, got {wait:?}"
        );
    }

    #[test]
    fn an_unsplittable_response_gets_exactly_one_connection() {
        assert!(!worth_splitting(Some(10 << 20), false, MIN_SPLIT), "no range support");
        assert!(!worth_splitting(None, true, MIN_SPLIT), "unknown length");
        assert!(!worth_splitting(Some(1024), true, MIN_SPLIT), "smaller than one split");
        assert!(worth_splitting(Some(10 << 20), true, MIN_SPLIT));
    }

    #[test]
    fn a_resume_refuses_a_file_that_changed_underneath_it() {
        let dir = std::env::temp_dir().join(format!("mdm-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("thing.iso.mdmdownload");
        let probe = Probe {
            url: "https://example.test/thing.iso".into(),
            size: Some(5000),
            resumable: true,
            filename: "thing.iso".into(),
            mime: String::new(),
            validator: Some("\"abc\"".into()),
        };
        save_state(&part, &probe, &[(0, 1000)]);

        // Same file: the finished ranges are trusted.
        assert_eq!(load_state(&part, &probe), Some(vec![(0, 1000)]));

        // A new ETag means these bytes belong to a different file, and
        // splicing the two would produce something that is neither.
        let changed = Probe { validator: Some("\"xyz\"".into()), ..probe.clone() };
        assert_eq!(load_state(&part, &changed), None);

        // A different length says the same thing without needing an ETag.
        let resized = Probe { size: Some(9999), ..probe.clone() };
        assert_eq!(load_state(&part, &resized), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
