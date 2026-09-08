//! Name resolution with a budget of its own.
//!
//! reqwest's `connect_timeout` covers DNS *and* the TCP handshake together,
//! which is fine until the resolver is slow. On a machine whose DNS
//! intermittently times out — and `nslookup` on the machine this was written
//! for prints "DNS request timed out" before succeeding — two stalled lookups
//! exhaust the whole budget, the connection is reported as a TCP failure that
//! never happened, and every retry pays the same cost again. The download then
//! fails against a server that `curl` reaches in a quarter of a second.
//!
//! So the lookup happens here instead, with its own per-attempt timeout, its
//! own total budget, and a short cache — and the addresses are handed to
//! reqwest ready-made, leaving `connect_timeout` to mean what it says.
//!
//! The second half is address *choice*. A host with an AAAA record on a
//! network with no IPv6 route is a connection that cannot succeed, and the
//! stack will try it before falling back. Where this machine has no route, the
//! v6 addresses are dropped rather than attempted.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long one lookup may take before it is abandoned and retried.
///
/// Short, because the failure being worked around is a lookup that hangs
/// rather than one that answers slowly: a resolver that has not replied in
/// five seconds is usually not about to.
const ATTEMPT: Duration = Duration::from_secs(5);

/// How long resolution may take in total, across attempts.
const BUDGET: Duration = Duration::from_secs(20);

/// How long a successful answer is reused.
///
/// Long enough that the thousands of segment requests in one stream, and the
/// retries after a dropped connection, do not each re-pay a lookup; short
/// enough that a CDN moving its addresses is picked up within the life of a
/// download rather than at the next launch.
const CACHE_TTL: Duration = Duration::from_secs(60);

type Cache = Mutex<HashMap<(String, u16), (Vec<SocketAddr>, Instant)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether this machine can reach the IPv6 internet at all.
///
/// A connected UDP socket sends nothing — it only asks the routing table
/// whether the destination is reachable — so this costs one syscall and no
/// traffic. The answer is cached for the life of the process: a machine does
/// not usually gain or lose IPv6 while a download is running, and being wrong
/// for one session is a far smaller cost than probing on every connection.
fn ipv6_usable() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        let probe = || -> std::io::Result<()> {
            let socket = std::net::UdpSocket::bind("[::]:0")?;
            // A well-known address that is not contacted, only routed to.
            socket.connect("[2001:4860:4860::8888]:53")
        };
        let usable = probe().is_ok();
        if !usable {
            log::debug!("no IPv6 route; AAAA addresses will be skipped");
        }
        usable
    })
}

/// Put the addresses in the order they should actually be tried.
///
/// Where there is no IPv6 route its addresses are removed outright rather than
/// ranked last: leaving them in means every connection waits for them to fail
/// first, which is the stall this module exists to remove. If a host offers
/// *nothing* else they are kept, because an address that probably will not
/// work still beats no address and a certain failure.
fn prefer_reachable(mut addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    if ipv6_usable() {
        return addrs;
    }
    let v4: Vec<SocketAddr> = addrs.iter().copied().filter(|a| a.is_ipv4()).collect();
    if !v4.is_empty() {
        return v4;
    }
    addrs.dedup();
    addrs
}

/// Resolve a host, with a timeout that belongs to the lookup rather than to
/// the connection that follows it.
pub async fn addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    // A literal address is already the answer, and asking the resolver about
    // one is a needless round trip that can itself hang.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let key = (host.to_ascii_lowercase(), port);
    if let Ok(cache) = cache().lock() {
        if let Some((addrs, at)) = cache.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(addrs.clone());
            }
        }
    }

    let started = Instant::now();
    let mut attempt = 0u32;
    let mut last: Option<String> = None;
    while started.elapsed() < BUDGET {
        attempt += 1;
        let query = format!("{host}:{port}");
        match tokio::time::timeout(ATTEMPT, tokio::net::lookup_host(query)).await {
            Ok(Ok(found)) => {
                let addrs = prefer_reachable(found.collect());
                if addrs.is_empty() {
                    last = Some("the name resolved to no usable address".into());
                    continue;
                }
                if attempt > 1 {
                    log::debug!("resolved {host} on attempt {attempt}");
                }
                if let Ok(mut cache) = cache().lock() {
                    cache.insert(key, (addrs.clone(), Instant::now()));
                }
                return Ok(addrs);
            }
            Ok(Err(e)) => {
                // A name that does not exist will not start existing; only a
                // timeout is worth another go.
                return Err(e).with_context(|| format!("resolving {host}"));
            }
            Err(_) => {
                last = Some(format!("no answer within {}s", ATTEMPT.as_secs()));
                log::debug!("dns attempt {attempt} for {host} timed out");
            }
        }
    }

    bail!(
        "could not resolve {host} ({}) — the DNS server is not answering",
        last.unwrap_or_else(|| "timed out".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(n: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)), 443)
    }

    fn v6() -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 443)
    }

    #[tokio::test]
    async fn a_literal_address_needs_no_lookup() {
        // Also the case that must not hang when DNS is down.
        let addrs = addresses("127.0.0.1", 8080).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)]);

        let addrs = addresses("::1", 8080).await.unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080)]);
    }

    #[test]
    fn v6_addresses_are_dropped_only_when_something_else_remains() {
        // The ordering logic, exercised directly: whatever this machine's own
        // IPv6 situation is, a list that is v6-only must survive it.
        let only_v6 = prefer_reachable(vec![v6()]);
        assert_eq!(only_v6, vec![v6()], "a v6-only host must still be attempted");

        let mixed = prefer_reachable(vec![v6(), v4(1)]);
        assert!(!mixed.is_empty());
        if !ipv6_usable() {
            assert_eq!(mixed, vec![v4(1)], "with no route, v6 is not worth trying");
        } else {
            assert_eq!(mixed.len(), 2, "with a route, nothing is thrown away");
        }
    }

    #[tokio::test]
    async fn a_name_that_cannot_exist_fails_rather_than_retrying_to_the_budget() {
        // `.invalid` is reserved precisely so it never resolves. The point is
        // the clock: NXDOMAIN must come back promptly, not after 20 seconds of
        // retries, because the retry loop above is for *timeouts* only.
        let started = Instant::now();
        let result = addresses("nothing.here.invalid", 443).await;
        assert!(result.is_err());
        assert!(
            started.elapsed() < BUDGET,
            "a non-existent name took the whole budget: {:?}",
            started.elapsed()
        );
    }
}
