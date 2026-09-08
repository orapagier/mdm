//! Compare connection counts on a real link, honestly.
//!
//!     cargo run --release --example fetch-bench -- <url> [rounds]
//!
//! Runs each variant once per round, in rotation, and reports medians. On a
//! link whose capacity wanders — which is most links, and certainly a shared
//! or wireless one — running all of A then all of B measures the weather as
//! much as the variants: whichever went second gets whatever the line was
//! doing later. Interleaving spreads that drift across all of them, and the
//! median throws out the round where something else grabbed the pipe.

use mdm_core::fetch::{self, Concurrency, Event, Spec};
use mdm_core::human_bytes;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

fn sha256(path: &std::path::Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| {
        eprintln!("usage: fetch-bench <url> [rounds]");
        std::process::exit(2);
    });
    let rounds: usize = args.next().and_then(|r| r.parse().ok()).unwrap_or(3);

    let variants: Vec<(&str, Concurrency)> = vec![
        ("1 connection", Concurrency::Fixed(1)),
        ("4 connections", Concurrency::Fixed(4)),
        ("8 connections", Concurrency::Fixed(8)),
        ("16 connections", Concurrency::Fixed(16)),
        ("auto", Concurrency::Auto { max: 32 }),
    ];
    let mut results: Vec<(String, Vec<f64>)> =
        variants.iter().map(|(n, _)| (n.to_string(), Vec::new())).collect();

    let dir = std::env::current_dir()?;
    // The first successful download sets the reference every later one is
    // checked against.
    let mut expected: Option<String> = None;
    for round in 1..=rounds {
        for (index, (name, concurrency)) in variants.iter().enumerate() {
            // Each run starts clean: a leftover partial file would let a later
            // variant resume someone else's work and look impossibly fast.
            for entry in std::fs::read_dir(&dir)?.flatten() {
                let path = entry.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if name.starts_with("bench-") {
                    std::fs::remove_file(&path).ok();
                }
            }

            let mut spec = Spec::new(url.clone(), &dir);
            spec.filename = Some(format!("bench-{index}"));
            spec.concurrency = *concurrency;

            let (tx, mut rx) = tokio::sync::mpsc::channel(256);
            let stop = Arc::new(AtomicBool::new(false));
            let started = Instant::now();
            let task = tokio::spawn(fetch::download(spec, tx, stop));

            let mut peak_connections = 0u64;
            let mut settled = None;
            while let Some(event) = rx.recv().await {
                match event {
                    Event::Progress(p) => peak_connections = peak_connections.max(p.connections),
                    Event::Concurrency { connections, .. } => settled = Some(connections),
                    _ => {}
                }
            }

            match task.await? {
                Ok(path) => {
                    let elapsed = started.elapsed().as_secs_f64();
                    let bytes = std::fs::metadata(&path)?.len();
                    let rate = bytes as f64 / elapsed;
                    results[index].1.push(rate);
                    // Every variant must produce the same bytes. A connection
                    // count that is fast and wrong is not a faster download,
                    // and reassembly bugs show up exactly here — under the
                    // settings that split the file the most.
                    let digest = sha256(&path)?;
                    let verdict = match &expected {
                        Some(known) if *known != digest => " ** HASH MISMATCH **",
                        Some(_) => "",
                        None => {
                            expected = Some(digest.clone());
                            " (reference hash)"
                        }
                    };
                    let note = settled
                        .map(|c| format!(" (settled on {c})"))
                        .unwrap_or_default();
                    println!(
                        "round {round}  {name:<15} {:>7.1}s  {:>10}/s  peak {peak_connections} conns{note}{verdict}",
                        elapsed,
                        human_bytes(rate as i64),
                    );
                    std::fs::remove_file(path).ok();
                }
                Err(e) => println!("round {round}  {name:<15} FAILED: {e:#}"),
            }
        }
    }

    if let Some(digest) = &expected {
        // Printed so it can be checked against curl, or the publisher's own
        // published checksum: "all my variants agree" only proves they share a
        // bug, until one of them is compared with something else.
        println!("\nsha256 (identical across every variant): {digest}");
    }
    println!("\nmedian throughput over {rounds} rounds:");
    let mut ranked: Vec<(String, f64)> = results
        .into_iter()
        .filter(|(_, rates)| !rates.is_empty())
        .map(|(name, mut rates)| {
            rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
            // A true median: indexing the midpoint of an even-length list
            // picks the higher of the middle pair, which on two rounds means
            // reporting the better run and calling it typical.
            let mid = rates.len() / 2;
            let median = if rates.len() % 2 == 0 {
                (rates[mid - 1] + rates[mid]) / 2.0
            } else {
                rates[mid]
            };
            (name, median)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let best = ranked.first().map(|(_, r)| *r).unwrap_or(1.0);
    for (name, rate) in &ranked {
        println!(
            "  {name:<15} {:>10}/s   {:.0}% of best",
            human_bytes(*rate as i64),
            rate / best * 100.0
        );
    }
    Ok(())
}
