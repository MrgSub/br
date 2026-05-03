//! Shared post-processing: HTML → markdown via readability + htmd.
//!
//! Used by both the generic `parse_html` tier and the `headless` tier.
//! Sync/CPU-bound; callers should `tokio::task::spawn_blocking` it.

use anyhow::Result;
use url::Url;

pub struct Extracted {
    pub title: String,
    pub markdown: String,
}

/// Convert hydrated HTML to clean markdown.
///
/// * `raw=false` — runs readability first to keep just the main content.
/// * `raw=true`  — converts the entire page (useful for tabular pages).
///
/// `content_type` is the raw HTTP header value (or `None` when the source
/// already produced UTF-8 — e.g. `headless` returns a `String` from
/// WKWebView). It's the strongest signal for charset; we still sniff
/// `<meta charset>` as a backup because plenty of static hosts don't set
/// the parameter.
pub fn html_to_markdown(
    html_bytes: &[u8],
    canonical: &Url,
    raw: bool,
    content_type: Option<&str>,
) -> Result<Extracted> {
    let html = decode_html(html_bytes, content_type);

    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "iframe", "svg"])
        .build();

    if raw {
        let title = sniff_title(&html).unwrap_or_default();
        let markdown = converter
            .convert(&html)
            .map_err(|e| anyhow::anyhow!("htmd: {e}"))?;
        return Ok(Extracted { title, markdown });
    }

    // readability::extract reads from a `Read`. It handles entity decoding
    // but assumes UTF-8 byte input — feed it our transcoded UTF-8 string.
    let mut cursor = std::io::Cursor::new(html.as_bytes());
    let product = readability::extractor::extract(&mut cursor, canonical)
        .map_err(|e| anyhow::anyhow!("readability: {e:?}"))?;
    let markdown = converter
        .convert(&product.content)
        .map_err(|e| anyhow::anyhow!("htmd: {e}"))?;
    Ok(Extracted {
        title: product.title,
        markdown,
    })
}

/// Decode an HTML byte slice to a UTF-8 `String`, honoring the document's
/// declared charset.
///
/// Sniff order, matching the HTML5 "encoding sniffing algorithm" closely
/// enough for our purposes (we don't bother with TR-46 or full BOM
/// classification — those edge cases never produce useful markdown anyway):
///
/// 1. **BOM.** UTF-8 / UTF-16LE / UTF-16BE BOM wins outright; the spec
///    says it overrides everything else.
/// 2. **HTTP `Content-Type: charset=…`** if supplied.
/// 3. **`<meta charset=…>`** or `<meta http-equiv="Content-Type"
///    content="…; charset=…">` in the first 4 KiB. We use the bytes
///    here — ASCII subset of all encodings we care about, so a
///    lossy-utf8 read is fine for the search.
/// 4. **Default to UTF-8.** Modern web is UTF-8 by default; this matches
///    `std::str::from_utf8`'s old behavior on the happy path.
///
/// Once we've picked an encoding, `encoding_rs` does the transcode. Its
/// `decode` helpfully replaces malformed sequences with U+FFFD instead of
/// erroring — which is what we want, because partial mojibake is more
/// useful to a downstream agent than a hard fetch failure.
pub fn decode_html(bytes: &[u8], content_type: Option<&str>) -> String {
    // 1. BOM check.
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return enc.decode(&bytes[bom_len..]).0.into_owned();
    }

    // 2. HTTP header.
    let mut chosen = content_type
        .and_then(charset_from_content_type)
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()));

    // 3. <meta> sniff. Only run if we don't already have a header-declared
    //    charset — the spec actually prefers `<meta>` over the header in
    //    some readings, but in practice a server that bothers to set the
    //    parameter is more reliable than a hand-rolled meta tag.
    if chosen.is_none() {
        let prefix = &bytes[..bytes.len().min(4096)];
        if let Some(label) = sniff_meta_charset(prefix) {
            chosen = encoding_rs::Encoding::for_label(label.as_bytes());
        }
    }

    // 4. Default — UTF-8. encoding_rs's UTF-8 decode is essentially free
    //    on already-UTF-8 input.
    let enc = chosen.unwrap_or(encoding_rs::UTF_8);
    enc.decode(bytes).0.into_owned()
}

