//! Drives the strategy chain.
//!
//! Three modes
//! -----------
//!
//! * **`HeadlessMode::On`** — skip the cheap chain entirely, render the
//!   page in WKWebView, run normal extraction. For agents that already
//!   know the URL is an SPA.
//!
//! * **`HeadlessMode::Off`** — walk the cheap chain only. Never spin up
//!   the renderer. Fast-fail when the cheap tiers can't deliver. Useful
//!   in scripts and tests.
//!
//! * **`HeadlessMode::Auto`** (default) — walk the cheap chain. If we
//!   come out with either nothing or a stub-shaped result, escalate to
//!   the headless renderer once and keep whichever output has more
//!   visible text. Then, if everything still failed, try Wayback. This
//!   is the "just works" path designed in coverage report Phase 6.
//!
//! Per-strategy timeout
//! --------------------
//!
//! Anti-bot WAFs sometimes return a 200 OK and then *never* finish the
//! response body, leaving us hung on a strategy for as long as the
//! global request timeout allows. With a single bad tier that can eat
//! the entire fetch budget, escalation to headless never happens. We
//! cap each cheap-chain strategy at `PER_STRATEGY_TIMEOUT` and the
//! headless escalation at `HEADLESS_TIMEOUT`. Anything that overruns is
//! recorded as a (terminal) error for that strategy and the chain
//! keeps walking. Real successes today complete well inside these
//! budgets (≤7 s for legitimate slow origins observed in the eval),
//! so the cap is conservative.
//!
//! Stub vs empty triggers
//! ----------------------
//!
//! Real-world testing showed two distinct failure modes that need
//! escalation:
//!
//! 1. **Stub.** A cheap tier returned `Some(resp)` but
//!    `looks_like_stub(resp.markdown)` is true — typically an SPA
//!    shell or "loading…" placeholder.
//! 2. **Empty.** Every cheap tier returned `None` or `Err` — typically
//!    an anti-bot site that hangs or 403s the plain HTTP request.
//!    `apartments.com` is the canonical example: the plain chain
//!    times out, headless renders successfully.
//!
//! The Auto branch escalates to headless on either trigger.

use super::strategies::{
    accept_md::AcceptMd, adapters::Adapters, cloudflare_md::CloudflareMd,
    headless::Headless, llms_txt::LlmsTxt, parse_html::ParseHtml,
    pdf::PdfText, wayback::Wayback, Strategy,
};
use super::{
    extract::{finalize, looks_like_bot_block, looks_like_stub, worth_escalating},
    FetchOptions, FetcherSet, HeadlessMode, MarkdownResponse,
};
use anyhow::{anyhow, Result};
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

/// Hard cap per cheap-chain strategy. Anti-bot WAFs sometimes hang on
/// us; without this, a single bad tier eats the entire fetch budget
/// and Auto-mode escalation never happens.
const PER_STRATEGY_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on the headless escalation, including the WKWebView render
/// (Phase 3 internally allows up to 12 s before its own backstop). One
/// extra second of slack keeps us comfortably above the worker's cap so
/// we don't pre-empt a render that's just-about-to-finish.
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(13);

/// Wayback's archive.org probe + snapshot fetch combined.
const WAYBACK_TIMEOUT: Duration = Duration::from_secs(15);

/// Cheap chain: every tier that doesn't need a full browser. Ordered by
/// (correctness, cost). Skips both `Headless` and `Wayback` — those are
/// driven by the auto-escalation logic in `run()`.
static CHEAP: OnceLock<Vec<Box<dyn Strategy>>> = OnceLock::new();

fn cheap_chain() -> &'static [Box<dyn Strategy>] {
    CHEAP.get_or_init(|| {
        vec![
            Box::new(AcceptMd),
            Box::new(CloudflareMd::new()),
            Box::new(LlmsTxt::new()),
            Box::new(Adapters::default_set()),
            // PDFs sit just before parse_html: the URL-extension sniff
            // is free, declines instantly on non-PDFs, and skipping it
            // would mean parse_html errors out on every PDF URL.
            Box::new(PdfText),
            Box::new(ParseHtml),
        ]
    })
}

static FETCHERS: OnceLock<FetcherSet> = OnceLock::new();

fn fetchers() -> &'static FetcherSet {
    FETCHERS.get_or_init(|| FetcherSet::new().expect("fetcher set init"))
}

