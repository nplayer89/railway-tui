//! Rate budget: tracks Railway's hourly API quota and derives a polling backstop.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;

pub const DEFAULT_HOURLY_LIMIT: u32 = 100;

/// Longest block honoured from a server-supplied Retry-After.
const MAX_BLOCK_SECS: u64 = 3600;

/// How long to back off after a 429 that carries no Retry-After.
const DEFAULT_RATE_LIMIT_BLOCK: Duration = Duration::from_secs(60);

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
        if let Some(retry_after) = parse_retry_after(headers) {
            self.block_for(retry_after, now);
        }
    }

    /// Records an HTTP 429 and returns how long to wait: `retry_after` (from
    /// [`parse_retry_after`]) or 60s when absent, clamped to the maximum block.
    pub fn on_rate_limited(&mut self, retry_after: Option<Duration>, now: Instant) -> Duration {
        self.block_for(retry_after.unwrap_or(DEFAULT_RATE_LIMIT_BLOCK), now)
    }

    fn block_for(&mut self, wait: Duration, now: Instant) -> Duration {
        let block = wait.min(Duration::from_secs(MAX_BLOCK_SECS));
        self.blocked_until = now.checked_add(block);
        block
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

/// One `RateBudget` shared by the API client and the worker. Its state is plain
/// counters, so a lock poisoned by a panic elsewhere is recovered, not spread.
#[derive(Debug, Clone)]
pub struct SharedBudget(Arc<Mutex<RateBudget>>);

impl SharedBudget {
    pub fn new(budget: RateBudget) -> Self {
        Self(Arc::new(Mutex::new(budget)))
    }

    pub fn lock(&self) -> MutexGuard<'_, RateBudget> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Time left on a rate-limit block at `now`, rounded up to whole seconds.
    pub fn blocked_for(&self, now: Instant) -> Option<Duration> {
        let until = self.lock().blocked_until()?;
        let left = until.checked_duration_since(now).filter(|d| !d.is_zero())?;
        Some(Duration::from_secs(
            left.as_secs() + u64::from(left.subsec_nanos() > 0),
        ))
    }
}

/// The seconds form of `Retry-After`; an HTTP-date is ignored.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_str(headers, "retry-after")
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
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
    fn shared_budget_rounds_a_block_up_and_survives_a_poisoned_lock() {
        let budget = SharedBudget::new(RateBudget::new(None));
        let now = Instant::now();
        assert_eq!(budget.blocked_for(now), None);
        budget
            .lock()
            .on_rate_limited(Some(Duration::from_secs(30)), now);
        let later = now + Duration::from_millis(500);
        assert_eq!(budget.blocked_for(later), Some(Duration::from_secs(30)));
        assert_eq!(budget.blocked_for(now + Duration::from_secs(30)), None);

        let held = budget.clone();
        let _ = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison the lock");
        })
        .join();
        assert!(budget.blocked_for(later).is_some());
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
    fn policy_header_as_railway_sends_it() {
        // Observed live (Task 12): the only rate-limit header Railway returns.
        let h = hm(&[("ratelimit-policy", "\"default\";q=1000;w=3600")]);
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
    fn rate_limited_defaults_to_60s_and_blocks() {
        let now = Instant::now();
        let mut b = RateBudget::new(None);
        assert_eq!(b.on_rate_limited(None, now), Duration::from_secs(60));
        assert_eq!(b.blocked_until(), Some(now + Duration::from_secs(60)));
    }

    #[test]
    fn rate_limited_clamps_huge_retry_after() {
        let now = Instant::now();
        let mut b = RateBudget::new(None);
        let d = b.on_rate_limited(Some(Duration::from_secs(999_999_999)), now);
        assert_eq!(d, Duration::from_secs(3600));
        assert_eq!(b.blocked_until(), Some(now + d));
    }

    #[test]
    fn rate_limited_uses_given_retry_after() {
        let now = Instant::now();
        let mut b = RateBudget::new(None);
        let d = b.on_rate_limited(Some(Duration::from_secs(30)), now);
        assert_eq!(d, Duration::from_secs(30));
        assert_eq!(b.blocked_until(), Some(now + d));
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
