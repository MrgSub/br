//! Search across cached tab markdowns.
//!
//! Powered by `fff-search` — same engine as fff.nvim. Indexes every tab
//! markdown stored at `<data>/tabs/<tab_id>.md` and supports plain / regex /
//! fuzzy modes with context lines, file-level pagination, and time budgets.

use anyhow::{Context, Result};
use fff_search::{
    file_picker::{FilePicker, FilePickerOptions},
    frecency::FrecencyTracker,
    grep::{GrepMatch, GrepMode, GrepResult, GrepSearchOptions},
    query_tracker::QueryTracker,
    FFFMode, QueryParser, SharedFrecency, SharedPicker, SharedQueryTracker,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Lives on the daemon. Holds all the fff-search Arc<RwLock<Option<T>>>s.
pub struct SearchEngine {
    pub picker: SharedPicker,
    pub frecency: SharedFrecency,
    #[allow(dead_code)] // internal use by FilePicker scoring
    pub queries: SharedQueryTracker,
}

impl SearchEngine {
    pub fn init() -> Result<Self> {
        let tabs_dir = crate::paths::tabs_dir()?;
        let index_dir = crate::paths::index_dir()?;

        let picker = SharedPicker::default();
        let frecency = SharedFrecency::default();
        let queries = SharedQueryTracker::default();

        let f = FrecencyTracker::new(index_dir.join("frecency"), false)
            .context("opening fff-search frecency db")?;
        frecency
            .init(f)
            .map_err(|e| anyhow::anyhow!("frecency init: {e:?}"))?;

        let q = QueryTracker::new(index_dir.join("queries"), false)
            .context("opening fff-search query tracker")?;
        queries
            .init(q)
            .map_err(|e| anyhow::anyhow!("query tracker init: {e:?}"))?;

        FilePicker::new_with_shared_state(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: tabs_dir.to_string_lossy().into_owned(),
                mode: FFFMode::Ai,
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("filepicker init: {e:?}"))?;

        // Don't block daemon startup waiting for the initial scan; tabs
        // accumulate over time and the watcher picks up new ones lazily.
        Ok(Self {
            picker,
            frecency,
            queries,
        })
    }

    /// Run a content search over all indexed tab markdowns.
    pub fn search(&self, req: &SearchReq) -> Result<SearchResp> {
        // Make sure we don't search before the first scan completes.
        self.picker
            .wait_for_scan(Duration::from_millis(req.scan_wait_ms.unwrap_or(2000)));

        // The default watcher in fff-search is NonRecursive and starts with
        // 0 watched directories on an empty tree. New files we drop into
        // tabs/ are seen as events but ignored. A manual rescan before each
        // search is cheap (single fs walk over a small dir) and keeps the
        // index honest without per-write coupling.
        if let Ok(mut guard) = self.picker.write() {
            if let Some(p) = guard.as_mut() {
                if let Err(e) = p.trigger_rescan(&self.frecency) {
                    tracing::warn!("trigger_rescan: {e:?}");
                }
            }
        }

        let parser = QueryParser::default();
        let query = parser.parse(&req.query);

        let mode = match req.mode {
            SearchMode::Plain => GrepMode::PlainText,
            SearchMode::Regex => GrepMode::Regex,
            SearchMode::Fuzzy => GrepMode::Fuzzy,
        };
        let opts = GrepSearchOptions {
            page_limit: req.limit.unwrap_or(50),
            before_context: req.before_context.unwrap_or(0),
            after_context: req.after_context.unwrap_or(0),
            mode,
            time_budget_ms: 2000,
            trim_whitespace: false,
            classify_definitions: false,
            ..Default::default()
        };

        let guard = self
            .picker
            .read()
            .map_err(|e| anyhow::anyhow!("picker read: {e:?}"))?;
        let picker = guard.as_ref().ok_or_else(|| anyhow::anyhow!("picker not initialized"))?;
        let result = picker.grep(&query, &opts);

        Ok(format_result(picker, result))
    }
}

fn format_result(picker: &FilePicker, r: GrepResult<'_>) -> SearchResp {
    let files: Vec<&fff_search::types::FileItem> = r.files.iter().copied().collect();
    let hits = r
        .matches
        .into_iter()
        .map(|m| format_hit(picker, &files, m))
        .collect();
    SearchResp {
        hits,
        total_files_searched: r.total_files_searched,
        total_files: r.total_files,
        files_with_matches: r.files_with_matches,
        regex_fallback_error: r.regex_fallback_error,
    }
}

fn format_hit(
    picker: &FilePicker,
    files: &[&fff_search::types::FileItem],
    m: GrepMatch,
) -> SearchHit {
    let file = files[m.file_index];
    let rel = file.relative_path(picker).to_string();
    // Strip the .md extension; the file basename IS the tab id.
    let tab_id = rel.trim_end_matches(".md").to_string();
    SearchHit {
        tab_id,
        line_number: m.line_number,
        line: m.line_content,
        context_before: m.context_before,
        context_after: m.context_after,
    }
}

// ── Wire types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Plain,
    Regex,
    Fuzzy,
}

