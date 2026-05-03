//! Unix-socket listener and per-connection request loop.

use crate::cache;
use crate::proto::{read_frame, write_frame, FetchOkResp, FetchReq, Frame, StatusResp};
use crate::registry::{agents, tabs};
use url::Url;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

pub async fn serve(daemon: Arc<super::Daemon>, path: PathBuf) {
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind socket {}: {e}", path.display());
            return;
        }
    };
    // Keep socket world-readable for the user only (default umask handles it).
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(daemon, stream).await {
                        tracing::warn!("connection ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("accept error: {e}");
                break;
            }
        }
    }
}

async fn handle_conn(daemon: Arc<super::Daemon>, mut stream: UnixStream) -> std::io::Result<()> {
    while let Some(frame) = read_frame(&mut stream).await? {
        let resp = dispatch(&daemon, frame).await;
        write_frame(&mut stream, &resp).await?;
    }
    Ok(())
}

async fn dispatch(daemon: &super::Daemon, frame: Frame) -> Frame {
    match frame {
        Frame::Ping => Frame::Pong,
        Frame::Status => Frame::StatusResp(status(daemon)),
        Frame::Shutdown => match super::request_shutdown(daemon).await {
            Ok(()) => Frame::Ok,
            Err(e) => Frame::Error {
                code: "shutdown_already".into(),
                message: e.to_string(),
            },
        },
        Frame::Fetch(req) => handle_fetch(daemon, req).await.unwrap_or_else(|e| Frame::Error {
            code: "fetch_failed".into(),
            message: format!("{e:#}"),
        }),
        Frame::Search(req) => match daemon.search.search(&req) {
            Ok(resp) => Frame::SearchOk(resp),
            Err(e) => Frame::Error {
                code: "search_failed".into(),
                message: format!("{e:#}"),
            },
        },
        Frame::TabRead(req) => match crate::search::read_tab(&daemon.db, &req) {
            Ok(resp) => Frame::TabReadOk(resp),
            Err(e) => Frame::Error {
                code: "tab_read_failed".into(),
                message: format!("{e:#}"),
            },
        },
        Frame::CacheStats => match cache::stats(&daemon.db) {
            Ok(s) => Frame::CacheStatsResp(s),
            Err(e) => Frame::Error {
                code: "cache_stats_failed".into(),
                message: format!("{e:#}"),
            },
        },
        Frame::CacheClear { also_drop_tabs } => {
            match cache::clear(&daemon.db, also_drop_tabs) {
                Ok(r) => Frame::CacheClearResp(r),
                Err(e) => Frame::Error {
                    code: "cache_clear_failed".into(),
                    message: format!("{e:#}"),
                },
            }
        }
        Frame::CacheGet { url } => match cache::get(&daemon.db, &url) {
            Ok(hit) => Frame::CacheGetResp { hit },
            Err(e) => Frame::Error {
                code: "cache_get_failed".into(),
                message: format!("{e:#}"),
            },
        },
        Frame::SessionStart { name, note } => {
            match agents::ensure_by_name_with_note(&daemon.db, &name, note.as_deref()) {
                Ok(id) => Frame::SessionStartResp { id, name },
                Err(e) => Frame::Error {
                    code: "session_start_failed".into(),
                    message: format!("{e:#}"),
                },
            }
        }
        Frame::SessionList => match agents::list(&daemon.db) {
            Ok(list) => Frame::SessionListResp(crate::proto::SessionListData { agents: list }),
            Err(e) => Frame::Error {
                code: "session_list_failed".into(),
                message: format!("{e:#}"),
            },
        },
        // Server-only frames received from a client are an error.
        other => Frame::Error {
            code: "unexpected_frame".into(),
            message: format!("daemon does not handle {other:?}"),
        },
    }
}

async fn handle_fetch(daemon: &super::Daemon, req: FetchReq) -> anyhow::Result<Frame> {
    let url = Url::parse(&req.url)
        .map_err(|e| anyhow::anyhow!("invalid url {:?}: {e}", req.url))?;

    // Cache lookup: fast path when we have a fresh result. Bypassed by
    // --no-cache and by --raw (raw mode produces non-canonical content).
    if !req.options.no_cache && !req.options.raw {
        if let Some(hit) = cache::lookup_fresh(&daemon.db, url.as_str())? {
            tracing::info!(
                "cache hit for {} (tab {}, age {}s)",
                url,
                hit.tab_id,
                hit.age_secs
            );
            let markdown = tabs::read_markdown(&hit.tab_id)?;
            return Ok(Frame::FetchOk(FetchOkResp {
                tab_id: hit.tab_id,
                markdown,
                source: parse_source(&hit.source),
                canonical_url: hit.canonical_url,
                title: hit.title,
            }));
        }
    }

    let agent_name = req.agent.unwrap_or_else(agents::default_name);
    let agent_id = agents::ensure_by_name(&daemon.db, &agent_name)?;
    let tab_id = tabs::open(&daemon.db, &agent_id, url.as_str())?;

    match crate::fetch::run(&url, &req.options).await {
        Ok(resp) => {
            tabs::record_success(&daemon.db, &tab_id, &resp)?;
            // Don't cache --raw fetches; they're a one-off mode.
            if !req.options.raw {
                if let Err(e) = cache::record(&daemon.db, url.as_str(), &tab_id, resp.source) {
                    tracing::warn!("cache::record failed: {e:#}");
                }
            }
            Ok(Frame::FetchOk(FetchOkResp {
                tab_id,
                markdown: resp.markdown,
                source: resp.source,
                canonical_url: resp.canonical_url.to_string(),
                title: resp.title,
            }))
        }
        Err(e) => {
            tabs::record_failure(&daemon.db, &tab_id, &format!("{e:#}"))?;
            Err(e)
        }
    }
}

/// Roundtrip a stored `source` string back to the enum. Falls back to
/// `Parse` for unknown values; cache hit replays just need *some* value
/// here for the wire response.
fn parse_source(s: &str) -> crate::fetch::FetchSource {
    use crate::fetch::FetchSource::*;
    match s {
        "accept_md" => AcceptMd,
        "cloudflare" => Cloudflare,
        "llms_full" => LlmsFull,
        "llms_index" => LlmsIndex,
        "adapter" => Adapter,
        "alt_link" => AltLink,
        "reader" => Reader,
        "headless" => Headless,
        _ => Parse,
    }
}

fn status(daemon: &super::Daemon) -> StatusResp {
    StatusResp {
        pid: std::process::id(),
        uptime_secs: daemon.started_at.elapsed().as_secs(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        db_path: crate::paths::db_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        socket_path: crate::paths::socket_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}
