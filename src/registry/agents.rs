//! Agent rows. One per logical caller (e.g. a CLI session, an MCP client).

use anyhow::Result;
use rusqlite::params;

use crate::db::DbPool;
use crate::registry::now_ms;

/// Look up an agent by `name`; create one if missing. Updates `last_seen_at`.
/// Returns the agent id (ULID).
pub fn ensure_by_name(db: &DbPool, name: &str) -> Result<String> {
    ensure_by_name_with_note(db, name, None)
}

/// Same as `ensure_by_name`, but also stores `note` (free-form metadata)
/// on first creation. Subsequent calls leave existing metadata alone
/// unless `--note` is explicitly passed.
pub fn ensure_by_name_with_note(
    db: &DbPool,
    name: &str,
    note: Option<&str>,
) -> Result<String> {
    let conn = db.get()?;
    let now = now_ms();
    let metadata = note.map(|n| serde_json::json!({ "note": n }).to_string());

    if let Ok(id) = conn.query_row(
        "SELECT id FROM agents WHERE name = ?1",
        params![name],
        |r| r.get::<_, String>(0),
    ) {
        if let Some(meta) = &metadata {
            conn.execute(
                "UPDATE agents SET last_seen_at = ?1, metadata = ?2 WHERE id = ?3",
                params![now, meta, id],
            )?;
        } else {
            conn.execute(
                "UPDATE agents SET last_seen_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
        }
        return Ok(id);
    }

    let id = ulid::Ulid::new().to_string();
    conn.execute(
        "INSERT INTO agents (id, name, created_at, last_seen_at, metadata)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        params![id, name, now, metadata],
    )?;
    Ok(id)
}

/// One row in the session list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub tab_count: i64,
    pub ready_count: i64,
    pub note: Option<String>,
}

pub fn list(db: &DbPool) -> Result<Vec<AgentSummary>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.created_at, a.last_seen_at,
                COUNT(t.id),
                SUM(CASE WHEN t.status = 'ready' THEN 1 ELSE 0 END),
                a.metadata
         FROM agents a LEFT JOIN tabs t ON t.agent_id = a.id
         GROUP BY a.id
         ORDER BY a.last_seen_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let metadata: Option<String> = r.get(6)?;
        let note = metadata
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| {
                v.get("note")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
            });
        Ok(AgentSummary {
            id: r.get(0)?,
            name: r.get(1)?,
            created_at: r.get(2)?,
            last_seen_at: r.get(3)?,
            tab_count: r.get(4).unwrap_or(0),
            ready_count: r.get(5).unwrap_or(0),
            note,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Default agent name when the client doesn't specify. Auto-generated per
/// caller-process so distinct invocations from the same parent shell
/// (or agent harness) group together.
pub fn default_name() -> String {
    let ppid = nix::unistd::getppid().as_raw();
    format!("cli-ppid-{ppid}")
}
