//! Rate budget: tracks Railway's hourly API quota and derives a polling backstop.

use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;

pub const DEFAULT_HOURLY_LIMIT: u32 = 100;

/// Longest block honoured from a server-supplied Retry-After.
const MAX_BLOCK_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    Default,
    Detected,
    Configured,
}

#[derive(Debug, Clone)]
pub struct RateBudget {
    configured: Option<u32>,
    detected: Option<u32>,
    remaining: Option<u32>,
    blocked_until: Option<Instant>,
}

impl RateBudget {
    pub fn new(configured_limit: Option<u32>) -> Self {
        Self {
            configured: configured_limit,
            detected: None,
            remaining: None,
            blocked_until: None,
        }
    }

    pub fn observe(&mut self, headers: &HeaderMap, now: Instant) {
        if let Some(limit) = parse_hourly_limit(headers) {
            self.detected = Some(limit);
        }
        if let Some(rem) = header_str(headers, "x-ratelimit-remaining").and_then(|s| s.parse().ok())
        {
            self.remaining = Some(rem);
        }
        // Only the seconds form of Retry-After is understood; an HTTP-date is ignored.
        if let Some(secs) = header_str(headers, "retry-after").and_then(|s| s.parse::<u64>().ok()) {
            let block = Duration::from_secs(secs.min(MAX_BLOCK_SECS));
            self.blocked_until = now.checked_add(block);
        }
    }

    pub fn hourly_limit(&self) -> (u32, LimitSource) {
        if let Some(c) = self.configured {
            (c, LimitSource::Configured)
        } else if let Some(d) = self.detected {
            (d, LimitSource::Detected)
        } else {
            (DEFAULT_HOURLY_LIMIT, LimitSource::Default)
        }
    }

    pub fn remaining(&self) -> Option<u32> {
        self.remaining
    }

    pub fn blocked_until(&self) -> Option<Instant> {
        self.blocked_until
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

/// Reads `RateLimit-Policy` (e.g. `10;w=1, 1000;w=3600`) and returns the quota
/// of the entry with a one-hour window.
pub fn parse_hourly_limit(headers: &HeaderMap) -> Option<u32> {
    header_str(headers, "ratelimit-policy")?
        .split(',')
        .find_map(|entry| {
            let parts: Vec<&str> = entry.split(';').map(str::trim).collect();
            if !parts.contains(&"w=3600") {
                return None;
            }
            // IETF draft form carries `q=<quota>`; the bare form leads with the quota.
            parts
                .iter()
                .find_map(|p| p.strip_prefix("q=").and_then(|q| q.parse().ok()))
                .or_else(|| parts.first()?.parse().ok())
        })
}

/// Polling backstop: a configured value wins (minimum 1s); otherwise spread
/// ten percent of the hourly quota evenly, clamped to 15..=600 seconds.
pub fn backstop_interval(hourly_limit: u32, configured_secs: Option<u64>) -> Duration {
    if let Some(s) = configured_secs {
        return Duration::from_secs(s.max(1));
    }
    let secs = (36_000 / u64::from(hourly_limit.max(1))).clamp(15, 600);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn policy_header_hourly_entry() {
        let h = hm(&[("RateLimit-Policy", "10;w=1, 1000;w=3600")]);
        assert_eq!(parse_hourly_limit(&h), Some(1000));
    }

    #[test]
    fn policy_header_q_form() {
        let h = hm(&[(
            "RateLimit-Policy",
            "\"burst\";q=10;w=1, \"hourly\";q=1000;w=3600",
        )]);
        assert_eq!(parse_hourly_limit(&h), Some(1000));
    }

    #[test]
    fn huge_retry_after_is_clamped() {
        let now = Instant::now();
        let mut b = RateBudget::new(None);
        b.observe(&hm(&[("Retry-After", "18446744073709551615")]), now);
        let until = b.blocked_until().unwrap();
        assert!(until <= now + Duration::from_secs(3600));
    }

    #[test]
    fn no_policy_is_none() {
        assert_eq!(parse_hourly_limit(&HeaderMap::new()), None);
        let h = hm(&[("X-RateLimit-Limit", "500"), ("RateLimit-Policy", "10;w=1")]);
        assert_eq!(parse_hourly_limit(&h), None);
    }

    #[test]
    fn configured_beats_detected() {
        let mut b = RateBudget::new(Some(50));
        b.observe(&hm(&[("RateLimit-Policy", "1000;w=3600")]), Instant::now());
        assert_eq!(b.hourly_limit(), (50, LimitSource::Configured));
        let mut b = RateBudget::new(None);
        b.observe(&hm(&[("RateLimit-Policy", "1000;w=3600")]), Instant::now());
        assert_eq!(b.hourly_limit(), (1000, LimitSource::Detected));
    }

    #[test]
    fn retry_after_sets_blocked_until() {
        let now = Instant::now();
        let mut b = RateBudget::new(None);
        b.observe(&hm(&[("X-RateLimit-Remaining", "7")]), now);
        assert_eq!(b.remaining(), Some(7));
        assert_eq!(b.blocked_until(), None);
        b.observe(&hm(&[("Retry-After", "30")]), now);
        assert_eq!(b.blocked_until(), Some(now + Duration::from_secs(30)));
        b.observe(&HeaderMap::new(), now);
        assert_eq!(b.blocked_until(), Some(now + Duration::from_secs(30)));
    }

    #[test]
    fn backstop_intervals() {
        assert_eq!(backstop_interval(100, None), Duration::from_secs(360));
        assert_eq!(backstop_interval(1000, None), Duration::from_secs(36));
        assert_eq!(backstop_interval(10000, None), Duration::from_secs(15));
        assert_eq!(backstop_interval(1000, Some(5)), Duration::from_secs(5));
        assert_eq!(backstop_interval(1000, Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn default_until_detected() {
        assert_eq!(
            RateBudget::new(None).hourly_limit(),
            (100, LimitSource::Default)
        );
    }
}
