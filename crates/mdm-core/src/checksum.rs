//! SHA-256 verification of finished downloads.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Hash a file in 1 MiB chunks so a multi-gigabyte ISO does not need to fit
/// in memory. Runs on a blocking thread — callers are async.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).context("reading file for hashing")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Compare a computed digest against an expected one, case- and
/// whitespace-insensitively.
pub fn matches(computed: &str, expected: &str) -> bool {
    computed.eq_ignore_ascii_case(expected.trim())
}
