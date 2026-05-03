//! Per-host rate limiter shared between [`PlainFetcher`] and
//! [`StealthFetcher`].
//!
//! Goals
//! -----
//! * Avoid tripping CAPTCHAs / 429s on shared hosts (DDG, Google, GitHub
//!   API, etc.) when an agent fires several `br tab` calls in a burst.
//! * Survive across fetcher kinds: the same `Host` should be throttled
//!   regardless of whether `Plain` or `Stealth` made the previous request,
//!   because the *server* sees one IP either way.
//! * Cache hits must not consume from the bucket. We satisfy this
//!   trivially: the limiter sits *inside* the fetcher, and the cache
//!   short-circuits before any fetcher is called.
//!
//! Mechanism
//! ---------
//! Per host:
//!
//! ```text
//!     last_request_at:  Instant
//!     min_gap:          Duration   // grows on 429/503, decays on 2xx
//! ```
//!
//! On `acquire(host)` we sleep for `(last_request_at + min_gap) - now`
//! if that's positive, then bump `last_request_at` to "now after sleep".
//! The lock is held only across the `Instant` math — the sleep itself
//! happens *outside* the mutex, so concurrent callers for *different*
//! hosts don't serialize.
//!
//! On `record_status(host, status)`:
//!
//! * `429` / `503` / `403`: `min_gap = (min_gap * 2).min(MAX_GAP)`.
//!   (`403` because Cloudflare's "blocked, but with a hint" sometimes
//!   surfaces as 403 rather than 429.)
//! * `2xx`: `min_gap = (min_gap / 2).max(BASE_GAP)`.
//! * other: leave it alone.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

const BASE_GAP: Duration = Duration::from_millis(250);
const MAX_GAP: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
struct Bucket {
    last: Instant,
    gap: Duration,
}

pub struct HostRateLimiter {
    inner: Mutex<HashMap<String, Bucket>>,
}

impl HostRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Block until this host is allowed to be hit again. Returns the
    /// host key we used (so callers can pass the same one to
    /// `record_status` without re-parsing).
    pub async fn acquire(&self, url: &Url) -> Option<String> {
        let host = host_key(url)?;
        let now = Instant::now();
        let wait = {
            let mut map = self.inner.lock().ok()?;
            let bucket = map.entry(host.clone()).or_insert(Bucket {
                last: now - BASE_GAP, // first hit free
                gap: BASE_GAP,
            });
            let earliest = bucket.last + bucket.gap;
            let wait = earliest.saturating_duration_since(now);
            // Reserve our slot now so simultaneous callers stagger.
            bucket.last = if wait.is_zero() { now } else { earliest };
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Some(host)
    }

    /// Update the host's gap based on the response status.
    pub fn record_status(&self, host: &str, status: u16) {
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        let Some(bucket) = map.get_mut(host) else {
            return;
        };
        match status {
            429 | 503 | 403 => {
                bucket.gap = (bucket.gap.saturating_mul(2)).min(MAX_GAP);
            }
            200..=299 => {
                bucket.gap = (bucket.gap / 2).max(BASE_GAP);
            }
            _ => {}
        }
    }

    /// Record a network-level error (no status code). Treat like a
    /// soft backoff so we don't hammer a flaky host.
    pub fn record_error(&self, host: &str) {
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        if let Some(bucket) = map.get_mut(host) {
            bucket.gap = (bucket.gap.saturating_mul(2)).min(MAX_GAP);
        }
    }
}

fn host_key(url: &Url) -> Option<String> {
    url.host_str().map(|h| h.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_hit_is_free() {
        let rl = HostRateLimiter::new();
        let url = Url::parse("https://example.com/a").unwrap();
        let t0 = Instant::now();
        let _ = rl.acquire(&url).await;
        assert!(t0.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn second_hit_waits_base_gap() {
        let rl = HostRateLimiter::new();
        let url = Url::parse("https://example.com/a").unwrap();
        let _ = rl.acquire(&url).await;
        let t0 = Instant::now();
        let _ = rl.acquire(&url).await;
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_millis(200), "elapsed={elapsed:?}");
    }

    #[tokio::test]
    async fn different_hosts_dont_block() {
        let rl = HostRateLimiter::new();
        let a = Url::parse("https://a.example/").unwrap();
        let b = Url::parse("https://b.example/").unwrap();
        let _ = rl.acquire(&a).await;
        let t0 = Instant::now();
        let _ = rl.acquire(&b).await;
        assert!(t0.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn backoff_grows_on_429() {
        let rl = HostRateLimiter::new();
        let url = Url::parse("https://example.com/").unwrap();
        let host = rl.acquire(&url).await.unwrap();
        rl.record_status(&host, 429);
        rl.record_status(&host, 429);
        let t0 = Instant::now();
        let _ = rl.acquire(&url).await;
        // base 250 → 500 → 1000 ms
        assert!(t0.elapsed() >= Duration::from_millis(900), "elapsed={:?}", t0.elapsed());
    }
}
