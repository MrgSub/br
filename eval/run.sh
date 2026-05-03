#!/usr/bin/env bash
# eval/run.sh — coverage harness driver
#
# Runs `br fetch` against each URL in eval/categories/*.txt, scores
# pass/fail with a content floor matching `looks_like_stub` (≥200
# chars trimmed AND ≥40 word tokens), and writes a dated Markdown
# report to eval/results/.
#
# Usage:
#   eval/run.sh                     # all categories
#   eval/run.sh charset spa         # subset
#   PER_URL_TIMEOUT=20 eval/run.sh  # tweak per-URL timeout (default 25s)
#
# Exit code: 0 if no regression vs the previous report on baseline-articles,
# else 1. Other categories never fail the script — they're meant to track
# improvement over time.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVAL_DIR="$ROOT/eval"
CAT_DIR="$EVAL_DIR/categories"
OUT_DIR="$EVAL_DIR/results"
mkdir -p "$OUT_DIR"

BR_BIN="${BR_BIN:-$ROOT/target/release/br}"
PER_URL_TIMEOUT="${PER_URL_TIMEOUT:-25}"

if [[ ! -x "$BR_BIN" ]]; then
  echo "br binary not found at $BR_BIN — build with 'cargo build --release'" >&2
  exit 2
fi

# ── daemon lifecycle ────────────────────────────────────────────────────────
# We start a daemon if one isn't running. We do NOT stop it on exit if we
# inherited a running one — that would surprise the user.
DAEMON_WAS_RUNNING=0
if "$BR_BIN" daemon status >/dev/null 2>&1; then
  DAEMON_WAS_RUNNING=1
else
  echo "starting daemon …" >&2
  nohup "$BR_BIN" daemon start >/tmp/br-eval-daemon.log 2>&1 &
  for _ in $(seq 1 20); do
    sleep 0.5
    "$BR_BIN" daemon status >/dev/null 2>&1 && break
  done
fi

