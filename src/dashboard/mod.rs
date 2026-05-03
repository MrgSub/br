//! GPUI dashboard — three-pane: agents | tabs | detail.
//!
//! Runs as its own process. Auto-spawns the daemon if needed (`br` / `br
//! dashboard`). Reads SQLite directly (WAL mode, concurrent-safe with the
//! daemon) and `tabs/<id>.md` from disk. Polls every second.

use anyhow::Result;
use gpui::{
    div, prelude::*, px, rgb, App, Application, Bounds, Context, ElementId, IntoElement, Rgba,
    SharedString, Window, WindowBounds, WindowOptions,
};
use rusqlite::params;
use std::time::Duration;

use crate::db::{self, DbPool};

mod theme {
    pub const BG: u32 = 0x111317;
    pub const BG_PANE: u32 = 0x171a20;
    pub const BG_HOVER: u32 = 0x1f242c;
    pub const BG_SELECT: u32 = 0x2d3641;
    pub const BORDER: u32 = 0x232831;
    pub const FG: u32 = 0xd6dce4;
    pub const FG_DIM: u32 = 0x8b94a3;
    pub const FG_TITLE: u32 = 0xffffff;
    pub const STATUS_OK: u32 = 0x4cc38a;
    pub const STATUS_FAIL: u32 = 0xff6e7a;
    pub const STATUS_PEND: u32 = 0xc3a64c;
}

#[inline]
fn col(c: u32) -> Rgba {
    rgb(c)
}

// ── Data shapes ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct AgentRow {
    id: String,
    name: SharedString,
    tab_count: i64,
}

#[derive(Clone, Debug)]
struct TabRow {
    id: String,
    agent_id: String,
    url: SharedString,
    title: Option<SharedString>,
    source: Option<SharedString>,
    status: SharedString,
    opened_at: i64,
}

// ── Dashboard entity ───────────────────────────────────────────────────────

struct Dashboard {
    db: DbPool,
    agents: Vec<AgentRow>,
    tabs: Vec<TabRow>,
    selected_agent: Option<String>, // None = "all"
    selected_tab: Option<TabRow>,
    selected_md: Option<SharedString>,
}

impl Dashboard {
    fn new(db: DbPool, cx: &mut Context<Self>) -> Self {
        let mut me = Self {
            db,
            agents: Vec::new(),
            tabs: Vec::new(),
            selected_agent: None,
            selected_tab: None,
            selected_md: None,
        };
        me.refresh();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            if this
                .update(cx, |this, cx| {
                    this.refresh();
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
        me
    }

    fn refresh(&mut self) {
        let Ok(conn) = self.db.get() else { return };

        // Agents
        let mut agents = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT a.id, a.name, COUNT(t.id) as cnt
             FROM agents a LEFT JOIN tabs t ON t.agent_id = a.id
             GROUP BY a.id ORDER BY a.last_seen_at DESC LIMIT 50",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok(AgentRow {
                    id: r.get(0)?,
                    name: SharedString::from(r.get::<_, String>(1)?),
                    tab_count: r.get(2)?,
                })
            }) {
                for row in rows.flatten() {
                    agents.push(row);
                }
            }
        }
        self.agents = agents;

        // Tabs
        let mut tabs = Vec::new();
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<TabRow> {
            Ok(TabRow {
                id: r.get(0)?,
                agent_id: r.get(1)?,
                url: SharedString::from(r.get::<_, String>(2)?),
                title: r.get::<_, Option<String>>(3)?.map(SharedString::from),
                source: r.get::<_, Option<String>>(4)?.map(SharedString::from),
                status: SharedString::from(r.get::<_, String>(5)?),
                opened_at: r.get(6)?,
            })
        };
        match &self.selected_agent {
            Some(id) => {
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT id, agent_id, url, title, source, status, opened_at
                     FROM tabs WHERE agent_id = ?1
                     ORDER BY opened_at DESC LIMIT 200",
                ) {
                    if let Ok(rows) = stmt.query_map(params![id], map_row) {
                        for row in rows.flatten() {
                            tabs.push(row);
                        }
                    }
                }
            }
            None => {
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT id, agent_id, url, title, source, status, opened_at
                     FROM tabs ORDER BY opened_at DESC LIMIT 200",
                ) {
                    if let Ok(rows) = stmt.query_map([], map_row) {
                        for row in rows.flatten() {
                            tabs.push(row);
                        }
                    }
                }
            }
        }
        self.tabs = tabs;
    }

    fn select_agent(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.selected_agent = id;
        self.refresh();
        cx.notify();
    }

    fn select_tab(&mut self, tab: TabRow, cx: &mut Context<Self>) {
        self.selected_tab = Some(tab.clone());
        self.selected_md = match crate::registry::tabs::read_markdown(&tab.id) {
            Ok(s) => {
                let max = 200_000;
                let s = if s.len() > max {
                    format!(
                        "{}\n\n…[truncated; {} more bytes]\n",
                        &s[..max],
                        s.len() - max
                    )
                } else {
                    s
                };
                Some(SharedString::from(s))
            }
            Err(_) => Some(SharedString::from("(could not read tab markdown)".to_string())),
        };
        cx.notify();
    }
}

