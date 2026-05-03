//! GitHub adapter.
//!
//! Recognises:
//!   * `github.com/<owner>/<repo>`            → README via API
//!   * `github.com/<owner>/<repo>/blob/<ref>/<path>` → raw file (preserves md)
//!   * `github.com/<owner>/<repo>/issues/<n>` → issue title+body via API
//!   * `github.com/<owner>/<repo>/pull/<n>`   → PR title+body via API
//!
//! Auth: if `BR_GITHUB_TOKEN` is set we use it (lifts rate limit 60→5000/h).

use super::{ok_resp, Adapter};
use crate::fetch::{FetchOptions, Fetcher, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

pub struct GitHub;

#[async_trait]
impl Adapter for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some("github.com" | "www.github.com"))
    }

    async fn fetch(
        &self,
        url: &Url,
        _opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        let segs: Vec<&str> = url
            .path_segments()
            .map(|p| p.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        match segs.as_slice() {
            // /<owner>/<repo>
            [owner, repo] => fetch_readme(fetcher, owner, repo).await,
            // /<owner>/<repo>/blob/<ref>/<path...>
            [owner, repo, "blob", reff, rest @ ..] => {
                fetch_blob(fetcher, owner, repo, reff, rest).await
            }
            // /<owner>/<repo>/issues/<n> or /pull/<n>
            [owner, repo, kind @ ("issues" | "pull"), n] if n.chars().all(|c| c.is_ascii_digit()) => {
                let path_kind = if *kind == "pull" { "pulls" } else { "issues" };
                fetch_issue(fetcher, owner, repo, path_kind, n).await
            }
            _ => Ok(None),
        }
    }
}

fn api_url(path: &str) -> Url {
    Url::parse(&format!("https://api.github.com{path}")).expect("valid github api url")
}

async fn api_get_json<T: for<'de> Deserialize<'de>>(
    fetcher: &dyn Fetcher,
    url: &Url,
) -> Result<Option<T>> {
    let body = fetcher.get(url, "application/vnd.github+json").await?;
    if body.status >= 400 {
        anyhow::bail!("github api {}: {}", body.status, url);
    }
    let parsed: T = serde_json::from_slice(&body.bytes)?;
    Ok(Some(parsed))
}

async fn fetch_readme(
    fetcher: &dyn Fetcher,
    owner: &str,
    repo: &str,
) -> Result<Option<MarkdownResponse>> {
    #[derive(Deserialize)]
    struct Readme {
        content: String,
        encoding: String,
        html_url: String,
    }
    let url = api_url(&format!("/repos/{owner}/{repo}/readme"));
    let Some(readme) = api_get_json::<Readme>(fetcher, &url).await? else {
        return Ok(None);
    };
    if readme.encoding != "base64" {
        anyhow::bail!("unexpected readme encoding: {}", readme.encoding);
    }
    use base64::{engine::general_purpose::STANDARD, Engine};
    let bytes = STANDARD.decode(readme.content.replace('\n', ""))?;
    let markdown = String::from_utf8_lossy(&bytes).into_owned();
    let canonical = Url::parse(&readme.html_url).unwrap_or_else(|_| url);
    Ok(ok_resp(
        markdown,
        canonical,
        Some(format!("{owner}/{repo} README")),
    ))
}

async fn fetch_blob(
    fetcher: &dyn Fetcher,
    owner: &str,
    repo: &str,
    reff: &str,
    rest: &[&str],
) -> Result<Option<MarkdownResponse>> {
    let path = rest.join("/");
    let raw = format!("https://raw.githubusercontent.com/{owner}/{repo}/{reff}/{path}");
    let raw_url = Url::parse(&raw)?;
    let body = fetcher.get(&raw_url, "text/plain, */*").await?;
    if body.status >= 400 {
        return Ok(None);
    }
    let markdown = String::from_utf8_lossy(&body.bytes).into_owned();
    let title = format!("{owner}/{repo}: {path}");
    Ok(ok_resp(markdown, body.canonical_url, Some(title)))
}

async fn fetch_issue(
    fetcher: &dyn Fetcher,
    owner: &str,
    repo: &str,
    kind: &str, // "issues" | "pulls"
    n: &str,
) -> Result<Option<MarkdownResponse>> {
    #[derive(Deserialize)]
    struct Issue {
        number: u64,
        title: String,
        body: Option<String>,
        state: String,
        html_url: String,
        user: Option<User>,
    }
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let url = api_url(&format!("/repos/{owner}/{repo}/{kind}/{n}"));
    let Some(issue) = api_get_json::<Issue>(fetcher, &url).await? else {
        return Ok(None);
    };
    let author = issue.user.as_ref().map(|u| u.login.as_str()).unwrap_or("?");
    let body = issue.body.as_deref().unwrap_or("_(no body)_");
    let kind_label = if kind == "pulls" { "PR" } else { "Issue" };
    let markdown = format!(
        "# {title}\n\n_{kind_label} #{n} · {state} · @{author}_\n\n{body}\n",
        title = issue.title,
        n = issue.number,
        state = issue.state,
    );
    let canonical = Url::parse(&issue.html_url).unwrap_or_else(|_| url);
    Ok(ok_resp(markdown, canonical, Some(issue.title)))
}