/// Extract `charset=foo` from a `Content-Type` value. Quotes optional.
fn charset_from_content_type(ct: &str) -> Option<String> {
    for part in ct.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("charset=").or_else(|| {
            // Case-insensitive prefix without allocating: handle `Charset=`
            // / `CHARSET=` etc.
            if part.len() >= 8 && part[..8].eq_ignore_ascii_case("charset=") {
                Some(&part[8..])
            } else {
                None
            }
        }) {
            let rest = rest.trim().trim_matches('"').trim_matches('\'');
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Find a charset declaration in HTML head bytes.
///
/// Looks for either form, case-insensitive:
///
/// ```html
/// <meta charset="shift_jis">
/// <meta http-equiv="Content-Type" content="text/html; charset=GBK">
/// ```
///
/// Implementation note: charset declarations are pure ASCII inside HTML
/// markup that's also pure ASCII in the prefix region for every encoding
/// we'd actually meet. So a lossy-UTF-8 read of the prefix is sufficient
/// to *find* the declaration; the declared encoding is then used to
/// re-decode the *full* body.
fn sniff_meta_charset(prefix: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(prefix);
    let lower = s.to_ascii_lowercase();
    // Walk every `<meta` tag and inspect its attributes.
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("<meta") {
        let start = i + rel + 5;
        let end = lower[start..]
            .find('>')
            .map(|p| start + p)
            .unwrap_or(lower.len());
        let tag_lower = &lower[start..end];
        let tag_orig = &s[start..end];

        // Form 1: <meta charset="...">
        if let Some(idx) = tag_lower.find("charset") {
            let after = &tag_lower[idx + "charset".len()..];
            let after_orig = &tag_orig[idx + "charset".len()..];
            // Skip whitespace + `=` + optional quote.
            if let Some(val) = strip_attr_value(after, after_orig) {
                return Some(val);
            }
        }
        // Form 2: <meta http-equiv="Content-Type" content="...; charset=...">
        // is already covered: the substring "charset" is inside `content="…"`,
        // and `strip_attr_value` finds the `=` after it.

        i = end;
    }
    None
}

/// Given the lowercased + original tail of a meta tag starting just after
/// `"charset"`, return the declared value if the next significant chars
/// are `=` + optional quote + token. The original-case version preserves
/// label casing (encoding_rs handles case-insensitivity but it's nicer
/// for logs).
fn strip_attr_value(lower_after: &str, orig_after: &str) -> Option<String> {
    let bytes = lower_after.as_bytes();
    let mut k = 0usize;
    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    if k >= bytes.len() || bytes[k] != b'=' {
        return None;
    }
    k += 1;
    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    let (quote, mut k2) = match bytes.get(k) {
        Some(b'"') => (Some(b'"'), k + 1),
        Some(b'\'') => (Some(b'\''), k + 1),
        _ => (None, k),
    };
    let start = k2;
    while k2 < bytes.len() {
        let c = bytes[k2];
        let stop = match quote {
            Some(q) => c == q,
            // Unquoted mode: stop on whitespace, tag-end, attr separators,
            // and — importantly — quote characters, since `charset` may
            // appear *inside* a `content="…; charset=foo"` attribute and
            // we don't want to swallow the closing quote.
            None => {
                c.is_ascii_whitespace()
                    || c == b'>'
                    || c == b'/'
                    || c == b';'
                    || c == b'"'
                    || c == b'\''
            }
        };
        if stop {
            break;
        }
        k2 += 1;
    }
    if k2 == start {
        return None;
    }
    Some(orig_after[start..k2].trim().to_string())
}

/// Convert a PDF byte stream to a markdown-shaped string.
///
/// Approach
/// --------
///
/// `pdf-extract` produces a single text dump with embedded form-feeds
/// (`\f`) between pages and lots of soft-wrapped lines. We do three
/// targeted post-processings to make it agent-friendly without
/// pretending we recovered the original layout:
///
/// 1. **Page split.** Each form-feed becomes a `\n\n---\n\n` divider.
///    Agents can then `br tab --section "Page 3"`-style address parts of
///    a long doc; we don't try harder than that because pdf-extract
///    doesn't give us heading info.
/// 2. **Ligature normalization.** Replace the common Unicode
///    ligatures pdf-extract emits ( ,  ,  , etc.)
///    with their ASCII expansions — they otherwise survive into the
///    markdown and confuse downstream search.
/// 3. **Hyphenation un-wrap.** A trailing `-\n` followed by a lowercase
///    letter is almost always a soft-hyphen line break in justified
///    typesetting; collapse it. Conservative: only join when the next
///    char is `[a-z]` to avoid eating en-dash starts.
///
/// We deliberately don't try to detect headings, columns, or tables —
/// every heuristic for those misfires on enough real-world PDFs to be a
/// net loss.
pub fn pdf_to_markdown(pdf_bytes: &[u8]) -> Result<Extracted> {
    let raw = pdf_extract::extract_text_from_mem(pdf_bytes)
        .map_err(|e| anyhow::anyhow!("pdf-extract: {e}"))?;

    // Page-split.
    let mut s = raw.replace('\u{000C}', "\n\n---\n\n");

    // Ligatures: Unicode private-use forms used by many PDF fonts.
    for (from, to) in [
        ('\u{FB00}', "ff"),  //  
        ('\u{FB01}', "fi"),  //  
        ('\u{FB02}', "fl"),  //  
        ('\u{FB03}', "ffi"), //  
        ('\u{FB04}', "ffl"), //  
        ('\u{FB05}', "st"),  //  
        ('\u{FB06}', "st"),  //  
    ] {
        if s.contains(from) {
            s = s.replace(from, to);
        }
    }

    // Soft-hyphen un-wrap. Walk by char so non-ASCII (CJK, accented
    // text) survives unscathed. Pattern: `<alpha>-\n<lowercase alpha>`
    // -> drop the `-\n`. Conservative: only joins when the next char is
    // ASCII lowercase, so we don't eat en-dash starts or genuine
    // hyphens before capitalized words.
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '-'
            && i + 2 < chars.len()
            && chars[i + 1] == '\n'
            && chars[i + 2].is_ascii_lowercase()
            && i > 0
            && chars[i - 1].is_alphabetic()
        {
            i += 2; // skip `-` and `\n`
            continue;
        }
        out.push(c);
        i += 1;
    }

    // Title sniff: first non-empty line of the first page is usually it.
    let title = out
        .split("\n---\n")
        .next()
        .unwrap_or(&out)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();

    Ok(Extracted {
        title,
        markdown: out,
    })
}

/// Heuristic: does this markdown look like readability ate the page?
///
/// Two failure modes we care about:
///
/// 1. Readability classifies a non-article layout (search results, listing
///    pages, dashboards) as boilerplate and throws it all away. Output is
///    usually 1–3 short paragraphs of nav.
/// 2. The HTML was a hydrated SPA shell where most content lives in script
///    payloads readability skips.
///
/// We flag both with a simple visible-text floor:
///
/// * ≤ 200 chars trimmed, **or**
/// * ≤ 40 whitespace-separated tokens.
///
/// We deliberately *don't* compare to `input_html_bytes`. Real articles on
/// payload-heavy sites can hit 1–2% extraction ratios while being
/// perfectly legit; the byte ratio has too many false positives. The
/// fallback path (re-extract in raw mode from the *same bytes*) is cheap
/// enough that we don't need a tighter gate.
///
/// Reused by Phase 6 (`headless: auto`) to decide whether a cheap-tier
/// result needs an SPA re-fetch.
pub fn looks_like_stub(md: &str) -> bool {
    let trimmed = md.trim();
    if trimmed.len() < 200 {
        return true;
    }
    if trimmed.split_whitespace().count() < 40 {
        return true;
    }
    false
}

/// Heuristic: does this look like a bot-block / WAF rejection page?
///
/// Sometimes anti-bot infrastructure (PerimeterX, Datadome, Akamai,
/// Imperva, the long tail of off-the-shelf bot detection) returns a
/// `200 OK` HTML page that *looks* like content — it has a few
/// paragraphs, passes `looks_like_stub`'s floor — but the body is just
/// "Your request could not be processed". Without explicit detection we
/// happily ship the rejection text as the result.
///
/// Pattern set picked from the most common phrasings observed across
/// real-estate (`realtor.com`, `zillow.com`), e-commerce (`walmart.com`,
/// `target.com`), travel (`expedia.com`), and review (`g2.com`,
/// `glassdoor.com`) hosts. Case-insensitive substring match — boring,
/// fast, robust to minor wording changes.
///
/// Returning `true` here tells the waterfall to ignore the body and try
/// further down the chain (auto-escalate to headless, then to Wayback).
pub fn looks_like_bot_block(md: &str) -> bool {
    if md.len() > 4096 {
        // Real bot-block pages are tiny. If the body is bigger than 4 KiB
        // we're almost certainly looking at real content even if it
        // mentions one of the trigger phrases ("verify" appears in
        // privacy policies, etc.).
        return false;
    }
    let lower = md.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        // PerimeterX / HUMAN.
        "your request could not be processed",
        "please note that your reference id is",
        "please contact support",
        // Akamai.
        "access denied",
        "reference #",
        "you don't have permission to access",
        // Imperva.
        "the requested url was rejected",
        // Datadome.
        "please verify you are a human",
        "prove you are a human",
        // Cloudflare challenge.
        "checking your browser",
        "just a moment",
        "verifying you are human",
        // Generic bot-detected wording.
        "pardon our interruption",
        "unusual traffic",
        "automated requests",
        "are you a robot",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Tighter cousin of [`looks_like_stub`] used to gate the Phase 6
/// auto-escalation to headless rendering.
///
/// Why a separate predicate? `looks_like_stub` is calibrated for
/// "would this be useful to ship to an agent?" and intentionally errs
/// on the side of "it's a stub" so the readability auto-fallback
/// retries in raw mode. That threshold (200 chars / 40 words) trips on
/// legitimately-tiny pages like `example.com` (184 chars / 49 words).
/// Escalating those to headless costs ~5–13 s and produces the same
/// output — a wasted render.
///
/// For *escalation* we want to be confident the cheap-tier output is
/// genuinely insufficient. The floor here is roughly half:
///
///   * trimmed length < 100 chars, **or**
///   * < 20 whitespace tokens.
///
/// Real SPA shells almost always come in well under either bound
/// (`<HomePage />`, `Loading…`, three-line `<noscript>` placeholders);
/// real pages rarely do.
pub fn worth_escalating(md: &str) -> bool {
    let trimmed = md.trim();
    if trimmed.len() < 100 {
        return true;
    }
    if trimmed.split_whitespace().count() < 20 {
        return true;
    }
    false
}

/// Cheap regex-free `<title>` sniff for raw mode.
pub fn sniff_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after = lower[start..].find('>')? + start + 1;
    let end = lower[after..].find("</title>")? + after;
    Some(html[after..end].trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_stub() {
        assert!(looks_like_stub(""));
        assert!(looks_like_stub("   \n\n  "));
    }

    #[test]
    fn short_nav_is_stub() {
        // ≤ 200 chars: classic readability-ate-the-page output.
        assert!(looks_like_stub("# Home\n\n[About](/about) [Contact](/contact)"));
    }

    #[test]
    fn long_but_few_words_is_stub() {
        // ~250 chars, but mostly a single URL — not real content.
        let s = "# Title\n\nhttps://very-long-url.example/with/lots/of/path/segments/that/inflate/the/character/count/without/adding/words/at/all";
        assert!(s.len() > 100);
        assert!(looks_like_stub(s));
    }

    #[test]
    fn decode_utf8_default() {
        let s = decode_html("hello".as_bytes(), None);
        assert_eq!(s, "hello");
    }

    #[test]
    fn decode_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("café".as_bytes());
        let s = decode_html(&bytes, None);
        assert_eq!(s, "café");
    }

    #[test]
    fn decode_shift_jis_via_meta() {
        // Encode a Japanese phrase in Shift_JIS and wrap with a meta tag.
        let (encoded, _, _) =
            encoding_rs::SHIFT_JIS.encode("こんにちは");
        let mut bytes = b"<html><head><meta charset=\"Shift_JIS\"></head><body>".to_vec();
        bytes.extend_from_slice(&encoded);
        bytes.extend_from_slice(b"</body></html>");
        let s = decode_html(&bytes, None);
        assert!(s.contains("こんにちは"), "got: {s:?}");
    }

    #[test]
    fn decode_gbk_via_http_header() {
        let (encoded, _, _) = encoding_rs::GBK.encode("你好世界");
        let s = decode_html(&encoded, Some("text/html; charset=GBK"));
        assert_eq!(s, "你好世界");
    }

    #[test]
    fn decode_windows_1252_via_meta_http_equiv() {
        let (encoded, _, _) = encoding_rs::WINDOWS_1252.encode("café résumé");
        let mut bytes =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\">".to_vec();
        bytes.extend_from_slice(&encoded);
        let s = decode_html(&bytes, None);
        assert!(s.contains("café résumé"), "got: {s:?}");
    }

    #[test]
    fn decode_header_beats_default_when_meta_absent() {
        // EUC-KR, no <meta>, only the header tells us.
        let (encoded, _, _) = encoding_rs::EUC_KR.encode("안녕하세요");
        let s = decode_html(&encoded, Some("text/html;charset=euc-kr"));
        assert_eq!(s, "안녕하세요");
    }

    #[test]
    fn charset_extract_handles_quoting_and_case() {
        assert_eq!(
            charset_from_content_type("text/html; CharSet=\"UTF-8\""),
            Some("UTF-8".into())
        );
        assert_eq!(
            charset_from_content_type("text/html;charset=gbk"),
            Some("gbk".into())
        );
        assert_eq!(charset_from_content_type("text/html"), None);
    }

    #[test]
    fn truncate_passthrough_when_under_budget() {
        let md = "# Short\n\nA paragraph.";
        assert_eq!(truncate_smart(md, 1000), md);
    }

    #[test]
    fn truncate_drops_link_table_first() {
        let body = "# Title\n\n".to_string()
            + &"This is a paragraph of body content. ".repeat(10);
        let table = "\n## Links\n\n".to_string()
            + &"[L1]: https://example.com/very/long/url/that/inflates/the/table/section\n"
                .repeat(20);
        let md = body.clone() + &table;
        // Budget covers body but not table.
        let body_tokens = body.len() / 4 + 50;
        let out = truncate_smart(&md, body_tokens);
        assert!(out.starts_with("# Title"));
        assert!(
            !out.contains("## Links"),
            "link table should have been dropped: {out:?}"
        );
        assert!(out.contains("truncated by br: dropped link table"), "marker missing: {out}");
    }

    #[test]
    fn truncate_cuts_at_heading_boundary() {
        let md = String::from("# H1\n\n")
            + &"alpha. ".repeat(50)
            + "\n## H2\n\n"
            + &"beta. ".repeat(50)
            + "\n## H3\n\n"
            + &"gamma. ".repeat(50);
        // Budget that fits H1 + H2 (everything up to byte position of
        // H3) plus the marker reserve, but not H3 itself. We aim a bit
        // *past* the H3 byte position in tokens so the body+H2 content
        // clears MARKER_RESERVE; still well below H3+H3-content.
        let h3_pos = md.find("## H3").unwrap();
        let target = (h3_pos + 200) / 4;
        let out = truncate_smart(&md, target);
        assert!(out.contains("## H1") || out.contains("# H1"));
        assert!(out.contains("## H2"));
        assert!(!out.contains("## H3"), "H3 leaked through: {out}");
        assert!(out.contains("truncated by br"));
    }

    #[test]
    fn truncate_is_utf8_safe() {
        // 1000 Japanese chars × 3 UTF-8 bytes each = 3000 bytes; budget
        // chosen to cut mid-codepoint if we ignore boundaries.
        let body: String = "こんにちは".repeat(200);
        let md = format!("# Title\n\n{body}");
        // Budget that lands somewhere awkward.
        let out = truncate_smart(&md, 250);
        assert!(
            !out.is_empty(),
            "truncation produced empty output"
        );
        // Most importantly: the output must be valid UTF-8 — if it isn't,
        // .chars() panics. Touch every char to confirm.
        assert!(out.chars().count() > 0);
    }

    #[test]
    fn linkify_skips_pages_with_no_duplicates() {
        // Every URL appears exactly once — the table machinery would
        // cost more tokens than it saves. Leave alone.
        let md = "# Article\n\nSee [a](https://example.com/a) [b](https://example.com/b) \
[c](https://example.com/c) [d](https://example.com/d) [e](https://example.com/e).";
        let out = linkify_references(md);
        assert!(!out.contains("## Links"), "emitted table for singletons: {out}");
        assert!(out.contains("[a](https://example.com/a)"));
    }

    #[test]
    fn linkify_dedupes_only_repeated_urls() {
        let md = "\
# Listings\n\
\n\
* [home](https://example.com/a), [main](https://example.com/a), [home again](https://example.com/a)\n\
* [Index B](https://example.com/b) and [B again](https://example.com/b)\n\
* [One-off C](https://example.com/c)\n\
* [One-off D](https://example.com/d)\n";
        let out = linkify_references(md);
        assert!(out.contains("[home][L1]"), "got: {out}");
        assert!(out.contains("[main][L1]"));
        assert!(out.contains("[home again][L1]"));
        assert!(out.contains("[Index B][L2]"));
        assert!(out.contains("[B again][L2]"));
        assert!(out.contains("[One-off C](https://example.com/c)"));
        assert!(out.contains("[One-off D](https://example.com/d)"));
        assert!(out.contains("[L1]: https://example.com/a"));
        assert!(out.contains("[L2]: https://example.com/b"));
        assert!(!out.contains("[L3]"), "singleton hoisted: {out}");
    }

    #[test]
    fn linkify_leaves_image_links_alone() {
        let md = "\
![logo](https://example.com/logo.png)\n\
[home](https://example.com/)\n\
[a](https://example.com/a) [b](https://example.com/b) [c](https://example.com/c) [d](https://example.com/d)\n";
        let out = linkify_references(md);
        assert!(out.contains("![logo](https://example.com/logo.png)"), "image rewritten: {out}");
    }

    #[test]
    fn linkify_leaves_code_blocks_alone() {
        let md = "\
# Examples\n\
\n\
```\n\
fetch('[link](https://example.com/in-code)')\n\
```\n\
\n\
[a](https://example.com/a) [b](https://example.com/b) [c](https://example.com/c) [d](https://example.com/d) [e](https://example.com/e)\n";
        let out = linkify_references(md);
        assert!(
            out.contains("fetch('[link](https://example.com/in-code)')"),
            "code-block link rewritten: {out}"
        );
    }

    #[test]
    fn detects_real_bot_block_pages() {
        // The exact body realtor.com served us during the eval.
        let realtor = "Your request could not be processed.\n\
Please note that your reference ID is 15770b8b-ab94-4bee-9eef-f17370690ecd.\n\
If this issue persists, please contact support@realtor-help.invalidunblockrequest@realtor.com";
        assert!(looks_like_bot_block(realtor));
        assert!(looks_like_bot_block("Just a moment...\nPlease wait while we verify"));
        assert!(looks_like_bot_block("<title>Access Denied</title> Reference #18.deadbeef"));
        assert!(looks_like_bot_block("Pardon Our Interruption As you were browsing"));
    }

    #[test]
    fn bot_block_doesnt_misfire_on_long_real_pages() {
        // Real article that happens to mention 'verify': must NOT trigger.
        let mut body = String::from(
            "# Trust and Safety\n\n\
We verify all listings against multiple data sources before publishing.\n\n",
        );
        // Pad past the 4 KiB guard.
        body.push_str(&"This is a real article paragraph. ".repeat(200));
        assert!(!looks_like_bot_block(&body));
    }

    #[test]
    fn worth_escalating_only_on_obviously_tiny() {
        // example.com-shaped (184 chars / 49 words) should NOT trigger.
        let example = "# Example Domain\n\nThis domain is for use in illustrative \
examples in documents. You may use this domain in literature without prior \
coordination or asking for permission.\n\n[More information...](https://www.iana.org/domains/example)";
        assert!(
            !worth_escalating(example),
            "example.com-class content shouldn't escalate (len={}, words={})",
            example.trim().len(),
            example.split_whitespace().count()
        );
        // Real SPA stub shapes should.
        assert!(worth_escalating(""));
        assert!(worth_escalating("Loading…"));
        assert!(worth_escalating("# Home\n\n[About](/) [Contact](/)"));
    }

    #[test]
    fn pdf_unwraps_soft_hyphens_and_unicode_safe() {
        // We don't run pdf-extract here — just exercise the post-process
        // step by extracting it. Quickest way: drive the same logic via
        // a hand-rolled raw string.
        let raw = "hyphen-\nated word and 一二三\u{000C}page two";
        // Inline the same pipeline as `pdf_to_markdown` to test the
        // string-shape transformations.
        let mut s = raw.replace('\u{000C}', "\n\n---\n\n");
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '-'
                && i + 2 < chars.len()
                && chars[i + 1] == '\n'
                && chars[i + 2].is_ascii_lowercase()
                && i > 0
                && chars[i - 1].is_alphabetic()
            {
                i += 2;
                continue;
            }
            out.push(c);
            i += 1;
        }
        s = out;
        assert!(s.contains("hyphenated word"), "got: {s:?}");
        assert!(s.contains("一二三"), "unicode lost: {s:?}");
        assert!(s.contains("\n---\n"), "page split missing: {s:?}");
    }

    #[test]
    fn real_article_passes() {
        // ~80 words, ~600 chars. Should pass.
        let body = "This is a paragraph of real content with enough words to clear \
the stub heuristic. ".repeat(8);
        let md = format!("# Article\n\n{body}");
        assert!(!looks_like_stub(&md));
    }
}

