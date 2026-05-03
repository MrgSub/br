//! Hacker News adapter — uses Algolia's HN API.
//!
//!   news.ycombinator.com/item?id=N → hn.algolia.com/api/v1/items/N
//!   news.ycombinator.com           → top stories via firebase API
//!   news.ycombinator.com/news      → same
//!   news.ycombinator.com/newest    → latest

use super::{ok_resp, Adapter};
use crate::fetch::{FetchOptions, Fetcher, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

pub struct HackerNews;

#[async_trait]
impl Adapter for HackerNews {
    fn name(&self) -> &'static str {
        "hackernews"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some("news.ycombinator.com"))
    }

    async fn fetch(
        &self,
        url: &Url,
        _opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        if url.path().starts_with("/item") {
            let id = url
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned());
            let Some(id) = id else { return Ok(None) };
            return fetch_item(fetcher, &id, url.clone()).await;
        }
        // Front pages — fall through for now (the HTML page is server-rendered
        // and parses fine via parse_html).
        Ok(None)
    }
}

#[derive(Deserialize)]
struct HnItem {
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    children: Vec<Value>,
}

async fn fetch_item(
    fetcher: &dyn Fetcher,
    id: &str,
    canonical: Url,
) -> Result<Option<MarkdownResponse>> {
    let api = Url::parse(&format!("https://hn.algolia.com/api/v1/items/{id}"))?;
    let body = fetcher.get(&api, "application/json").await?;
    if body.status >= 400 {
        return Ok(None);
    }
    let item: HnItem = serde_json::from_slice(&body.bytes)?;

    let mut md = String::new();
    let title = item.title.clone().unwrap_or_else(|| format!("HN item {id}"));
    md.push_str(&format!("# {title}\n\n"));
    md.push_str(&format!(
        "_HN #{id} · {points}↑ · by {author}_\n\n",
        id = item.id,
        points = item.points.unwrap_or(0),
        author = item.author.as_deref().unwrap_or("?"),
    ));
    if let Some(link) = &item.url {
        md.push_str(&format!("**Link:** {link}\n\n"));
    }
    if let Some(text) = item.text.as_deref().filter(|t| !t.is_empty()) {
        md.push_str(&strip_html(text));
        md.push_str("\n\n");
    }
    md.push_str("---\n\n## Comments\n\n");
    walk(&item.children, 0, &mut md);
    Ok(ok_resp(md, canonical, Some(title)))
}

fn walk(children: &[Value], depth: usize, out: &mut String) {
    for c in children {
        let author = c.get("author").and_then(|v| v.as_str()).unwrap_or("?");
        let text = c
            .get("text")
            .and_then(|v| v.as_str())
            .map(strip_html)
            .unwrap_or_default();
        let indent = "> ".repeat(depth);
        if !text.is_empty() {
            for line in text.lines() {
                out.push_str(&format!("{indent}**{author}**: {line}\n"));
            }
            out.push_str(&format!("{indent}\n"));
        }
        if let Some(arr) = c.get("children").and_then(|v| v.as_array()) {
            walk(arr, depth + 1, out);
        }
    }
}

/// HN comments come as HTML. Cheap stripper good enough for an MD context.
fn strip_html(s: &str) -> String {
    // Replace <p> with double newline, <br> with single, drop everything else.
    let s = s.replace("<p>", "\n\n").replace("</p>", "");
    let s = s.replace("<br>", "\n").replace("<br/>", "\n");
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_escape::decode_html_entities(&out).into_owned()
}
