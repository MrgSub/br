use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

/// Output format for `br fetch` results.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Plain markdown to stdout (default). Meta lines, if requested
    /// via `--meta`, go to stderr in `# key value` shape.
    Markdown,
    /// JSON envelope with `{ markdown, source, canonical_url, title,
    /// quality, bytes_html }`. Goes to stdout in one piece; agents
    /// that want typed access to the link table or source tier should
    /// use this.
    Json,
}

mod cache;
mod client;
mod daemon;
mod dashboard;
mod mcp;
mod webkit;
mod db;
mod fetch;
mod paths;
mod proto;
mod registry;
mod search;

const LONG_ABOUT: &str = r#"
Browser for agents — fetch URLs as clean markdown, then search/slice them.

The core loop:
  br fetch <url>                fetch + cache + return markdown
  br search "<query>"           search across all cached pages
  br tab <id> --section "..."   pull one section out of a cached page
  br tab <id> --lines L:R       pull a line range

All fetches go through a waterfall of strategies, cheapest first:

  1. accept_md      Accept: text/markdown content negotiation
  2. cloudflare     URL+.md suffix (Cloudflare auto-markdown)
  3. llms_txt       /llms.txt index lookup; /llms-full.txt for site root
  4. adapters       github, wikipedia, reddit, hn, npm, pypi (API-direct)
  5. parse_html     readability + html-to-markdown (generic fallback)

The `source` of each result is recorded; check it via `br fetch URL --meta`.

HIGH-LEVERAGE PATTERN — docs sites with `llms-full.txt`

  Some sites (e.g. nuxt.com) publish a single concatenated `/llms-full.txt`
  with their full documentation. Fetch it once, then keep searching:

      br fetch https://nuxt.com/                     # ~2s, ~4 MB cached
      br search "useFetch" --before 1 --after 2      # ~50ms
      br tab <tab_id> --section "useFetch"           # ~50ms

  Fetching the *root* of a site triggers `llms_full` if available, or
  `llms_index` if only `/llms.txt` is published. After that, all queries
  hit local disk and run in well under 100 ms even on multi-MB blobs.

USEFUL EXAMPLES

  # Server-authoritative markdown:
  br fetch https://blog.cloudflare.com/markdown-for-agents/

  # API-direct (no parsing):
  br fetch https://github.com/better-auth/better-auth
  br fetch https://en.wikipedia.org/wiki/Rust_(programming_language)
  br fetch https://www.npmjs.com/package/react

  # Generic parse + raw mode (skip readability for tabular pages):
  br fetch --raw https://lite.duckduckgo.com/lite/?q=sushi+vancouver

  # Search across everything cached, with context:
  br search "authentication" --before 1 --after 1 --limit 10
  br search --regex '\(\d{3}\) ?\d{3}[-. ]?\d{4}'   # phone numbers
  br search --fuzzy "asyncdata"                       # typo-tolerant

  # Slice a cached page:
  br tab 01K… --section "Authentication"
  br tab 01K… --lines 100:160

FILES

  ~/Library/Application Support/dev.br.br/
    br.sqlite       registry: agents, tabs, history, fetch_cache
    tabs/<id>.md    one file per cached tab (indexed by fff-search)
    index/          fff-search frecency + query-tracker dbs
    br.sock         daemon Unix socket
    br.pid          single-instance lockfile

The daemon auto-spawns when needed; `br daemon stop` to terminate.
"#;

