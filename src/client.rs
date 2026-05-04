//! Thin client used by `br ping`, `br status`, `br daemon stop`, etc.
//! Connects to the daemon's Unix socket, sends one frame, prints the reply.

use crate::fetch::FetchOptions;
use crate::proto::{read_frame, write_frame, FetchReq, Frame};
use anyhow::{anyhow, bail, Result};
use tokio::net::UnixStream;

async fn connect() -> Result<UnixStream> {
    let path = crate::paths::socket_path()?;
    UnixStream::connect(&path)
        .await
        .map_err(|e| anyhow!("could not connect to {}: {e} (is the daemon running?)", path.display()))
}

/// Connect to the daemon, autospawning it if no socket exists yet.
/// Used by long-lived clients (e.g. the MCP server) that don't want the
/// user to remember `br daemon start` first.
pub async fn connect_or_spawn() -> Result<UnixStream> {
    use std::time::{Duration, Instant};

    let path = crate::paths::socket_path()?;
    if let Ok(s) = UnixStream::connect(&path).await {
        return Ok(s);
    }

    // Spawn `br daemon start --no-window` detached and poll the socket.
    let exe = std::env::current_exe().map_err(|e| anyhow!("current_exe: {e}"))?;
    std::process::Command::new(&exe)
        .args(["daemon", "start", "--no-window"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("spawn daemon: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok(s);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("daemon failed to come up at {} within 8s", path.display());
}

/// Round-trip a request, autospawning the daemon if needed.
pub async fn round_trip_or_spawn(req: Frame) -> Result<Frame> {
    let mut stream = connect_or_spawn().await?;
    write_frame(&mut stream, &req).await?;
    match read_frame(&mut stream).await? {
        Some(f) => Ok(f),
        None => bail!("daemon closed connection without responding"),
    }
}

/// Round-trip without autospawning. For commands where "daemon not
/// running" is itself the answer the user wants (`ping`, `status`,
/// `daemon stop`).
async fn round_trip(req: Frame) -> Result<Frame> {
    let mut stream = connect().await?;
    write_frame(&mut stream, &req).await?;
    match read_frame(&mut stream).await? {
        Some(f) => Ok(f),
        None => bail!("daemon closed connection without responding"),
    }
}

pub async fn ping() -> Result<()> {
    match round_trip(Frame::Ping).await? {
        Frame::Pong => {
            println!("pong");
            Ok(())
        }
        Frame::Error { code, message } => bail!("daemon error: {code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn print_status() -> Result<()> {
    match round_trip(Frame::Status).await? {
        Frame::StatusResp(s) => {
            println!("pid:        {}", s.pid);
            println!("version:    {}", s.version);
            println!("uptime:     {}s", s.uptime_secs);
            println!("db:         {}", s.db_path);
            println!("socket:     {}", s.socket_path);
            Ok(())
        }
        Frame::Error { code, message } => bail!("daemon error: {code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn fetch(
    url: String,
    agent: Option<String>,
    options: FetchOptions,
    print_meta: bool,
    format: crate::OutputFormat,
) -> Result<()> {
    let req = Frame::Fetch(FetchReq { url, agent, options });
    // Autospawn: the daemon is an implementation detail to a user just
    // running `br fetch <url>`; they shouldn't have to remember to
    // start it first.
    match round_trip_or_spawn(req).await? {
        Frame::FetchOk(r) => {
            // JSON mode short-circuits the markdown-with-meta path: the
            // envelope already contains everything --meta would print.
            if format == crate::OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            if print_meta {
                eprintln!("# tab    {}", r.tab_id);
                eprintln!("# url    {}", r.canonical_url);
                eprintln!("# source {}", r.source.as_str());
                if let Some(t) = &r.title {
                    eprintln!("# title  {t}");
                }
                eprintln!();
            }
            print!("{}", r.markdown);
            if !r.markdown.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn search(req: crate::search::SearchReq, json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::Search(req)).await? {
        Frame::SearchOk(r) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            if let Some(err) = &r.regex_fallback_error {
                eprintln!("# regex compile error (fell back to literal): {err}");
            }
            eprintln!(
                "# {} hits across {}/{} files",
                r.hits.len(),
                r.files_with_matches,
                r.total_files_searched
            );
            for h in &r.hits {
                for c in &h.context_before {
                    println!("  {}", c);
                }
                println!("{}:{}  {}", h.tab_id, h.line_number, h.line);
                for c in &h.context_after {
                    println!("  {}", c);
                }
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn tab_read(req: crate::search::TabReadReq, json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::TabRead(req)).await? {
        Frame::TabReadOk(r) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            eprintln!("# tab    {}", r.tab_id);
            eprintln!("# url    {}", r.url);
            if let Some(t) = &r.title {
                eprintln!("# title  {t}");
            }
            eprintln!(
                "# lines  {}-{} of {}",
                r.returned_lines.0, r.returned_lines.1, r.total_lines
            );
            eprintln!();
            print!("{}", r.markdown);
            if !r.markdown.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn cache_stats(json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::CacheStats).await? {
        Frame::CacheStatsResp(s) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
                return Ok(());
            }
            println!("rows:          {}", s.rows);
            println!("  fresh:       {}", s.fresh_rows);
            println!("  expired:     {}", s.expired_rows);
            println!("on-disk md:    {} bytes ({})", s.total_md_bytes, human_bytes(s.total_md_bytes));
            println!("tabs dir:      {}", s.tabs_dir);
            if let Some(o) = s.oldest_fetched_at {
                println!("oldest fetch:  {}", iso_ms(o));
            }
            if let Some(n) = s.newest_fetched_at {
                println!("newest fetch:  {}", iso_ms(n));
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn cache_clear(all: bool, json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::CacheClear { also_drop_tabs: all }).await? {
        Frame::CacheClearResp(r) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            println!("cleared {} cache row(s)", r.cache_rows_removed);
            if all {
                println!("removed {} tab row(s) and {} .md file(s)", r.tab_rows_removed, r.tab_files_removed);
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn cache_get(url: String, json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::CacheGet { url }).await? {
        Frame::CacheGetResp { hit } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&hit)?);
                return Ok(());
            }
            match hit {
                Some(h) => {
                    println!("tab        {}", h.tab_id);
                    println!("url        {}", h.url);
                    println!("canonical  {}", h.canonical_url);
                    println!("source     {}", h.source);
                    if let Some(t) = &h.title {
                        println!("title      {t}");
                    }
                    println!("age        {}s", h.age_secs);
                    if let Some(exp) = h.expires_at {
                        println!("expires    {}", iso_ms(exp));
                    }
                }
                None => println!("(no cache entry)"),
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn human_bytes(b: i64) -> String {
    let (n, u) = if b >= 1 << 30 {
        (b as f64 / (1u64 << 30) as f64, "GB")
    } else if b >= 1 << 20 {
        (b as f64 / (1u64 << 20) as f64, "MB")
    } else if b >= 1 << 10 {
        (b as f64 / (1u64 << 10) as f64, "KB")
    } else {
        (b as f64, "B")
    };
    format!("{:.1} {}", n, u)
}

fn iso_ms(ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_millis(ms as u64);
    // RFC 3339 without external deps: pull seconds and format manually.
    let secs = dt.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let now_secs = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now_secs as i64 - secs as i64;
    if delta >= 0 {
        format!("{}s ago ({})", delta, secs)
    } else {
        format!("in {}s ({})", -delta, secs)
    }
}

pub async fn session_start(name: String, note: Option<String>, json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::SessionStart { name: name.clone(), note }).await? {
        Frame::SessionStartResp { id, name } => {
            if json {
                println!("{}", serde_json::json!({ "id": id, "name": name }));
                return Ok(());
            }
            // Eval-friendly export so users can do:
            //   eval "$(br session start --name foo)"
            // and have BR_AGENT set in their shell.
            println!("export BR_AGENT={}", shell_escape(&name));
            eprintln!("# session: {name} (id {id})");
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub async fn session_list(json: bool) -> Result<()> {
    match round_trip_or_spawn(Frame::SessionList).await? {
        Frame::SessionListResp(data) => {
            let rows = data.agents;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!("(no agents yet)");
                return Ok(());
            }
            println!(
                "{:<24}  {:<7}  {:<7}  {:<10}  {:<10}  {}",
                "NAME", "TABS", "READY", "LAST SEEN", "CREATED", "NOTE"
            );
            for r in &rows {
                let note = r.note.as_deref().unwrap_or("");
                println!(
                    "{:<24}  {:<7}  {:<7}  {:<10}  {:<10}  {}",
                    truncate(&r.name, 24),
                    r.tab_count,
                    r.ready_count,
                    age_short(r.last_seen_at),
                    age_short(r.created_at),
                    truncate(note, 50),
                );
            }
            Ok(())
        }
        Frame::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

pub fn session_current(json: bool) -> Result<()> {
    let val = std::env::var("BR_AGENT").ok();
    if json {
        println!("{}", serde_json::json!({ "BR_AGENT": val }));
        return Ok(());
    }
    match val {
        Some(v) => println!("{v}"),
        None => {
            eprintln!("BR_AGENT is not set; fetches will be attributed to the");
            eprintln!("auto-generated `cli-ppid-<pid>` agent of each shell.");
            eprintln!();
            eprintln!("To start a session for this shell:");
            eprintln!("  eval \"$(br session start --name your-name)\"");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn age_short(ms: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = (now - ms) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub async fn stop_daemon() -> Result<()> {
    match round_trip(Frame::Shutdown).await? {
        Frame::Ok => {
            println!("shutdown requested");
            Ok(())
        }
        Frame::Error { code, message } => bail!("daemon error: {code}: {message}"),
        other => bail!("unexpected reply: {other:?}"),
    }
}
