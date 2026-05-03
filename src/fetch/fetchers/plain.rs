//! Vanilla reqwest fetcher with realistic Chrome-shaped headers but no TLS
//! fingerprint trickery. Right tool for trusted APIs and content-negotiation.

use super::{FetchedBody, Fetcher, HostRateLimiter};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub struct PlainFetcher {
    client: reqwest::Client,
    rate: Arc<HostRateLimiter>,
}

impl PlainFetcher {
    pub fn new(rate: Arc<HostRateLimiter>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(crate::fetch::ua::CHROME_UA)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .gzip(true)
            .brotli(true)
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        Ok(Self { client, rate })
    }
}

#[async_trait]
impl Fetcher for PlainFetcher {
    async fn get(&self, url: &Url, accept: &str) -> Result<FetchedBody> {
        let host = self.rate.acquire(url).await;
        let resp = match self
            .client
            .get(url.as_str())
            .header(reqwest::header::ACCEPT, accept)
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(reqwest::header::ACCEPT_ENCODING, "gzip, deflate, br")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(h) = host.as_deref() {
                    self.rate.record_error(h);
                }
                return Err(e.into());
            }
        };
        if let Some(h) = host.as_deref() {
            self.rate.record_status(h, resp.status().as_u16());
        }

        let status = resp.status().as_u16();
        let canonical_url = resp.url().clone();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp.bytes().await?;
        Ok(FetchedBody {
            bytes,
            canonical_url,
            status,
            content_type,
        })
    }
}
