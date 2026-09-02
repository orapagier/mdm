//! Clipboard monitoring, the way IDM offers to grab a URL you just copied.
//!
//! Wayland gets an event-driven watcher via `wl-paste --watch`, so nothing runs
//! between copies. X11 has no equivalent without an extra helper binary, so it
//! falls back to polling — which is why this is opt-in rather than default-on.

use crate::supervisor::which;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// How often the X11 fallback re-reads the selection.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Longest clipboard value we will look at. Pasting a large document should
/// not have us scanning megabytes on every copy.
const MAX_LEN: usize = 8 * 1024;

/// Start watching. Sends each newly-copied URL exactly once.
///
/// Returns `false` when no clipboard tool is available, so the caller can tell
/// the user why the feature is inert instead of failing silently.
///
/// Takes an explicit runtime handle rather than calling `tokio::spawn`: this is
/// invoked from Tauri's setup hook, which runs outside any Tokio runtime, and a
/// bare spawn there panics the process before the window ever appears.
pub fn watch(runtime: &tokio::runtime::Handle, tx: mpsc::Sender<String>) -> bool {
    if which("wl-paste").is_some() {
        runtime.spawn(watch_wayland(tx));
        return true;
    }
    if which("xclip").is_some() || which("xsel").is_some() {
        runtime.spawn(poll_x11(tx));
        return true;
    }
    log::warn!("clipboard watching needs wl-clipboard (Wayland) or xclip/xsel (X11)");
    false
}

/// `wl-paste --watch CMD` pipes each new clipboard value to CMD's stdin. We
/// have it echo the value followed by a NUL so records stay separable even
/// when the copied text itself contains newlines.
async fn watch_wayland(tx: mpsc::Sender<String>) {
    loop {
        let mut child = match Command::new("wl-paste")
            .args([
                "--watch",
                "sh",
                "-c",
                r#"cat; printf '\0'"#,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("could not start wl-paste --watch: {e}");
                return;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            return;
        };
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        let mut last = String::new();

        loop {
            buf.clear();
            match reader.read_until(0, &mut buf).await {
                Ok(0) => break, // wl-paste exited; restart it below
                Ok(_) => {}
                Err(e) => {
                    log::debug!("clipboard read failed: {e}");
                    break;
                }
            }
            if buf.last() == Some(&0) {
                buf.pop();
            }
            if buf.len() > MAX_LEN {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&buf) else {
                continue;
            };
            if let Some(url) = extract_url(text) {
                if url != last {
                    last = url.clone();
                    if tx.send(url).await.is_err() {
                        return; // receiver gone: app shutting down
                    }
                }
            }
        }

        // The compositor can restart, taking wl-paste with it. Back off a
        // little rather than spinning on a persistent failure.
        let _ = child.kill().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn poll_x11(tx: mpsc::Sender<String>) {
    let mut last = String::new();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let Some(text) = read_x11_clipboard().await else {
            continue;
        };
        if text.len() > MAX_LEN {
            continue;
        }
        if let Some(url) = extract_url(&text) {
            if url != last {
                last = url.clone();
                if tx.send(url).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn read_x11_clipboard() -> Option<String> {
    for (bin, args) in [
        ("xclip", vec!["-selection", "clipboard", "-o"]),
        ("xsel", vec!["--clipboard", "--output"]),
    ] {
        if which(bin).is_none() {
            continue;
        }
        if let Ok(out) = Command::new(bin).args(&args).output().await {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

/// Pull a downloadable URL out of copied text.
///
/// Only accepts text that is *entirely* one URL. Scanning prose for embedded
/// links would fire constantly while a user copies ordinary text.
pub fn extract_url(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > 4096 {
        return None;
    }
    if trimmed.split_whitespace().count() != 1 {
        return None;
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return None;
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return None;
    }
    // The original text, not `parsed.as_str()`: the url crate normalises, and
    // handing aria2 anything other than exactly what the user copied risks
    // breaking signed URLs whose signature covers the literal path.
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::extract_url;

    #[test]
    fn accepts_a_bare_url() {
        assert_eq!(
            extract_url("  https://example.com/a.zip \n"),
            Some("https://example.com/a.zip".into())
        );
    }

    #[test]
    fn rejects_prose_containing_a_url() {
        assert_eq!(extract_url("see https://example.com/a.zip for details"), None);
    }

    #[test]
    fn rejects_non_http_schemes_and_plain_text() {
        assert_eq!(extract_url("ftp://example.com/x"), None);
        assert_eq!(extract_url("magnet:?xt=urn:btih:abc"), None);
        assert_eq!(extract_url("just some text"), None);
        assert_eq!(extract_url(""), None);
    }

    #[test]
    fn rejects_a_url_with_no_host() {
        assert_eq!(extract_url("https://"), None);
        assert_eq!(extract_url("http://"), None);
    }

    #[test]
    fn a_copied_url_is_passed_through_verbatim() {
        // Not normalised: a presigned URL's signature covers the exact path,
        // so re-encoding it would invalidate the download.
        let signed = "https://s3.example.com/b/k.zip?X-Amz-Signature=a%2Fb%3Dc";
        assert_eq!(extract_url(signed), Some(signed.to_string()));
    }
}