/// Convert *duplicated* inline `[text](url)` links to reference style
/// `[text][L7]` and collect them into a `## Links` table at the end.
///
/// Singletons stay inline. Earlier versions of this function rewrote
/// every link, but the eval showed that on pages with mostly-unique
/// links (Cloudflare blog, Rust book root) the per-URL table-row
/// overhead exceeded the per-occurrence savings, costing tokens.
///
/// Token math, per occurrence:
///
/// ```text
/// inline:    [text](url)            → text + url + 4 chars
/// reference: [text][L42] + table row [L42]: url   → text + label + 4 + (label + url + 4)
///
/// savings(N occurrences) = N*(url - label) - (label + url + 4)
/// ```
///
/// For label ≈4, savings is positive only when N ≥ 2 *and* the URL is
/// non-trivially long. We approximate with a flat "replace iff count
/// ≥ 2" rule — simple, monotonically beneficial, and easy to verify.
///
/// What we leave alone
/// -------------------
///
/// * Image links (`![alt](src)`) — src must stay inline to render.
/// * Fenced code blocks (` ```...``` `) and inline code (`` `...` ``)
///   — we don't rewrite URLs inside example snippets.
/// * Singletons — each only-once URL.
///
/// Why not normalize URLs (trailing slash etc.) before counting?
/// -------------------------------------------------------------
///
/// Two reasons. (a) Sites use distinct trailing-slash variants for
/// real reasons — `/foo` vs `/foo/` can route to different things on
/// some routers. (b) The wins from aggressive normalization are small
/// next to the wins from de-duping the *exact-match* repeats that
/// already dominate listing pages.
pub fn linkify_references(md: &str) -> String {
    let parsed = parse_links(md);
    if parsed.duplicated_urls.is_empty() {
        return md.to_string();
    }

    let mut out = String::with_capacity(md.len() + parsed.duplicated_urls.len() * 64);
    out.push_str(&parsed.rewritten);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str("## Links\n\n");
    // Emit in encounter order.
    for (label, url) in &parsed.duplicated_urls {
        out.push_str(&format!("[{label}]: {url}\n"));
    }
    out
}

