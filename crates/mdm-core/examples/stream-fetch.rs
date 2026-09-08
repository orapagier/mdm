//! Fetch and remux one HLS or DASH stream, without the rest of the app.
//!
//!     cargo run --release --example stream-fetch -- <manifest-url> [dir]
//!
//! The engine path is several moving parts away from the stream code; this is
//! the short way to point the downloader at a real manifest and see what comes
//! out the other end.

use mdm_core::fetch::{Event, Spec};
use mdm_core::human_bytes;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("mdm_core=info"),
    )
    .init();

    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| {
        eprintln!("usage: stream-fetch <manifest-url> [dir]");
        std::process::exit(2);
    });
    let dir = args.next().unwrap_or_else(|| ".".into());

    let mut spec = Spec::new(url.clone(), &dir);
    spec.filename = Some("stream-fetch-out".into());

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();

    let reporter = tokio::spawn(async move {
        let mut last = 0u64;
        while let Some(event) = rx.recv().await {
            match event {
                Event::Progress(p) => {
                    // One line per 8 MiB, so a long stream does not scroll the
                    // interesting parts away.
                    if p.downloaded - last > 8 << 20 {
                        last = p.downloaded;
                        println!(
                            "  {} at {}/s",
                            human_bytes(p.downloaded as i64),
                            human_bytes(p.speed as i64)
                        );
                    }
                }
                Event::Done(path) => println!("  wrote {}", path.display()),
                _ => {}
            }
        }
    });

    let out = mdm_core::stream::download(spec, tx, stop).await?;
    let _ = reporter.await;

    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!(
        "\n{} — {} in {:.1}s",
        out.display(),
        human_bytes(size as i64),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
