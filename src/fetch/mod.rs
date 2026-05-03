//! URL → markdown fetch pipeline.
//!
//! The pipeline is a *waterfall* of [`Strategy`] impls. Each strategy is
//! tried in order; the first one to return `Some(MarkdownResponse)` wins.
//! Strategies further down the chain are progressively more expensive
//! (and lossier).

use serde::{Deserialize, Serialize};
use url::Url;

pub mod extract;
pub mod fetchers;
pub mod strategies;
pub mod ua;
pub mod waterfall;

pub use fetchers::{Fetcher, FetcherKind, FetcherSet};
pub use waterfall::run;

/// Tier of fetch that produced a result. Surfaced to clients (and the
/// dashboard) so they know whether they're looking at server-authoritative
/// markdown vs. our best-effort parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchSource {
    /// Server returned `Content-Type: text/markdown` directly.
    AcceptMd,
    /// Cloudflare auto-markdown via `.md` suffix.
    Cloudflare,
    /// `<origin>/llms-full.txt`.
    LlmsFull,
    /// Single entry from `<origin>/llms.txt`.
    LlmsIndex,
    /// Hostname-specific adapter (GitHub, Wikipedia, …).
    Adapter,
    /// `<link rel="alternate" type="text/markdown">`.
    AltLink,
    /// Reader-mode endpoint of a known generator.
    Reader,
    /// HTML fetched + readability + html→md.
    Parse,
    /// Headless Chromium render + parse.
    Headless,
    /// Wayback Machine snapshot of the original URL.
    Wayback,
    /// Direct PDF — text extracted via `pdf-extract`, no readability.
    Pdf,
}

impl FetchSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FetchSource::AcceptMd => "accept_md",
            FetchSource::Cloudflare => "cloudflare",
            FetchSource::LlmsFull => "llms_full",
            FetchSource::LlmsIndex => "llms_index",
            FetchSource::Adapter => "adapter",
            FetchSource::AltLink => "alt_link",
            FetchSource::Reader => "reader",
            FetchSource::Parse => "parse",
            FetchSource::Headless => "headless",
            FetchSource::Wayback => "wayback",
            FetchSource::Pdf => "pdf",
        }
    }

    pub fn quality_hint(self) -> QualityHint {
        match self {
            FetchSource::AcceptMd | FetchSource::LlmsFull | FetchSource::LlmsIndex => {
                QualityHint::Authoritative
            }
            FetchSource::Cloudflare | FetchSource::AltLink => QualityHint::Negotiated,
            FetchSource::Adapter | FetchSource::Reader => QualityHint::Adapted,
            FetchSource::Parse => QualityHint::Parsed,
            FetchSource::Headless => QualityHint::Rendered,
            FetchSource::Wayback => QualityHint::Archived,
            // PDFs are extracted lossily (layout reflow, ligature
            // normalization), but the text content is the document's
            // own — closer to "adapted" than "parsed".
            FetchSource::Pdf => QualityHint::Adapted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityHint {
    /// Server gave us markdown directly.
    Authoritative,
    /// Server-side conversion (Cloudflare, alternate link).
    Negotiated,
    /// Site-specific adapter or reader-mode endpoint.
    Adapted,
    /// Generic HTML→markdown by us.
    Parsed,
    /// Required JS execution.
    Rendered,
    /// Recovered from the Wayback Machine; the live origin failed.
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchOptions {
    /// Skip cache lookup and force a fresh fetch.
    #[serde(default)]
    pub no_cache: bool,
    /// Per-strategy deadline. None → default (30s).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Whether to allow falling all the way through to headless. Defaults
    /// to "auto" semantics (try cheap tiers, but don't spawn Chromium yet —
    /// headless arrives in M8).
    #[serde(default)]
    pub allow_headless: bool,
    /// Skip the readability pass in the parse-html tier. Returns the
    /// whole page converted to markdown. Useful for search result pages,
    /// table-heavy reference pages, etc.
    #[serde(default)]
    pub raw: bool,
    /// Headless rendering mode (WKWebView):
    ///   * `Auto` (default) — cheap tiers first; escalate to WebKit only
    ///                        when their output is a stub or every tier
    ///                        timed-out / errored.
    ///   * `On`              — force-render, bypassing all cheaper tiers.
    ///                        Use when you already know the page is an SPA.
    ///   * `Off`             — never escalate. Fail fast if cheap tiers
    ///                        can't deliver. Useful in scripts and tests.
    #[serde(default)]
    pub headless: HeadlessMode,
    /// Phase 4: CSS selector to wait for after the readiness signal but
    /// before extraction. Polled inside the worker for up to 5 s.
    /// Forwarded only on the headless path.
    #[serde(default)]
    pub wait_for: Option<String>,
    /// Phase 4: arbitrary JS to run after `wait_for`, before extraction.
    /// Errors swallowed. Forwarded only on the headless path.
    #[serde(default)]
    pub eval_js: Option<String>,
    /// Phase 4: enable the baked-in cookie/geo-gate dismissal hook.
    /// Defaults to `true` so auto-escalation "just works" on the most
    /// common interstitials. Set false to opt out.
    #[serde(default = "default_true")]
    pub auto_consent: bool,
    /// M10: convert inline `[text](url)` links to reference-style and
    /// emit a `## Links` table at the end. Default true: cuts agent
    /// token cost on link-heavy pages, dedupes repeated URLs, and lets
    /// `--max-tokens`-style truncation drop the table cleanly. Set
    /// false to keep classic inline-link markdown.
    #[serde(default = "default_true")]
    pub link_table: bool,
    /// M10: cap the rendered markdown at approximately N tokens. Uses
    /// the rough "1 token ≈ 4 chars" heuristic to avoid pulling in a
    /// real tokenizer dependency — fine for context-budgeting purposes.
    /// `None` = no cap.
    ///
    /// When the budget is exceeded:
    ///   1. Drop the `## Links` table (it's the densest, least
    ///      structurally important section, and the body keeps inline
    ///      links via the still-intact reference-style markup).
    ///   2. If still over, truncate body at the most recent heading
    ///      boundary before the budget point.
    ///   3. Append a `<!-- truncated: ~N tokens omitted -->` marker.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

fn default_true() -> bool {
    true
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            no_cache: false,
            timeout_ms: None,
            allow_headless: false,
            raw: false,
            headless: HeadlessMode::default(),
            wait_for: None,
            eval_js: None,
            // Match the serde default. Auto-consent is on so callers that
            // construct `FetchOptions::default()` get the same UX as
            // CLI/MCP defaults.
            auto_consent: true,
            link_table: true,
            max_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessMode {
    Off,
    On,
    #[default]
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkdownResponse {
    pub markdown: String,
    pub source: FetchSource,
    pub canonical_url: Url,
    pub title: Option<String>,
    pub bytes_html: Option<usize>,
}
