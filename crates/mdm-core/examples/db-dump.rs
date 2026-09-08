//! Print what the app's database actually holds, for debugging a bad capture.
//!
//!     cargo run --release --example db-dump -- [n]
//!
//! Reads the live database read-only, so it is safe to run while the app is
//! up. "What did it actually download, and from what URL" is nearly always the
//! first question when a download turns out to be the wrong file, and guessing
//! at it from the UI loses exactly the fields that matter — the full URL, the
//! MIME the server sent, and whether an extractor was involved.

use mdm_core::store::Store;

fn main() -> anyhow::Result<()> {
    let limit: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(15);

    let store = Store::open()?;
    for d in store.list(500)?.into_iter().take(limit) {
        println!("#{} [{}] {}", d.id, d.status.as_str(), d.filename);
        println!("    url      {}", d.url);
        println!(
            "    mime     {:?}   yt-dlp {}   bytes {}/{}",
            d.mime, d.use_ytdlp, d.completed_bytes, d.total_bytes
        );
        if !d.directory.is_empty() {
            println!("    dir      {}", d.directory);
        }
        if let Some(f) = &d.format_id {
            println!("    format   {f}");
        }
        if let Some(e) = &d.error {
            println!("    error    {e}");
        }
        if !d.referrer.is_empty() {
            println!("    referrer {}", d.referrer);
        }
        println!();
    }
    Ok(())
}
