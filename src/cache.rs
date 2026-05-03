//! URL → tab_id cache. Markdown bodies live in `tabs/<tab_id>.md`; this
//! table just maps URLs to the freshest tab that produced their content,
//! plus revalidation metadata.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::DbPool;
use crate::fetch::FetchSource;
use crate::registry::now_ms;

/// Per-tier default TTL. Canonical/API content is stable for a day; generic
/// HTML parsing is treated as more volatile.
fn ttl_secs(source: FetchSource) -> u64 {
    match source {
        FetchSource::Parse => 60 * 60,                  // 1 hour
        FetchSource::Headless => 30 * 60,               // 30 min — futureproof
        // Wayback recovery is a fallback for a *broken* origin. We want
        // to recheck the live origin reasonably often in case it comes
        // back, so keep the TTL short — same as a generic parse, not the
        // 24h "authoritative" tier. Agents that explicitly want the
        // archive can pass `--no-cache`.
        FetchSource::Wayback => 30 * 60,                // 30 min
        FetchSource::AcceptMd
        | FetchSource::Cloudflare
        | FetchSource::LlmsFull
        | FetchSource::LlmsIndex
        | FetchSource::Adapter
        | FetchSource::AltLink
        | FetchSource::Reader
        // PDFs at a stable URL are immutable for our purposes.
        | FetchSource::Pdf => 24 * 60 * 60,             // 24 hours
    }
}

/// Successful cache lookup. The caller reads the markdown from disk via
/// `registry::tabs::read_markdown(tab_id)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheHit {
    pub tab_id: String,
    pub url: String,
    pub canonical_url: String,
    pub title: Option<String>,
    pub source: String,
    pub fetched_at: i64,
    pub expires_at: Option<i64>,
    pub age_secs: i64,
}

/// Look up a fresh (non-expired) cache row for the given URL. Returns
/// `Ok(None)` if there's no row or it has expired.
pub fn lookup_fresh(db: &DbPool, url: &str) -> Result<Option<CacheHit>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT
            c.tab_id, c.fetched_at, c.expires_at,
            t.url, COALESCE(t.canonical_url, t.url), t.title, t.source
         FROM fetch_cache c
         JOIN tabs t ON t.id = c.tab_id
         WHERE c.url = ?1",
    )?;
    let mut rows = stmt.query(params![url])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let now = now_ms();
    let fetched_at: i64 = row.get(1)?;
    let expires_at: Option<i64> = row.get(2)?;
    if let Some(exp) = expires_at {
        if exp < now {
            return Ok(None);
        }
    }
    Ok(Some(CacheHit {
        tab_id: row.get(0)?,
        fetched_at,
        expires_at,
        age_secs: (now - fetched_at) / 1000,
        url: row.get(3)?,
        canonical_url: row.get(4)?,
        title: row.get(5)?,
        source: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
    }))
}

/// Record a fresh cache entry. URL is the agent-supplied URL (so subsequent
/// lookups by that exact URL find this row); `tab_id` points at the canonical
/// tab.
pub fn record(
    db: &DbPool,
    url: &str,
    tab_id: &str,
    source: FetchSource,
) -> Result<()> {
    let now = now_ms();
    let expires = now + (ttl_secs(source) as i64) * 1000;
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO fetch_cache (url, tab_id, fetched_at, expires_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(url) DO UPDATE SET
            tab_id        = excluded.tab_id,
            fetched_at    = excluded.fetched_at,
            expires_at    = excluded.expires_at",
        params![url, tab_id, now, expires],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub rows: i64,
    pub fresh_rows: i64,
    pub expired_rows: i64,
    pub oldest_fetched_at: Option<i64>,
    pub newest_fetched_at: Option<i64>,
    pub total_md_bytes: i64,
    pub tabs_dir: String,
}

pub fn stats(db: &DbPool) -> Result<CacheStats> {
    let conn = db.get()?;
    let now = now_ms();
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM fetch_cache", [], |r| r.get(0))?;
    let fresh_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fetch_cache WHERE expires_at IS NULL OR expires_at > ?1",
        params![now],
        |r| r.get(0),
    )?;
    let expired_rows = rows - fresh_rows;
    let (oldest, newest): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT MIN(fetched_at), MAX(fetched_at) FROM fetch_cache",
            [],
            |r| Ok((r.get(0).ok(), r.get(1).ok())),
        )
        .unwrap_or((None, None));

    // Sum on-disk size of all tab files referenced from the cache.
    let mut total = 0i64;
    let tabs_dir = crate::paths::tabs_dir()?;
    if let Ok(rd) = std::fs::read_dir(&tabs_dir) {
        for entry in rd.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len() as i64;
                }
            }
        }
    }

    Ok(CacheStats {
        rows,
        fresh_rows,
        expired_rows,
        oldest_fetched_at: oldest,
        newest_fetched_at: newest,
        total_md_bytes: total,
        tabs_dir: tabs_dir.display().to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearReport {
    pub cache_rows_removed: usize,
    pub tab_files_removed: usize,
    pub tab_rows_removed: usize,
}

/// Drop cache entries.
///
/// * `also_drop_tabs = false` — only the cache rows. Tabs and their `.md`
///   files survive (history preserved); subsequent `br fetch` for the same
///   URL will go to the network and create a new tab.
/// * `also_drop_tabs = true` — full reset: also delete every tab row and
///   every `tabs/<id>.md`. The `tabs/` and `index/` dirs are recreated.
pub fn clear(db: &DbPool, also_drop_tabs: bool) -> Result<ClearReport> {
    let conn = db.get()?;
    let cache_rows_removed: usize = conn.execute("DELETE FROM fetch_cache", [])?;

    let mut tab_rows_removed = 0;
    let mut tab_files_removed = 0;
    if also_drop_tabs {
        tab_rows_removed = conn.execute("DELETE FROM tab_content", [])?;
        let _ = conn.execute("DELETE FROM tab_history", [])?;
        let n_tabs: usize = conn.execute("DELETE FROM tabs", [])?;
        tab_rows_removed = tab_rows_removed.max(n_tabs);

        let dir = crate::paths::tabs_dir()?;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".md")
                    && std::fs::remove_file(entry.path()).is_ok()
                {
                    tab_files_removed += 1;
                }
            }
        }
    }

    Ok(ClearReport {
        cache_rows_removed,
        tab_files_removed,
        tab_rows_removed,
    })
}

/// One row, fetched directly by URL — for `br cache get URL`.
pub fn get(db: &DbPool, url: &str) -> Result<Option<CacheHit>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT
            c.tab_id, c.fetched_at, c.expires_at,
            t.url, COALESCE(t.canonical_url, t.url), t.title, t.source
         FROM fetch_cache c
         JOIN tabs t ON t.id = c.tab_id
         WHERE c.url = ?1",
    )?;
    let mut rows = stmt.query(params![url])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let fetched_at: i64 = row.get(1)?;
    let now = now_ms();
    Ok(Some(CacheHit {
        tab_id: row.get(0)?,
        fetched_at,
        expires_at: row.get(2)?,
        age_secs: (now - fetched_at) / 1000,
        url: row.get(3)?,
        canonical_url: row.get(4)?,
        title: row.get(5)?,
        source: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
    }))
}