struct Parsed {
    /// Body with duplicated-URL `[text](url)` rewritten to `[text][Lnn]`.
    /// Singletons left as inline links.
    rewritten: String,
    /// `(label, url)` pairs in encounter order, only for URLs that
    /// appeared 2+ times.
    duplicated_urls: Vec<(String, String)>,
}

fn parse_links(md: &str) -> Parsed {
    let bytes = md.as_bytes();

    // First pass: count URL occurrences. We need this before we can
    // decide which links to rewrite.
    let mut counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    walk_links(bytes, |_text, url| {
        *counts.entry(url.to_string()).or_insert(0) += 1;
    });

    // Assign labels in *encounter* order to the URLs that occur 2+ times.
    let mut url_to_label: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut duplicated_urls: Vec<(String, String)> = Vec::new();
    walk_links(bytes, |_text, url| {
        let owned = url.to_string();
        if counts.get(&owned).copied().unwrap_or(0) < 2 {
            return;
        }
        if !url_to_label.contains_key(&owned) {
            let label = format!("L{}", duplicated_urls.len() + 1);
            duplicated_urls.push((label.clone(), owned.clone()));
            url_to_label.insert(owned, label);
        }
    });

    // Second pass: emit the rewritten body, replacing only URLs that
    // ended up with a label.
    let mut out = String::with_capacity(md.len());
    let mut i = 0usize;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];

        // Fenced code block (``` ... ``` or ~~~ ... ~~~).
        if (b == b'`' || b == b'~')
            && i + 2 < len
            && bytes[i + 1] == b
            && bytes[i + 2] == b
            && (i == 0 || bytes[i - 1] == b'\n')
        {
            let fence = b;
            let start = i;
            i += 3;
            while i < len {
                if bytes[i] == b'\n'
                    && i + 3 < len
                    && bytes[i + 1] == fence
                    && bytes[i + 2] == fence
                    && bytes[i + 3] == fence
                {
                    i += 4;
                    while i < len && bytes[i] != b'\n' {
                        i += 1;
                    }
                    break;
                }
                i += 1;
            }
            out.push_str(&md[start..i.min(len)]);
            continue;
        }

        // Inline code: `...`
        if b == b'`' {
            let start = i;
            i += 1;
            while i < len && bytes[i] != b'`' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < len && bytes[i] == b'`' {
                i += 1;
            }
            out.push_str(&md[start..i.min(len)]);
            continue;
        }

        // Image link: pass through.
        if b == b'!' && i + 1 < len && bytes[i + 1] == b'[' {
            if let Some((end, _, _)) = scan_link(bytes, i + 1) {
                out.push_str(&md[i..end]);
                i = end;
                continue;
            }
        }

        // Inline link.
        if b == b'[' {
            if let Some((end, text, url)) = scan_link(bytes, i) {
                if let Some(label) = url_to_label.get(url) {
                    out.push_str(&format!("[{text}][{label}]"));
                    i = end;
                    continue;
                }
                // Singleton: leave as-is.
                out.push_str(&md[i..end]);
                i = end;
                continue;
            }
        }

        out.push(b as char);
        i += 1;
    }

    Parsed { rewritten: out, duplicated_urls }
}