cleanup() {
  if [[ "$DAEMON_WAS_RUNNING" -eq 0 ]]; then
    "$BR_BIN" daemon stop >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# ── pick categories ─────────────────────────────────────────────────────────
if [[ $# -gt 0 ]]; then
  CATS=("$@")
else
  CATS=()
  for f in "$CAT_DIR"/*.txt; do
    CATS+=("$(basename "$f" .txt)")
  done
fi

# ── output report ───────────────────────────────────────────────────────────
TS="$(date +%Y%m%d-%H%M%S)"
REPORT="$OUT_DIR/$TS.md"
SUMMARY="$OUT_DIR/$TS-summary.tsv"
: > "$REPORT"
: > "$SUMMARY"

{
  echo "# br coverage eval — $TS"
  echo
  echo "Binary: \`$BR_BIN\`"
  echo "Per-URL timeout: ${PER_URL_TIMEOUT}s"
  echo
} >> "$REPORT"

# ── scoring ─────────────────────────────────────────────────────────────────────────
# Pass criterion: the content floor we use everywhere else.
#   trimmed length ≥ 200 chars AND whitespace-token count ≥ 40
#
# Important: we strip any `> [!warning] **br:` banner that the daemon
# prepends on bot-block pages BEFORE scoring. Otherwise the banner +
# tiny rejection body would clear the floor and we'd score "site is
# WAF-rejecting us" as a pass. The banner is good UX for agents but
# bad for harness honesty.
strip_br_banner() {
  awk 'BEGIN { skip = 0 }
       /^> \[!warning\] \*\*br:/ { skip = 1; next }
       skip == 1 && /^---$/ { skip = 2; next }
       skip == 2 { skip = 0; if ($0 == "") next }
       skip == 0 { print }
       skip == 1 { next }'
}
score_pass() {
  local body="$1"
  body=$(printf '%s' "$body" | strip_br_banner)
  local trimmed_len
  trimmed_len=$(printf '%s' "$body" | awk '{$1=$1};1' | wc -c | tr -d ' ')
  if (( trimmed_len < 200 )); then return 1; fi
  local words
  words=$(printf '%s' "$body" | wc -w | tr -d ' ')
  if (( words < 40 )); then return 1; fi
  return 0
}

# ── main loop ───────────────────────────────────────────────────────────────
TOTAL_PASS=0
TOTAL_RUN=0
BASELINE_PASS=0
BASELINE_TOTAL=0

for cat in "${CATS[@]}"; do
  cat_file="$CAT_DIR/$cat.txt"
  if [[ ! -f "$cat_file" ]]; then
    echo "skipping unknown category: $cat" >&2
    continue
  fi

  echo "=== $cat ===" >&2
  {
    echo "## $cat"
    echo
    echo "| URL | result | source | bytes | ms |"
    echo "|---|---|---|---:|---:|"
  } >> "$REPORT"

  cat_pass=0
  cat_total=0

  while IFS= read -r line || [[ -n "$line" ]]; do
    # strip comments + trailing whitespace
    url="${line%%#*}"
    url="${url%"${url##*[![:space:]]}"}"
    [[ -z "$url" ]] && continue

    cat_total=$((cat_total + 1))
    TOTAL_RUN=$((TOTAL_RUN + 1))

    meta_file="$(mktemp)"
    body_file="$(mktemp)"
    t0=$(date +%s%N)
    set +e
    timeout "${PER_URL_TIMEOUT}s" \
      "$BR_BIN" fetch "$url" --no-cache --meta \
      >"$body_file" 2>"$meta_file"
    rc=$?
    set -e
    t1=$(date +%s%N)
    ms=$(( (t1 - t0) / 1000000 ))

    source_tier="$(grep -E '^# source ' "$meta_file" | awk '{print $3}' | head -1)"
    source_tier="${source_tier:-—}"
    bytes=$(wc -c < "$body_file" | tr -d ' ')

    if [[ "$rc" -ne 0 ]]; then
      result="❌ err($rc)"
    elif body=$(cat "$body_file") && score_pass "$body"; then
      result="✅"
      cat_pass=$((cat_pass + 1))
      TOTAL_PASS=$((TOTAL_PASS + 1))
    else
      result="⚠️ stub"
    fi

    # Markdown-escape pipe in the URL display
    safe_url="${url//|/\\|}"
    printf '| `%s` | %s | %s | %s | %s |\n' \
      "$safe_url" "$result" "$source_tier" "$bytes" "$ms" >> "$REPORT"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$cat" "$url" "$result" "$source_tier" "$bytes" "$ms" >> "$SUMMARY"

    rm -f "$meta_file" "$body_file"
  done < "$cat_file"

  rate="0"
  if (( cat_total > 0 )); then
    rate="$(awk "BEGIN{printf \"%.0f\", 100 * $cat_pass / $cat_total}")"
  fi
  {
    echo
    echo "**$cat: $cat_pass/$cat_total ($rate%)**"
    echo
  } >> "$REPORT"

  if [[ "$cat" == "baseline-articles" ]]; then
    BASELINE_PASS=$cat_pass
    BASELINE_TOTAL=$cat_total
  fi

  echo "  $cat: $cat_pass/$cat_total" >&2
done

# ── summary header ──────────────────────────────────────────────────────────
overall_rate="0"
if (( TOTAL_RUN > 0 )); then
  overall_rate="$(awk "BEGIN{printf \"%.0f\", 100 * $TOTAL_PASS / $TOTAL_RUN}")"
fi

# Prepend overall summary by writing a new file then concatenating.
HEADER="$(mktemp)"
{
  echo "# br coverage eval — $TS"
  echo
  echo "**Overall: $TOTAL_PASS/$TOTAL_RUN ($overall_rate%)**"
  echo
  echo "Binary: \`$BR_BIN\`  ·  per-URL timeout: ${PER_URL_TIMEOUT}s"
  echo
  echo "Pass = trimmed body ≥200 chars **and** ≥40 whitespace tokens."
  echo
  echo "---"
  echo
} > "$HEADER"
# Drop the duplicate first-line header from the body
tail -n +5 "$REPORT" > "$REPORT.body"
cat "$HEADER" "$REPORT.body" > "$REPORT"
rm -f "$HEADER" "$REPORT.body"

echo >&2
echo "report: $REPORT" >&2
echo "summary: $SUMMARY" >&2
echo "overall: $TOTAL_PASS/$TOTAL_RUN ($overall_rate%)" >&2

# ── regression gate on baseline ────────────────────────────────────────────
PREV="$(ls -1 "$OUT_DIR"/*-summary.tsv 2>/dev/null | grep -v "$TS" | tail -1 || true)"
if [[ -n "$PREV" && "$BASELINE_TOTAL" -gt 0 ]]; then
  prev_baseline_pass=$(awk -F'\t' '$1=="baseline-articles" && $3 ~ /✅/' "$PREV" | wc -l | tr -d ' ')
  if (( BASELINE_PASS < prev_baseline_pass )); then
    echo "REGRESSION: baseline pass $BASELINE_PASS < previous $prev_baseline_pass" >&2
    exit 1
  fi
fi
exit 0
