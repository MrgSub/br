//! Tier 9 (last) - Wayback Machine fallback.
//!
//! Runs only when every cheaper tier declined or errored. The waterfall's
//! "first `Some` wins" semantics already give us that for free: we sit at
//! the bottom of the chain.
//!
//! Flow
//! ----
//!
//! 1. Loop guard. If the URL is already on `web.archive.org` /
//!    `archive.org`, return `None` - fetching a wayback URL through
//!    wayback is meaningless and would hide errors.
//! 2. Probe `https://archive.org/wayback/available?url=<encoded>`. The
//!    response is small JSON of the form
//!    `{"archived_snapshots":{"closest":{"url":"...","timestamp":"...","status":"200"}}}`.
//!    No archive -> empty `archived_snapshots` -> return `None`.
//! 3. Rewrite the snapshot URL into the **`id_`** form so the response
//!    body is the raw page bytes without wayback's toolbar / iframe
//!    injection. The transform is purely string-level: insert `id_` after
//!    the timestamp segment.
//! 4. Fetch via the plain HTTP fetcher (archive.org doesn't fingerprint
//!    us). Run the same `extract::html_to_markdown` we use for `parse_html`.
//! 5. The returned `MarkdownResponse.canonical_url` is the **original**
//!    URL - agents care about content identity, not the archive path. The
//!    `FetchSource::Wayback` source tells them it's recovered, not live.
//!
//! Why plain (not stealth) for the snapshot fetch? archive.org is a
//! friendly endpoint with no anti-bot. rquest's BoringSSL stack is heavier
//! than rustls; no need.

use crate::fetch::{
    extract, strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind,
    MarkdownResponse,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

pub struct Wayback;

#[derive(Debug, Deserialize)]
struct AvailableResp {
    archived_snapshots: Option<Snapshots>,
}

#[derive(Debug, Deserialize)]
struct Snapshots {
    closest: Option<Closest>,
}

#[derive(Debug, Deserialize)]
struct Closest {
    available: Option<bool>,
    url: Option<String>,
    status: Option<String>,
}

#[async_trait]
impl Strategy for Wayback {
    fn name(&self) -> &'static str {
        "wayback"
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
        // Loop guard.
        if let Some(host) = url.host_str() {
            let h = host.to_ascii_lowercase();
            if h == "web.archive.org" || h == "archive.org" || h.ends_with(".archive.org") {
                return Ok(None);
            }
        }

        // 1. Availability probe.
        let probe_url = build_probe_url(url)?;
        tracing::debug!("wayback: probing {probe_url}");
        // archive.org's `available` endpoint always returns JSON, but only
        // if we *don't* mention it in `Accept`. Sending `application/json`
        // makes some intermediaries 406 it; `*/*` is the safe pick.
        let probe = match fetcher.get(&probe_url, "*/*").await {
            Ok(b) if b.status < 400 => b,
            Ok(b) => {
                tracing::info!("wayback: probe http {}", b.status);
                return Ok(None);
            }
            Err(e) => {
                tracing::info!("wayback: probe error: {e}");
                return Ok(None);
            }
        };
        tracing::debug!(
            "wayback: probe ok status={} ct={:?} len={}",
            probe.status,
            probe.content_type,
            probe.bytes.len()
        );
        let parsed: AvailableResp = match serde_json::from_slice(&probe.bytes) {
            Ok(p) => p,
            Err(e) => {
                let prefix = String::from_utf8_lossy(
                    &probe.bytes[..probe.bytes.len().min(200)],
                );
                tracing::info!("wayback: probe json parse failed: {e}; prefix: {prefix:?}");
                return Ok(None);
            }
        };
        let Some(closest) = parsed.archived_snapshots.and_then(|s| s.closest) else {
            tracing::debug!("wayback: no snapshot for {url}");
            return Ok(None);
        };
        if !closest.available.unwrap_or(false) {
            tracing::debug!("wayback: snapshot not available for {url}");
            return Ok(None);
        }
        // We only trust 2xx snapshots: archived 4xx/5xx pages are usually
        // the same error we just got from the live origin.
        if let Some(s) = closest.status.as_deref() {
            if !s.starts_with('2') {
                tracing::debug!("wayback: closest snapshot status={s} for {url}, declining");
                return Ok(None);
            }
        }
        let Some(snapshot_url) = closest.url else {
            tracing::info!("wayback: snapshot record had no url");
            return Ok(None);
        };

        // 2. Rewrite to id_ form (raw-bytes mode, no toolbar injection).
        let raw_url = to_id_form(&snapshot_url).unwrap_or(snapshot_url);
        let raw_url = Url::parse(&raw_url).context("wayback snapshot url parse")?;
        tracing::info!("wayback: recovering {url} from {raw_url}");

        // 3. Fetch the snapshot.
        let body = fetcher.get(&raw_url, "text/html, */*").await?;
        if body.status >= 400 {
            tracing::info!("wayback: snapshot fetch http {}", body.status);
            return Ok(None);
        }
        if !body.is_html() && body.content_type.is_some() {
            tracing::info!("wayback: snapshot non-html ({:?})", body.content_type);
            return Ok(None);
        }

        // 4. Extract. Pass the original URL as the canonical so relative
        //    links resolve against the live host (archived pages embed the
        //    original `<base href>` already, but readability uses the URL
        //    we hand it for resolution).
        let original_url = url.clone();
        let html_bytes = body.bytes;
        let bytes_html = html_bytes.len();
        let content_type = body.content_type.clone();

        let canonical_for_extract = original_url.clone();
        let extracted = tokio::task::spawn_blocking(move || -> Result<_> {
            let primary = extract::html_to_markdown(
                &html_bytes,
                &canonical_for_extract,
                false,
                content_type.as_deref(),
            )?;
            if !extract::looks_like_stub(&primary.markdown) {
                return Ok(primary);
            }
            // Same auto-fallback as parse_html: archived pages often have
            // odd layout that readability mishandles.
            match extract::html_to_markdown(
                &html_bytes,
                &canonical_for_extract,
                true,
                content_type.as_deref(),
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
            source: FetchSource::Wayback,
            canonical_url: original_url,
            title: Some(extracted.title).filter(|t| !t.is_empty()),
            bytes_html: Some(bytes_html),
        }))
    }
}