impl Default for SearchMode {
    fn default() -> Self {
        SearchMode::Plain
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReq {
    pub query: String,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub before_context: Option<usize>,
    #[serde(default)]
    pub after_context: Option<usize>,
    #[serde(default)]
    pub scan_wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResp {
    pub hits: Vec<SearchHit>,
    pub total_files_searched: usize,
    pub total_files: usize,
    pub files_with_matches: usize,
    pub regex_fallback_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub tab_id: String,
    pub line_number: u64,
    pub line: String,
    #[serde(default)]
    pub context_before: Vec<String>,
    #[serde(default)]
    pub context_after: Vec<String>,
}

// ── Tab content slicing ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabReadReq {
    pub tab_id: String,
    /// `--lines L:R` 1-based inclusive line range.
    #[serde(default)]
    pub lines: Option<(usize, usize)>,
    /// `--section "Heading text"` returns the heading and everything until
    /// the next heading at the same or higher level.
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabReadResp {
    pub markdown: String,
    pub tab_id: String,
    pub url: String,
    pub title: Option<String>,
    pub total_lines: usize,
    pub returned_lines: (usize, usize),
}

pub fn read_tab(
    db: &crate::db::DbPool,
    req: &TabReadReq,
) -> Result<TabReadResp> {
    let meta = crate::registry::tabs::meta(db, &req.tab_id)?
        .ok_or_else(|| anyhow::anyhow!("no such tab: {}", req.tab_id))?;
    let body = crate::registry::tabs::read_markdown(&req.tab_id)?;
    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();

    let (start, end) = if let Some((l, r)) = req.lines {
        (l.max(1).min(total), r.max(1).min(total))
    } else if let Some(heading) = &req.section {
        section_range(&lines, heading).unwrap_or((1, total))
    } else {
        (1, total)
    };

    let slice = if start <= end && start <= total {
        lines[start - 1..end].join("\n")
    } else {
        String::new()
    };

    Ok(TabReadResp {
        markdown: slice,
        tab_id: req.tab_id.clone(),
        url: meta.canonical_url,
        title: meta.title,
        total_lines: total,
        returned_lines: (start, end),
    })
}

/// Find a heading line whose text matches (case-insensitive substring) and
/// return the 1-based inclusive range covering it and its content until the
/// next heading at the same or shallower depth.
fn section_range(lines: &[&str], needle: &str) -> Option<(usize, usize)> {
    let lc = needle.to_ascii_lowercase();
    let (start_idx, depth) = lines.iter().enumerate().find_map(|(i, l)| {
        let trimmed = l.trim_start();
        let level = trimmed.bytes().take_while(|&b| b == b'#').count();
        if level == 0 || level > 6 {
            return None;
        }
        let text = trimmed[level..].trim().to_ascii_lowercase();
        if text.contains(&lc) {
            Some((i, level))
        } else {
            None
        }
    })?;
    // Walk forward to find the end.
    let end_idx = lines
        .iter()
        .enumerate()
        .skip(start_idx + 1)
        .find_map(|(i, l)| {
            let trimmed = l.trim_start();
            let level = trimmed.bytes().take_while(|&b| b == b'#').count();
            if level >= 1 && level <= depth {
                Some(i)
            } else {
                None
            }
        })
        .unwrap_or(lines.len());
    Some((start_idx + 1, end_idx))
}
