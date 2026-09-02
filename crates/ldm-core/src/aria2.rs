//! JSON-RPC 2.0 client for a local aria2c daemon.
//!
//! Transport is plain HTTP POST to 127.0.0.1. aria2 also offers a WebSocket
//! endpoint that pushes notifications, but the polling model here is simpler,
//! has one fewer dependency, and a 700 ms tick is well under the threshold
//! where a progress bar looks stuttery.

use crate::model::Header;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Fields worth asking for. Requesting a narrow key set keeps aria2's replies
/// small — `tellStatus` with no keys serialises the entire piece bitfield,
/// which for a large download is hundreds of kilobytes per poll.
/// Hard ceiling on connections per server.
///
/// Not a policy choice: aria2's *RPC* validator rejects anything above 16 for
/// `max-connection-per-server`, in both `addUri` and `changeGlobalOption`,
/// answering "We encountered a problem while processing the option". Its
/// *command line* happily accepts 128 and `--help` advertises `1-*`, which
/// makes this easy to get wrong — but LDM drives aria2 entirely over RPC, so
/// 16 is the real limit. `split` has no such cap.
pub const MAX_CONNECTIONS: u8 = 16;

pub const STATUS_KEYS: &[&str] = &[
    "gid",
    "status",
    "totalLength",
    "completedLength",
    "downloadSpeed",
    "connections",
    "errorCode",
    "errorMessage",
    "dir",
    "files",
];

pub struct Aria2 {
    client: reqwest::Client,
    endpoint: String,
    secret: String,
    counter: AtomicU64,
}

