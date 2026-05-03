//! Daemon-side handle to the `br webkit-worker` subprocess.
//!
//! One singleton per daemon. Lazy-spawns the worker on first call, queues
//! render requests serially (Phase 1 worker accepts one in-flight render
//! at a time), and auto-restarts on crash with exponential backoff.

use anyhow::{anyhow, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use super::proto::{WebKitReq, WebKitResp};

const RESTART_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);
const RENDER_DEADLINE: Duration = Duration::from_secs(30);

/// How long to wait for a job before recycling the worker. WKWebView is
/// the heaviest dependency in the daemon (~150 MB RSS); when nobody's
/// using it, kill it and respawn lazily on the next render. The first
/// fetch after a long idle pays a ~500 ms cold-start; subsequent
/// fetches are warm again.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Override [`IDLE_TIMEOUT`] from `BR_WEBKIT_IDLE_SECS`. Useful for
/// integration tests and ad-hoc "does the recycle path actually fire?"
/// validation — set to a small number, do two fetches with a sleep in
/// between, watch the daemon log for the recycle line.
fn idle_timeout() -> Duration {
    if let Ok(s) = std::env::var("BR_WEBKIT_IDLE_SECS") {
        if let Ok(n) = s.parse::<u64>() {
            return Duration::from_secs(n);
        }
    }
    IDLE_TIMEOUT
}

/// Recycle the worker after this many successful renders. WKWebView
/// can leak under sustained use; restarting periodically keeps RSS
/// in check without an explicit memory probe. Picked to be far above
/// any single-agent-burst we've observed but low enough that long-
/// running daemons (days) don't accumulate gigabytes of cruft.
const MAX_NAVIGATIONS: u64 = 200;

#[derive(Debug, Clone)]
pub struct RenderOk {
    pub html: String,
    pub final_url: String,
}

struct RenderJob {
    url: String,
    opts: RenderOpts,
    reply: oneshot::Sender<Result<RenderOk>>,
}

/// Phase 4 knobs forwarded into the WebKit worker.
#[derive(Debug, Clone, Default)]
pub struct RenderOpts {
    pub wait_for: Option<String>,
    pub eval: Option<String>,
    pub auto_consent: bool,
}

pub struct WorkerHandle {
    /// Send-side of the queue. `worker_loop` drains it.
    queue: mpsc::UnboundedSender<RenderJob>,
}

static SHARED: OnceLock<WorkerHandle> = OnceLock::new();

/// Lazily-initialized process-wide handle. Safe to call from any tokio task.
pub fn shared() -> &'static WorkerHandle {
    SHARED.get_or_init(WorkerHandle::spawn)
}

impl WorkerHandle {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(worker_loop(rx));
        Self { queue: tx }
    }

    /// Render a URL via the worker. Times out after [`RENDER_DEADLINE`].
    pub async fn render(&self, url: &str, opts: RenderOpts) -> Result<RenderOk> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.queue
            .send(RenderJob {
                url: url.to_string(),
                opts,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("webkit worker queue closed"))?;
        let timeout = tokio::time::timeout(RENDER_DEADLINE, reply_rx)
            .await
            .map_err(|_| anyhow!("webkit render timeout after {RENDER_DEADLINE:?}"))?;
        timeout.map_err(|_| anyhow!("webkit worker dropped reply"))?
    }
}

