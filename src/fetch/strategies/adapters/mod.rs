//! Tier 5 — site-specific adapters.
//!
//! When a URL matches a known site, take a shortcut: hit the site's API,
//! the raw markdown source, or another well-known canonical endpoint.
//! Cheaper, more accurate, and structured.
//!
//! Adapters live behind a single `Strategy` impl that walks a list and picks
//! the first match. Each adapter is responsible for choosing its own fetcher
//! kind (most use `Plain` since they're hitting friendly APIs).

use crate::fetch::{
    strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind, MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub mod github;
pub mod hackernews;
pub mod npm;
pub mod pypi;
pub mod reddit;
pub mod wikipedia;

/// Single adapter. Each one is responsible for both deciding it's the right
/// match (`matches`) and producing markdown (`fetch`). The dispatcher gives
/// it whichever `Fetcher` matches its preferred kind.
#[async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, url: &Url) -> bool;

    /// Most adapters hit JSON APIs and don't need TLS impersonation.
    /// Reserved for a future per-adapter routing pass; today the dispatcher
    /// just hands every adapter its supplied fetcher.
    #[allow(dead_code)]
    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Plain
    }

    async fn fetch(
        &self,
        url: &Url,
        opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>>;
}

/// Strategy that walks the adapter list, asking each whether it claims the
/// URL and, if so, attempting a fetch.
pub struct Adapters {
    inner: Vec<Box<dyn Adapter>>,
}

impl Adapters {
    pub fn default_set() -> Self {
        Self {
            inner: vec![
                Box::new(github::GitHub),
                Box::new(wikipedia::Wikipedia),
                Box::new(hackernews::HackerNews),
                Box::new(reddit::Reddit),
                Box::new(npm::Npm),
                Box::new(pypi::PyPi),
            ],
        }
    }
}

#[async_trait]
impl Strategy for Adapters {
    fn name(&self) -> &'static str {
        "adapter"
    }

    /// Adapters select their own fetcher kind; the dispatcher requests the
    /// most-permissive (`Stealth`) so it's available, and per-adapter calls
    /// pick what they need from the `FetcherSet` via the waterfall. For now
    /// we just use the supplied fetcher directly — the waterfall could be
    /// extended later if we want strict per-adapter routing.
    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Plain
    }

    async fn try_fetch(
        &self,
        url: &Url,
        opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        for adapter in &self.inner {
            if !adapter.matches(url) {
                continue;
            }
            tracing::info!("adapter {} claims {url}", adapter.name());
            match adapter.fetch(url, opts, fetcher).await {
                Ok(Some(resp)) => return Ok(Some(resp)),
                Ok(None) => {
                    tracing::debug!("adapter {} declined", adapter.name());
                }
                Err(e) => {
                    tracing::warn!("adapter {} failed: {e:#}", adapter.name());
                }
            }
        }
        Ok(None)
    }
}

// ── Helpers shared across adapters ─────────────────────────────────────────

/// Build a `MarkdownResponse` from raw markdown + canonical url.
pub(crate) fn ok_resp(
    markdown: String,
    canonical_url: Url,
    title: Option<String>,
) -> Option<MarkdownResponse> {
    if markdown.trim().is_empty() {
        return None;
    }
    Some(MarkdownResponse {
        markdown,
        source: FetchSource::Adapter,
        canonical_url,
        title,
        bytes_html: None,
    })
}
