# `br` — coverage report: which sites still don't give us markdown

A field guide to the failure modes that reduce our markdown-extraction rate,
ordered by ROI. Written after auditing the strategy chain (`accept_md` →
`cloudflare_md` → `llms_txt` → adapters → `parse_html`, plus optional
`headless`) and the fetchers (`Plain` reqwest, `Stealth` rquest with
Chrome-136 TLS+H2 fingerprint, shared per-host rate limiter).

The goal of this document is to enumerate concrete, actionable gaps. Each
item has a **symptom**, a **fix**, and a **rough effort** so we can pick
work by ROI rather than vibes.

Legend (effort): **XS** ≤1 hr · **S** 1–3 hr · **M** ½–1 day · **L** 1–3 days.

---

## Tier 0 — already in the queue, mentioned for context

These are already in `next-steps.md` and would each unlock a meaningful
slice of failures on their own. Listed here only so the rest of the report
doesn't double-count.

- **M8 Phase 6 — `headless: auto`** — ✅ done. Auto is now the default
  mode. Two triggers: stub (cheap-tier output below `worth_escalating`'s
  100-char / 20-word floor) and empty (every cheap tier timed out / errored).
  10 s per-strategy timeout prevents one hung tier from eating the budget.
  Picker keeps whichever has more visible text. Anti-bot real-estate eval:
  4 hard fails → 0 hard fails (6 pass + 4 escalated-but-interstitial stubs).
  See `waterfall::run` for the state machine.
  Real-world testing exposed an important refinement: the original design
  used `looks_like_stub`'s 200-char / 40-word floor for the escalation
  decision, which over-fired on legitimately small pages like `example.com`
  (184 chars / 49 words, real content). Split into two predicates:
  `looks_like_stub` ("would I ship this?") and `worth_escalating`
  ("is headless likely to do better?").

- **M8 Phase 4 — `--wait-for` / `--eval` / auto-consent** — ✅ done.
  Worker now runs a post-ready hook (between `__brReady` and DOM
  extraction) that does `wait_for` selector poll, optional auto-consent
  dismissal (known library IDs + text-match on Accept/Agree/Yes), and a
  user `eval` snippet. Auto-consent is default-on whenever Phase 6
  escalates. Anti-bot real-estate eval: 6/10 → 8/10 with no happy-path
  regressions. apartmentlist (27 b geo-gate → 2262 b) and yelp (9 b
  banner → 960 b) flipped from stub to pass. Remaining stubs (forrent,
  realtor.com) have banner shapes the regex doesn't catch yet — agents
  can pass an explicit `--eval` selector to bypass.
- **Phase 6 host learning** (XS, slips into Phase 6). Per-host cache of
  "use headless" vs "cheap tier was fine", so we don't pay the headless
  cost twice on the same site.

Everything below is *not* in `next-steps.md` yet.

---

## Tier 1 — high-leverage gaps that don't need a browser

Cheap wins. Implement these before adding more headless features —
they cover a long tail without 5–13 s render times.

### 1.1 Charset / encoding handling — ✅ done

**Status:** `decode_html()` lives in `extract.rs`. Sniff order is BOM →
`Content-Type: charset` → `<meta charset>` / `http-equiv` in the first
4 KiB → UTF-8 default. Transcode via `encoding_rs::Encoding::decode`,
which replaces malformed sequences with U+FFFD (better than hard-fail).
Applied before both readability and the raw htmd path; `headless`
passes `None` since WKWebView already returns UTF-8.

Charset eval: **7/8** (the remaining fail is `naver.com`, an SPA shell
— not a charset issue, will be picked up by Phase 6). Asahi/Yomiuri,
Aozora's Shift_JIS literature pages, Baidu, People's Daily, Lenta.ru,
and iDNES.cz all extract clean text.

Left for context (original symptom + fix):

**Symptom:** `extract.rs` did `std::str::from_utf8(html_bytes)` and bailed
on non-UTF-8 HTML. Readability's `extract` reads bytes via cursor and
also assumes UTF-8 internally. **Any** Japanese (Shift_JIS, EUC-JP),
Chinese (GBK, GB18030), Korean (EUC-KR), or legacy Western (Windows-1252,
ISO-8859-1) page that *doesn't* explicitly serve UTF-8 produces either
mojibake or a hard failure depending on the byte sequence.

