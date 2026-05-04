# br

Browser for agents. Fetch URLs as clean markdown, then search and slice
them. Built for LLM tool-use loops where context is scarce and "did the
fetch actually return real content?" matters more than "was the request
RFC-compliant?".

```sh
br fetch https://en.wikipedia.org/wiki/Rust_(programming_language)
# 180 KB of clean markdown to stdout
br fetch https://www.apartments.com/san-francisco-ca/
# WAF-walled site → automatic headless escalation → 2 KB of real listings
br fetch https://arxiv.org/pdf/1706.03762.pdf --max-tokens 8000
# 32 KB cap; truncated at heading boundary with a marker comment
```

## What's interesting about it

* A **strategy waterfall** that tries the cheapest tier first
  (`Accept: text/markdown`, Cloudflare's `.md` suffix, `llms.txt`,
  per-host adapters, generic HTML→markdown via readability+htmd, PDF
  text extraction, headless WKWebView render, Wayback Machine).
  The first tier that returns substantive content wins; everything
  else falls through.
* **Headless: auto** by default. When the cheap chain returns a stub
  (escalation-worthy by a tighter floor than the harness's pass
  predicate) or every tier errors/times out, the daemon spins up a
  WKWebView via a subprocess worker, runs JS-aware extraction with
  mutation-quiescence ready detection, and picks whichever output
  has more visible text.
* **Auto-consent dismissal.** A baked-in JS hook clicks common
  GDPR/cookie/geo-gate banners (OneTrust, TrustArc, Iubenda,
  Cookiebot, Didomi, plus text-match on `Accept` / `Agree` / `I'm a
  US Resident`). 8/8 on a major-EU-news eval set.
* **Bot-block detection.** Recognizes WAF rejection bodies
  (PerimeterX, Akamai, Imperva, Datadome, Cloudflare challenge) and
  surfaces them with a `> [!warning] **br:**` banner so agents can
  tell rejection from content.
* **Worker hardening.** Idle timeout (10 min → ~150 MB RSS reclaimed),
  navigation cap (200 renders → recycle), parent-pid watch (worker
  self-exits if daemon is `kill -9`'d), separate `webkit.log`.
* **Token-budget output.** `--max-tokens N` smart-truncates at heading
  boundaries (UTF-8 safe). Reference-style link table at the end
  dedupes repeated URLs into `[L7]`-style refs (up to 17.6 % savings
  on listing-heavy pages, neutral on uniques).

Coverage: **102/113 (90.3 %)** across 14 eval categories. See
[`docs/coverage-report.md`](docs/coverage-report.md) for what's left
on the table.

## Quickstart

macOS arm64 only for now (depends on WKWebView / `objc2-app-kit` /
`gpui`). Linux port is ~2–3 days, not on the roadmap — see
[`docs/next-steps.md`](docs/next-steps.md) caveat #9.

### Option A: one-liner installer

```sh
curl -fsSL https://raw.githubusercontent.com/MrgSub/br/main/install.sh | bash
```

Resolves the latest release, downloads the macOS arm64 binary,
verifies the SHA-256 checksum, strips the Gatekeeper quarantine
attribute, and installs to `~/.local/bin/br`. Prints a PATH hint if
`~/.local/bin` isn't on yours.

Knobs (env vars):

```sh
# pin a specific release
curl -fsSL .../install.sh | BR_VERSION=v0.1.0 bash

# install elsewhere
curl -fsSL .../install.sh | BR_INSTALL_DIR=/usr/local/bin bash

# skip the PATH suggestion
curl -fsSL .../install.sh | BR_NO_MODIFY_PATH=1 bash
```

### Option B: build from source

```sh
git clone https://github.com/MrgSub/br
cd br
cargo build --release
ln -s "$PWD/target/release/br" /usr/local/bin/br
```

Cold release build is ~90 s; incremental rebuilds are <5 s.

### First fetch

```sh
br fetch https://example.com/
# Example Domain ... documentation ... ICANN

br fetch https://www.apartments.com/san-francisco-ca/
# WAF-walled → automatic headless escalation → real listings

br fetch https://arxiv.org/pdf/1706.03762.pdf --max-tokens 8000
# PDF text extraction, capped at ~32 KB with a truncation marker
```

The daemon auto-spawns on first command (~600 ms cold start). Run
`br daemon status` to confirm it's up; `br daemon stop` to shut down.

## Usage

```sh
br fetch <url>                      # url → markdown on stdout
br fetch <url> --meta               # …with `# tab/url/source/title` to stderr
br fetch <url> --max-tokens 4000    # cap at ~16 KB; drop link table first
br fetch <url> --headless           # force-render via WKWebView
br fetch <url> --no-headless        # disable auto-escalation
br fetch <url> --eval "JS"          # run JS after page is ready
br fetch <url> --wait-for "css"     # wait for selector before extracting
br fetch <url> --format json        # FetchOkResp envelope as JSON

br search "term" --before 1 --after 2     # ripgrep over the tab cache
br tab <tab_id> --section "Heading"       # slice a previously-fetched tab
br cache stats / clear / get              # inspect the URL→tab cache
br session start <name>                   # start an eval-friendly session
br mcp [--agent NAME]                     # serve over stdin as an MCP server
br daemon start / stop / status           # manage the long-running daemon
```

The daemon auto-spawns on the first command if it's not already
running (~600 ms cold start on a fresh data dir).

## Eval harness

```sh
./eval/run.sh                          # full sweep across all categories
./eval/run.sh charset spa              # one or more categories
PER_URL_TIMEOUT=20 ./eval/run.sh ...   # tweak per-URL timeout
```

See [`eval/README.md`](eval/README.md). Pass criterion is the same
floor we use everywhere else (≥200 chars trimmed AND ≥40 whitespace
tokens). Regression-gated against the previous `baseline-articles`
run.

## Layout

```
src/
  main.rs            # CLI entry
  daemon/            # tokio daemon, socket, lifecycle
  client.rs          # CLI → daemon RPC
  fetch/             # the strategy waterfall and fetchers
    fetchers/        # PlainFetcher (reqwest+rustls), StealthFetcher (rquest+TLS impersonation), HostRateLimiter
    strategies/      # accept_md, cloudflare_md, llms_txt, adapters/, pdf, parse_html, headless, wayback
    extract.rs       # readability+htmd, charset sniff, linkify, smart truncate, bot-block detect
    waterfall.rs     # the orchestrator with per-strategy timeouts, escalation triggers, auto-consent
  webkit/            # WebKit subprocess worker + daemon-side handle (idle/nav/pid hardening)
  registry/          # tab metadata + git-status scanner
  search/            # fff-search wrapper
  dashboard/         # GPUI three-pane window (auto-spawns daemon)
  mcp.rs             # MCP server (six tools)
  cache.rs           # URL→tab cache with per-tier TTLs
  proto.rs           # daemon-client wire frames

docs/
  next-steps.md      # roadmap + caveats
  coverage-report.md # extraction failure modes & fixes

eval/
  run.sh             # categorized URL sweep
  categories/*.txt   # 14 categories, ~113 URLs
  results/           # timestamped reports (gitignored)
```

## Status

`v0.1.0` — milestones M1–M10 done, M8 phases 1–6 done, M11 (interactive
mode) and M12 (eval harness) done. Adapter pack (YouTube transcripts,
X syndication, Mastodon, Reddit deep-links) is the next single biggest
coverage lever. See [`docs/next-steps.md`](docs/next-steps.md) for the
full state.