/// aria2 reports every integer as a JSON string. This unwraps that.
fn num(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// A download's state as aria2 sees it.
#[derive(Debug, Clone)]
pub struct TaskStatus {
    pub gid: String,
    pub status: String,
    pub total_length: i64,
    pub completed_length: i64,
    pub download_speed: i64,
    pub connections: i64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub dir: String,
    /// Absolute path aria2 chose, which may differ from the name we requested
    /// when the server supplied its own via Content-Disposition.
    pub path: Option<String>,
}

impl TaskStatus {
    fn from_value(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        let path = o
            .get("files")
            .and_then(Value::as_array)
            .and_then(|f| f.first())
            .and_then(|f| f.get("path"))
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_owned);

        let error_code = o
            .get("errorCode")
            .and_then(Value::as_str)
            .filter(|c| *c != "0")
            .map(str::to_owned);

        Some(Self {
            gid: text(o.get("gid")),
            status: text(o.get("status")),
            total_length: num(o.get("totalLength")),
            completed_length: num(o.get("completedLength")),
            download_speed: num(o.get("downloadSpeed")),
            connections: num(o.get("connections")),
            error_code,
            error_message: o
                .get("errorMessage")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_owned),
            dir: text(o.get("dir")),
            path,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlobalStat {
    #[serde(default)]
    pub download_speed: i64,
    #[serde(default)]
    pub num_active: i64,
    #[serde(default)]
    pub num_waiting: i64,
    #[serde(default)]
    pub num_stopped: i64,
}

/// Options for a single `addUri` call.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub dir: String,
    pub out: Option<String>,
    pub headers: Vec<Header>,
    pub referer: Option<String>,
    pub connections: u8,
    pub split: u8,
    pub min_split_size: String,
    pub max_speed: u64,
    pub retry_limit: u8,
    /// Start life paused, so the engine controls when it actually runs.
    pub paused: bool,
    pub extra: Vec<(String, String)>,
}

impl AddOptions {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("dir".into(), json!(self.dir));
        if let Some(out) = &self.out {
            m.insert("out".into(), json!(out));
        }
        if !self.headers.is_empty() {
            let hs: Vec<String> = self.headers.iter().map(Header::to_arg).collect();
            m.insert("header".into(), json!(hs));
        }
        if let Some(r) = &self.referer {
            if !r.is_empty() {
                m.insert("referer".into(), json!(r));
            }
        }
        // aria2 documents this as "1-*"; the ceiling is ours, matching IDM's
        // maximum. Past ~32 the extra sockets buy nothing and invite throttling.
        let conns = self.connections.clamp(1, MAX_CONNECTIONS);
        m.insert("max-connection-per-server".into(), json!(conns.to_string()));
        m.insert("split".into(), json!(self.split.max(1).to_string()));
        m.insert("min-split-size".into(), json!(self.min_split_size.clone()));
        m.insert("max-tries".into(), json!(self.retry_limit.to_string()));
        m.insert("continue".into(), json!("true"));
        // Without this a server that ignores Range silently yields a truncated
        // file; aria2 should fail loudly instead so we can retry unsegmented.
        m.insert("always-resume".into(), json!("false"));
        m.insert("max-resume-failure-tries".into(), json!("2"));
        // falloc is instantaneous on ext4/btrfs/xfs and avoids fragmenting a
        // multi-gigabyte file across 16 concurrently-writing segments.
        m.insert("file-allocation".into(), json!("falloc"));
        m.insert("auto-file-renaming".into(), json!("true"));
        m.insert("allow-overwrite".into(), json!("false"));
        m.insert("remote-time".into(), json!("true"));
        if self.max_speed > 0 {
            m.insert("max-download-limit".into(), json!(self.max_speed.to_string()));
        }
        if self.paused {
            m.insert("pause".into(), json!("true"));
        }
        for (k, v) in &self.extra {
            m.insert(k.clone(), json!(v));
        }
        Value::Object(m)
    }
}

impl Aria2 {
    pub fn new(port: u16, secret: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                // The daemon is on loopback; a pool this small is plenty and
                // keeps file descriptors down.
                .pool_max_idle_per_host(4)
                .build()
                .expect("reqwest client"),
            endpoint: format!("http://127.0.0.1:{port}/jsonrpc"),
            secret: secret.into(),
            counter: AtomicU64::new(1),
        }
    }

    /// Every aria2 method takes the secret as a leading `token:` parameter.
    fn token(&self) -> Value {
        json!(format!("token:{}", self.secret))
    }

    async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut full = vec![self.token()];
        full.extend(params);

        let body = json!({
            "jsonrpc": "2.0",
            "id": id.to_string(),
            "method": method,
            "params": full,
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("aria2 rpc {method}"))?;

        let value: Value = resp
            .json()
            .await
            .with_context(|| format!("decoding aria2 reply for {method}"))?;

        if let Some(err) = value.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("aria2 {method}: {msg}");
        }

        value
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("aria2 {method}: reply had no result"))
    }

    /// Cheap liveness probe used while waiting for the daemon to come up.
    pub async fn version(&self) -> Result<String> {
        let v = self.call("aria2.getVersion", vec![]).await?;
        Ok(text(v.get("version")))
    }

    /// Queue one file, optionally naming several servers that hold it.
    ///
    /// aria2 treats the list as sources for a *single* file, not several
    /// downloads: it spreads its connections across them and drops the ones
    /// that turn out slow or dead. That is what makes a mirrored file arrive
    /// faster than any single-source downloader can manage — and why the
    /// caller passes every mirror it knows of rather than picking one.
    pub async fn add_uri(&self, urls: &[String], opts: &AddOptions) -> Result<String> {
        if urls.is_empty() {
            bail!("no URL to download");
        }
        let v = self
            .call("aria2.addUri", vec![json!(urls), opts.to_json()])
            .await?;
        v.as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("addUri did not return a gid"))
    }

    pub async fn tell_status(&self, gid: &str) -> Result<TaskStatus> {
        let v = self
            .call("aria2.tellStatus", vec![json!(gid), json!(STATUS_KEYS)])
            .await?;
        TaskStatus::from_value(&v).ok_or_else(|| anyhow!("malformed tellStatus reply"))
    }

    /// Active, waiting and stopped downloads in one round trip.
    ///
    /// `system.multicall` matters here: polling three endpoints separately at
    /// 700 ms intervals triples the request rate for no benefit, and the three
    /// replies could disagree with each other mid-transition.
    pub async fn tell_all(&self, stopped_limit: i64) -> Result<Vec<TaskStatus>> {
        let calls = json!([
            { "methodName": "aria2.tellActive", "params": [self.token(), STATUS_KEYS] },
            { "methodName": "aria2.tellWaiting", "params": [self.token(), 0, 1000, STATUS_KEYS] },
            { "methodName": "aria2.tellStopped", "params": [self.token(), 0, stopped_limit, STATUS_KEYS] },
        ]);

        // multicall itself takes no token — the token rides on each inner call.
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id.to_string(),
            "method": "system.multicall",
            "params": [calls],
        });

        let value: Value = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .context("aria2 multicall")?
            .json()
            .await
            .context("decoding aria2 multicall")?;

        let mut out = Vec::new();
        // Shape is [[<result>], [<result>], [<result>]]; a failing inner call
        // yields a fault object instead, which we skip rather than abort on.
        let Some(groups) = value.get("result").and_then(Value::as_array) else {
            return Ok(out);
        };
        for group in groups {
            let Some(list) = group
                .as_array()
                .and_then(|g| g.first())
                .and_then(Value::as_array)
            else {
                continue;
            };
            out.extend(list.iter().filter_map(TaskStatus::from_value));
        }
        Ok(out)
    }

    pub async fn pause(&self, gid: &str) -> Result<()> {
        // force_pause does not wait for the connection to close cleanly, which
        // is what a user clicking "pause" expects.
        self.call("aria2.forcePause", vec![json!(gid)]).await?;
        Ok(())
    }

    pub async fn unpause(&self, gid: &str) -> Result<()> {
        self.call("aria2.unpause", vec![json!(gid)]).await?;
        Ok(())
    }

    pub async fn remove(&self, gid: &str) -> Result<()> {
        // A removed download stays in aria2's stopped list until purged, which
        // would otherwise resurrect it on the next poll.
        let _ = self.call("aria2.forceRemove", vec![json!(gid)]).await;
        let _ = self
            .call("aria2.removeDownloadResult", vec![json!(gid)])
            .await;
        Ok(())
    }

    pub async fn set_global(&self, opts: &[(&str, String)]) -> Result<()> {
        let mut m = Map::new();
        for (k, v) in opts {
            m.insert((*k).to_owned(), json!(v));
        }
        self.call("aria2.changeGlobalOption", vec![Value::Object(m)])
            .await?;
        Ok(())
    }

    pub async fn set_option(&self, gid: &str, opts: &[(&str, String)]) -> Result<()> {
        let mut m = Map::new();
        for (k, v) in opts {
            m.insert((*k).to_owned(), json!(v));
        }
        self.call("aria2.changeOption", vec![json!(gid), Value::Object(m)])
            .await?;
        Ok(())
    }

    pub async fn global_stat(&self) -> Result<GlobalStat> {
        let v = self.call("aria2.getGlobalStat", vec![]).await?;
        let o = v.as_object();
        Ok(GlobalStat {
            download_speed: num(o.and_then(|o| o.get("downloadSpeed"))),
            num_active: num(o.and_then(|o| o.get("numActive"))),
            num_waiting: num(o.and_then(|o| o.get("numWaiting"))),
            num_stopped: num(o.and_then(|o| o.get("numStopped"))),
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.call("aria2.shutdown", vec![]).await;
        Ok(())
    }

    pub async fn save_session(&self) -> Result<()> {
        let _ = self.call("aria2.saveSession", vec![]).await;
        Ok(())
    }
}
