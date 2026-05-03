//! Daemon lifecycle: lockfile → DB → socket listener → wait for shutdown.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::time::Instant;
use tokio::sync::oneshot;

mod socket;

pub struct Options {
    /// Open the GPUI dashboard window once available. Currently a no-op.
    pub with_window: bool,
}

pub struct Daemon {
    pub started_at: Instant,
    pub db: crate::db::DbPool,
    pub search: crate::search::SearchEngine,
    pub shutdown_tx: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
}

pub async fn run(opts: Options) -> Result<()> {
    let pid_path = crate::paths::pid_path()?;
    let socket_path = crate::paths::socket_path()?;
    let db_path = crate::paths::db_path()?;

    // ── single-instance lockfile ────────────────────────────────────────
    if let Some(existing) = read_live_pid(&pid_path)? {
        bail!(
            "daemon already running (pid {}). use `br daemon stop` first.",
            existing
        );
    }
    write_pid_file(&pid_path)?;
    let _pid_guard = scopeguard::OnDrop::new(|| {
        let _ = std::fs::remove_file(&pid_path);
    });

    // ── DB ──────────────────────────────────────────────────────────────
    let db = crate::db::open(&db_path)?;
    tracing::info!("db ready at {}", db_path.display());

    let search = crate::search::SearchEngine::init()?;
    tracing::info!("search engine ready");

    // ── socket listener ─────────────────────────────────────────────────
    if socket_path.exists() {
        // Stale socket from a crashed daemon — pid lock proved no live owner.
        std::fs::remove_file(&socket_path).ok();
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let daemon = std::sync::Arc::new(Daemon {
        started_at: Instant::now(),
        db,
        search,
        shutdown_tx: tokio::sync::Mutex::new(Some(shutdown_tx)),
    });

    if opts.with_window {
        tracing::info!("(window mode requested; UI lands in M3 — running headless for now)");
    }

    tracing::info!("listening on {}", socket_path.display());
    let listener_task = tokio::spawn(socket::serve(daemon.clone(), socket_path.clone()));

    // ── wait for shutdown signal (frame-based or SIGINT) ────────────────
    tokio::select! {
        _ = shutdown_rx => {
            tracing::info!("shutdown requested via socket");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received");
        }
    }

    listener_task.abort();
    let _ = std::fs::remove_file(&socket_path);
    tracing::info!("daemon stopped");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────

fn write_pid_file(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("writing pid file {}", path.display()))?;
    write!(f, "{}", std::process::id())?;
    Ok(())
}

/// Returns Some(pid) if the file exists and that pid is alive; None otherwise.
fn read_live_pid(path: &std::path::Path) -> Result<Option<u32>> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(pid) = s.trim().parse::<u32>() else {
        return Ok(None);
    };
    if is_alive(pid) {
        Ok(Some(pid))
    } else {
        // Stale; clean it up.
        std::fs::remove_file(path).ok();
        Ok(None)
    }
}

fn is_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

// Tiny inline scopeguard so we don't pull in another dep.
mod scopeguard {
    pub struct OnDrop<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> OnDrop<F> {
        pub fn new(f: F) -> Self {
            Self(Some(f))
        }
    }
    impl<F: FnOnce()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
}

// Re-exported so socket.rs can call it.
pub(crate) async fn request_shutdown(daemon: &Daemon) -> Result<()> {
    let mut guard = daemon.shutdown_tx.lock().await;
    if let Some(tx) = guard.take() {
        let _ = tx.send(());
        Ok(())
    } else {
        Err(anyhow!("shutdown already requested"))
    }
}
