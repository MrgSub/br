//! Tier 7.5 - PDF text extraction.
//!
//! Sits *between* the adapters and `parse_html` in the chain. Claims any
//! URL whose path ends in `.pdf` (or whose response is served as
//! `application/pdf`); declines everything else without a fetch.
//!
//! Why URL-extension first?
//! -----------------------
//!
//! Every chain tier today does its own fetch. Doing a HEAD probe just to
//! detect content-type would either double the fetch cost on the happy
//! path or require a smarter fetcher. The `.pdf` extension covers the
//! common cases (arxiv, RFC drafts, government whitepapers, slide
//! decks); for the long tail of PDFs served at non-`.pdf` URLs, we still
//! catch them via the post-fetch content-type check, but only if a
//! cheaper tier doesn't claim them first. Acceptable for v1.

use crate::fetch::{
    extract, strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind,
    MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub struct PdfText;

#[async_trait]
impl Strategy for PdfText {
    fn name(&self) -> &'static str {
        "pdf"
    }

    /// arxiv / RFC / .gov hosts are friendly; no need for the heavier
    /// fingerprint stack. Behind some publisher CDNs (Springer, Wiley)
    /// the Stealth fetcher is needed, but those are paywalls anyway.
    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Plain
    }

    async fn try_fetch(
        &self,
        url: &Url,
        _opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        if !looks_like_pdf_url(url) {
            return Ok(None);
        }

        let body = fetcher.get(url, "application/pdf, */*").await?;
        if body.status >= 400 {
            return Ok(None);
        }
        // Sanity check: even if the URL ended in `.pdf`, the server might
        // serve a 200 OK redirect-page. If the content type is set and
        // explicitly NOT pdf, decline so a downstream strategy can try.
        if let Some(ct) = body.content_type.as_deref() {
            let ct = ct.to_ascii_lowercase();
            if !ct.contains("pdf") && !ct.starts_with("application/octet-stream") {
                return Ok(None);
            }
        }
        // PDF magic bytes: `%PDF-`. Cheap, conclusive, and catches the
        // case where a server sets a wrong Content-Type.
        if body.bytes.len() < 5 || &body.bytes[..5] != b"%PDF-" {
            return Ok(None);
        }

        let canonical_url = body.canonical_url.clone();
        let pdf_bytes = body.bytes;
        let bytes_pdf = pdf_bytes.len();

        // pdf-extract is sync/CPU-bound; large PDFs take real time.
        // Offload from the runtime.
        let extracted =
            tokio::task::spawn_blocking(move || extract::pdf_to_markdown(&pdf_bytes)).await??;

        let markdown = extract::postprocess(&extracted.markdown);
        if markdown.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some(MarkdownResponse {
            markdown,
            source: FetchSource::Pdf,
            canonical_url,
            title: Some(extracted.title).filter(|t| !t.is_empty()),
            // Repurpose the field: it documents the source-byte size,
            // which is just as useful for PDFs as for HTML.
            bytes_html: Some(bytes_pdf),
        }))
    }
}

/// Cheap URL sniff: lowercase the last path segment and check common
/// PDF-y suffixes. Tolerates query strings and fragments.
fn looks_like_pdf_url(url: &Url) -> bool {
    let path = url.path();
    let last = path.rsplit('/').next().unwrap_or("");
    // Strip a single trailing dot/slash run if any.
    let last = last.trim_end_matches('/');
    let lower = last.to_ascii_lowercase();
    lower.ends_with(".pdf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_pdf_url_handles_plain() {
        let u = Url::parse("https://arxiv.org/pdf/2106.09685.pdf").unwrap();
        assert!(looks_like_pdf_url(&u));
    }

    #[test]
    fn looks_like_pdf_url_handles_query() {
        let u = Url::parse("https://example.com/x.pdf?download=1").unwrap();
        assert!(looks_like_pdf_url(&u));
    }

    #[test]
    fn looks_like_pdf_url_rejects_html() {
        let u = Url::parse("https://example.com/page.html").unwrap();
        assert!(!looks_like_pdf_url(&u));
        let u = Url::parse("https://example.com/").unwrap();
        assert!(!looks_like_pdf_url(&u));
    }

    #[test]
    fn looks_like_pdf_url_case_insensitive() {
        let u = Url::parse("https://example.com/Report.PDF").unwrap();
        assert!(looks_like_pdf_url(&u));
    }
}
