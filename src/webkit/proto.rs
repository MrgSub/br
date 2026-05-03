//! Wire protocol between the daemon and a `br webkit-worker` subprocess.
//!
//! Same shape as `crate::proto`: 4-byte little-endian length prefix + UTF-8
//! JSON. We can't reuse `crate::proto::{read,write}_frame` directly because
//! the worker isn't a tokio context; this module provides sync helpers
//! and the daemon side wraps them in `tokio::task::spawn_blocking`.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Daemon → worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebKitReq {
    /// Render a URL and return its hydrated HTML.
    ///
    /// Optional fields drive Phase 4 post-ready hooks. All three are
    /// composed into a single JS "hook" snippet on the worker side and
    /// run *after* the readiness signal but *before* DOM extraction.
    Render {
        id: u64,
        url: String,
        /// CSS selector to wait for before extracting (capped at 5 s).
        /// Useful when the readiness heuristic clears before the page
        /// hydrates a particular element. None = no wait.
        #[serde(default)]
        wait_for: Option<String>,
        /// Arbitrary JS to run after `wait_for`. Errors are swallowed.
        /// None = no eval.
        #[serde(default)]
        eval: Option<String>,
        /// Run a baked-in dismissal script that clicks common cookie /
        /// consent / geo-gate buttons. Cheap; safe to leave on.
        #[serde(default)]
        auto_consent: bool,
    },
    /// Tell the worker to exit cleanly.
    Shutdown,
}

/// Worker → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebKitResp {
    /// Sent once, after WebView is ready.
    Hello { version: String },
    /// Successful render.
    RenderOk {
        id: u64,
        html: String,
        final_url: String,
    },
    /// Render failed (timeout, navigation error, JS exception, …).
    RenderErr { id: u64, message: String },
}

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

pub fn write_frame_sync<W: Write>(w: &mut W, frame: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io_err)?;
    let len = u32::try_from(bytes.len()).map_err(|_| io_err("frame too large"))?;
    if len > MAX_FRAME_BYTES {
        return Err(io_err("frame exceeds MAX_FRAME_BYTES"));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame_sync<R: Read, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> std::io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io_err("incoming frame exceeds MAX_FRAME_BYTES"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    let parsed: T = serde_json::from_slice(&buf).map_err(io_err)?;
    Ok(Some(parsed))
}

fn io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}