/// Outcome of running one strategy. Internal helper to keep the main
/// loop short and the auto-escalation conditions readable.
///
/// `Stub` here uses the *escalation* threshold ([`worth_escalating`]),
/// not the looser `looks_like_stub`. The latter would over-trigger on
/// legitimately small pages like `example.com` and waste headless
/// renders.
enum One {
    /// Strategy returned a body that's substantive enough to ship.
    Ok(MarkdownResponse),
    /// Strategy returned a body so small that headless escalation is
    /// likely to do better. Held aside as a fallback in case nothing
    /// further comes up.
    Stub(MarkdownResponse),
    /// Strategy declined (`Ok(None)`).
    Declined,
    /// Strategy errored or timed out.
    Failed(anyhow::Error),
}

async fn run_one(strat: &dyn Strategy, url: &Url, opts: &FetchOptions, timeout: Duration) -> One {
    let name = strat.name();
    let kind = strat.fetcher_kind();
    let fetcher = fetchers().pick(kind);
    tracing::debug!("trying {name} via {kind:?} for {url}");
    match tokio::time::timeout(timeout, strat.try_fetch(url, opts, fetcher)).await {
        Ok(Ok(Some(resp))) => {
            // Two reasons to treat a successful response as escalation-
            // worthy rather than ship it: it's too small to be useful,
            // or it's a bot-block / WAF challenge masquerading as 200 OK.
            // Both funnel through the same downstream logic.
            if looks_like_bot_block(&resp.markdown) {
                tracing::info!(
                    "strategy {name} returned a bot-block / WAF page ({} bytes); will escalate",
                    resp.markdown.len()
                );
                One::Stub(resp)
            } else if worth_escalating(&resp.markdown) {
                tracing::info!(
                    "strategy {name} returned an escalation-worthy stub ({} bytes)",
                    resp.markdown.len()
                );
                One::Stub(resp)
            } else {
                One::Ok(resp)
            }
        }
        Ok(Ok(None)) => {
            tracing::debug!("strategy {name} declined");
            One::Declined
        }
        Ok(Err(e)) => {
            tracing::warn!("strategy {name} failed: {e:#}");
            One::Failed(e)
        }
        Err(_) => {
            tracing::warn!("strategy {name} timed out after {timeout:?}");
            One::Failed(anyhow!("strategy {name} timed out after {timeout:?}"))
        }
    }
}

/// Apply final post-processing to a successful response. Lives here
/// rather than in each strategy so adapter-tier results (GitHub,
/// Wikipedia, HN, etc.) get the same treatment as parse-tier ones.
fn polish(mut resp: MarkdownResponse, opts: &FetchOptions) -> MarkdownResponse {
    resp.markdown = finalize(&resp.markdown, opts);
    resp
}

