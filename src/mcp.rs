//! MCP server — exposes `br` as Model Context Protocol tools over stdio.
//!
//! Run as `br mcp`. Auto-spawns the daemon, connects to it via the existing
//! Unix-socket protocol, and forwards requests as MCP tool calls.

use crate::cache::CacheHit;
use crate::client::round_trip_or_spawn;
use crate::fetch::FetchOptions;
use crate::proto::{FetchOkResp, FetchReq, Frame};
use crate::search::{SearchHit, SearchMode, SearchReq, TabReadReq};

use anyhow::Result;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const INSTRUCTIONS: &str = r#"
br is a "browser for agents" — fetch URLs as clean markdown and search across
cached results. Prefer it over raw HTTP fetching when you need:
  • lossless markdown for documentation, READMEs, articles, API docs
  • a stable cache so repeated lookups for the same URL are ~free
  • full-text + regex search across everything you've already fetched

Tools:
  • br_fetch     — fetch a URL, return markdown, cache by URL
  • br_search    — content search across cached pages
  • br_read_tab  — slice a section or line range from a cached page
  • br_cache_stats / br_cache_clear / br_cache_get

Tips:
  - For doc sites, fetch the *root* (e.g. https://nuxt.com/) — when the site
    publishes /llms-full.txt we cache the whole concatenated documentation
    in one shot, then `br_search` and `br_read_tab` make slicing cheap.
  - Cache TTL is 24h for canonical/API content, 1h for generic HTML parses.
    Pass `no_cache: true` only when you need a verified-fresh result.
"#;

// ─── Tool argument schemas ────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FetchArgs {
    /// The URL to fetch. Must be `http(s)://`.
    pub url: String,
    /// Skip cache lookup and force a fresh network fetch. Default false.
    #[serde(default)]
    pub no_cache: bool,
    /// Skip readability extraction; convert the entire page to markdown.
    /// Useful for table-heavy reference pages. Default false.
    #[serde(default)]
    pub raw: bool,
    /// Force JS rendering via WebKit (for SPAs).
    ///
    /// Default behavior is auto-escalation: the daemon tries cheap
    /// tiers first and only falls back to WebKit when they produce a
    /// stub or every tier errored. Set `headless: true` to bypass the
    /// cheap chain entirely; set `no_headless: true` to disable
    /// escalation.
    #[serde(default)]
    pub headless: bool,
    /// Disable auto-escalation to headless rendering. Useful when you
    /// want fast-fail behavior on a script-driven path. Ignored if
    /// `headless` is also true.
    #[serde(default)]
    pub no_headless: bool,
    /// CSS selector to wait for after the page becomes ready, before
    /// extracting. Up to 5 s. Headless-only (silently ignored on the
    /// cheap chain). Useful for tabs/dropdowns that hydrate late.
    #[serde(default)]
    pub wait_for: Option<String>,
    /// JS snippet to run after the page is ready (and any `wait_for`
    /// matched) but before extraction. Errors are swallowed. Headless-only.
    #[serde(default)]
    pub eval: Option<String>,
    /// Disable the baked-in cookie/consent dismissal hook. Default
    /// behavior tries to click common Accept / Agree / geo-gate
    /// buttons before extracting; set true to turn that off.
    #[serde(default)]
    pub no_auto_consent: bool,
    /// Disable the reference-link conversion. By default br rewrites
    /// inline `[text](url)` links to `[text][L7]` and emits a
    /// `## Links` table at the end — cuts agent token cost on
    /// link-heavy pages. Set true to keep classic inline links.
    #[serde(default)]
    pub no_link_table: bool,
    /// Cap output at approximately N tokens (1 token ≈ 4 chars).
    /// Drops the link table first, then truncates at the nearest
    /// heading boundary, and appends a `<!-- truncated by br -->`
    /// marker. Useful for keeping huge pages under a context budget.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Content query. Plain-text by default.
    pub query: String,
    /// Treat `query` as a regex. Mutually exclusive with `fuzzy`.
    #[serde(default)]
    pub regex: bool,
    /// Fuzzy/typo-tolerant match. Slower on large blobs (~5s on 4MB).
    /// Mutually exclusive with `regex`.
    #[serde(default)]
    pub fuzzy: bool,
    /// Max hits to return. Default 50.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Lines of context before each match. Default 0.
    #[serde(default)]
    pub before: Option<usize>,
    /// Lines of context after each match. Default 0.
    #[serde(default)]
    pub after: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadTabArgs {
    /// Tab id (ULID) returned by a previous `br_fetch`.
    pub tab_id: String,
    /// Inclusive 1-based line range, e.g. `[10, 50]`.
    #[serde(default)]
    pub lines: Option<[usize; 2]>,
    /// Heading text (case-insensitive substring). Returns the heading and
    /// all content until the next heading at the same or shallower depth.
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CacheGetArgs {
    /// URL to look up in the cache.
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CacheClearArgs {
    /// Also drop every tab row and `tabs/<id>.md` file. Default false
    /// (only the cache rows are removed; tab history survives).
    #[serde(default)]
    pub all: bool,
}

// ─── Tool result shapes (returned to the agent as JSON) ───────────────────

#[derive(Serialize)]
struct FetchResult {
    tab_id: String,
    canonical_url: String,
    title: Option<String>,
    source: String,
    markdown: String,
}

#[derive(Serialize)]
struct SearchResult {
    hits: Vec<SearchHit>,
    total_files_searched: usize,
    files_with_matches: usize,
    regex_fallback_error: Option<String>,
}



#[derive(Serialize)]
struct ReadTabResult {
    tab_id: String,
    url: String,
    title: Option<String>,
    total_lines: usize,
    returned_lines: (usize, usize),
    markdown: String,
}

// ─── Server handler ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BrMcp {
    tool_router: ToolRouter<Self>,
    agent_name: String,
}

impl BrMcp {
    pub fn new(agent_name: Option<String>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            agent_name: agent_name.unwrap_or_else(|| "mcp".to_string()),
        }
    }
}

impl Default for BrMcp {
    fn default() -> Self {
        Self::new(None)
    }
}

#[tool_router(router = tool_router)]
impl BrMcp {
    /// Fetch a URL through the strategy waterfall and return its markdown.
    /// Cached by URL: a follow-up call returns the same content instantly.
    #[tool(
        name = "br_fetch",
        description = "Fetch a URL and return clean markdown. Caches the result keyed by URL; subsequent calls within the TTL (24h for canonical sources, 1h for HTML parses) hit the cache."
    )]
    async fn br_fetch(&self, p: Parameters<FetchArgs>) -> String {
        let args = p.0;
        let req = Frame::Fetch(FetchReq {
            url: args.url,
            agent: Some(self.agent_name.clone()),
            options: FetchOptions {
                no_cache: args.no_cache,
                raw: args.raw,
                headless: if args.headless {
                    crate::fetch::HeadlessMode::On
                } else if args.no_headless {
                    crate::fetch::HeadlessMode::Off
                } else {
                    crate::fetch::HeadlessMode::Auto
                },
                wait_for: args.wait_for.clone(),
                eval_js: args.eval.clone(),
                auto_consent: !args.no_auto_consent,
                link_table: !args.no_link_table,
                max_tokens: args.max_tokens,
                ..Default::default()
            },
        });
        match round_trip_or_spawn(req).await {
            Ok(Frame::FetchOk(FetchOkResp {
                tab_id,
                markdown,
                source,
                canonical_url,
                title,
            })) => json_or_err(&FetchResult {
                tab_id,
                canonical_url,
                title,
                source: source.as_str().to_string(),
                markdown,
            }),
            Ok(Frame::Error { code, message }) => err_obj(&code, &message),
            Ok(other) => err_obj("unexpected_reply", &format!("{other:?}")),
            Err(e) => err_obj("transport", &e.to_string()),
        }
    }

    /// Full-text / regex / fuzzy search across cached tab markdowns.
    #[tool(
        name = "br_search",
        description = "Search the markdown of every cached page. Plain by default; pass regex=true or fuzzy=true. Returns matching lines with optional context."
    )]
    async fn br_search(&self, p: Parameters<SearchArgs>) -> String {
        let args = p.0;
        let mode = if args.regex {
            SearchMode::Regex
        } else if args.fuzzy {
            SearchMode::Fuzzy
        } else {
            SearchMode::Plain
        };
        let req = Frame::Search(SearchReq {
            query: args.query,
            mode,
            limit: args.limit.or(Some(50)),
            before_context: args.before,
            after_context: args.after,
            scan_wait_ms: None,
        });
        match round_trip_or_spawn(req).await {
            Ok(Frame::SearchOk(r)) => json_or_err(&SearchResult {
                hits: r.hits,
                total_files_searched: r.total_files_searched,
                files_with_matches: r.files_with_matches,
                regex_fallback_error: r.regex_fallback_error,
            }),
            Ok(Frame::Error { code, message }) => err_obj(&code, &message),
            Ok(other) => err_obj("unexpected_reply", &format!("{other:?}")),
            Err(e) => err_obj("transport", &e.to_string()),
        }
    }

    /// Slice a previously-fetched tab by section heading or line range.
    #[tool(
        name = "br_read_tab",
        description = "Read part of a tab. Pass `section` (a heading text fragment) to get that subtree, or `lines` as `[L, R]` (1-based inclusive). With neither, returns the full tab body."
    )]
    async fn br_read_tab(&self, p: Parameters<ReadTabArgs>) -> String {
        let args = p.0;
        let lines = args.lines.map(|[l, r]| (l, r));
        let req = Frame::TabRead(TabReadReq {
            tab_id: args.tab_id,
            lines,
            section: args.section,
        });
        match round_trip_or_spawn(req).await {
            Ok(Frame::TabReadOk(r)) => json_or_err(&ReadTabResult {
                tab_id: r.tab_id,
                url: r.url,
                title: r.title,
                total_lines: r.total_lines,
                returned_lines: r.returned_lines,
                markdown: r.markdown,
            }),
            Ok(Frame::Error { code, message }) => err_obj(&code, &message),
            Ok(other) => err_obj("unexpected_reply", &format!("{other:?}")),
            Err(e) => err_obj("transport", &e.to_string()),
        }
    }

    /// Cache statistics — useful for diagnosing.
    #[tool(
        name = "br_cache_stats",
        description = "Return URL→tab cache statistics: row counts (fresh / expired), on-disk markdown size, oldest/newest fetch timestamps."
    )]
    async fn br_cache_stats(&self) -> String {
        match round_trip_or_spawn(Frame::CacheStats).await {
            Ok(Frame::CacheStatsResp(s)) => json_or_err(&s),
            Ok(Frame::Error { code, message }) => err_obj(&code, &message),
            Ok(other) => err_obj("unexpected_reply", &format!("{other:?}")),
            Err(e) => err_obj("transport", &e.to_string()),
        }
    }

    /// Drop cache entries.
    #[tool(
        name = "br_cache_clear",
        description = "Drop cache rows. With all=true, also delete every tab row and on-disk markdown file (full reset)."
    )]
    async fn br_cache_clear(&self, p: Parameters<CacheClearArgs>) -> String {
        let req = Frame::CacheClear {
            also_drop_tabs: p.0.all,
        };
        match round_trip_or_spawn(req).await {
            Ok(Frame::CacheClearResp(r)) => json_or_err(&r),
            Ok(Frame::Error { code, message }) => err_obj(&code, &message),
            Ok(other) => err_obj("unexpected_reply", &format!("{other:?}")),
            Err(e) => err_obj("transport", &e.to_string()),
        }
    }

    /// Inspect the cache row for one URL.
    #[tool(
        name = "br_cache_get",
        description = "Return the cache row for a URL (tab id, source, title, age, expiry). Null if the URL has never been fetched."
    )]
    async fn br_cache_get(&self, p: Parameters<CacheGetArgs>) -> String {
        match round_trip_or_spawn(Frame::CacheGet { url: p.0.url }).await {
            Ok(Frame::CacheGetResp { hit }) => json_or_err::<Option<CacheHit>>(&hit),
            Ok(Frame::Error { code, message }) => err_obj(&code, &message),
            Ok(other) => err_obj("unexpected_reply", &format!("{other:?}")),
            Err(e) => err_obj("transport", &e.to_string()),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BrMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut imp = Implementation::from_build_env();
        imp.name = "br".to_string();
        imp.version = env!("CARGO_PKG_VERSION").to_string();
        info.server_info = imp;
        info.instructions = Some(INSTRUCTIONS.trim().to_string());
        info
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn json_or_err<T: Serialize>(v: &T) -> String {
    match serde_json::to_string(v) {
        Ok(s) => s,
        Err(e) => err_obj("serialize", &e.to_string()),
    }
}

fn err_obj(code: &str, message: &str) -> String {
    serde_json::json!({ "error": { "code": code, "message": message } }).to_string()
}

// ─── Entry point ──────────────────────────────────────────────────────────

pub async fn run(agent_name: Option<String>) -> Result<()> {
    let server = BrMcp::new(agent_name);
    let transport = stdio();
    let running = server.serve(transport).await?;
    running.waiting().await?;
    Ok(())
}
