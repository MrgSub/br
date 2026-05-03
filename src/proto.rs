//! Wire protocol for the Unix-socket IPC.
//!
//! Frames: 4-byte little-endian length prefix, then a UTF-8 JSON-encoded
//! `Frame`. Connection-oriented; both sides may send multiple frames.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// All client→daemon and daemon→client messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    // ── client → daemon ────────────────────────────────────────────────
    /// Smoke test.
    Ping,
    /// Ask the daemon to shut down cleanly.
    Shutdown,
    /// Ask for a status snapshot.
    Status,
    /// Fetch a URL through the strategy waterfall, register a tab.
    Fetch(FetchReq),
    /// Content search across cached tab markdowns.
    Search(crate::search::SearchReq),
    /// Read (a slice of) a tab.
    TabRead(crate::search::TabReadReq),
    /// Cache: stats / clear / get.
    CacheStats,
    CacheClear {
        #[serde(default)]
        also_drop_tabs: bool,
    },
    CacheGet {
        url: String,
    },
    /// Sessions / agents.
    SessionStart {
        name: String,
        #[serde(default)]
        note: Option<String>,
    },
    SessionList,

    // ── daemon → client ────────────────────────────────────────────────
    Pong,
    Ok,
    StatusResp(StatusResp),
    FetchOk(FetchOkResp),
    SearchOk(crate::search::SearchResp),
    TabReadOk(crate::search::TabReadResp),
    CacheStatsResp(crate::cache::CacheStats),
    CacheClearResp(crate::cache::ClearReport),
    CacheGetResp {
        hit: Option<crate::cache::CacheHit>,
    },
    SessionStartResp {
        id: String,
        name: String,
    },
    SessionListResp(SessionListData),
    Error { code: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListData {
    pub agents: Vec<crate::registry::agents::AgentSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchReq {
    pub url: String,
    pub agent: Option<String>,
    #[serde(default)]
    pub options: crate::fetch::FetchOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchOkResp {
    pub tab_id: String,
    pub markdown: String,
    pub source: crate::fetch::FetchSource,
    pub canonical_url: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResp {
    pub pid: u32,
    pub uptime_secs: u64,
    pub version: String,
    pub db_path: String,
    pub socket_path: String,
}

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024; // 64 MiB hard cap

pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io_err)?;
    let len = u32::try_from(bytes.len()).map_err(|_| io_err("frame too large"))?;
    if len > MAX_FRAME_BYTES {
        return Err(io_err("frame exceeds MAX_FRAME_BYTES"));
    }
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io_err("incoming frame exceeds MAX_FRAME_BYTES"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let frame: Frame = serde_json::from_slice(&buf).map_err(io_err)?;
    Ok(Some(frame))
}

fn io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}
