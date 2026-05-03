# `br` — next steps & caveats

Snapshot of where the project stands, what's queued, and the rough edges
worth knowing about before they bite. Update as we go.

---

## What's done

| Milestone | What it gives you |
|---|---|
| **M1** daemon spine | tokio daemon, SQLite (WAL), Unix-socket protocol, lockfile, clean shutdown |
| **M2** fetch waterfall + extraction | `accept_md` + `parse_html` tiers; readability + html→md |
| **M2.5/2.6** TLS impersonation | `Plain` (reqwest) + `Stealth` (rquest, Chrome 136 TLS+H2) fetchers, cookie jars, redirects |
| **M4** more tiers | `cloudflare_md` (.md suffix), `llms_txt` (with cache), `llms_full` |
| **M6** site adapters | github, wikipedia, reddit, hackernews, npm, pypi |
| search + slicing (fff-search) | `br search`, `br tab --section/--lines`, ripgrep-class on big blobs |
| **M7** cache | URL→tab indirection, per-tier TTLs, `br cache stats/clear/get`, `--no-cache` |
| **M9** MCP server | `br mcp [--agent NAME]`, six tools, autospawn daemon, stdout-clean |
| **M3** dashboard | three-pane GPUI window, polls every second, autospawns daemon |
| **M5** sessions | `br session start/list/current`, eval-friendly export, named agents |
| **M8 Phase 1** | `br webkit-worker` subprocess + framed JSON IPC; smoke-tested |
| **M8 Phase 2** | daemon-side `WorkerHandle`, auto-spawn, retries, `Headless` strategy, `--headless` flag, `headless: bool` in MCP tool |
| **Per-host rate limit** | `HostRateLimiter` shared by `Plain` + `Stealth`; 250 ms base gap, exponential backoff on 429/503/403, decay on 2xx |
| **M8 Phase 3** smart ready detection | document-start init script + `MutationObserver` quiescence (1.5 s); polled from Rust every 200 ms; 12 s cap fallback, 20 s hard timeout backstop |
| **Readability auto-fallback** | `parse_html` re-runs htmd in raw mode on the same bytes when readability output looks like a stub (< 200 chars or < 40 words); reusable `looks_like_stub` predicate also feeds Phase 6 |
| **M12** coverage eval harness | `eval/run.sh` + 7 categorized URL lists (baseline, charset, pdf, spa, cf-challenge, social, docs-generators); regression-gates `baseline-articles`; per-run Markdown report + TSV diff (see [`eval/README.md`](../eval/README.md)) |
| **Coverage 1.1** charset/encoding | `decode_html()` in `extract.rs` sniffs BOM → `Content-Type: charset` → `<meta charset>`/`http-equiv` → UTF-8; `encoding_rs` transcode before readability. Charset eval: 7/8 passing (the lone fail is `naver.com`, an SPA — Phase 6 territory) |
| **Coverage 1.4** Wayback fallback | `Wayback` strategy at the chain tail. Probes `archive.org/wayback/available`, follows the closest 2xx snapshot via the `id_` raw form (no toolbar injection). Loop-guards `archive.org` hosts. Surfaces as `FetchSource::Wayback` / `QualityHint::Archived`; cache TTL 30 min so live origin is reprobed often |
| **Coverage 1.3** PDF extraction | `PdfText` strategy claims `*.pdf` URLs (and post-fetch verifies the `%PDF-` magic). `pdf-extract` text dump + page-split on `\f` + ligature normalization + soft-hyphen un-wrap. Surfaces as `FetchSource::Pdf`. PDF eval: 5/5 (arxiv Attention/LoRA/GPT-3/BERT, RFC 9110 — 0.7–2.4 s each, up to 468 KB markdown) |
| **M8 Phase 6** `headless: auto` | Default mode is now Auto. Two escalation triggers: stub (cheap tier returned content but failed `worth_escalating`'s tighter floor of 100 chars / 20 words) and empty (every cheap tier errored or declined). 10 s per-strategy timeout so a single hung tier can't eat the budget. Headless picker keeps whichever has more visible text vs. any cheap-tier stub. Verified: `tomsushi.ca` (stub trigger → 250 bytes recovered), `apartments.com` (empty trigger → 3197 bytes recovered). Anti-bot real-estate eval: 4 hard fails → 0 hard fails (6 pass + 4 interstitial stubs awaiting Phase 4 `--eval` to dismiss banners) |
| **M8 Phase 4** post-ready hook | New IPC fields `wait_for: Option<String>`, `eval: Option<String>`, `auto_consent: bool` on `WebKitReq::Render`. Worker installs a self-installing JS hook between `__brReady` and DOM extraction; main thread polls `__brHookDone`. Hook runs (1) `wait_for` selector poll up to 5 s, (2) baked-in dismissal that clicks known consent-library button IDs (OneTrust/TrustArc/Iubenda/Cookiebot/Didomi) then text-matches `Accept`/`Agree`/`Yes`/`I'm a US...`/etc. on visible button-likes, (3) the user `eval` snippet. CLI: `--wait-for`, `--eval`, `--no-auto-consent`. MCP: `wait_for`, `eval`, `no_auto_consent`. Auto-consent default-on: turned 2 anti-bot-real-estate stubs into passes (apartmentlist 27 b → 2262 b, yelp 9 b → 960 b) without regressing the happy path |
| **Headless auto-fallback** | Mirror of the readability auto-fallback that's been in `parse_html` since caveat #1: when readability output looks like a stub, re-run htmd in raw mode on the same hydrated HTML and keep whichever is longer. Crucial on SPAs (real-estate listings, product grids) whose layout doesn't fit readability's article template. forrent.com: 223 b → 13.7 KB |
| **Bot-block detection + UX** | `looks_like_bot_block(md)` recognizes WAF rejection bodies (PerimeterX, Akamai, Imperva, Datadome, Cloudflare challenge, generic "unusual traffic"); guarded by 4 KiB length cap to avoid false-positives on long pages that mention "verify". Detection feeds into the waterfall: a bot-block result is treated as escalation-worthy regardless of length, so we try headless then Wayback before giving up. When all paths still fail, the final body gets a `> [!warning] **br: site rejected our automated request**` banner so agents can distinguish a 200-OK rejection from real content. Eval harness strips the banner before scoring to stay honest about coverage |
| **M10 link table** | `linkify_references()` deduplicates URLs that appear 2+ times into a `## Links` table at the end (`[text][L7]` references, original URL once). Singletons stay inline so the rewrite never *costs* tokens. Centralized in `extract::finalize` and applied uniformly in the waterfall so adapter / accept_md / parse / headless / wayback / pdf all benefit. Toggle via `--no-link-table` (CLI) or `no_link_table: true` (MCP). Real savings: forrent listings -17.6 %, travel blog -8.3 %, Wikipedia -6.5 %; pages without duplicates unchanged |
| **M10 `--max-tokens N`** | `truncate_smart()` caps the rendered markdown at ~N tokens (1 token ≈ 4 chars heuristic; no tokenizer dependency). Three-tier strategy: (1) drop the `## Links` table, (2) cut body at the most recent heading boundary (`# `/`## `/`### `) inside a 16 KiB window, (3) fall back to paragraph then line break. Always appends a `<!-- truncated by br: ~N tokens / Y bytes omitted -->` marker. UTF-8-safe via `is_char_boundary` on both window start and target. CLI: `--max-tokens N`. MCP: `max_tokens: u32`. Real-world: RFC 9110 PDF 468 KB → 31 KB at 8000 tokens, Wikipedia article 179 KB → 15 KB at 4000 tokens |
| **M10 `--format json`** | New `OutputFormat` enum on the CLI (`markdown` default, `json`). JSON mode short-circuits the markdown+meta path: emits the raw `FetchOkResp` (tab_id, markdown, source, canonical_url, title, source_quality, bytes_html) as pretty JSON to stdout. MCP already returns structured data; this is purely a CLI niceness for clients that want typed access |
| **M8 Phase 5** lifecycle hardening | Four behaviors: (1) worker stderr routes to `<data>/webkit.log` (was inheriting daemon stderr — noisy); (2) idle timeout: daemon kills worker after 10 min of no requests, lazily respawns on next render (saves ~150 MB RSS while idle); (3) navigation cap: worker recycled after 200 renders to bound WKWebView leakage; (4) parent-pid watch: worker polls `getppid()` every 5 s and self-exits if reparented (catches `kill -9` on daemon, prevents zombie workers). `BR_WEBKIT_IDLE_SECS` env-var override for testing. Verified end-to-end: log routing, parent-pid watch (2 s self-exit on daemon SIGKILL), idle recycle (worker pid changes after sleep) |

---

## Open milestones

### M8 Phase 4 — agent ergonomics (~½ day)

Add `WebKitReq::Render` arguments and matching CLI/MCP flags:

- `--wait-for "css-selector"` → wait until selector matches (additional
  to mutation-quiescence; useful for tabs/dropdowns that hydrate late).
- `--eval "JS code"` → run a snippet *after* ready, *before* extracting.
  Lets agents click load-more, scroll-to-bottom, dismiss banners.
- `--screenshot PATH` → call `WKWebView.takeSnapshotWithConfiguration`,
  write a PNG. Useful for vision models on canvas-heavy pages.

**Caching:** any of these flags should bypass the cache (they're
non-deterministic relative to URL) — handle by skipping `cache::record`
when these are set, or include a content hash in the cache key. Default
to skip.

### M8 Phase 5 — lifecycle hardening (~½ day)

Robustness for sustained use:

- **Parent-pid watch in worker.** Worker checks every N seconds whether
  the daemon is still alive (`getppid() == 1` means orphaned); exits if
  not. Prevents leaked worker after a daemon SIGKILL.
- **Idle timeout.** Daemon kills the worker after 10 min of no requests,
  saves ~150 MB RSS. Re-spawn on next call.
- **RSS / navigation limits.** Kill+respawn after 1 GB RSS or 200
  navigations. WKWebView can leak under sustained use.
- **Worker logs to file.** Currently `eprintln!` inherits daemon stderr —
  noisy. Route to `<data>/webkit.log` with rotation.

### M8 Phase 6 — `headless: auto` (~½ day)

The default UX win — figure out when to use the worker without an
explicit flag. Heuristic for `auto`:

- Run cheap tiers normally.
- If they return content but it's a "stub" — fewer than ~8 word-tokens,
  or contains `<HomePage />` style placeholders, or just a navigation
  scaffold — record the result *and* re-fetch via headless, then return
  whichever has more text.
- If they return no content at all, fall back to headless directly.
- Cache the choice per-host (similar to `cloudflare_md`'s positive/negative
  cache): once we know `tomsushi.ca` is an SPA, future fetches go straight
  to headless.

This requires letting `Headless` strategy run *after* `parse_html` in the
chain, with a "result-quality" signal. Doable; affects `Strategy` trait.

---

## Other unfinished / deferred work

### M10 agent niceties (~1 day)

Specifically valuable for agent context-window pressure:

- `--max-tokens N` — smart truncation (drop boilerplate first, then
  paginate by section). Output a `<!-- truncated: N omitted -->` marker
  with hints like `--section "X"` to retrieve.
- **Link table at end.** Agents copy URLs around constantly; a dedicated
  numbered link list at the end of the markdown would cut context cost
  and make `[L7]`-style addressing possible.
- **Addressable elements.** When `Headless` is used, attach stable IDs
  (`[L7]`, `[I3]`, etc.) to interactive elements; expose a side-table
  mapping ID → CSS-selector via the `WebKitResp` payload. Foundation
  for M11.
- `--format json` — return structured `{ markdown, links, metadata }`
  instead of plain markdown.

### M11 interactive mode

Multi-step flows: `br interactive URL` opens a session; agents can `click
[L7]`, `fill [I3] "value"`, `submit`, `re-extract`. Builds on M8 Phase 4
(`--eval`) and M10 addressable elements.

Not on the immediate critical path; only relevant for agents that need to
log in / fill forms / etc.

---

## Known caveats & rough edges

### 1. Readability is too aggressive (mitigated)

**Status:** `parse_html` now auto-falls-back. After readability runs, if
`looks_like_stub(md)` (< 200 chars trimmed, or < 40 whitespace tokens),
we re-run htmd on the same bytes in raw mode and keep whichever output
has more visible text. The fallback is pure-CPU, no extra fetch.

Verified:

- Wikipedia article extraction unchanged (~179 KB markdown out of a
  long article — no false-positive raw fallback).
- DuckDuckGo lite, when it returns a CAPTCHA challenge page, now
  surfaces the challenge text instead of an empty body.

**Outstanding cases that still need work:**

- SPA shells where readability *and* raw both miss content because the
  data lives in script payloads (e.g. `tomsushi.ca` without
  `--headless`). The stub predicate flags these correctly; M8 Phase 6
  (`headless: auto`) is the lever that escalates them to a render. The
  same `looks_like_stub` is the predicate Phase 6 should call.
- Sites where readability returns *medium* output (200–1000 chars) that
  is still nav-only. Tightening the predicate risks regressing real
  short articles. Leave alone unless we see a real failure.

### 2a. Phase 3 ready cap can fire on pages with always-on animations

Observed on `tomsushi.ca`: the readiness snippet's `MutationObserver`
never sees a 1.5 s quiet window because some element (clock, marquee,
intersection-driven fade) keeps mutating. We hit the 12 s `READY_CAP`
and extract anyway. Result is fine because the DOM is fully hydrated
long before the cap, but it costs latency.

**Possible tunings** (don't bother until we hit a real problem case):

- Drop `characterData: true` from the observer to ignore clock-style
  text changes. Risk: misses pages that hydrate into existing text
  nodes.
- Filter mutations by target: ignore mutations whose target is inside
  an element with `aria-live`, or a known animation class. Fragile.
- Two-tier cap: if `readyState === 'complete'` happened ≥5 s ago, give
  up sooner. Probably the right move long-term.

### 2. WKWebView returns JSON-encoded values from `evaluate_script`

**Symptom:** `evaluate_script("document.documentElement.outerHTML")` came
back as `"<html lang=\\"en\\">…"` — quoted, with backslash-escaped
internal quotes — instead of raw HTML.

**Fix in place (Phase 2):** worker JSON-decodes the result before sending
back. `serde_json::from_str::<String>(&raw).unwrap_or(raw)` — works for
both string-valued JS expressions (decoded) and accidental non-JSON
fallbacks (kept as-is).

**Watch out:** any future JS we run via `evaluate_script_with_callback`
(e.g. for `--eval`) needs the same decode pass.

### 3. Single in-flight render in the worker

Phase 1 worker enforces serial rendering; the daemon's `WorkerHandle`
queue serializes incoming requests. Two parallel `--headless` calls run
back-to-back, not concurrently.

**Adequate for now** at <1 req/s. If we need parallelism later: a small
pool of worker subprocesses (1 WKWebView each), not threads in one
process — Cocoa is single-threaded.

### 4. Worker stderr inherits the daemon's stderr

Lines like `[webkit-worker] render id=…` appear in
`/tmp/d.log` (or wherever the daemon is logging). Cosmetic but noisy.

**Fix in Phase 5:** route worker stderr to a separate file under
`<data>/webkit.log` with size-based rotation.

### 5. Cocoa / NSApplication eats the main thread

Anything that uses AppKit must run on the main thread. The dashboard
(`gpui`) and the WebKit worker (`tao` + `wry`) both do. We solved this
by making them **separate processes** rather than threading inside the
daemon. Don't try to colocate them.

### 6. Daemon autospawn on macOS sometimes spins

On a fully-fresh data dir, the autospawned daemon takes ~600 ms before
the socket exists (SQLite migrations + fff-search index init). Clients
poll for up to 8 s before giving up; the dashboard's `wait_for_db` adds
another 5 s on top. Comfortable in practice; if it ever feels slow,
slim the cold-start by deferring `SearchEngine::init` until first use.

### 7. `accept_md` accepts SPA placeholders unless we filter

Sites like `docs.anthropic.com` honor `Accept: text/markdown` and return
text/markdown bodies *but* the body is a JSX placeholder (e.g.
`<HomePage />`, 12 bytes). We added `body_has_real_content()` (≥32 chars
+ ≥8 whitespace tokens) which catches these and forces fall-through.

**Future risk:** if a site genuinely has a 1-sentence page that happens
to satisfy the heuristic but readability rejects it, we get junk.
Re-evaluate when seen.

### 8. `rquest` redirect default is `Policy::none()`

We override to `Policy::limited(10)` to match `reqwest`. If a future
`rquest` upgrade changes the default, double-check (we already saw a
silent drop of redirect-following before catching it).

### 9. macOS-only

Hard-coded throughout (`paths.rs`, `objc2-app-kit`, etc.). Adding Linux
support would mean: webkit2gtk for `wry` (already supported by feature),
GPUI's Linux backend (works), different paths. Not a quick port, ~2-3
days. Not currently a goal.

### 10. Cloudflare blog / Anthropic / others quietly use Mintlify

Many "pretty" docs sites are Mintlify under the hood. They mostly do
`accept_md` correctly but some routes return placeholders. The
`accept_md` content filter handles it; document this so we don't have to
re-discover it next time a site behaves oddly.

### 11. `cargo build --release` time

Initial release builds take ~2-3 minutes due to `boring2` (BoringSSL via
`rquest`), `wry`/`tao`, and `gpui`'s render pipeline. Incremental
recompiles are <5 s. Worth it for the capabilities; just don't be
surprised by the cold build.

### 12. fff-search watcher is `NonRecursive`, doesn't pick up new files

We mitigate by calling `trigger_rescan()` before every search. Cheap
on a small `tabs/` directory but would scale poorly if `tabs/` ever
grows past ~10K files. By then we'd want to either patch fff-search to
watch correctly or implement our own simple in-memory index.

### 13. The fetch_cache has revalidation columns we don't use yet

`etag` and `last_modified` are stored on `fetch_cache` but we never
populate them. Conditional revalidation (`If-None-Match` / `304`) is a
follow-up — the obvious one is to send these on TTL miss, treat 304 as
a refresh-without-content.

### 14. No graceful daemon upgrade

Currently: `br daemon stop` then start the new binary. If we ever ship
this for non-developer use, version-bump migrations need a story (we
don't have one); the migration runner adds the row but doesn't track
binary version.

---

## Recommended order for the next sessions

0. **Run the M12 harness for a baseline** — one `./eval/run.sh` against
   the current binary; that's the number every coverage change is
   measured against. See [`docs/coverage-report.md`](coverage-report.md)
   for the categorized gap analysis driving the URL lists.
1. **Adapter pack** (Tier 2 from `coverage-report.md`) — arxiv,
   YouTube transcripts, StackExchange API, X syndication, Mastodon.
   Each adapter is small (~120 LoC by precedent) and lossless on the
   sites it claims. Biggest single coverage win still on the table.
2. **`--screenshot`** (last sliver of Phase 4) — call
   `WKWebView.takeSnapshotWithConfiguration`, write a PNG. Useful for
   vision models on canvas-heavy pages.
3. **Conditional revalidation** (caveat #13) — send `If-None-Match` /
   `If-Modified-Since` on TTL miss; treat 304 as a refresh-without-
   content. Lets us shorten cache TTLs without paying for it.
4. **RSS-based worker recycling** (Phase 5 follow-up) — the navigation
   count is a proxy; an actual RSS probe (e.g. via
   `mach_task_basic_info`) lets us trip the recycle on memory growth
   even when nav-count is low. Add when sustained-use leaks become an
   observed problem, not a theoretical one.
3. **M10 agent niceties** — paid for by token savings in real agent loops.
4. **M8 Phase 5** (lifecycle hardening) — when we've used headless
   enough to feel the need.

Everything else (M11 interactive, multi-platform, etc.) is open.