fn build_probe_url(url: &Url) -> Result<Url> {
    // archive.org's `wayback/available` endpoint is unusually fussy: if
    // the `url` query param is fully RFC 3986 percent-encoded (e.g.
    // `https%3A%2F%2F...`) it silently returns `{"archived_snapshots":{}}`
    // even when a snapshot exists. Curl-style "raw" encoding works:
    //
    //     ?url=https://www.example.com/path
    //
    // We honor that by escaping only the characters that would otherwise
    // break query-string parsing: `#` (fragment delim), `&` (next pair),
    // ` ` (space), `+` (form-encoded space), and `%` (we never want a
    // double-decode interpretation).
    let raw = url.as_str();
    let mut escaped = String::with_capacity(raw.len() + 8);
    for ch in raw.chars() {
        match ch {
            '#' => escaped.push_str("%23"),
            '&' => escaped.push_str("%26"),
            '+' => escaped.push_str("%2B"),
            '%' => escaped.push_str("%25"),
            ' ' => escaped.push_str("%20"),
            c => escaped.push(c),
        }
    }
    Url::parse(&format!("https://archive.org/wayback/available?url={escaped}"))
        .context("wayback probe url")
}

/// Transform `https://web.archive.org/web/20240101120000/https://example.com/`
/// into the raw-bytes form
/// `https://web.archive.org/web/20240101120000id_/https://example.com/`.
///
/// The `id_` flag tells wayback to serve the original response body,
/// without rewriting links and without prepending its toolbar iframe.
fn to_id_form(snapshot: &str) -> Option<String> {
    let marker = "/web/";
    let i = snapshot.find(marker)?;
    let after = i + marker.len();
    let bytes = snapshot.as_bytes();
    let mut j = after;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    if j == after || j >= bytes.len() || bytes[j] != b'/' {
        return None;
    }
    Some(format!("{}id_{}", &snapshot[..j], &snapshot[j..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_form_inserts_correctly() {
        let s = "https://web.archive.org/web/20240101120000/https://example.com/path?q=1";
        assert_eq!(
            to_id_form(s).unwrap(),
            "https://web.archive.org/web/20240101120000id_/https://example.com/path?q=1"
        );
    }

    #[test]
    fn id_form_rejects_garbage() {
        assert!(to_id_form("https://example.com/").is_none());
        assert!(to_id_form("https://web.archive.org/web/").is_none());
    }

    #[test]
    fn probe_url_keeps_url_largely_unencoded() {
        // archive.org's API misbehaves on full RFC-3986 encoding; we must
        // leave `:` and `/` alone. Regression test for that quirk.
        let u = Url::parse("https://www.example.com/path/to/page").unwrap();
        let probe = build_probe_url(&u).unwrap();
        assert_eq!(
            probe.as_str(),
            "https://archive.org/wayback/available?url=https://www.example.com/path/to/page"
        );
    }

    #[test]
    fn probe_url_escapes_dangerous_chars() {
        // `&` would terminate the param; `#` would start a fragment.
        let u = Url::parse("https://example.com/?a=1&b=2#frag").unwrap();
        let probe = build_probe_url(&u).unwrap();
        assert!(probe.as_str().contains("a=1%26b=2%23frag"), "got: {probe}");
    }

    #[test]
    fn loop_guard_skips_archive_hosts() {
        for host in &["web.archive.org", "archive.org", "iframe.archive.org"] {
            let u = Url::parse(&format!("https://{host}/something")).unwrap();
            let h = u.host_str().unwrap().to_ascii_lowercase();
            let is_archive =
                h == "web.archive.org" || h == "archive.org" || h.ends_with(".archive.org");
            assert!(is_archive, "{host}");
        }
    }
}
