//! Per-client-subnet rate limiting (see `main.rs::track_http_metrics`, the
//! one call site) plus an IP/subnet allowlist that bypasses it entirely.
//!
//! Keyed by subnet (`main.rs::client_subnet`), not the exact client IP, for
//! the same reason `client_subnet_requests_total` is: a single abusive
//! caller can trivially rotate through many addresses within its own /24
//! or /64 (IPv6 privacy addresses do this routinely even for ordinary
//! clients), so limiting by exact IP would barely limit anything.

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::time::Duration;

use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use ipnet::IpNet;

pub type SubnetRateLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

/// Bundled so `AppState::new` takes one rate-limiting parameter instead of
/// three positional ones alongside its existing config args.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub per_second: NonZeroU32,
    pub burst: NonZeroU32,
    pub allowlist: Allowlist,
}

impl RateLimitConfig {
    pub fn new_limiter(&self) -> SubnetRateLimiter {
        RateLimiter::dashmap(Quota::per_second(self.per_second).allow_burst(self.burst))
    }

    /// A quota generous enough that no unrelated test could plausibly trip
    /// it, for tests that don't care about rate limiting at all.
    #[cfg(test)]
    pub fn permissive() -> Self {
        Self {
            per_second: NonZeroU32::new(1000).expect("1000 != 0"),
            burst: NonZeroU32::new(1000).expect("1000 != 0"),
            allowlist: Allowlist::default(),
        }
    }
}

/// A parsed `RATE_LIMIT_ALLOWLIST`: IPs and/or CIDR ranges exempt from rate
/// limiting entirely (their requests still count in every other metric,
/// they just never get a 429).
#[derive(Debug, Clone, Default)]
pub struct Allowlist(Vec<IpNet>);

impl Allowlist {
    /// Parses a comma-separated list of bare IPs (`203.0.113.5`,
    /// `2600:1702:7310:20e0::69`) and/or CIDR ranges
    /// (`10.251.254.0/24`, `2600:1702:7310:20e0::/64`). Empty or
    /// whitespace-only entries are skipped, so a trailing comma or extra
    /// whitespace in the env var isn't a hard error.
    pub fn parse(raw: &str) -> Result<Self, String> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Self::parse_entry)
            .collect::<Result<Vec<IpNet>, String>>()
            .map(Allowlist)
    }

    /// A single allowlist entry: either already a CIDR range
    /// (`10.251.254.0/24`) or a bare IP, which becomes its exact-match
    /// `/32`or `/128`.
    fn parse_entry(entry: &str) -> Result<IpNet, String> {
        if let Ok(net) = entry.parse::<IpNet>() {
            return Ok(net);
        }
        let ip: IpAddr = entry
            .parse()
            .map_err(|_| format!("{entry:?} is not a valid IP or CIDR range"))?;
        let prefix_len = if ip.is_ipv4() { 32 } else { 128 };
        Ok(IpNet::new(ip, prefix_len).expect("32/128 is always a valid prefix length"))
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(&ip))
    }
}

/// How long to wait, if `key` is currently rate limited under `limiter` -
/// `None` means the request is allowed (and has been recorded against the
/// limiter).
pub fn check(limiter: &SubnetRateLimiter, key: &str) -> Option<Duration> {
    limiter
        .check_key(&key.to_owned())
        .err()
        .map(|not_until| not_until.wait_time_from(DefaultClock::default().now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_parses_bare_ips_and_cidr_ranges() {
        let list =
            Allowlist::parse("203.0.113.5, 10.251.254.0/24 ,2600:1702:7310:20e0::/64").unwrap();
        assert!(list.contains("203.0.113.5".parse().unwrap()));
        assert!(list.contains("10.251.254.20".parse().unwrap()));
        assert!(list.contains("2600:1702:7310:20e0::69".parse().unwrap()));
        assert!(!list.contains("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn allowlist_skips_empty_entries() {
        let list = Allowlist::parse(" , 203.0.113.5/32,").unwrap();
        assert!(list.contains("203.0.113.5".parse().unwrap()));
    }

    #[test]
    fn allowlist_rejects_garbage() {
        assert!(Allowlist::parse("not-an-ip").is_err());
    }

    #[test]
    fn empty_allowlist_contains_nothing() {
        let list = Allowlist::default();
        assert!(!list.contains("203.0.113.5".parse().unwrap()));
    }

    fn test_config(per_second: u32, burst: u32) -> RateLimitConfig {
        RateLimitConfig {
            per_second: NonZeroU32::new(per_second).unwrap(),
            burst: NonZeroU32::new(burst).unwrap(),
            allowlist: Allowlist::default(),
        }
    }

    #[test]
    fn check_allows_up_to_burst_then_rejects() {
        let limiter = test_config(1, 2).new_limiter();
        assert_eq!(check(&limiter, "203.0.113.0/24"), None);
        assert_eq!(check(&limiter, "203.0.113.0/24"), None);
        assert!(check(&limiter, "203.0.113.0/24").is_some());
    }

    #[test]
    fn check_tracks_keys_independently() {
        let limiter = test_config(1, 1).new_limiter();
        assert_eq!(check(&limiter, "203.0.113.0/24"), None);
        assert!(check(&limiter, "203.0.113.0/24").is_some());
        // A different key has its own untouched bucket.
        assert_eq!(check(&limiter, "198.51.100.0/24"), None);
    }
}
