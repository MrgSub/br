//! npm adapter — package README via the registry API.
//!
//!   npmjs.com/package/<name>            → registry.npmjs.org/<name> → readme
//!   npmjs.com/package/<scope>/<name>    → ditto for scoped packages

use super::{ok_resp, Adapter};
use crate::fetch::{FetchOptions, Fetcher, MarkdownResponse};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

pub struct Npm;

#[async_trait]
impl Adapter for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some("npmjs.com" | "www.npmjs.com"))
            && url.path().starts_with("/package/")
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
        // ["package", name] or ["package", "@scope", name]
        let pkg = match segs.as_slice() {
            ["package", name] => (*name).to_string(),
            ["package", scope, name] if scope.starts_with('@') => format!("{scope}/{name}"),
            _ => return Ok(None),
        };

        // Registry expects %2F for the slash in scoped names.
        let registry_pkg = pkg.replace('/', "%2F");
        // /<pkg>/latest returns just the current version doc, which always
        // includes the README. The top-level /<pkg> doc has it as an aliased
        // field that is empty for many modern packages.
        let api = Url::parse(&format!("https://registry.npmjs.org/{registry_pkg}/latest"))?;
        let body = fetcher.get(&api, "application/json").await?;
        if body.status >= 400 {
            return Ok(None);
        }

        #[derive(Deserialize)]
        struct Pkg {
            #[serde(default)]
            readme: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            version: Option<String>,
            #[serde(default)]
            homepage: Option<String>,
        }
        let p: Pkg = serde_json::from_slice(&body.bytes)?;
        let latest = p.version.as_deref().unwrap_or("?");

        let mut md = format!("# {pkg}\n\n_npm package · v{latest}_\n\n");
        if let Some(d) = p.description.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(&format!("_{d}_\n\n"));
        }
        if let Some(h) = p.homepage.as_deref().filter(|s| !s.is_empty()) {
            md.push_str(&format!("**Homepage:** {h}\n\n"));
        }
        // Some popular packages (react, next, …) ship the README in the
        // tarball but not in the registry doc. Fall back to unpkg, which
        // serves files straight from any published version.
        let readme = match p.readme.as_deref().filter(|s| !s.is_empty()) {
            Some(r) => Some(r.to_string()),
            None => fetch_unpkg_readme(fetcher, &registry_pkg).await.ok().flatten(),
        };
        if let Some(r) = readme {
            md.push_str("---\n\n");
            md.push_str(&r);
            md.push('\n');
        } else {
            md.push_str("_(no README found in registry or unpkg)_\n");
        }
        Ok(ok_resp(md, url.clone(), Some(pkg)))
    }
}

async fn fetch_unpkg_readme(fetcher: &dyn Fetcher, pkg: &str) -> Result<Option<String>> {
    // `?meta=true` on directories doesn't apply here; unpkg serves the file
    // directly with a 302 redirect to the version-pinned URL.
    for fname in ["README.md", "readme.md", "Readme.md"] {
        let url = Url::parse(&format!("https://unpkg.com/{pkg}/{fname}"))?;
        let body = fetcher.get(&url, "text/plain, text/markdown, */*").await?;
        if body.status < 400 && !body.bytes.is_empty() {
            let ct = body.content_type.as_deref().unwrap_or("").to_ascii_lowercase();
            // unpkg serves a directory listing as text/html when the file is
            // missing; the README itself comes back as text/markdown or
            // text/plain. (READMEs frequently contain inline HTML like
            // `<div align="center">` so a leading-`<` check would reject
            // valid markdown.)
            if !ct.contains("html") {
                let s = String::from_utf8_lossy(&body.bytes).into_owned();
                return Ok(Some(s));
            }
        }
    }
    Ok(None)
}
