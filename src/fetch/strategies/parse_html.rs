//! Tier 8 — fetch HTML, run readability, convert to markdown.
//!
//! Generic fallback for the open web. Uses the **stealth** fetcher because
//! most modern HTML pages live behind WAFs / CDN bot detection.

use crate::fetch::{
    strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind, MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub struct ParseHtml;

#[async_trait]
impl Strategy for ParseHtml {
    fn name(&self) -> &'static str {
        "parse_html"
    }

    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Stealth
    }

    async fn try_fetch(
        &self,
        url: &Url,
        opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        let body = fetcher
            .get(url, "text/html, application/xhtml+xml;q=0.9, */*;q=0.5")
            .await?;

        if body.status >= 400 {
            anyhow::bail!("http {}", body.status);
        }
        // Only pretend to parse if it actually looks like HTML (or is unlabeled).
        if !body.is_html() && body.content_type.is_some() {
            return Ok(None);
        }

        let canonical_url = body.canonical_url.clone();
        let html_bytes = body.bytes;
        let bytes_html = html_bytes.len();
        let content_type = body.content_type.clone();

        // readability + htmd are sync/CPU-bound; offload from the runtime.
        //
        // When the caller didn't force `--raw`, we may end up running htmd
        // twice on the same bytes: once via readability, then again
        // without if readability returned suspiciously little. The second
        // pass is pure-CPU, doesn't re-fetch, and only triggers on stub
        // outputs — cheap insurance against readability eating non-article
        // layouts (search results, listings, SPA shells; see
        // `looks_like_stub`).
        let canonical_for_extract = canonical_url.clone();
        let force_raw = opts.raw;
        let extracted = tokio::task::spawn_blocking(move || -> Result<_> {
            let ct = content_type.as_deref();
            let primary = crate::fetch::extract::html_to_markdown(
                &html_bytes,
                &canonical_for_extract,
                force_raw,
                ct,
            )?;
            // If the user asked for raw, or readability produced something
            // that looks substantive, we're done.
            if force_raw || !crate::fetch::extract::looks_like_stub(&primary.markdown) {
                return Ok(primary);
            }
            // Stub output — try raw on the same bytes. Keep whichever has
            // more visible text; fall back to readability's title since
            // raw's `sniff_title` is dumber.
            match crate::fetch::extract::html_to_markdown(
                &html_bytes,
                &canonical_for_extract,
                true,
                ct,
            ) {
                Ok(raw_ext) => {
                    if raw_ext.markdown.trim().len() > primary.markdown.trim().len() {
                        Ok(crate::fetch::extract::Extracted {
                            title: if !primary.title.is_empty() {
                                primary.title
                            } else {
                                raw_ext.title
                            },
                            markdown: raw_ext.markdown,
                        })
                    } else {
                        Ok(primary)
                    }
                }
                Err(_) => Ok(primary),
            }
        })
        .await??;

        // `finalize` (linkify etc.) lives in the waterfall so adapter-tier
        // outputs benefit too. Here we just hand off the raw extracted
        // markdown after whitespace cleanup.
        let markdown = crate::fetch::extract::postprocess(&extracted.markdown);
        if markdown.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some(MarkdownResponse {
            markdown,
            source: FetchSource::Parse,
            canonical_url,
            title: Some(extracted.title).filter(|t| !t.is_empty()),
            bytes_html: Some(bytes_html),
        }))
    }
}


