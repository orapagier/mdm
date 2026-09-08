//! What the app's own resolver and connector make of a host.
//!
//!     cargo run --release --example net-check -- <host> [port]
//!
//! "curl reaches it and the app does not" is a claim about two different
//! resolvers and two different connect paths. This prints ours, so the two can
//! be compared instead of argued about.

use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "example.com".into());
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(443);

    let started = Instant::now();
    let addrs = mdm_core::resolve::addresses(&host, port).await?;
    println!("resolved {host}:{port} in {:?}", started.elapsed());
    for a in &addrs {
        println!("  {a}");
    }

    for a in &addrs {
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::net::TcpStream::connect(a),
        )
        .await;
        match result {
            Ok(Ok(_)) => println!("  connect {a}: ok in {:?}", started.elapsed()),
            Ok(Err(e)) => println!("  connect {a}: failed in {:?} — {e}", started.elapsed()),
            Err(_) => println!("  connect {a}: timed out after {:?}", started.elapsed()),
        }
    }
    // Where TCP succeeds but the request does not, the difference is TLS or
    // the request shape, and this narrows it to one of them.
    let url = format!("https://{host}/");
    for (label, build) in [
        ("plain reqwest", 0u8),
        ("http1_only", 1),
        ("http1_only + browser UA", 2),
    ] {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(25));
        if build >= 1 {
            b = b.http1_only();
        }
        if build >= 2 {
            b = b.user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:143.0) Gecko/20100101 Firefox/143.0",
            );
        }
        let client = b.build()?;
        let started = Instant::now();
        match client.get(&url).send().await {
            Ok(r) => println!("  {label}: {} in {:?}", r.status(), started.elapsed()),
            Err(e) => println!("  {label}: FAILED in {:?} — {e}", started.elapsed()),
        }
    }
    Ok(())
}
