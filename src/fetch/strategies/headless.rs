//! Headless tier — render via WKWebView, then run the same readability +
//! htmd post-processing as `parse_html`.
//!
//! Uses the daemon's shared `WorkerHandle` (`crate::webkit::handle::shared`);
//! the worker subprocess is auto-spawned on first call.

use crate::fetch::{
    extract, strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind,
    HeadlessMode, MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub struct Headless;

#[async_trait]
impl Strategy for Headless {
    fn name(&self) -> &'static str {
        "headless"
    }

    /// We bypass the HTTP fetcher and drive the WebKit worker instead.
    /// `Plain` is a placeholder; `try_fetch` ignores the supplied fetcher.
    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Plain
    }

    async fn try_fetch(
        &self,
        url: &Url,
        opts: &FetchOptions,
        _fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        // Only render when the caller asked for it (`On`) or the
        // waterfall has decided to auto-escalate (`Auto`). The waterfall
        // is responsible for the escalation decision; we just gate `Off`
        // here as a belt-and-suspenders check so a future caller that
        // bypasses the waterfall can't accidentally spin up a renderer.
        if opts.headless == HeadlessMode::Off {
            return Ok(None);
        }

        let handle = crate::webkit::handle::shared();
        // Phase 4 hooks. Forward user-supplied wait-for / eval, and
        // default `auto_consent = true` whenever we're escalating to
        // headless: we're only here because the cheap chain failed,
        // and the most common reason is a consent / geo-gate banner.
        let render_opts = crate::webkit::handle::RenderOpts {
            wait_for: opts.wait_for.clone(),
            eval: opts.eval_js.clone(),
            auto_consent: opts.auto_consent,
        };
        let result = handle.render(url.as_str(), render_opts).await?;

        let canonical_url = Url::parse(&result.final_url).unwrap_or_else(|_| url.clone());
        let html_bytes = result.html.into_bytes();
        let bytes_html = html_bytes.len();
        let raw = opts.raw;
        let canonical_for_extract = canonical_url.clone();

        // WKWebView hands us a UTF-8 `String`; no header to forward. The
        // decode helper falls through to UTF-8 default, which is correct
        // and effectively zero-cost.
        //
        // Same readability auto-fallback we run in `parse_html`: if
        // readability strips the page down to a stub, re-run htmd in raw
        // mode on the same bytes and keep whichever is longer. This
        // matters more on headless than on parse_html because hydrated
        // SPAs *frequently* don't fit readability's article template
        // (e.g. real-estate listings, product grids, search-result lists).
        // Without the fallback we'd ship a 200-byte blurb on top of
        // a 50 KB legitimate listing page.
        let extracted = tokio::task::spawn_blocking(move || -> Result<_> {
            let primary = extract::html_to_markdown(
                &html_bytes,
                &canonical_for_extract,
                raw,
                None,
            )?;
            if raw || !extract::looks_like_stub(&primary.markdown) {
                return Ok(primary);
            }
            match extract::html_to_markdown(
                &html_bytes,
                &canonical_for_extract,
                true,
                None,
            ) {
                Ok(raw_ext)
                    if raw_ext.markdown.trim().len() > primary.markdown.trim().len() =>
                {
                    Ok(extract::Extracted {
                        title: if !primary.title.is_empty() {
                            primary.title
                        } else {
                            raw_ext.title
                        },
                        markdown: raw_ext.markdown,
                    })
                }
                _ => Ok(primary),
            }
        })
        .await??;

        let markdown = extract::postprocess(&extracted.markdown);
        if markdown.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some(MarkdownResponse {
            markdown,
            source: FetchSource::Headless,
            canonical_url,
            title: Some(extracted.title).filter(|t| !t.is_empty()),
            bytes_html: Some(bytes_html),
        }))
    }
}
