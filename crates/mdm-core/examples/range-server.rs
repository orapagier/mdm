//! A range-capable HTTP server on loopback, for testing the fetcher.
//!
//!     cargo run --release --example range-server -- <megabytes> [port] [KB/s]
//!
//! The Python one in `packaging/` needs a Python this project's Windows
//! machines do not have. This serves the same thing — `206 Partial Content`
//! for a `Range` request, `200` otherwise — out of a buffer.
//!
//! The rate limit, when given, is **shared across every connection**, which is
//! the case a connection governor has to get right and a real server will
//! rarely reproduce on demand: the link is full, so a second socket cannot add
//! a byte, and the correct answer is to give the socket back rather than keep
//! it open for the length of the download.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One budget for the whole server, handed out in the order it is asked for.
struct Shared {
    next: Mutex<Instant>,
    per_byte: Duration,
}

impl Shared {
    async fn spend(&self, bytes: usize) {
        if self.per_byte.is_zero() {
            return;
        }
        let until = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            let start = (*next).max(now);
            *next = start + self.per_byte * bytes as u32;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(until)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let arg = |n: usize| std::env::args().nth(n);
    let mb: usize = arg(1).and_then(|v| v.parse().ok()).unwrap_or(64);
    let port: u16 = arg(2).and_then(|v| v.parse().ok()).unwrap_or(8099);
    let kbs: u64 = arg(3).and_then(|v| v.parse().ok()).unwrap_or(0);

    let body: Arc<Vec<u8>> = Arc::new((0..mb * 1024 * 1024).map(|i| (i % 251) as u8).collect());
    let ranged = Arc::new(AtomicUsize::new(0));
    let limit = Arc::new(Shared {
        next: Mutex::new(Instant::now()),
        per_byte: if kbs == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(1.0 / (kbs as f64 * 1024.0))
        },
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!(
        "serving {mb} MB at http://127.0.0.1:{port}/file.bin{}",
        if kbs == 0 { String::new() } else { format!(", capped at {kbs} KB/s in total") }
    );

    loop {
        let (mut sock, _) = listener.accept().await?;
        let body = body.clone();
        let ranged = ranged.clone();
        let limit = limit.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let head_only = head.starts_with("HEAD");
                let range = head.lines().find_map(|l| {
                    l.strip_prefix("Range: bytes=").or_else(|| l.strip_prefix("range: bytes="))
                });
                let total = body.len();
                let (status, from, to) = match range {
                    Some(spec) => {
                        ranged.fetch_add(1, Ordering::Relaxed);
                        let (a, b) = spec.trim().split_once('-').unwrap_or((spec.trim(), ""));
                        let from: usize = a.parse().unwrap_or(0);
                        let to: usize = b.parse().unwrap_or(total - 1).min(total - 1);
                        ("206 Partial Content", from, to)
                    }
                    None => ("200 OK", 0, total - 1),
                };
                let len = to + 1 - from;
                let mut headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {len}\r\n\
                     Content-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\n"
                );
                if range.is_some() {
                    headers.push_str(&format!("Content-Range: bytes {from}-{to}/{total}\r\n"));
                }
                headers.push_str("\r\n");
                if sock.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                if head_only {
                    continue;
                }
                for chunk in body[from..=to].chunks(32 * 1024) {
                    limit.spend(chunk.len()).await;
                    if sock.write_all(chunk).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
}
