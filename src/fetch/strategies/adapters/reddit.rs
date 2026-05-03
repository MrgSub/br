//! Reddit adapter — uses the public `.json` endpoints.
//!
//!   /r/<sub>                  → top posts of subreddit
//!   /r/<sub>/comments/<id>/.. → post + comment thread

use super::{ok_resp, Adapter};
use crate::fetch::{FetcherKind, FetchOptions, Fetcher, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

pub struct Reddit;

#[async_trait]
impl Adapter for Reddit {
    fn name(&self) -> &'static str {
        "reddit"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(
            url.host_str(),
            Some("reddit.com" | "www.reddit.com" | "old.reddit.com" | "new.reddit.com")
        )
    }

    fn fetcher_kind(&self) -> FetcherKind {
        // Reddit's anti-bot is moderate; stealth helps. Plain works in most
        // cases too but we choose reliability.
        FetcherKind::Stealth
    }

    async fn fetch(
        &self,
        url: &Url,
        _opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        // Build the JSON URL. Reddit appends `.json` *before* any query string.
        let mut path = url.path().trim_end_matches('/').to_string();
        if !path.ends_with(".json") {
            path.push_str(".json");
        }
        let mut json_url = url.clone();
        json_url.set_host(Some("www.reddit.com"))?;
        json_url.set_path(&path);

        let body = fetcher.get(&json_url, "application/json").await?;
        if body.status >= 400 {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&body.bytes)?;

        let segs: Vec<&str> = url
            .path_segments()
            .map(|p| p.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let is_thread = segs.len() >= 4 && segs[0] == "r" && segs[2] == "comments";

        let canonical = url.clone();
        if is_thread {
            Ok(render_thread(&v, canonical))
        } else {
            let sub = segs.get(1).copied().unwrap_or("frontpage");
            Ok(render_listing(&v, sub, canonical))
        }
    }
}

#[derive(Deserialize)]
struct PostData {
    title: String,
    selftext: Option<String>,
    url: Option<String>,
    permalink: Option<String>,
    score: Option<i64>,
    num_comments: Option<u64>,
    author: Option<String>,
    subreddit: Option<String>,
}

fn render_listing(v: &Value, sub: &str, canonical: Url) -> Option<MarkdownResponse> {
    let children = v
        .get("data")?
        .get("children")?
        .as_array()?
        .iter()
        .filter_map(|c| serde_json::from_value::<PostData>(c.get("data")?.clone()).ok())
        .collect::<Vec<_>>();
    if children.is_empty() {
        return None;
    }
    let mut md = format!("# r/{sub}\n\n");
    for (i, p) in children.iter().enumerate() {
        let score = p.score.unwrap_or(0);
        let n_c = p.num_comments.unwrap_or(0);
        let author = p.author.as_deref().unwrap_or("?");
        let permalink = p
            .permalink
            .as_deref()
            .map(|s| format!("https://reddit.com{s}"))
            .unwrap_or_default();
        let link = p.url.clone().unwrap_or_else(|| permalink.clone());
        md.push_str(&format!(
            "{i}. [{title}]({link}) — {score}↑ · {n_c} comments · u/{author}\n",
            i = i + 1,
            title = p.title,
        ));
        if !permalink.is_empty() && permalink != link {
            md.push_str(&format!("   thread: {permalink}\n"));
        }
    }
    ok_resp(md, canonical, Some(format!("r/{sub}")))
}

fn render_thread(v: &Value, canonical: Url) -> Option<MarkdownResponse> {
    // /r/x/comments/y.json returns [post_listing, comments_listing].
    let arr = v.as_array()?;
    let post_data = arr.first()?.get("data")?.get("children")?.as_array()?.first()?
        .get("data")?
        .clone();
    let post: PostData = serde_json::from_value(post_data).ok()?;

    let mut md = format!(
        "# {title}\n\n_r/{sub} · {score}↑ · {n}c · u/{author}_\n\n",
        title = post.title,
        sub = post.subreddit.as_deref().unwrap_or("?"),
        score = post.score.unwrap_or(0),
        n = post.num_comments.unwrap_or(0),
        author = post.author.as_deref().unwrap_or("?"),
    );
    if let Some(body) = post.selftext.as_deref().filter(|s| !s.is_empty()) {
        md.push_str(body);
        md.push_str("\n\n");
    } else if let Some(link) = post.url.as_deref() {
        md.push_str(&format!("**Link:** {link}\n\n"));
    }

    md.push_str("---\n\n## Comments\n\n");
    if let Some(comments) = arr.get(1).and_then(|c| c.get("data")?.get("children").cloned()) {
        if let Some(arr) = comments.as_array() {
            walk_comments(arr, 0, &mut md);
        }
    }
    ok_resp(md, canonical, Some(post.title))
}

fn walk_comments(children: &[Value], depth: usize, out: &mut String) {
    for c in children {
        let Some(data) = c.get("data") else { continue };
        let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "t1" {
            continue; // skip "more" markers
        }
        let author = data.get("author").and_then(|v| v.as_str()).unwrap_or("?");
        let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let score = data.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
        let indent = "> ".repeat(depth);
        for line in body.lines() {
            out.push_str(&format!("{indent}**u/{author}** ({score}↑): {line}\n"));
        }
        out.push_str(&format!("{indent}\n"));
        if let Some(replies) = data.get("replies") {
            if let Some(arr) = replies.get("data").and_then(|d| d.get("children")?.as_array()) {
                walk_comments(arr, depth + 1, out);
            }
        }
    }
}