pub async fn run(url: &Url, opts: &FetchOptions) -> Result<MarkdownResponse> {
    // Force-headless mode: render and done.
    if opts.headless == HeadlessMode::On {
        return match run_one(&Headless, url, opts, HEADLESS_TIMEOUT).await {
            One::Ok(resp) | One::Stub(resp) => {
                tracing::info!(
                    "fetched {url} via headless ({} bytes md)",
                    resp.markdown.len()
                );
                Ok(polish(resp, opts))
            }
            One::Declined => Err(anyhow!("headless declined for {url}")),
            One::Failed(e) => Err(e),
        };
    }

    // Walk the cheap chain. Hold the first stub aside so we can compare
    // against any later success (or against the headless escalation).
    let mut best_stub: Option<MarkdownResponse> = None;
    let mut last_err: Option<anyhow::Error> = None;
    let mut cheap_did_succeed = false;

    for strat in cheap_chain() {
        match run_one(strat.as_ref(), url, opts, PER_STRATEGY_TIMEOUT).await {
            One::Ok(resp) => {
                tracing::info!(
                    "fetched {url} via {} ({} bytes md)",
                    strat.name(),
                    resp.markdown.len()
                );
                return Ok(polish(resp, opts));
            }
            One::Stub(resp) => {
                cheap_did_succeed = true;
                if best_stub
                    .as_ref()
                    .map(|s| resp.markdown.trim().len() > s.markdown.trim().len())
                    .unwrap_or(true)
                {
                    best_stub = Some(resp);
                }
            }
            One::Declined => {}
            One::Failed(e) => {
                last_err = Some(e);
            }
        }
    }

    // Auto-escalation. Triggers:
    //   stub      — at least one cheap tier returned content but it
    //               failed `looks_like_stub`'s floor.
    //   empty     — no cheap tier returned content at all.
    let stub_trigger = best_stub.is_some();
    let empty_trigger = !cheap_did_succeed;
    let should_escalate =
        opts.headless == HeadlessMode::Auto && (stub_trigger || empty_trigger);

    if should_escalate {
        tracing::info!(
            "auto-escalating to headless for {url} (stub={stub_trigger} empty={empty_trigger})"
        );
        match run_one(&Headless, url, opts, HEADLESS_TIMEOUT).await {
            One::Ok(resp) => {
                tracing::info!(
                    "fetched {url} via headless ({} bytes md)",
                    resp.markdown.len()
                );
                // If we *also* have a cheap-tier stub and it happens to
                // be longer than the headless render (rare, but happens
                // with consent banners that headless can't dismiss yet),
                // prefer whichever has more visible text.
                let candidate = match best_stub.take() {
                    Some(s) if s.markdown.trim().len() > resp.markdown.trim().len() => s,
                    _ => resp,
                };
                return Ok(polish(candidate, opts));
            }
            One::Stub(resp) => {
                // Headless came back with a stub too. Pick whichever's
                // longer between it and any prior cheap-tier stub.
                tracing::info!(
                    "headless also returned a stub ({} bytes); picking the longer of the two",
                    resp.markdown.len()
                );
                let candidate = match best_stub.take() {
                    Some(s) if s.markdown.trim().len() > resp.markdown.trim().len() => s,
                    _ => resp,
                };
                best_stub = Some(candidate);
            }
            One::Declined => {
                tracing::debug!("headless declined for {url}");
            }
            One::Failed(e) => {
                tracing::warn!("headless escalation failed: {e:#}");
                last_err = Some(e);
            }
        }
    }

    // Last-resort: Wayback. Cheap+headless both failed; try the archive.
    // Only worth running if we still have nothing substantive.
    // Wayback is worth it when we have nothing, a clearly-stubby cheap
    // result, or a bot-block page. We use `looks_like_stub` (the looser
    // one) here — by this point we've already decided the cheap path
    // can't escalate further, and we'd rather pay the wayback latency
    // than ship a 60-char body or a "Your request could not be processed".
    let best_is_useless = best_stub
        .as_ref()
        .map(|s| looks_like_stub(&s.markdown) || looks_like_bot_block(&s.markdown))
        .unwrap_or(false);
    if best_stub.is_none() || best_is_useless {
        match run_one(&Wayback, url, opts, WAYBACK_TIMEOUT).await {
            One::Ok(resp) => {
                tracing::info!(
                    "fetched {url} via wayback ({} bytes md)",
                    resp.markdown.len()
                );
                return Ok(polish(resp, opts));
            }
            One::Stub(resp) => {
                if best_stub
                    .as_ref()
                    .map(|s| resp.markdown.trim().len() > s.markdown.trim().len())
                    .unwrap_or(true)
                {
                    best_stub = Some(resp);
                }
            }
            One::Declined => {}
            One::Failed(e) => {
                last_err = Some(e);
            }
        }
    }

    // Final fallback: if all we have is a stub, return it. A stub is
    // strictly more useful than an error to a downstream agent (it
    // can still see the title, navigation, what *did* render). When the
    // stub is a bot-block, prepend a banner so the agent doesn't
    // mistake "Your request could not be processed" for content.
    if let Some(mut s) = best_stub {
        if looks_like_bot_block(&s.markdown) {
            tracing::info!(
                "returning bot-block stub for {url} ({} bytes md) — no wayback / fallback succeeded",
                s.markdown.len()
            );
            s.markdown = format!(
                "> [!warning] **br: site rejected our automated request**\n\
>\n\
> The body below is the rejection page itself, not real content. \
This host (`{}`) returned a bot-block / WAF challenge that we couldn't \
bypass, and the Wayback Machine has no usable snapshot. Try again from \
a different network, or fetch a related URL on the same site.\n\n\
---\n\n{}",
                url.host_str().unwrap_or(""),
                s.markdown,
            );
        } else {
            tracing::info!(
                "returning stub for {url} ({} bytes md) — nothing better available",
                s.markdown.len()
            );
        }
        return Ok(polish(s, opts));
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no strategy could fetch {url}")))
}
