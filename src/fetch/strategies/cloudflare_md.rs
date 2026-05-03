//! Tier 2 — Cloudflare `.md` suffix.
//!
//! Cloudflare's "Markdown for Agents" feature serves any HTML page as
//! markdown when `.md` is appended to the path (or, equivalently, when
//! `Accept: text/markdown` is sent — the `accept_md` tier already covers
//! that case for hosts that do *content* negotiation, but some sites only
//! enable the URL-suffix form).
//!
//! Heuristics:
//!  * Skip if the URL path already ends with `.md`/`.markdown`/`.txt`.
//!  * Per-host cache: once we know a host *doesn't* serve `.md`, stop probing.
//!    Cache lives until daemon shutdown — TTL not needed at this scale.

use crate::fetch::{
    strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind, MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use url::Url;

pub struct CloudflareMd {
    /// host → does-md-suffix-work? `Some(true)` means we've seen it work,
    /// `Some(false)` means we've seen it 404 / return non-markdown.
    cache: Mutex<HashMap<String, bool>>,
}

impl CloudflareMd {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Strategy for CloudflareMd {
    fn name(&self) -> &'static str {
        "cloudflare_md"
    }

    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Plain
    }

    async fn try_fetch(
        &self,
        url: &Url,
        _opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        // Skip: already-markdown URLs (covered by `accept_md`) and obvious
        // non-html resource extensions.
        if let Some(ext) = url.path().rsplit('.').next() {
            let ext = ext.to_ascii_lowercase();
            if matches!(
                ext.as_str(),
                "md" | "markdown" | "txt" | "json" | "xml" | "css" | "js"
            ) {
                return Ok(None);
            }
        }
        let Some(host) = url.host_str() else {
            return Ok(None);
        };

        // Cached negative result?
        if let Ok(c) = self.cache.lock() {
            if let Some(false) = c.get(host) {
                return Ok(None);
            }
        }

        // Build `<url>.md`: append `.md` to the path, preserving query/fragment.
        let mut suffixed = url.clone();
        let new_path = {
            let p = url.path();
            if p == "/" {
                "/index.md".to_string()
            } else if let Some(stripped) = p.strip_suffix('/') {
                format!("{stripped}.md")
            } else {
                format!("{p}.md")
            }
        };
        suffixed.set_path(&new_path);

        let body = fetcher.get(&suffixed, "text/markdown, text/plain, */*").await?;
        let success = body.status < 400 && (body.looks_like_markdown() || body.is_text_plain());
        if !success {
            self.remember(host, false);
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&body.bytes).into_owned();
        // Defend against a server that just rewrites .md → 200 OK with HTML
        // (some misconfigurations). If the body looks like HTML, decline.
        if text.trim_start().starts_with("<!DOCTYPE html")
            || text.trim_start().starts_with("<html")
        {
            self.remember(host, false);
            return Ok(None);
        }
        // Reject SPA placeholders dressed up as markdown.
        if !body.body_has_real_content() {
            self.remember(host, false);
            return Ok(None);
        }
        self.remember(host, true);

        Ok(Some(MarkdownResponse {
            markdown: text,
            source: FetchSource::Cloudflare,
            canonical_url: url.clone(),
            title: first_h1(&body_str(&body.bytes)),
            bytes_html: None,
        }))
    }
}

impl CloudflareMd {
    fn remember(&self, host: &str, ok: bool) {
        if let Ok(mut c) = self.cache.lock() {
            c.insert(host.to_string(), ok);
        }
    }
}

fn body_str(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn first_h1(md: &str) -> Option<String> {
    md.lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
}


