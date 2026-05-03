//! Tier 3/4 — `llms.txt` discovery.
//!
//! Per <https://llmstxt.org>, sites can publish a curated index at
//! `/<origin>/llms.txt` listing the canonical markdown URLs an LLM should
//! prefer for that site. Some sites also publish `/llms-full.txt`, a single
//! concatenated dump of everything important.
//!
//! Strategy:
//!  1. On first encounter with a host, fetch `/<host>/llms.txt`.
//!  2. Parse it as the standard format (`# title`, `> blurb`,
//!     `## section` blocks of `- [title](url): desc` items).
//!  3. Cache the result per-host.
//!  4. For any subsequent fetch to a URL on that host, look for an entry
//!     whose URL exactly matches our target. If found, fetch *that* URL —
//!     it's the site's canonical markdown for the page we want.
//!  5. If the *root* URL of the host is requested and there's no per-page
//!     entry, return the parsed llms.txt itself as the answer (or
//!     `llms-full.txt` if it exists).

use crate::fetch::{
    strategies::Strategy, FetchOptions, FetchSource, Fetcher, FetcherKind, MarkdownResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

const TTL: Duration = Duration::from_secs(60 * 60); // 1h

#[derive(Clone)]
struct CachedIndex {
    entries: Vec<Entry>,
    has_full: bool,
    fetched_at: Instant,
}

#[derive(Clone, Debug)]
struct Entry {
    url: String,
    #[allow(dead_code)]
    title: String,
}

pub struct LlmsTxt {
    cache: Mutex<HashMap<String, Option<CachedIndex>>>,
}

impl LlmsTxt {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Strategy for LlmsTxt {
    fn name(&self) -> &'static str {
        "llms_txt"
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
        let Some(host) = url.host_str() else {
            return Ok(None);
        };
        let scheme = url.scheme();

        // Skip our own llms.txt fetches to avoid recursion.
        if matches!(url.path(), "/llms.txt" | "/llms-full.txt") {
            return Ok(None);
        }

        // Look up (and lazily populate) the cache.
        let cached = self.lookup(host);
        let index = match cached {
            Some(Some(idx)) if idx.fetched_at.elapsed() < TTL => idx,
            _ => match probe(fetcher, scheme, host).await? {
                Some(idx) => {
                    self.store(host, Some(idx.clone()));
                    idx
                }
                None => {
                    self.store(host, None);
                    return Ok(None);
                }
            },
        };

        // Match an entry whose URL equals ours (modulo trailing slashes).
        let want = canonical_match_key(url);
        let matched = index
            .entries
            .iter()
            .find(|e| canonical_match_key_str(&e.url) == want);

        if let Some(entry) = matched {
            // Fetch the entry's URL directly via accept-md style — most
            // llms.txt entries point at .md files on the same origin.
            let entry_url = Url::parse(&entry.url)?;
            let body = fetcher
                .get(&entry_url, "text/markdown, text/plain, */*")
                .await?;
            if body.status >= 400 {
                return Ok(None);
            }
            if body.looks_like_markdown() || body.is_text_plain() {
                let md = String::from_utf8_lossy(&body.bytes).into_owned();
                let title = first_h1(&md);
                return Ok(Some(MarkdownResponse {
                    markdown: md,
                    source: FetchSource::LlmsIndex,
                    canonical_url: body.canonical_url,
                    title,
                    bytes_html: None,
                }));
            }
            return Ok(None);
        }

        // Asked for the host root and there's no per-page entry → return
        // the index (or llms-full.txt if available) so the caller gets
        // *something* useful.
        if is_origin_root(url) {
            if index.has_full {
                let full_url = Url::parse(&format!("{scheme}://{host}/llms-full.txt"))?;
                let body = fetcher.get(&full_url, "text/plain, */*").await?;
                if body.status < 400 {
                    let md = String::from_utf8_lossy(&body.bytes).into_owned();
                    return Ok(Some(MarkdownResponse {
                        markdown: md,
                        source: FetchSource::LlmsFull,
                        canonical_url: full_url,
                        title: Some(format!("{host} (llms-full.txt)")),
                        bytes_html: None,
                    }));
                }
            }
            // Return the cached llms.txt body as the result.
            let llms_url = Url::parse(&format!("{scheme}://{host}/llms.txt"))?;
            let body = fetcher.get(&llms_url, "text/plain, */*").await?;
            if body.status < 400 {
                let md = String::from_utf8_lossy(&body.bytes).into_owned();
                return Ok(Some(MarkdownResponse {
                    markdown: md,
                    source: FetchSource::LlmsIndex,
                    canonical_url: llms_url,
                    title: Some(format!("{host} (llms.txt)")),
                    bytes_html: None,
                }));
            }
        }

        Ok(None)
    }
}

impl LlmsTxt {
    fn lookup(&self, host: &str) -> Option<Option<CachedIndex>> {
        self.cache.lock().ok()?.get(host).cloned()
    }
    fn store(&self, host: &str, val: Option<CachedIndex>) {
        if let Ok(mut c) = self.cache.lock() {
            c.insert(host.to_string(), val);
        }
    }
}

async fn probe(fetcher: &dyn Fetcher, scheme: &str, host: &str) -> Result<Option<CachedIndex>> {
    let llms_url = Url::parse(&format!("{scheme}://{host}/llms.txt"))?;
    let body = fetcher.get(&llms_url, "text/plain, */*").await?;
    if body.status >= 400 || !(body.is_text_plain() || body.looks_like_markdown()) {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&body.bytes).into_owned();
    if text.trim().is_empty() {
        return Ok(None);
    }
    let entries = parse_entries(&text);

    // Probe llms-full.txt with HEAD-equivalent (just a GET, we only look at status).
    let full_url = Url::parse(&format!("{scheme}://{host}/llms-full.txt"))?;
    let has_full = match fetcher.get(&full_url, "text/plain, */*").await {
        Ok(b) => b.status < 400 && b.is_text_plain(),
        Err(_) => false,
    };

    Ok(Some(CachedIndex {
        entries,
        has_full,
        fetched_at: Instant::now(),
    }))
}

/// Parse markdown bullets of the form `- [title](url)[: description]` from
/// every section in the file. Lenient — discards lines we don't understand.
fn parse_entries(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix('-').or_else(|| l.strip_prefix('*')) else {
            continue;
        };
        let rest = rest.trim_start();
        // Match `[title](url)`.
        let Some((title, after)) = take_bracketed(rest) else {
            continue;
        };
        let Some((url, _)) = take_paren(after) else {
            continue;
        };
        if !url.starts_with("http") {
            continue;
        }
        out.push(Entry {
            url: url.trim().to_string(),
            title: title.trim().to_string(),
        });
    }
    out
}

fn take_bracketed(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('[')?;
    let end = s.find(']')?;
    Some((&s[..end], &s[end + 1..]))
}

fn take_paren(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('(')?;
    let end = s.find(')')?;
    Some((&s[..end], &s[end + 1..]))
}

fn canonical_match_key(u: &Url) -> String {
    canonical_match_key_str(u.as_str())
}

fn canonical_match_key_str(s: &str) -> String {
    // Drop fragment, normalize trailing slash.
    let mut s = s.split('#').next().unwrap_or(s).to_string();
    if s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    s
}

fn is_origin_root(u: &Url) -> bool {
    u.path() == "/" || u.path().is_empty()
}

fn first_h1(md: &str) -> Option<String> {
    md.lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
}
