//! Tab rows + content blobs.

use anyhow::{Context, Result};
use rusqlite::params;

use crate::db::DbPool;
use crate::fetch::MarkdownResponse;
use crate::registry::now_ms;

/// Insert a tab in `pending` state. Returns the new tab id.
pub fn open(db: &DbPool, agent_id: &str, url: &str) -> Result<String> {
    let id = ulid::Ulid::new().to_string();
    let now = now_ms();
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO tabs (id, agent_id, url, status, opened_at)
         VALUES (?1, ?2, ?3, 'pending', ?4)",
        params![id, agent_id, url, now],
    )?;
    Ok(id)
}

/// Mark a tab as ready, recording the fetch result + content blobs.
///
/// Also writes the markdown to `<data>/tabs/<tab_id>.md` so the fff-search
/// FilePicker can index it.
pub fn record_success(db: &DbPool, tab_id: &str, resp: &MarkdownResponse) -> Result<()> {
    let conn = db.get()?;
    let now = now_ms();
    conn.execute(
        "UPDATE tabs SET
            status        = 'ready',
            canonical_url = ?2,
            title         = ?3,
            source        = ?4,
            quality_hint  = ?5,
            bytes_md      = ?6,
            bytes_html    = ?7,
            fetched_at    = ?8
         WHERE id = ?1",
        params![
            tab_id,
            resp.canonical_url.as_str(),
            resp.title,
            resp.source.as_str(),
            format!("{:?}", resp.source.quality_hint()).to_lowercase(),
            resp.markdown.len() as i64,
            resp.bytes_html.map(|b| b as i64),
            now,
        ],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO tab_content (tab_id, markdown) VALUES (?1, ?2)",
        params![tab_id, resp.markdown.as_bytes()],
    )?;

    // Mirror to disk for the search index. Header lines first so a human or
    // agent reading the file directly knows the provenance.
    let path = crate::paths::tab_md_path(tab_id)?;
    let header = format!(
        "<!-- br-tab\n  id: {tab_id}\n  url: {url}\n  source: {source}\n  title: {title}\n  fetched_at: {ts}\n-->\n\n",
        url = resp.canonical_url,
        source = resp.source.as_str(),
        title = resp.title.as_deref().unwrap_or(""),
        ts = now,
    );
    let mut body = String::with_capacity(header.len() + resp.markdown.len());
    body.push_str(&header);
    body.push_str(&resp.markdown);
    std::fs::write(&path, body)
        .with_context(|| format!("writing tab markdown to {}", path.display()))?;
    Ok(())
}

/// Read a tab's markdown back from disk. Returns the body **without** the
/// `<!-- br-tab ... -->` provenance header.
pub fn read_markdown(tab_id: &str) -> Result<String> {
    let path = crate::paths::tab_md_path(tab_id)?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    // Strip the header if present.
    if let Some(rest) = raw.strip_prefix("<!-- br-tab") {
        if let Some(end) = rest.find("-->") {
            let body = &rest[end + 3..];
            return Ok(body.trim_start_matches('\n').to_string());
        }
    }
    Ok(raw)
}

/// Look up minimal tab metadata (url, title, agent_id, source).
pub fn meta(db: &DbPool, tab_id: &str) -> Result<Option<TabMeta>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT url, COALESCE(canonical_url, url), title, source, agent_id
         FROM tabs WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![tab_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(TabMeta {
            url: row.get::<_, String>(0)?,
            canonical_url: row.get::<_, String>(1)?,
            title: row.get::<_, Option<String>>(2)?,
            source: row.get::<_, Option<String>>(3)?,
            agent_id: row.get::<_, String>(4)?,
        }))
    } else {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct TabMeta {
    pub url: String,
    pub canonical_url: String,
    pub title: Option<String>,
    pub source: Option<String>,
    pub agent_id: String,
}

/// Mark a tab as failed.
pub fn record_failure(db: &DbPool, tab_id: &str, err: &str) -> Result<()> {
    let conn = db.get()?;
    let now = now_ms();
    let json = serde_json::json!({ "message": err }).to_string();
    conn.execute(
        "UPDATE tabs SET status = 'failed', error = ?2, fetched_at = ?3 WHERE id = ?1",
        params![tab_id, json, now],
    )?;
    Ok(())
}