/// Walk every inline link in `md_bytes` and call `f(text, url)`.
/// Skips images and code spans, matching the rules above.
fn walk_links<F: FnMut(&str, &str)>(bytes: &[u8], mut f: F) {
    let mut i = 0usize;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        // Skip fenced code blocks.
        if (b == b'`' || b == b'~')
            && i + 2 < len
            && bytes[i + 1] == b
            && bytes[i + 2] == b
            && (i == 0 || bytes[i - 1] == b'\n')
        {
            let fence = b;
            i += 3;
            while i < len {
                if bytes[i] == b'\n'
                    && i + 3 < len
                    && bytes[i + 1] == fence
                    && bytes[i + 2] == fence
                    && bytes[i + 3] == fence
                {
                    i += 4;
                    while i < len && bytes[i] != b'\n' {
                        i += 1;
                    }
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Skip inline code.
        if b == b'`' {
            i += 1;
            while i < len && bytes[i] != b'`' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < len && bytes[i] == b'`' {
                i += 1;
            }
            continue;
        }
        // Skip image links.
        if b == b'!' && i + 1 < len && bytes[i + 1] == b'[' {
            if let Some((end, _, _)) = scan_link(bytes, i + 1) {
                i = end;
                continue;
            }
        }
        // Inline link.
        if b == b'[' {
            if let Some((end, text, url)) = scan_link(bytes, i) {
                f(text, url);
                i = end;
                continue;
            }
        }
        i += 1;
    }
}

/// Scan an inline link starting at `bytes[start]` (which must be `[`).
/// Returns `(end_byte_offset, text_slice, url_slice)` if the link is
/// well-formed, else None.
///
/// Implementation notes:
///
/// * Bracketed text may contain nested `[]` (e.g. `[[wikilink]]`-style
///   quirks); we track depth.
/// * The URL portion stops at the first unescaped `)` and may itself
///   contain `()` if balanced; we track depth there too. Markdown
///   parsers vary on this, but htmd doesn't emit balanced-paren URLs in
///   practice, so the simple count is fine.
/// * Title (`"...")`) at end of the link is dropped — we keep just the
///   URL.
fn scan_link<'a>(bytes: &'a [u8], start: usize) -> Option<(usize, &'a str, &'a str)> {
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut i = start + 1;
    let text_start = i;
    let mut depth = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                i += 1;
            }
            b'\n' if {
                // Reject text spans crossing a blank line — those aren't links.
                let mut k = i + 1;
                while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                    k += 1;
                }
                k < bytes.len() && bytes[k] == b'\n'
            } =>
            {
                return None;
            }
            _ => i += 1,
        }
    }
    if i >= bytes.len() || bytes[i] != b']' {
        return None;
    }
    let text_end = i;
    i += 1;
    if i >= bytes.len() || bytes[i] != b'(' {
        return None;
    }
    i += 1;
    let url_start = i;
    let mut paren_depth = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'(' => {
                paren_depth += 1;
                i += 1;
            }
            b')' => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    break;
                }
                i += 1;
            }
            // Bail on whitespace + quote ("title" form): the URL ends
            // there. Markdown's rule is `[text](url "title")`.
            b' ' | b'\t' if matches!(bytes.get(i + 1), Some(b'"') | Some(b'\'')) => {
                let url_end = i;
                // Skip past the title to the closing `)`.
                while i < bytes.len() && bytes[i] != b')' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                let text = std::str::from_utf8(&bytes[text_start..text_end]).ok()?;
                let url = std::str::from_utf8(&bytes[url_start..url_end]).ok()?.trim();
                if url.is_empty() {
                    return None;
                }
                return Some((i + 1, text, url));
            }
            b'\n' => return None,
            _ => i += 1,
        }
    }
    if i >= bytes.len() || bytes[i] != b')' {
        return None;
    }
    let url_end = i;
    let text = std::str::from_utf8(&bytes[text_start..text_end]).ok()?;
    let url = std::str::from_utf8(&bytes[url_start..url_end]).ok()?.trim();
    if url.is_empty() {
        return None;
    }
    // We can't return `&str` borrowing `url` since we trimmed; but the
    // trim only removes leading/trailing ASCII whitespace, so the slice
    // is still a valid sub-slice of the original. Find its bounds.
    let original = std::str::from_utf8(&bytes[url_start..url_end]).ok()?;
    // `url` is &str of trimmed; recover its byte range within `original`.
    let trimmed_offset = original.find(url).unwrap_or(0);
    let actual_url_start = url_start + trimmed_offset;
    let actual_url_end = actual_url_start + url.len();
    let url_slice = std::str::from_utf8(&bytes[actual_url_start..actual_url_end]).ok()?;
    Some((i + 1, text, url_slice))
}

