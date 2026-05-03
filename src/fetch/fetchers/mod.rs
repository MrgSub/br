//! HTTP fetcher abstraction.
//!
//! Strategies don't talk to `reqwest` / `rquest` directly — they call into a
//! `Fetcher` chosen by [`FetcherKind`]. This lets each strategy pick the
//! cheapest stack that will work:
//!
//! * [`FetcherKind::Plain`]   — vanilla reqwest+rustls. For trusted endpoints
//!   (APIs, llms.txt, raw markdown). No bot-evasion ceremony.
//! * [`FetcherKind::Stealth`] — rquest with Chrome TLS+H2 fingerprint and a
//!   cookie jar. For sites with WAF/JA3 fingerprinting (Google, Cloudflare,
//!   most modern public web).
//! * [`FetcherKind::WebKit`]  — (M8) WKWebView render. For SPAs that need
//!   JS execution.

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;
use url::Url;

pub mod plain;
pub mod rate_limit;
pub mod stealth;

pub use rate_limit::HostRateLimiter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetcherKind {
    Plain,
    Stealth,
    #[allow(dead_code)] // M8
    WebKit,
}

#[derive(Debug, Clone)]
pub struct FetchedBody {
    pub bytes: Bytes,
    pub canonical_url: Url,
    pub status: u16,
    pub content_type: Option<String>,
}

impl FetchedBody {
    pub fn is_html(&self) -> bool {
        self.content_type
            .as_deref()
            .map(|c| c.to_ascii_lowercase().contains("html"))
            .unwrap_or(false)
    }

    pub fn is_text_plain(&self) -> bool {
        self.content_type
            .as_deref()
            .map(|c| c.to_ascii_lowercase().starts_with("text/plain"))
            .unwrap_or(false)
    }

    /// Markdown bodies under this size or with too few words are usually
    /// SPA placeholders (e.g. JSX `<HomePage />` returned with text/markdown
    /// Content-Type by Mintlify-style sites for not-yet-statically-rendered
    /// routes). Strategies that produce "server said it's markdown" results
    /// should call this to avoid claiming success on garbage.
    pub fn body_has_real_content(&self) -> bool {
        let s = String::from_utf8_lossy(&self.bytes);
        let trimmed = s.trim();
        trimmed.len() >= 32 && trimmed.split_whitespace().take(8).count() >= 8
    }

    pub fn looks_like_markdown(&self) -> bool {
        let ct = self
            .content_type
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        if ct.starts_with("text/markdown") || ct.starts_with("text/x-markdown") {
            return true;
        }
        // text/plain + .md/.markdown extension is common (GitHub raw, etc.)
        if ct.starts_with("text/plain") {
            if let Some(ext) = self.canonical_url.path().rsplit('.').next() {
                if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") {
                    return true;
                }
            }
        }
        false
    }
}

#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn get(&self, url: &Url, accept: &str) -> Result<FetchedBody>;
}

/// Holds one of each fetcher; constructed once at daemon startup.
pub struct FetcherSet {
    pub plain: Arc<dyn Fetcher>,
    pub stealth: Arc<dyn Fetcher>,
}

impl FetcherSet {
    pub fn new() -> Result<Self> {
        // One limiter shared by both fetchers — the *server* sees one IP
        // regardless of which TLS stack we use, so throttling has to be
        // joint. See `rate_limit.rs` for the algorithm.
        let rate = Arc::new(HostRateLimiter::new());
        Ok(Self {
            plain: Arc::new(plain::PlainFetcher::new(rate.clone())?),
            stealth: Arc::new(stealth::StealthFetcher::new(rate)?),
        })
    }

    pub fn pick(&self, kind: FetcherKind) -> &dyn Fetcher {
        match kind {
            FetcherKind::Plain => &*self.plain,
            FetcherKind::Stealth => &*self.stealth,
            FetcherKind::WebKit => panic!("WebKit fetcher lands in M8"),
        }
    }
}