// ── Render ─────────────────────────────────────────────────────────────────

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total_tabs = self.tabs.len();
        let total_agents = self.agents.len();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(col(theme::BG))
            .text_color(col(theme::FG))
            .text_sm()
            // top bar
            .child(
                div()
                    .h(px(36.0))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(col(theme::BORDER))
                    .bg(col(theme::BG_PANE))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_color(col(theme::FG_TITLE)).child("br"))
                            .child(div().text_color(col(theme::FG_DIM)).child("· dashboard")),
                    )
                    .child(
                        div()
                            .text_color(col(theme::FG_DIM))
                            .child(format!("{total_agents} agents · {total_tabs} tabs")),
                    ),
            )
            // body
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_agents_pane(cx))
                    .child(self.render_tabs_pane(cx))
                    .child(self.render_detail_pane()),
            )
    }
}

impl Dashboard {
    fn render_agents_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total_tabs: i64 = self.agents.iter().map(|a| a.tab_count).sum();
        let all_active = self.selected_agent.is_none();

        let mut col_el = div()
            .id("agents-pane")
            .w(px(220.0))
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(col(theme::BORDER))
            .bg(col(theme::BG_PANE))
            .overflow_y_scroll()
            .child(pane_header("agents"))
            .child(agent_row_view(
                "agent-all",
                "all".into(),
                total_tabs,
                all_active,
                cx.listener(move |this, _, _, cx| this.select_agent(None, cx)),
            ));

        for a in self.agents.clone() {
            let is_active = self.selected_agent.as_deref() == Some(&a.id);
            let id = a.id.clone();
            let elem_id: ElementId = SharedString::from(format!("agent-{}", a.id)).into();
            col_el = col_el.child(agent_row_view(
                elem_id,
                a.name.clone(),
                a.tab_count,
                is_active,
                cx.listener(move |this, _, _, cx| {
                    this.select_agent(Some(id.clone()), cx);
                }),
            ));
        }
        col_el
    }

    fn render_tabs_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col_el = div()
            .id("tabs-pane")
            .w(px(440.0))
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(col(theme::BORDER))
            .bg(col(theme::BG_PANE))
            .overflow_y_scroll()
            .child(pane_header("tabs"));

        if self.tabs.is_empty() {
            return col_el.child(
                div()
                    .px_4()
                    .py_8()
                    .text_color(col(theme::FG_DIM))
                    .child("No tabs yet. Try `br fetch <url>` in another shell."),
            );
        }
        for t in self.tabs.clone() {
            let is_selected = self.selected_tab.as_ref().map(|s| &s.id) == Some(&t.id);
            let elem_id: ElementId = SharedString::from(format!("tab-{}", t.id)).into();
            let tab_clone = t.clone();
            col_el = col_el.child(tab_row_view(
                elem_id,
                &t,
                is_selected,
                cx.listener(move |this, _, _, cx| {
                    this.select_tab(tab_clone.clone(), cx);
                }),
            ));
        }
        col_el
    }

    fn render_detail_pane(&self) -> impl IntoElement {
        let mut col_el = div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w(px(0.))
            .child(pane_header("detail"));

        let Some(t) = &self.selected_tab else {
            return col_el.child(
                div()
                    .px_4()
                    .py_8()
                    .text_color(col(theme::FG_DIM))
                    .child("Select a tab to view its markdown."),
            );
        };

        let agent_name = self
            .agents
            .iter()
            .find(|a| a.id == t.agent_id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| SharedString::from(t.agent_id.clone()));

        col_el = col_el.child(
            div()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(col(theme::BORDER))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div().text_color(col(theme::FG_TITLE)).child(
                        t.title
                            .clone()
                            .unwrap_or_else(|| t.url.clone()),
                    ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(col(theme::FG_DIM))
                        .child(t.url.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(col(theme::FG_DIM))
                        .flex()
                        .gap_4()
                        .child(format!("agent: {agent_name}"))
                        .child(format!(
                            "source: {}",
                            t.source.clone().unwrap_or_else(|| "—".into())
                        ))
                        .child(format!("status: {}", t.status))
                        .child(format!("tab: {}", t.id)),
                ),
        );

        let body = self
            .selected_md
            .clone()
            .unwrap_or_else(|| SharedString::from("(loading…)"));
        col_el.child(
            div()
                .id("detail-body")
                .flex_1()
                .overflow_y_scroll()
                .p_4()
                .font_family("Menlo")
                .text_xs()
                .child(body),
        )
    }
}

