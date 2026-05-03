//! Tier 1 — `Accept: text/markdown` content negotiation.
//!
//! Many sites (and Cloudflare's edge for opted-in customers) return
//! markdown directly when asked. This is the cheapest, lossless tier.
//!
//! Uses the **plain** fetcher: trusted endpoints don't need TLS impersonation
//! and rquest's BoringSSL is heavier than rustls.

use crate::fetch::{
    strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind, MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub struct AcceptMd;

#[async_trait]
impl Strategy for AcceptMd {
    fn name(&self) -> &'static str {
        "accept_md"
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
        let body = fetcher
            .get(
                url,
                "text/markdown, text/x-markdown;q=0.9, text/plain;q=0.5",
            )
            .await?;

        if body.status >= 400 {
            return Ok(None);
        }
        if !body.looks_like_markdown() {
            return Ok(None);
        }

        if !body.body_has_real_content() {
            return Ok(None);
        }
        let markdown = String::from_utf8_lossy(&body.bytes).into_owned();
        let title = first_h1(&markdown);

        Ok(Some(MarkdownResponse {
            markdown,
            source: FetchSource::AcceptMd,
            canonical_url: body.canonical_url,
            title,
            bytes_html: None,
        }))
    }
}

fn first_h1(md: &str) -> Option<String> {
    md.lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
}