/// One-stop post-extraction pipeline. Each strategy that produces
/// markdown calls this with the raw extractor output and the relevant
/// FetchOptions; behavior is centralized here so every caller benefits
/// uniformly from new post-processing steps as we add them.
///
/// Pipeline:
///   1. Whitespace cleanup (always).
///   2. Optional reference-link conversion (`opts.link_table`).
///   3. Optional smart truncation (`opts.max_tokens`).
pub fn finalize(md: &str, opts: &super::FetchOptions) -> String {
    let cleaned = postprocess(md);
    let linked = if opts.link_table {
        linkify_references(&cleaned)
    } else {
        cleaned
    };
    if let Some(max) = opts.max_tokens {
        truncate_smart(&linked, max as usize)
    } else {
        linked
    }
}

/// Smart-truncate `md` so it fits within ~`max_tokens` tokens.
///
/// Token estimate uses the OpenAI-ish "1 token ≈ 4 chars" rule. We don't
/// pull in a real tokenizer because (a) it'd add a multi-MB BPE table
/// to every binary, (b) the agent reading the output uses *its own*
/// tokenizer anyway, so any byte-level dependency we adopt is wrong
/// for whichever model isn't ours. The 4-chars rule is good enough
/// for budgeting decisions — callers can always ask for `max_tokens *
/// 0.8` if they want a margin.
///
/// Strategy (in priority order):
///
/// 1. **Drop the link table.** Cheap, lossless from a content
///    standpoint (the body's reference labels still tell agents which
///    link is which; only the URL strings are gone). Often gets us
///    under budget on pages with link-heavy footers.
///
/// 2. **Truncate body at a heading boundary.** Walk backward from the
///    budget mark, find the most recent `# ` / `## ` / `### ` start.
///    Cut just before. This keeps each preserved section
///    self-contained — agents won't see half a section's content
///    without its title.
///
/// 3. **Fall back to a paragraph boundary.** If no heading boundary
///    exists in the kept range (long single-section pages), cut at
///    the most recent `\n\n`.
///
/// 4. **Final fallback: hard char cut.** Pages with no breaks at all
///    (rare) get a flat slice.
///
/// Always append a marker comment so the consumer sees that truncation
/// happened and how much was dropped.
pub fn truncate_smart(md: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if md.len() <= max_chars {
        return md.to_string();
    }

    // Step 1: try dropping the link table.
    let body_only = match md.rfind("\n## Links\n") {
        Some(idx) => &md[..idx],
        None => md,
    };
    let dropped_links = body_only.len() < md.len();

    if body_only.len() <= max_chars {
        let omitted = md.len() - body_only.len();
        let omitted_tokens = omitted / 4;
        return format!(
            "{}\n\n<!-- truncated by br: dropped link table, ~{omitted_tokens} tokens / {omitted} bytes omitted -->\n",
            body_only.trim_end()
        );
    }

    // Step 2: truncate at a heading boundary. Reserve ~120 chars for
    // the marker comment so we don't blow the budget after appending.
    const MARKER_RESERVE: usize = 128;
    let cut_target = max_chars.saturating_sub(MARKER_RESERVE);
    let cut = best_cut(body_only, cut_target);
    let kept = &body_only[..cut];
    let omitted = md.len() - cut;
    let omitted_tokens = omitted / 4;
    let dropped_table_note = if dropped_links { " (link table dropped first)" } else { "" };
    format!(
        "{}\n\n<!-- truncated by br: ~{omitted_tokens} tokens / {omitted} bytes omitted{dropped_table_note}; raise --max-tokens or use --section to retrieve -->\n",
        kept.trim_end()
    )
}

