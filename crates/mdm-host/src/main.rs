//! Firefox native messaging host for MDM.
//!
//! Firefox launches this process, speaks the length-prefixed JSON protocol on
//! stdin/stdout, and expects nothing else on stdout — a single stray byte
//! desynchronises the stream permanently, so every diagnostic goes to stderr.
//!
//! All this binary does is bridge that protocol to the app's Unix socket,
//! starting the app if it is not already running.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Firefox will not send us anything larger than this, and refusing to
/// allocate on a bogus length header keeps a corrupt stream from OOMing us.
const MAX_MESSAGE: u32 = 64 * 1024 * 1024;

fn main() {
    // stderr goes to the browser's console; stdout is protocol-only.
    eprintln!("[mdm-host] started");

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    let mut app = AppLink::new();

    loop {
        let msg = match read_message(&mut input) {
            Ok(Some(m)) => m,
            Ok(None) => {
                eprintln!("[mdm-host] browser closed the port");
                return;
            }
            Err(e) => {
                eprintln!("[mdm-host] read error: {e}");
                return;
            }
        };

        let reply = app.round_trip(&msg).unwrap_or_else(|e| {
            eprintln!("[mdm-host] {e}");
            // The extension treats a non-accepting reply as "fail open", so a
            // dead app leaves the download with Firefox rather than losing it.
            let id = extract_id(&msg);
            format!(
                r#"{{"accepted":false,"error":{},"id":{}}}"#,
                json_string(&e.to_string()),
                id.map(|i| json_string(&i)).unwrap_or_else(|| "null".into())
            )
        });

        if let Err(e) = write_message(&mut output, reply.as_bytes()) {
            eprintln!("[mdm-host] write error: {e}");
            return;
        }
    }
}

/* ---------------------------------------------------------------------- *
 * Native messaging framing
 * ---------------------------------------------------------------------- */

fn read_message(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    // The length is native-endian by specification, not network order.
    let len = u32::from_ne_bytes(len_buf);
    if len > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len} exceeds maximum"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    input.read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn write_message(output: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    output.write_all(&len.to_ne_bytes())?;
    output.write_all(payload)?;
    output.flush()
}

/* ---------------------------------------------------------------------- *
 * Link to the running app
 * ---------------------------------------------------------------------- */

struct AppLink {
    stream: Option<BufReader<UnixStream>>,
}

/// Why an exchange failed, which decides whether re-sending is safe.
enum Failure {
    /// The connection is unusable and the app never received the message.
    Broken(String),
    /// The message went out but no answer came back.
    Silent(String),
}

impl Failure {
    fn broken(e: io::Error) -> Self {
        Failure::Broken(e.to_string())
    }

    fn into_message(self) -> String {
        match self {
            Failure::Broken(m) | Failure::Silent(m) => m,
        }
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl AppLink {
    fn new() -> Self {
        Self { stream: None }
    }

    /// Send one message and read one reply, reconnecting or launching the app
    /// as needed.
    fn round_trip(&mut self, msg: &[u8]) -> Result<String, String> {
        // A carried-over connection may have died since the last message, in
        // which case the write lands in a dead socket and the read comes back
        // empty. That is the one failure worth retrying: the app never saw the
        // message, so re-sending cannot act on it twice.
        let reused = self.stream.is_some();
        if !reused {
            self.connect(true)?;
        }
        match self.exchange(msg) {
            Ok(reply) => Ok(reply),
            Err(Failure::Silent(e)) => {
                // The app took the message and did not answer. Re-sending
                // would run whatever it was a second time, so report it.
                eprintln!("[mdm-host] no reply: {e}");
                self.stream = None;
                Err(e)
            }
            Err(Failure::Broken(e)) if reused => {
                eprintln!("[mdm-host] connection lost ({e}); reconnecting");
                self.stream = None;
                // No launch here: if the app really is gone the next message
                // starts it, and a 15s wait inside a blocking webRequest
                // listener would hold up the browser for nothing.
                self.connect(false)?;
                self.exchange(msg).map_err(Failure::into_message)
            }
            Err(e) => {
                self.stream = None;
                Err(e.into_message())
            }
        }
    }

    fn exchange(&mut self, msg: &[u8]) -> Result<String, Failure> {
        let reader = self
            .stream
            .as_mut()
            .ok_or_else(|| Failure::Broken("not connected".into()))?;
        {
            let sock = reader.get_mut();
            sock.write_all(msg).map_err(Failure::broken)?;
            sock.write_all(b"\n").map_err(Failure::broken)?;
            sock.flush().map_err(Failure::broken)?;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            // Nothing at all came back: the app is gone and never saw this.
            Ok(0) => Err(Failure::Broken("app closed the connection".into())),
            Ok(_) => Ok(line.trim().to_owned()),
            // A timeout means the app is alive but silent — it may well have
            // acted on the message already.
            Err(e) if is_timeout(&e) => Err(Failure::Silent(format!("MDM did not answer: {e}"))),
            Err(e) => Err(Failure::Broken(e.to_string())),
        }
    }

    fn connect(&mut self, launch_if_absent: bool) -> Result<(), String> {
        let path = socket_path();
        if let Ok(sock) = UnixStream::connect(&path) {
            configure(&sock);
            self.stream = Some(BufReader::new(sock));
            return Ok(());
        }
        if launch_if_absent {
            // Nothing is listening. On the first attempt that usually just
            // means the app is not running yet, so start it.
            launch_app()?;
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(200));
                if let Ok(sock) = UnixStream::connect(&path) {
                    configure(&sock);
                    self.stream = Some(BufReader::new(sock));
                    return Ok(());
                }
            }
            return Err("MDM did not start within 15s".into());
        }
        Err(format!("cannot connect to {}", path.display()))
    }
}

fn configure(sock: &UnixStream) {
    // A hung app must not wedge the browser's blocking listener; the extension
    // gives up well before this, but the host should not linger either — every
    // message behind this one waits for it.
    let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(5)));
}

fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("mdm").join("mdm.sock"),
        _ => std::env::temp_dir()
            .join(format!("mdm-{}", uid()))
            .join("mdm.sock"),
    }
}

fn uid() -> u32 {
    // Avoid a libc dependency in a binary this small.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next().map(str::to_owned))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

/// Start the app detached, so it outlives this host process.
fn launch_app() -> Result<(), String> {
    let exe = find_app().ok_or("the mdm binary could not be located")?;
    eprintln!("[mdm-host] launching {}", exe.display());
    Command::new(&exe)
        .arg("--background")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("launching {}: {e}", exe.display()))?;
    Ok(())
}

fn find_app() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MDM_APP_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // Installed layout puts the host next to the app.
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let sibling = dir.join("mdm");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("mdm"))
            .find(|p| p.is_file())
    })
}

/* ---------------------------------------------------------------------- *
 * Tiny JSON helpers
 *
 * Pulling in a JSON parser to read one field and quote one string would more
 * than double this binary, which the browser spawns on every session.
 * ---------------------------------------------------------------------- */

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Pull `"id":"..."` out of a request so failures can still be correlated.
fn extract_id(msg: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(msg).ok()?;
    let idx = text.find("\"id\"")?;
    let rest = &text[idx + 4..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let quoted = after.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_owned())
}
