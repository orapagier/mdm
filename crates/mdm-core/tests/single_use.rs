//! Hosts that hand out addresses good for one request, against a server that
//! really does that.
//!
//! The behaviour under test is a *reversal*. A host whose address had once
//! answered a capture with a landing page went on a list, and every capture
//! from it afterwards was declined on sight and left to the browser. Which is
//! right about an address that is genuinely spent and wrong about every other
//! reason a page comes back — a cooldown between downloads, a request without
//! the referrer the host wants, a CDN node that had not heard of the token —
//! and one unlucky download was enough to put a host on the list for good. The
//! complaint it produced is the plainest kind there is: the download manager
//! does not manage the download.
//!
//! So the list no longer decides; it only says who to ask carefully. The
//! asking is [`Engine::preempt`], which opens the connection, decides from the
//! response rather than from the address, and downloads on that same
//! connection when a file comes back. These two cases are the fork in it:
//!
//!   * an address that answers -- MDM takes the download, and the extension
//!     cancels the browser's copy
//!   * an address already spent -- MDM declines, and the browser's own
//!     request, which is the one holding the file, keeps it
//!
//! Over loopback rather than the network, so this is an ordinary test.

use mdm_core::engine::Engine;
use mdm_core::model::{Job, Settings, Status};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Big enough that the fetcher would want several connections for it, so the
/// held-connection path is exercised rather than sidestepped by a file small
/// enough to arrive in one read.
const SIZE: usize = 512 * 1024;

/// What this host says to anyone arriving without a live token: a page, 200,
/// and no hint in the status line that anything is wrong. Being indistinguishable
/// from success at the HTTP level is the whole difficulty.
const LANDING: &str = "<!doctype html><title>Download</title><h1>Link expired</h1>";

/// A file host that spends its links, and one that only looks like it.
///
/// `/once/<anything>` answers the first request with the file and every
/// request after it with the landing page. `/always/<anything>` answers every
/// request with the file — the host that was put on the list by one bad
/// capture and has been refused ever since.
async fn spawn_host() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let hits = counted.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();

                // A range request is one connection of a multi-part download,
                // and it is not a fresh visit to the address. Counting it as
                // one would have this server spend its own link on the
                // download it had just handed over.
                let ranged = head.to_ascii_lowercase().contains("\nrange:");
                let spent = if path.starts_with("/once/") && !ranged {
                    hits.fetch_add(1, Ordering::SeqCst) > 0
                } else {
                    false
                };

                let response = if spent {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{LANDING}",
                        LANDING.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Disposition: attachment; filename=\"payload.bin\"\r\n\
                         Accept-Ranges: bytes\r\nContent-Length: {SIZE}\r\n\
                         Connection: close\r\n\r\n"
                    )
                };
                if sock.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if !spent {
                    let _ = sock.write_all(&vec![b'M'; SIZE]).await;
                }
                let _ = sock.flush().await;
            });
        }
    });

    (format!("http://{addr}"), hits)
}

/// A capture as the extension sends one: the browser has already asked, so
/// `spends_the_link` is true and this is exactly the job that used to be
/// declined without being tried.
fn capture(url: &str) -> Job {
    serde_json::from_value(serde_json::json!({
        "url": url,
        "source": "downloads",
        "reason": "downloads API",
    }))
    .expect("a job with a url and a source is a valid job")
}

/// Wait for a row to settle, so a test never hangs on one that will not.
async fn settle(engine: &Arc<Engine>, id: i64) -> Status {
    for _ in 0..300 {
        if let Some(d) = engine.download(id) {
            if d.status.is_terminal() {
                return d.status;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("#{id} never finished");
}

/// One test, not two: the engine reads its directories out of the environment,
/// and two tests setting those in one process would race each other.
#[tokio::test]
async fn a_single_use_host_is_asked_rather_than_assumed_about() {
    let tmp = std::env::temp_dir().join(format!("mdm-single-use-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    // Every directory the engine might touch, so the test cannot reach the
    // database or the settings of whoever is running it.
    for var in ["XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"] {
        std::env::set_var(var, &tmp);
    }

    let (base, hits) = spawn_host().await;

    let mut settings = Settings::default();
    settings.download_dir = tmp.to_string_lossy().into_owned();
    settings.categorize = false;
    // The host is on the list, which is the state this whole test is about.
    settings.single_use_hosts = vec!["127.0.0.1".into()];
    let engine = Engine::start(settings).await.expect("engine starts");

    /* The host that only looked single-use. This is the case that used to be
     * refused for good, and it is the one people notice. */
    let id = engine
        .preempt(capture(&format!("{base}/always/token-a")))
        .await
        .expect("preempt does not error on a live address")
        .expect("a live address is a download, not a page");
    assert_eq!(
        settle(&engine, id).await,
        Status::Complete,
        "the held connection did not finish the file"
    );
    let landed = engine.download(id).expect("the row is still there");
    assert_eq!(
        std::fs::metadata(std::path::Path::new(&landed.directory).join(&landed.filename))
            .expect("the file is on disk")
            .len(),
        SIZE as u64,
        "the file on disk is not the file the server sent"
    );

    /* And the host that really does spend its links. The browser has already
     * made the request -- that is what a capture is -- so MDM's is the second,
     * and the second gets the landing page. Declining is the whole of the
     * correct behaviour here: the browser is still holding the file. */
    let url = format!("{base}/once/token-b");
    let spend = reqwest::get(&url).await.expect("the browser's own request");
    assert_eq!(spend.status(), 200);
    let _ = spend.bytes().await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the server did not count the first request");

    let verdict = engine.preempt(capture(&url)).await.expect("preempt does not error on a page");
    assert!(
        verdict.is_none(),
        "a spent address was taken off the browser, which is the one way to lose the file"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
