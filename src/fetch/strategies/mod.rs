//! Strategy trait + the concrete tier impls.

use super::{FetchOptions, Fetcher, FetcherKind, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub mod accept_md;
pub mod adapters;
pub mod cloudflare_md;
pub mod headless;
pub mod llms_txt;
pub mod parse_html;
pub mod pdf;
pub mod wayback;

/// One step in the fetch waterfall.
///
/// `try_fetch` returns:
///   - `Ok(Some(_))` — strategy succeeded, stop the waterfall
///   - `Ok(None)`    — strategy declines (not applicable, or empty)
///   - `Err(_)`      — strategy attempted and failed; logged but waterfall continues
#[async_trait]
pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;

    /// Which fetcher this strategy needs. Default: stealth — most general
    /// HTML scraping wants the realistic fingerprint.
    fn fetcher_kind(&self) -> FetcherKind {
        FetcherKind::Stealth
    }

    async fn try_fetch(
        &self,
        url: &Url,
        opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>>;
}