/// Find the best place to cut `md` to keep no more than `target` bytes.
///
/// Preference order: heading start → paragraph break → line break →
/// hard char cut. Always returns a value in `0..=target`.
fn best_cut(md: &str, target: usize) -> usize {
    if target == 0 {
        return 0;
    }
    // Round target down to a char boundary so all subsequent slicing
    // is safe even on multibyte content (CJK, accented Latin, etc.).
    let mut target = target.min(md.len());
    while target > 0 && !md.is_char_boundary(target) {
        target -= 1;
    }
    // Search a window of up to 16 KiB before the target for a heading
    // start. Beyond that we'd be giving up so much content that the
    // boundary preference isn't worth it.
    let mut win_start = target.saturating_sub(16 * 1024);
    while win_start > 0 && !md.is_char_boundary(win_start) {
        win_start -= 1;
    }
    let window = &md[win_start..target];

    // Heading: `\n# ` / `\n## ` / `\n### ` etc. Find the rightmost.
    if let Some(off) = window.rfind("\n# ") {
        return win_start + off + 1; // include the newline-end of prior section
    }
    if let Some(off) = window.rfind("\n## ") {
        return win_start + off + 1;
    }
    if let Some(off) = window.rfind("\n### ") {
        return win_start + off + 1;
    }
    // Paragraph break.
    if let Some(off) = window.rfind("\n\n") {
        return win_start + off + 1;
    }
    // Line break.
    if let Some(off) = window.rfind('\n') {
        return win_start + off + 1;
    }
    target
}

/// Collapse runs of blank lines and trim trailing whitespace.
pub fn postprocess(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank = 0;
    for line in md.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank += 1;
            if blank <= 1 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out
}
