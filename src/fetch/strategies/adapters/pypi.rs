//! PyPI adapter — package description via the JSON API.
//!
//!   pypi.org/project/<name>/  → pypi.org/pypi/<name>/json → info.description

use super::{ok_resp, Adapter};
use crate::fetch::{FetchOptions, Fetcher, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

pub struct PyPi;

#[async_trait]
impl Adapter for PyPi {
    fn name(&self) -> &'static str {
        "pypi"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some("pypi.org" | "www.pypi.org"))
            && url.path().starts_with("/project/")
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
        let name = match segs.as_slice() {
            ["project", n] | ["project", n, _] => (*n).to_string(),
            _ => return Ok(None),
        };

        let api = Url::parse(&format!("https://pypi.org/pypi/{name}/json"))?;
        let body = fetcher.get(&api, "application/json").await?;
        if body.status >= 400 {
            return Ok(None);
        }

        #[derive(Deserialize)]
        struct Info {
            #[serde(default)]
            summary: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            description_content_type: Option<String>,
            #[serde(default)]
            version: Option<String>,
            #[serde(default)]
            home_page: Option<String>,
            #[serde(default)]
            project_url: Option<String>,
        }
        #[derive(Deserialize)]
        struct Doc {
            info: Info,
        }
        let d: Doc = serde_json::from_slice(&body.bytes)?;
        let info = d.info;

        let mut md = format!(
            "# {name}\n\n_PyPI · {ver}_\n\n",
            ver = info.version.as_deref().unwrap_or("?")
        );
        if let Some(s) = info.summary.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(&format!("_{s}_\n\n"));
        }
        let homepage = info.home_page.or(info.project_url);
        if let Some(h) = homepage.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(&format!("**Homepage:** {h}\n\n"));
        }

        let ct = info
            .description_content_type
            .as_deref()
            .unwrap_or("text/x-rst");
        if let Some(desc) = info.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("---\n\n");
            // PyPI hosts both markdown and RST. Markdown passes through; RST
            // we render as an indented preformatted block (better than nothing,
            // and it's often <1% of fetches).
            if ct.contains("markdown") {
                md.push_str(desc);
            } else {
                md.push_str("```rst\n");
                md.push_str(desc);
                md.push_str("\n```\n");
            }
            md.push('\n');
        }
        Ok(ok_resp(md, url.clone(), Some(name)))
    }
}
