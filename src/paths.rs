//! Filesystem layout. macOS-only.
//!
//!   ~/Library/Application Support/br/
//!     br.sqlite        — registry + cache
//!     br.sock          — Unix socket for client/daemon IPC
//!     br.pid           — pid lockfile (single-instance enforcement)
//!     logs/            — (future)

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("dev", "br", "br").context("could not resolve project dirs")
}

pub fn data_dir() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    let p = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&p)
        .with_context(|| format!("creating data dir {}", p.display()))?;
    Ok(p)
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("br.sqlite"))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("br.sock"))
}

pub fn pid_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("br.pid"))
}

/// Where the WebKit worker subprocess writes its stderr. Routed here
/// (instead of inherited from the daemon) so render-time noise doesn't
/// pollute the daemon's log.
pub fn webkit_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("webkit.log"))
}

/// One markdown file per tab, named `<tab_id>.md`. Indexed by fff-search.
pub fn tabs_dir() -> Result<PathBuf> {
    let p = data_dir()?.join("tabs");
    std::fs::create_dir_all(&p)
        .with_context(|| format!("creating tabs dir {}", p.display()))?;
    Ok(p)
}

pub fn tab_md_path(tab_id: &str) -> Result<PathBuf> {
    Ok(tabs_dir()?.join(format!("{tab_id}.md")))
}

/// fff-search auxiliary databases (frecency, query history).
pub fn index_dir() -> Result<PathBuf> {
    let p = data_dir()?.join("index");
    std::fs::create_dir_all(&p)
        .with_context(|| format!("creating index dir {}", p.display()))?;
    Ok(p)
}