// ── small UI helpers ───────────────────────────────────────────────────────

fn pane_header(label: &str) -> impl IntoElement {
    div()
        .h(px(28.0))
        .px_3()
        .flex()
        .items_center()
        .border_b_1()
        .border_color(col(theme::BORDER))
        .text_xs()
        .text_color(col(theme::FG_DIM))
        .child(label.to_string())
}

fn agent_row_view<F>(
    id: impl Into<ElementId>,
    name: SharedString,
    count: i64,
    active: bool,
    on_click: F,
) -> impl IntoElement
where
    F: Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
{
    let bg = if active { theme::BG_SELECT } else { theme::BG_PANE };
    div()
        .id(id)
        .px_3()
        .py_2()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .bg(col(bg))
        .hover(|s| s.bg(col(theme::BG_HOVER)))
        .cursor_pointer()
        .on_click(on_click)
        .child(div().truncate().child(name))
        .child(
            div()
                .text_xs()
                .text_color(col(theme::FG_DIM))
                .child(count.to_string()),
        )
}

fn tab_row_view<F>(
    id: impl Into<ElementId>,
    t: &TabRow,
    selected: bool,
    on_click: F,
) -> impl IntoElement
where
    F: Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
{
    let bg = if selected { theme::BG_SELECT } else { theme::BG_PANE };
    let status_color = match t.status.as_ref() {
        "ready" => theme::STATUS_OK,
        "failed" => theme::STATUS_FAIL,
        _ => theme::STATUS_PEND,
    };
    let title = t.title.clone().unwrap_or_else(|| t.url.clone());
    let source = t.source.clone().unwrap_or_else(|| "—".into());
    let age = format_age_ms(crate::registry::now_ms() - t.opened_at);

    div()
        .id(id)
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .border_b_1()
        .border_color(col(theme::BORDER))
        .bg(col(bg))
        .hover(|s| s.bg(col(theme::BG_HOVER)))
        .cursor_pointer()
        .on_click(on_click)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(col(status_color)),
                )
                .child(
                    div()
                        .truncate()
                        .text_color(col(theme::FG))
                        .child(title),
                ),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .text_xs()
                .text_color(col(theme::FG_DIM))
                .child(source)
                .child(format!("· {age}")),
        )
}

fn format_age_ms(d: i64) -> String {
    let s = (d.max(0)) / 1000;
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

// ── Entry point ────────────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    ensure_daemon_running()?;

    let db_path = crate::paths::db_path()?;
    let db = wait_for_db(&db_path, Duration::from_secs(5))?;

    Application::new().run(move |cx: &mut App| {
        cx.activate(true);
        let bounds = Bounds::centered(None, gpui::size(px(1200.0), px(760.0)), cx);
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("br".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let db = db.clone();
        let _ = cx.open_window(opts, |_, cx| cx.new(|cx| Dashboard::new(db, cx)));
    });
    Ok(())
}

fn ensure_daemon_running() -> Result<()> {
    let socket = crate::paths::socket_path()?;
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(&exe)
        .args(["daemon", "start", "--no-window"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("daemon failed to come up at {}", socket.display())
}

fn wait_for_db(path: &std::path::Path, timeout: Duration) -> Result<DbPool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match db::open(path) {
            Ok(p) => return Ok(p),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
