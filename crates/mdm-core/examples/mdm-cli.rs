//! Headless driver for the engine — the app without the window.
//!
//! Useful for testing the aria2 path in isolation, and for scripting:
//!
//!     cargo run --example mdm-cli -- https://example.com/big.iso
//!
//! Honours the same XDG variables as the app, so pointing XDG_DATA_HOME at a
//! scratch directory gives a throwaway database and session.

use mdm_core::engine::{job_from_url, Engine};
use mdm_core::model::Status;
use mdm_core::{config, human_bytes};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("mdm_cli=info,mdm_core=info"),
    )
    .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // --probe inspects a streaming page without downloading, which is how the
    // app's quality picker is populated.
    if args.first().map(String::as_str) == Some("--probe") {
        let url = args.get(1).cloned().unwrap_or_default();
        if url.is_empty() {
            eprintln!("usage: mdm-cli --probe <page-url>");
            std::process::exit(2);
        }
        let info = mdm_core::ytdlp::probe(&url, Some("firefox"), &[], true, &[]).await?;
        println!("title    {}", info.title);
        println!("site     {}", info.extractor);
        if let Some(d) = info.duration {
            println!("duration {:.0}s", d);
        }
        println!("formats  {}", info.formats.len());
        for f in info.formats.iter().rev().take(12) {
            let kind = match (f.vcodec.as_str(), f.acodec.as_str()) {
                ("none", _) => "audio only",
                (_, "none") => "video only",
                _ => "video+audio",
            };
            println!(
                "  {:<9} {:<12} {:<12} {}",
                f.format_id,
                f.resolution,
                kind,
                f.filesize.map(human_bytes).unwrap_or_else(|| "—".into())
            );
        }
        return Ok(());
    }

    let urls: Vec<String> = args.drain(..).collect();
    if urls.is_empty() {
        eprintln!("usage: mdm-cli <url> [url...]   |   mdm-cli --probe <page-url>");
        std::process::exit(2);
    }

    let mut settings = config::load();
    // A CLI run should not inherit the app's category sorting; put files where
    // the caller is standing.
    settings.categorize = false;
    settings.download_dir = std::env::current_dir()?.to_string_lossy().into_owned();

    let engine = Engine::start(settings).await?;
    println!("engine up, aria2 running");

    let mut ids = Vec::new();
    for url in &urls {
        let id = engine.submit(job_from_url(url)).await?;
        println!("  #{id}  {url}");
        ids.push(id);
    }

    let started = Instant::now();
    let mut last_line = String::new();

    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snap = engine.snapshot()?;

        let tracked: Vec<_> = snap
            .downloads
            .iter()
            .filter(|d| ids.contains(&d.id))
            .collect();

        if tracked.iter().all(|d| d.status.is_terminal()) {
            println!();
            for d in &tracked {
                match d.status {
                    Status::Complete => println!(
                        "done  {}  {}  in {:.1}s",
                        d.filename,
                        human_bytes(d.total_bytes),
                        started.elapsed().as_secs_f64()
                    ),
                    _ => println!(
                        "FAIL  {}  {}",
                        d.filename,
                        d.error.as_deref().unwrap_or("unknown error")
                    ),
                }
            }
            engine.shutdown().await;
            let failed = tracked.iter().any(|d| d.status != Status::Complete);
            std::process::exit(if failed { 1 } else { 0 });
        }

        // Single status line, redrawn in place.
        let line = tracked
            .iter()
            .map(|d| {
                format!(
                    "{} {:.1}% {} ({} conns)",
                    d.filename,
                    d.progress() * 100.0,
                    if d.download_speed > 0 {
                        format!("{}/s", human_bytes(d.download_speed))
                    } else {
                        d.status.as_str().to_string()
                    },
                    d.connections
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");

        if line != last_line {
            print!("\r\x1b[K{line}");
            use std::io::Write;
            std::io::stdout().flush().ok();
            last_line = line;
        }
    }
}
