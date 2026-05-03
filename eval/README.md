# `eval/` — coverage harness (M12)

A categorized URL sweep that scores `br fetch` against a fixed pass
predicate. Lets us track "what fraction of the web does br extract
markdown from?" as a single number, per category, over time. We use it
to decide whether a coverage change actually helped.

## Layout

```
eval/
  README.md                  # this file
  run.sh                     # harness driver
  categories/
    baseline-articles.txt    # regression sentinel — must always pass
    charset.txt              # non-UTF-8 / CJK / Cyrillic
    pdf.txt                  # direct PDF URLs
    spa.txt                  # JS-shell SPAs
    cf-challenge.txt         # WAF / anti-bot hosts
    social.txt               # YouTube / X / Stack Overflow / etc.
    docs-generators.txt      # Docusaurus / Mintlify / GitBook / Substack
  results/
    YYYYMMDD-HHMMSS.md       # human-readable report per run
    YYYYMMDD-HHMMSS-summary.tsv  # machine-readable, for diffing
```

Each category file is one URL per line; lines starting with `#` and
trailing `# comment` text are ignored.

## Pass predicate

A fetch counts as a pass iff the body satisfies
[`looks_like_stub`](../src/fetch/extract.rs)'s **inverse**:

* trimmed length ≥ 200 chars **and**
* whitespace-token count ≥ 40

Same threshold the readability auto-fallback uses, deliberately. If we
ever change the predicate, the harness moves with it and old reports
remain comparable via the per-URL byte / source-tier columns.

The harness can't tell mojibake from real text. Charset regressions
need a manual spot-check on the rendered output; the harness only
guards against *structural* failures.

## Running

Build the release binary first (the harness uses
`./target/release/br`):

```sh
cargo build --release
```

Full sweep:

```sh
./eval/run.sh
```

Single category (handy when iterating on a fix):

```sh
./eval/run.sh charset
./eval/run.sh charset pdf
```

Per-URL timeout (default 25 s):

```sh
PER_URL_TIMEOUT=15 ./eval/run.sh
```

Output binary location override:

```sh
BR_BIN=/path/to/br ./eval/run.sh
```

The harness:

1. Starts a daemon if one isn't already running. Stops it on exit
   only if it spawned it (won't surprise an interactive session).
2. Iterates each URL, runs `br fetch URL --no-cache --meta` with the
   per-URL timeout.
3. Records `{result, source-tier, body-bytes, elapsed-ms}` per URL.
4. Writes a Markdown report and a TSV summary to `results/`.
5. Diffs `baseline-articles` against the previous run; exits non-zero
   if pass-count regressed (other categories never fail the script —
   they're improvement trackers).

## Workflow for a coverage change

```sh
# 1. Establish a baseline.
./eval/run.sh                  # capture results/<ts>.md as "before"

# 2. Implement the fix (e.g. charset transcoding).
…
cargo build --release

# 3. Re-run.
./eval/run.sh                  # capture results/<ts>.md as "after"

# 4. Eyeball the diff:
diff results/<before>-summary.tsv results/<after>-summary.tsv
```

The TSV columns are `category, url, result, source, bytes, ms`. `awk`
or `sort | comm` works for ad-hoc diffs.

## Known harness limitations

- **Network-dependent.** A flaky DNS / Wi-Fi run will look like
  regressions. Re-run before assuming we broke something.
- **Adversarial hosts.** WAF responses sometimes flap based on IP
  reputation, time of day, etc. Treat single-URL changes in
  `cf-challenge` as noise; trends across runs are signal.
- **No mojibake detection.** As noted above; charset category needs
  human eyeballs.
- **Cache is bypassed (`--no-cache`).** Each run is a real network
  fetch. Don't run on a metered connection.
- **Daemon is shared.** If you have a long-running daemon with cookies
  set or rate-limit state, results may vary vs. a cold daemon. The
  harness does *not* restart a pre-existing daemon.

## Adding URLs

Add to the appropriate category file. Two rules:

1. **Stable.** If the URL is a news front page that changes hourly,
   prefer a permalink to a specific article instead.
2. **Diverse.** Don't add three URLs from the same host unless they
   exercise different code paths (e.g. an article + a search results
   page).

For new categories: add `categories/<name>.txt`, document what it
exercises in a comment header, and reference the
relevant gap from `docs/coverage-report.md` so future readers know
*why* the category exists.