This silently truncates a large fraction of the non-Anglo web. The
`StealthFetcher` already gets the bytes — we just don't transcode.

**Fix:**

- Detect via header `Content-Type: text/html; charset=…` first.
- Fall back to `<meta charset="…">` and `<meta http-equiv="Content-Type" content="…charset=…">` in the first 4 KiB.
- Transcode to UTF-8 with `encoding_rs::Encoding::for_label`.
- Run readability/htmd on the transcoded bytes.

Tested on every fetch path that goes through `extract::html_to_markdown`
(`parse_html`, `headless`).

### 1.2 `<link rel="alternate" type="text/markdown">` discovery (**S**, medium ROI)

**Symptom:** The `FetchSource::AltLink` enum variant exists but no
strategy populates it. A small but growing set of sites publish a
machine-discoverable alternate markdown URL. We're not even looking.

**Fix:**

- Add `AltLink` strategy that runs *after* `accept_md` fails: fetch the
  HTML cheaply (could be a 5 KiB HEAD-then-GET-prefix), regex the
  `<head>` for `<link rel="alternate" type="text/markdown" href="…">`,
  fetch that URL via the markdown path.
- Cache positive/negative per-host like `cloudflare_md` does.

Not huge on its own but sets up tier 1.6 (reader endpoints).

### 1.3 PDF text extraction — ✅ done

