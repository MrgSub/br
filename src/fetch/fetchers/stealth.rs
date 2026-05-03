//! rquest-based fetcher with Chrome TLS+H2 fingerprint and a per-process
//! cookie jar. Defeats JA3/JA4 and Akamai-style detection used by Google,
//! Cloudflare WAF, etc.

use super::{FetchedBody, Fetcher, HostRateLimiter};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use rquest_util::Emulation;
use std::sync::Arc;
use url::Url;

pub struct StealthFetcher {
    client: rquest::Client,
    rate: Arc<HostRateLimiter>,
}

impl StealthFetcher {
    pub fn new(rate: Arc<HostRateLimiter>) -> Result<Self> {
        let client = rquest::Client::builder()
            .emulation(Emulation::Chrome136)
            .cookie_store(true)
            // rquest defaults to no-follow; we want browser-like redirect behavior.
            .redirect(rquest::redirect::Policy::limited(10))
            .build()
            .map_err(|e| anyhow!("rquest client build: {e}"))?;
        Ok(Self { client, rate })
    }
}

#[async_trait]
impl Fetcher for StealthFetcher {
    async fn get(&self, url: &Url, accept: &str) -> Result<FetchedBody> {
        // rquest re-exports its own `Url` type but accepts `&str` everywhere.
        let host = self.rate.acquire(url).await;
        let resp = match self
            .client
            .get(url.as_str())
            .header("accept", accept)
            .header("accept-language", "en-US,en;q=0.9")
            // Don't set accept-encoding manually — rquest's emulation layer
            // sets the exact set Chrome sends (gzip, deflate, br, zstd) in
            // the right order.
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(h) = host.as_deref() {
                    self.rate.record_error(h);
                }
                return Err(anyhow!("rquest send: {e}"));
            }
        };
        if let Some(h) = host.as_deref() {
            self.rate.record_status(h, resp.status().as_u16());
        }

        let status = resp.status().as_u16();
        let canonical_url = Url::parse(resp.url().as_str())
            .map_err(|e| anyhow!("canonical url parse: {e}"))?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes: Bytes = resp.bytes().await.map_err(|e| anyhow!("rquest body: {e}"))?;

        Ok(FetchedBody {
            bytes,
            canonical_url,
            status,
            content_type,
        })
    }
}