/// Top-level loop: spawn-worker → drain jobs → restart on crash → repeat.
async fn worker_loop(mut jobs: mpsc::UnboundedReceiver<RenderJob>) {
    let mut backoff = RESTART_BACKOFF_INITIAL;
    // A side-buffer for one job that survived a crash, so we don't lose it.
    let mut pending_job: Option<RenderJob> = None;

    loop {
        let outcome = run_one_worker(&mut jobs, &mut pending_job).await;
        match outcome {
            WorkerOutcome::ChannelClosed => return,
            WorkerOutcome::Crashed(reason) => {
                tracing::warn!("webkit worker died ({reason}); restarting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RESTART_BACKOFF_MAX);
            }
            WorkerOutcome::CleanRestart => {
                // Worker shut down cleanly (idle timeout or navigation
                // cap). Don't eagerly respawn; wait for the *next*
                // request and then re-enter run_one_worker, which will
                // spawn a fresh subprocess. This is the whole point of
                // recycling: keeping RSS at zero while idle.
                backoff = RESTART_BACKOFF_INITIAL;
                match jobs.recv().await {
                    Some(j) => {
                        debug_assert!(pending_job.is_none());
                        pending_job = Some(j);
                    }
                    None => return,
                }
            }
        }
    }
}

enum WorkerOutcome {
    ChannelClosed,
    Crashed(String),
    /// Worker shut down on its own (idle timeout, navigation cap).
    /// The outer loop pauses until the next request before respawning.
    CleanRestart,
}

async fn run_one_worker(
    jobs: &mut mpsc::UnboundedReceiver<RenderJob>,
    pending_job: &mut Option<RenderJob>,
) -> WorkerOutcome {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return WorkerOutcome::Crashed(format!("current_exe: {e}")),
    };

    // Route worker stderr to its own log file so render-time noise
    // doesn't drown out the daemon's tracing output. Roll forward by
    // append (size cap is a separate cleanup concern, not relevant for
    // the typical install where the daemon restarts often enough).
    let log_stderr = match crate::paths::webkit_log_path()
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).map_err(Into::into))
    {
        Ok(f) => Stdio::from(f),
        Err(e) => {
            tracing::warn!("webkit log open failed ({e}); inheriting stderr");
            Stdio::inherit()
        }
    };

    let mut child = match Command::new(&exe)
        .arg("webkit-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log_stderr)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return WorkerOutcome::Crashed(format!("spawn: {e}")),
    };

    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => return WorkerOutcome::Crashed("missing stdin".into()),
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => BufReader::new(s),
        None => return WorkerOutcome::Crashed("missing stdout".into()),
    };

    // Wait for Hello.
    match read_frame::<_, WebKitResp>(&mut stdout).await {
        Ok(Some(WebKitResp::Hello { version })) => {
            tracing::info!("webkit worker up (v{version})");
        }
        Ok(Some(other)) => {
            return WorkerOutcome::Crashed(format!("unexpected first frame: {other:?}"));
        }
        Ok(None) => return WorkerOutcome::Crashed("worker closed before Hello".into()),
        Err(e) => return WorkerOutcome::Crashed(format!("Hello read: {e}")),
    }

    let mut next_id: u64 = 1;
    let mut nav_count: u64 = 0;
    let outcome = loop {
        // Recycle proactively once we hit the navigation cap. Send a
        // graceful Shutdown so the worker can release the WKWebView
        // cleanly; the outer loop respawns on the next request.
        if nav_count >= MAX_NAVIGATIONS {
            tracing::info!(
                "webkit worker hit MAX_NAVIGATIONS ({MAX_NAVIGATIONS}); recycling"
            );
            let _ = write_frame(&mut stdin, &WebKitReq::Shutdown).await;
            break WorkerOutcome::CleanRestart;
        }

        let job = match pending_job.take() {
            Some(j) => j,
            None => {
                // Idle-timeout the worker. If no request arrives within
                // IDLE_TIMEOUT, ask it to shut down. Saves ~150 MB RSS
                // when the daemon is idle for hours.
                let timeout = idle_timeout();
                match tokio::time::timeout(timeout, jobs.recv()).await {
                    Ok(Some(j)) => j,
                    Ok(None) => break WorkerOutcome::ChannelClosed,
                    Err(_) => {
                        tracing::info!(
                            "webkit worker idle for {timeout:?}; recycling"
                        );
                        let _ = write_frame(&mut stdin, &WebKitReq::Shutdown).await;
                        break WorkerOutcome::CleanRestart;
                    }
                }
            }
        };

        let id = next_id;
        next_id += 1;
        let req = WebKitReq::Render {
            id,
            url: job.url.clone(),
            wait_for: job.opts.wait_for.clone(),
            eval: job.opts.eval.clone(),
            auto_consent: job.opts.auto_consent,
        };
        if let Err(e) = write_frame(&mut stdin, &req).await {
            // Worker died mid-write; resurrect the job and let the outer
            // loop respawn.
            *pending_job = Some(job);
            break WorkerOutcome::Crashed(format!("write Render: {e}"));
        }

        match read_frame::<_, WebKitResp>(&mut stdout).await {
            Ok(Some(WebKitResp::RenderOk {
                id: rid,
                html,
                final_url,
            })) => {
                if rid != id {
                    let _ = job.reply.send(Err(anyhow!(
                        "id mismatch: expected {id}, got {rid}"
                    )));
                } else {
                    nav_count += 1;
                    let _ = job.reply.send(Ok(RenderOk { html, final_url }));
                }
            }
            Ok(Some(WebKitResp::RenderErr { id: rid, message })) => {
                if rid == id {
                    let _ = job.reply.send(Err(anyhow!("worker: {message}")));
                } else {
                    let _ = job.reply.send(Err(anyhow!(
                        "id mismatch: expected {id}, got {rid}: {message}"
                    )));
                }
            }
            Ok(Some(other)) => {
                let _ = job
                    .reply
                    .send(Err(anyhow!("unexpected response: {other:?}")));
            }
            Ok(None) | Err(_) => {
                // Worker died — notify caller and break to restart.
                let _ = job.reply.send(Err(anyhow!("worker exited mid-render")));
                break WorkerOutcome::Crashed("stdout closed".into());
            }
        }
    };

    let _ = child.kill().await;
    outcome
}

// ── tokio framed-JSON helpers ─────────────────────────────────────────────

async fn write_frame<W: AsyncWriteExt + Unpin, T: Serialize>(
    w: &mut W,
    frame: &T,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io_err)?;
    let len = u32::try_from(bytes.len()).map_err(|_| io_err("frame too large"))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> std::io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > 64 * 1024 * 1024 {
        return Err(io_err("incoming frame > 64 MiB"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf).map_err(io_err)?))
}

fn io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}