**Status:** `PdfText` strategy claims any URL whose path ends in
`.pdf`; post-fetch we verify the `%PDF-` magic before extracting (so a
200-OK redirect-to-HTML page can't fool us). Uses `pdf-extract`
(pure-Rust, no native deps). Page splits on `\f` become `\n---\n`
dividers, common Unicode ligatures (` `, ` `, ` `, etc.) normalize to ASCII,
soft-hyphenated line breaks (`<alpha>-\n<lower>`) collapse. Sync work
offloaded via `spawn_blocking`. Surfaces as `FetchSource::Pdf` /
`QualityHint::Adapted` (text content is the doc's own — closer to
adapter-quality than parse-quality).

PDF eval: **5/5**, all under 2.5 s:

| paper | bytes md | ms |
|---|---:|---:|
| Attention Is All You Need | 39,671 | 1004 |
| LoRA | 83,926 | 1345 |
| BERT | 64,216 | 767 |
| GPT-3 | 239,829 | 1285 |
| RFC 9110 (HTTP semantics) | 468,507 | 2352 |

Known rough edges:

- **Title sniff is naive.** First non-empty line of page 1 wins;
  arxiv prepends a Google attribution preamble that gets caught
  instead of the real paper title. Could improve via font-size
  inspection but `pdf-extract` doesn't expose it. Acceptable.
- **Non-`.pdf`-URL PDFs aren't claimed yet.** A `/report?id=42` URL
  that returns `Content-Type: application/pdf` falls through to
  `parse_html`, which declines. To fix, either probe via HEAD or
  let `parse_html` route to `pdf_to_markdown` on detected PDF
  content. v2 work.
- **Scanned PDFs (image-only) yield empty text.** OCR is out of
  scope; surface as a clean stub-decline so the chain falls through
  to wayback (often the indexed text version is mirrored).

### 1.4 Wayback Machine fallback — ✅ done

**Status:** `Wayback` strategy lives at the chain tail. Probes
`archive.org/wayback/available`, follows the closest 2xx snapshot via
the `id_` raw-bytes form (skips the toolbar / link-rewrite injection),
then runs the same extraction we use for live HTML. Loop-guards any
`*.archive.org` host to prevent recursion. Surfaces as
`FetchSource::Wayback` / `QualityHint::Archived`. Cache TTL is 30 min
so the live origin gets reprobed often once it comes back.

Gotcha worth remembering: archive.org's `available` API silently
returns `{"archived_snapshots":{}}` when the `url` query param is fully
RFC-3986 percent-encoded (e.g. `https%3A%2F%2F…`). Curl-style "raw" URL
works. We hand-escape only the chars that would otherwise corrupt the
query: `#`, `&`, `+`, `%`, space. Regression test in
`wayback::tests::probe_url_keeps_url_largely_unencoded`.

Verified end-to-end:

- `https://www.fogcreek.com/FogBugz/` (DNS dead) → recovered the 2018
  splash page via wayback ("Fog Creek Software").
- `https://www.geocities.com/Athens/Forum/2496/` (defunct host) →
  recovered 3.4 KB of personal-page content from a 2009 snapshot.
- `https://blog.fogcreek.com/` (no snapshot exists) → declined cleanly,
  upstream fetch error preserved.

### 1.5 Cloudflare / WAF challenge-page detection (**S**, medium ROI)

**Symptom:** Some anti-bot challenges return HTTP 200 with HTML that
looks valid but is a JS challenge ("Just a moment…", "Verify you are
human"). `looks_like_stub` *sometimes* catches it, but when the
challenge HTML contains enough nav/footer text to clear 200 chars + 40
words, we silently return junk.

**Fix:**

- Pattern-match the body for known signatures: `cf-mitigated`,
  `<title>Just a moment…</title>`, `_cf_chl_opt`, `cf_challenge_page`,
  Akamai's "Reference #18.…", Datadome's `dd-blocker`, PerimeterX's
  `_pxhd`.
- Treat hits as `Ok(None)` from `parse_html` so the waterfall continues
  to `Headless`.
- Headless tier should also probe for these and escalate the cookie/JS
  challenge through (current Phase 3 ready-detection generally handles
  it, but a 12 s cap can fire before the challenge clears on slow
  hosts).

Pairs naturally with Phase 6: a positive challenge match is a strong
"escalate to headless" signal.

### 1.6 Reader-endpoint detection for known generators (**M**, medium ROI)

**Symptom:** Many "pretty" docs/blog sites are built with the same
small set of generators, and each ships a documented machine-readable
endpoint we currently ignore.

| Generator | Detect via | Markdown path |
|---|---|---|
| Docusaurus | `<meta name="generator" content="Docusaurus">` | `<page>/index.json` for raw MDX, or `?format=md` on some configs |
| GitBook | `<meta name="generator" content="GitBook">` or `gitbook.com` host | `<page>.md` (often), or `/api/v1/spaces/.../content` |
| Mintlify | `<meta name="generator">` includes "Mintlify" | `accept_md` already covers most; some routes need `<page>.md` |
| MkDocs | meta generator | source markdown URL pattern when `repo_url` present |
| Hugo / Jekyll | meta generator + GitHub repo link in meta | derive raw markdown from repo |
| Notion (public) | host pattern | Notion's public API has a markdown export |
| Substack | host pattern + `__SUBSTACK__` blob | `?format=text` suffix surfaces a clean version on most posts |
| Medium | host pattern + `__APOLLO_STATE__` blob | `https://medium.com/p/<post-id>?format=json` (undocumented but stable) |

**Fix:**

- Generic `Reader` strategy that runs *between* adapters and
  `parse_html`. Cheap probe: fetch `<head>` (use `Range: bytes=0-8191`
  if the host honors it; else just fetch HTML and regex). Identify the
  generator, route to the matching reader endpoint.
- Surface as `FetchSource::Reader`.

We don't need every generator on day one. Substack + Medium + Docusaurus
alone covers a sizable chunk of the dev-blog / docs web.

### 1.7 Per-fetcher charset-aware `Content-Length` floor (**XS**, mostly hygiene)

**Symptom:** Some hosts respond `200 OK` with an empty body (zero-byte
or whitespace-only) when they don't have the page but haven't bothered
to 404. We propagate that to readability which produces an empty
result.

**Fix:** in `FetchedBody`, add an `is_likely_empty()` helper (`bytes.len()
< 64 && trimmed-utf8 has no letters`). Use it as an early decline in
every strategy.

Five minutes; prevents a class of "succeeded with nothing" errors.

---

## Tier 2 — site-specific adapters

Each adapter buys us a known-good extraction path for a popular host
that the generic chain handles poorly. Adapters are small (~120 LoC each
based on the existing six) and lossless.

Listed by traffic / agent-relevance:

### 2.1 arxiv.org (**S**, very high ROI for research agents)

`https://arxiv.org/abs/<id>` → fetch `https://export.arxiv.org/abs/<id>`
with content negotiation; arxiv exposes `application/x-research-info-systems`
and a clean abstract page. Combine with PDF text (1.3) for the full body
when `--full` is passed.

### 2.2 YouTube (**M**, high ROI)

Video pages are JS shells; content lives in the transcript.

- Detect `youtube.com/watch?v=…` or `youtu.be/…`.
- Hit `https://www.youtube.com/api/timedtext?v=…&lang=en` (and a couple
  of language fallbacks).
- Format as `# Title\n\n> Channel: …\n\n<transcript with [hh:mm:ss] markers>`.

### 2.3 Stack Exchange / Stack Overflow (**S**, high ROI)

Public API at `https://api.stackexchange.com/2.3/questions/<id>?…&filter=withbody`.
Returns markdown-able body for question + all answers in one call.
Massively cleaner than scraping the rendered HTML.

### 2.4 X / Twitter (**S**, medium ROI)

The unauthenticated syndication endpoint
`https://cdn.syndication.twimg.com/tweet-result?id=<id>&token=<token>`
returns a rich JSON for any public tweet. Render to markdown with
embedded media listed inline.

### 2.5 Mastodon / Fediverse (**S**, medium ROI)

Any post URL → append `.json` (Mastodon convention) → render. Works
across all instances.

### 2.6 Substack (**S**, medium ROI; subset of 1.6)

Even if 1.6 lands, a dedicated adapter catches Substack-on-custom-domain
(detected via `__SUBSTACK__` global or `cdn.substack.com` asset URL in
HTML).

### 2.7 Google Docs public (**S**, low-medium ROI)

`docs.google.com/document/d/<id>/...` → transform to
`docs.google.com/document/d/<id>/export?format=md`. Works on any
publicly-shared doc; returns clean markdown with no auth.

### 2.8 Notion public (**S**, low-medium ROI)

Public Notion pages have a public-page-data endpoint that returns a
block tree. Convertible to markdown. Niche but used a lot in startup
docs.

### 2.9 PyPI / npm extensions (already exist, **XS** to extend)

Today these resolve to README. Could also expose CHANGELOG, install
counts, recent versions when the URL has a `#changelog` fragment or
`/changelogs/` path.

---

## Tier 3 — fetcher / network-layer gaps

These are about *getting bytes at all* on hostile origins. Some need
infrastructure decisions (proxies) we may not want to take.

### 3.1 HTTP/2 + ALPN edge cases on certain CDNs (**S**)

`rquest` is good but some hosts respond differently to HTTP/2 vs
HTTP/1.1 even with the same fingerprint (Akamai, Fastly hardened
configs). Add a per-host fallback: if `Stealth` returns a 403/503
twice, transparently retry over HTTP/1.1 once.

### 3.2 Conditional revalidation (**S**, low coverage but big perf win)

Already noted in `next-steps.md` caveat #13: we store `etag` and
`last_modified` but never send `If-None-Match` / `If-Modified-Since`.
Implementing this turns most TTL-miss refetches into 304s, which in
turn means we can shorten TTLs without paying for it — fresher content
without more bandwidth.

Coverage angle: on hosts that 429 us under load, conditional fetches
also reduce request count, which keeps us under the rate limit for the
*real* pages we want.

### 3.3 Geo-blocked / region-locked content (**L**, defer)

Some content is truly inaccessible from a single IP. Solution
(rotating proxies, residential exit) is out of scope for a local
agent tool. Document as "won't fix" rather than pretend.

### 3.4 Cookie / consent walls (GDPR) (**M**, headless)

Many EU news/blog sites bounce every first request to a consent
modal. Today readability sees the modal text and stops there.

**Fix in headless tier (depends on Phase 4 `--eval`):**

- Pre-set a generic "all consents accepted" cookie set
  (`euconsent-v2=…`, `OptanonAlertBoxClosed=…`, etc., 6 cookies cover
  ~80% of TrustArc/OneTrust deployments).
- Run a dismissal eval snippet that clicks any visible button matching
  the regex `accept|agree|got it|understand|allow all` in the first
  three modals it finds.

Could ship as an opt-in `--auto-consent` flag; default-on once we trust
it.

### 3.5 Login walls / paywalls (**out of scope**)

Genuine auth-required content is not a coverage gap, it's a different
product. Surface a clear error (`auth_required`) instead of returning
the paywall page as content.

---

## Tier 4 — extraction-quality gaps

We *get* the bytes but produce poor markdown. These regressions are
quieter than outright failures but still cost agents tokens.

### 4.1 Wrong-element selection by readability (**M**)

**Symptom:** readability picks a sidebar / related-articles block as
the "main" element on layouts where the article markup is unusual.
Output looks coherent so `looks_like_stub` doesn't fire, but it's the
wrong content.

**Fix:**

- When the page has `<main>`, `<article>`, or
  `<[role=main]>`, scope readability to that subtree.
- Honor `<link rel="canonical">` to drop tracking params from the
  canonical URL we cache under.

### 4.2 Code blocks lose language hints (**S**)

htmd default rules don't always preserve `<pre><code class="language-rust">`
→ ` ```rust ` fences. Verify htmd's options; if they're off, enable.
Big readability win for docs.

### 4.3 Tables get mangled on non-standard markup (**M**)

Tables built with `<div>` grid layouts (Notion, modern docs) come out
as flat text. The Phase 6 `--raw` path fares better on these. If we
detect a table-heavy page (lots of repeated grid-row classes), prefer
`--raw` even when readability didn't stub.

### 4.4 RTL / mixed-direction text (**S**)

readability + htmd handle direction OK, but our `postprocess` collapses
some Unicode bidi marks that legitimate Arabic/Hebrew content needs.
Audit `postprocess` for `\u{200F}` / `\u{200E}` handling.

### 4.5 Math (**S**)

MathJax / KaTeX rendered output is a forest of `<span>`s; readability
keeps it but the htmd output is unreadable. Detect KaTeX
(`<span class="katex">`), pull the `<annotation encoding="application/x-tex">`
LaTeX source instead. Same for MathML.

---

## Tier 5 — feed / sitemap discovery (foundation work)

Not direct extractions, but infrastructure that lets us find canonical
URLs we can extract from cheaper tiers.

### 5.1 RSS / Atom feed discovery (**S**)

Most blogs and news sites have a feed at a discoverable location
(`<link rel="alternate" type="application/rss+xml">`). Feed entries
typically include a clean summary or even full content.

For root-URL fetches, surface the feed-derived list as the answer when
no `llms.txt` is present.

### 5.2 Sitemap.xml fallback for `br fetch <root>` (**S**)

When `llms.txt`/`llms-full.txt` are absent, fall back to
`/sitemap.xml`: parse it, return a markdown index of the top N URLs
(weighted by `priority` and `lastmod`). Doesn't replace per-page
extraction but gives agents a map of the site for free.

### 5.3 OpenGraph / `<meta>` extraction for failed pages (**XS**)

If every strategy fails or returns a stub, last-ditch: build a
markdown "card" from `og:title`, `og:description`, `og:image`,
`og:url`. Not the page content but better than an error for link
preview-style use.

---

## Suggested phasing

If we did the top items in order, the cumulative coverage gain looks
like this (rough, based on logged failures from heavy MCP use):

1. **Phase 6 (`headless: auto`)** + **1.5 challenge detection** —
   maybe 30–40% of current failures.
2. **1.1 Charset handling** — surprisingly large; opens the entire
   non-Latin web. Maybe another 15%.
3. **1.4 Wayback fallback** — picks up dead-link / blocked-host cases,
   ~5–10%.
4. **1.3 PDF extraction** + **2.1 arxiv** — research agents love this,
   ~5%.
5. **2.2 YouTube** + **2.3 StackExchange** + **2.4 X** — adapter pack
   for the most-requested social/Q&A hosts, another ~5–10%.
6. **1.6 Reader endpoints** + **1.2 alt-link discovery** — Substack,
   Medium, Docusaurus polish.
7. **3.4 Cookie consent walls** — once Phase 4 lands.
8. **Tier 4 quality** items.

Items I'd defer indefinitely without a specific request:

- 3.3 (geo blocks / rotating proxies) — wrong product.
- 4.4 (RTL text) — fix once a real failure surfaces.
- 2.7/2.8 (Google Docs / Notion public) — useful but rarely-blocking.

---

## How to validate

For any of these we should pick a 20–30 URL eval set per category:

- `eval/charset.txt` — Japanese, Chinese, Korean, Russian, Czech.
- `eval/pdf.txt` — arxiv abstracts, RFC drafts, government PDFs.
- `eval/spa.txt` — Notion-public, modern docs, e-commerce.
- `eval/cf-challenge.txt` — known Cloudflare-challenged hosts.
- `eval/social.txt` — YouTube, X, Mastodon, Reddit, HN.

Run `br fetch` against each, score with a simple `looks_like_stub`-style
floor + manual spot check. Track pass-rate over time as a coverage
metric. This belongs as a milestone of its own (call it **M12 — coverage
eval harness**) before we sink another half-week into adapters; without
it, every change is anecdotal.