#[derive(Parser)]
#[command(
    name = "br",
    about = "Browser for agents — URLs to clean, searchable markdown",
    long_about = LONG_ABOUT,
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon lifecycle.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Show daemon status (alias for `daemon status`).
    Status,
    /// Ping the daemon (smoke test).
    Ping,
    /// Search across cached tab markdowns.
    ///
    /// Powered by fff-search (ripgrep-class). On a single 4 MB cached page
    /// (e.g. nuxt.com's llms-full.txt), plain & regex queries return in
    /// ~50 ms; fuzzy is slower (~5 s) so prefer plain/regex on large blobs.
    ///
    /// Tip: combine with `br fetch <site-root>` to bulk-cache a site's
    /// llms-full.txt once, then keep querying with --before/--after for
    /// surrounding context.
    Search {
        /// Query string. With `--regex`, treated as a regex; with `--fuzzy`,
        /// fuzzy-matched.
        query: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        fuzzy: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        before: usize,
        #[arg(long, default_value_t = 0)]
        after: usize,
        /// Output as JSON instead of pretty text.
        #[arg(long)]
        json: bool,
    },
    /// Read (a slice of) a previously-fetched tab.
    ///
    /// `--section "X"` finds the first heading whose text contains X
    /// (case-insensitive) and returns it plus all content until the next
    /// heading at the same or shallower depth. Faster than re-fetching and
    /// far cheaper for the agent context window.
    ///
    /// `--lines L:R` returns a 1-based inclusive line range.
    Tab {
        tab_id: String,
        /// `L:R` (1-based, inclusive). E.g. `10:50`.
        #[arg(long)]
        lines: Option<String>,
        /// Return the section under this heading (case-insensitive substring).
        #[arg(long)]
        section: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Fetch a URL and print markdown to stdout.
    ///
    /// Tries the strategy waterfall (accept_md → cloudflare → llms_txt →
    /// adapters → parse_html). Whichever tier wins is recorded as the
    /// tab's `source`; see `--meta` to inspect it.
    ///
    /// Fetching a site root URL is the high-leverage case — it triggers
    /// `llms_full` for sites that publish `/llms-full.txt` (one fetch,
    /// indexed for cheap repeated `br search`).
    ///
    /// Results are cached by URL (24h for canonical/API tiers, 1h for
    /// generic HTML parse). Pass `--no-cache` to force a refetch.
    Fetch {
        url: String,
        /// Override the agent name (default: cli-ppid-<parent-pid>).
        #[arg(long, env = "BR_AGENT")]
        agent: Option<String>,
        /// Print fetch metadata (tab id, source, title) to stderr.
        #[arg(long)]
        meta: bool,
        /// Skip cache lookup.
        #[arg(long)]
        no_cache: bool,
        /// Skip readability extraction; convert the whole page.
        #[arg(long)]
        raw: bool,
        /// Force JS rendering via the WebKit worker (use for SPAs).
        /// Bypasses every cheaper tier; expect ~2–6s per fetch.
        ///
        /// Default behavior is `auto`: cheap tiers run first, and we
        /// escalate to headless only if they produce a stub or hang.
        /// Pass `--no-headless` to opt out of escalation entirely.
        #[arg(long, conflicts_with = "no_headless")]
        headless: bool,
        /// Disable the auto-escalation to headless rendering.
        /// Useful in scripts where the cheap-tier output is enough and
        /// you want to fail fast rather than spin up a renderer.
        #[arg(long)]
        no_headless: bool,
        /// (Phase 4) Wait for this CSS selector to appear after the
        /// page is otherwise ready, before extracting. Up to 5 s.
        /// Headless-only.
        #[arg(long, value_name = "CSS")]
        wait_for: Option<String>,
        /// (Phase 4) JS snippet to run after the page is ready and any
        /// `--wait-for` matched. Errors are swallowed. Headless-only.
        #[arg(long, value_name = "JS")]
        eval: Option<String>,
        /// Disable the baked-in cookie/consent dismissal hook. Default
        /// behavior tries to click common Accept / Agree / geo-gate
        /// buttons before extracting; this flag turns that off.
        #[arg(long)]
        no_auto_consent: bool,
        /// Disable the reference-link conversion. By default br rewrites
        /// inline `[text](url)` links to `[text][L7]` and emits a
        /// `## Links` table at the end — cuts agent token cost on
        /// link-heavy pages. This flag returns to classic inline-link
        /// markdown.
        #[arg(long)]
        no_link_table: bool,
        /// Cap output at approximately N tokens (1 token ≈ 4 chars).
        /// Drops the link table first, then truncates the body at the
        /// nearest heading boundary, and appends a comment showing how
        /// many tokens were omitted. Useful for fitting big pages into
        /// a context window.
        #[arg(long, value_name = "N")]
        max_tokens: Option<u32>,
        /// Output format: `markdown` (default) or `json`.
        ///
        /// `json` returns `{ markdown, source, canonical_url, title,
        /// quality, bytes_html }` for clients that want typed access.
        /// Implies meta-on-stdout (the meta lines are inside the JSON).
        #[arg(long, value_enum, default_value_t = OutputFormat::Markdown)]
        format: OutputFormat,
    },
    /// Inspect or clear the URL→tab cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Run the MCP server on stdio.
    ///
    /// Configure your MCP-capable client (Claude Code, Cursor, …) to spawn
    /// `br mcp`. The daemon is auto-spawned on first tool call. Tools:
    /// br_fetch, br_search, br_read_tab, br_cache_stats, br_cache_clear,
    /// br_cache_get.
    ///
    /// `--agent NAME` attributes every tool call to that agent in the
    /// registry/dashboard. Defaults to `mcp`. Useful when distinguishing
    /// multiple MCP clients (e.g. `claude-code` vs `cursor`).
    Mcp {
        #[arg(long)]
        agent: Option<String>,
    },
    /// Open the GPUI dashboard.
    ///
    /// Three-pane window showing every agent, every tab they've fetched,
    /// and the markdown body of whichever tab you click. Auto-spawns the
    /// daemon if needed. Polls the registry every second.
    Dashboard,
    /// (Internal) Run as the WebKit worker subprocess. Spawned by the
    /// daemon when a fetch needs JS rendering. Reads `WebKitReq` frames on
    /// stdin, writes `WebKitResp` frames on stdout. Not meant to be invoked
    /// by humans.
    #[command(hide = true)]
    WebkitWorker,
    /// Manage named sessions (= agents in the registry).
    ///
    /// Sessions persist forever; this is a naming layer for attribution
    /// in the dashboard and `br tabs` queries. Typical use:
    ///
    ///     eval "$(br session start --name claude-x)"
    ///     # subsequent `br fetch` calls attribute to claude-x
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// Ensure a session exists and emit `export BR_AGENT=NAME` for shell
    /// eval. Re-running with the same `--name` is a no-op (idempotent).
    Start {
        #[arg(long)]
        name: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List every agent with tab/ready counts and last-seen ages.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print the current `BR_AGENT` env value (or non-zero exit if unset).
    Current {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show row counts and on-disk size.
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Drop cache rows (URLs become refetchable). With `--all`, also nuke
    /// every tab row and every `tabs/<id>.md` file.
    Clear {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show the cache row for one URL.
    Get {
        url: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon in the foreground.
    Start {
        /// Don't open the dashboard window (no-op for now; UI lands in M3).
        #[arg(long)]
        no_window: bool,
    },
    /// Stop a running daemon.
    Stop,
    /// Show daemon status.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Tracing must go to stderr in MCP mode (stdout = JSON-RPC channel).
    let mcp_mode = matches!(cli.cmd, Some(Cmd::Mcp { .. }));
    init_tracing(mcp_mode);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Dashboard takes over the main thread (GPUI requires it on macOS),
    // so don't spawn the tokio runtime for these cases.
    if matches!(cli.cmd, None | Some(Cmd::Dashboard)) {
        return dashboard::run();
    }
    // The WebKit worker also needs the main thread (NSApplication run
    // loop, no tokio runtime).
    if matches!(cli.cmd, Some(Cmd::WebkitWorker)) {
        return webkit::worker::run();
    }

    rt.block_on(async {
        match cli.cmd {
            None | Some(Cmd::Dashboard) => unreachable!("handled above"),
            Some(Cmd::Daemon { action }) => match action {
                DaemonAction::Start { no_window } => {
                    daemon::run(daemon::Options {
                        with_window: !no_window,
                    })
                    .await
                }
                DaemonAction::Stop => client::stop_daemon().await,
                DaemonAction::Status => client::print_status().await,
            },
            Some(Cmd::Status) => client::print_status().await,
            Some(Cmd::Ping) => client::ping().await,
            Some(Cmd::Fetch {
                url,
                agent,
                meta,
                no_cache,
                raw,
                headless,
                no_headless,
                wait_for,
                eval,
                no_auto_consent,
                no_link_table,
                max_tokens,
                format,
            }) => {
                let mode = if headless {
                    fetch::HeadlessMode::On
                } else if no_headless {
                    fetch::HeadlessMode::Off
                } else {
                    fetch::HeadlessMode::Auto
                };
                let opts = fetch::FetchOptions {
                    no_cache,
                    raw,
                    headless: mode,
                    wait_for,
                    eval_js: eval,
                    auto_consent: !no_auto_consent,
                    link_table: !no_link_table,
                    max_tokens,
                    ..Default::default()
                };
                client::fetch(url, agent, opts, meta, format).await
            }
            Some(Cmd::Search {
                query,
                regex,
                fuzzy,
                limit,
                before,
                after,
                json,
            }) => {
                let mode = if regex {
                    search::SearchMode::Regex
                } else if fuzzy {
                    search::SearchMode::Fuzzy
                } else {
                    search::SearchMode::Plain
                };
                let req = search::SearchReq {
                    query,
                    mode,
                    limit: Some(limit),
                    before_context: Some(before),
                    after_context: Some(after),
                    scan_wait_ms: None,
                };
                client::search(req, json).await
            }
            Some(Cmd::Mcp { agent }) => mcp::run(agent).await,
            Some(Cmd::WebkitWorker) => unreachable!("handled above"),
            Some(Cmd::Session { action }) => match action {
                SessionAction::Start { name, note, json } => {
                    client::session_start(name, note, json).await
                }
                SessionAction::List { json } => client::session_list(json).await,
                SessionAction::Current { json } => client::session_current(json),
            },
            Some(Cmd::Cache { action }) => match action {
                CacheAction::Stats { json } => client::cache_stats(json).await,
                CacheAction::Clear { all, json } => client::cache_clear(all, json).await,
                CacheAction::Get { url, json } => client::cache_get(url, json).await,
            },
            Some(Cmd::Tab {
                tab_id,
                lines,
                section,
                json,
            }) => {
                let line_range = lines
                    .as_deref()
                    .map(parse_line_range)
                    .transpose()?;
                let req = search::TabReadReq {
                    tab_id,
                    lines: line_range,
                    section,
                };
                client::tab_read(req, json).await
            }
        }
    })
}

fn parse_line_range(s: &str) -> anyhow::Result<(usize, usize)> {
    let (l, r) = s.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("--lines must be `L:R` (e.g. 10:50)")
    })?;
    Ok((l.parse()?, r.parse()?))
}

fn init_tracing(force_stderr: bool) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_env("BR_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let layer = fmt::layer().with_target(false);
    if force_stderr {
        tracing_subscriber::registry()
            .with(filter)
            .with(layer.with_writer(std::io::stderr))
            .init();
    } else {
        tracing_subscriber::registry().with(filter).with(layer).init();
    }
}
