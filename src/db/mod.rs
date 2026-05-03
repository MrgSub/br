//! SQLite registry + cache. WAL mode. Single connection pool.

use anyhow::{Context, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::Path;

pub type DbPool = Pool<SqliteConnectionManager>;

const MIGRATIONS: &[(&str, &str)] = &[
    ("001_init", include_str!("migrations/001_init.sql")),
    (
        "002_cache_indirect",
        include_str!("migrations/002_cache_indirect.sql"),
    ),
];

pub fn open(path: &Path) -> Result<DbPool> {
    // 1. One-shot connection: enable WAL + run migrations.
    //    Doing this on a single connection avoids a startup race where multiple
    //    pool connections try to upgrade the journal mode in parallel and one of
    //    them gets SQLITE_BUSY.
    {
        let mut conn = Connection::open(path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
    }

    // 2. Pool for the rest of the daemon's life. Cheap per-conn pragmas only.
    let manager = SqliteConnectionManager::file(path).with_init(|c: &mut Connection| {
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    });
    let pool = Pool::builder()
        .max_size(8)
        .build(manager)
        .with_context(|| format!("building pool for {}", path.display()))?;
    Ok(pool)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS schema_migrations (
            name      TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    for (name, sql) in MIGRATIONS {
        let already: bool = conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE name = ?1",
                [name],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if already {
            continue;
        }
        tracing::info!("applying migration {name}");
        conn.execute_batch(sql)
            .with_context(|| format!("running migration {name}"))?;
        conn.execute(
            "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
            rusqlite::params![name, now_ms()],
        )?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
