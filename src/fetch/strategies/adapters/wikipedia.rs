//! Wikipedia adapter — uses the REST v1 page summary + html endpoints.
//!
//!   `*.wikipedia.org/wiki/<title>` → `<lang>.wikipedia.org/api/rest_v1/page/html/<title>`

use super::{ok_resp, Adapter};
use crate::fetch::{FetchOptions, Fetcher, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

pub struct Wikipedia;

#[async_trait]
impl Adapter for Wikipedia {
    fn name(&self) -> &'static str {
        "wikipedia"
    }

    fn matches(&self, url: &Url) -> bool {
        url.host_str()
            .map(|h| h.ends_with(".wikipedia.org"))
            .unwrap_or(false)
    }

    async fn fetch(
        &self,
        url: &Url,
        _opts: &FetchOptions,
        fetcher: &dyn Fetcher,
    ) -> Result<Option<MarkdownResponse>> {
        let host = url.host_str().unwrap_or("en.wikipedia.org");
        let mut iter = url.path_segments().map(|s| s.collect::<Vec<_>>()).unwrap_or_default();
        // Expecting ["wiki", "<title>"].
        if iter.first().copied() != Some("wiki") || iter.len() < 2 {
            return Ok(None);
        }
        iter.remove(0);
        let title = iter.join("/");

        let api = Url::parse(&format!(
            "https://{host}/api/rest_v1/page/html/{title}",
            title = title
        ))?;
        let body = fetcher.get(&api, "text/html").await?;
        if body.status >= 400 {
            return Ok(None);
        }
        let html = String::from_utf8_lossy(&body.bytes).into_owned();

        // The REST html is clean enough to skip readability — convert directly.
        let canonical = url.clone();
        let markdown = tokio::task::spawn_blocking(move || -> Result<String> {
            Ok(htmd::HtmlToMarkdown::builder()
                .skip_tags(vec![
                    "script", "style", "noscript", "table.metadata", "sup.reference",
                ])
                .build()
                .convert(&html)
                .map_err(|e| anyhow::anyhow!("htmd: {e}"))?)
        })
        .await??;

        let title_pretty = title.replace('_', " ");
        Ok(ok_resp(markdown, canonical, Some(title_pretty)))
    }
}
