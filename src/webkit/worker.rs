//! `br webkit-worker` — subprocess hosting an offscreen WKWebView.
//!
//! Reads `WebKitReq` frames from stdin, writes `WebKitResp` frames to stdout.
//! Tracing goes to stderr (never stdout — that's the protocol channel).
//!
//! Single-threaded from Cocoa's perspective: a stdin reader thread parses
//! frames and forwards them to the main thread via a tao `EventLoopProxy`.
//! All WebView interaction happens on the tao event loop (= the main run
//! loop on macOS).

use anyhow::Result;
use std::io::{stdin, stdout, BufReader, Stdout, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::WindowBuilder,
};
use wry::{PageLoadEvent, WebViewBuilder};

use super::proto::{read_frame_sync, write_frame_sync, WebKitReq, WebKitResp};

/// Internal events posted to the tao loop.
#[derive(Debug, Clone)]
enum WorkerEvent {
    Render {
        id: u64,
        url: String,
        wait_for: Option<String>,
        eval: Option<String>,
        auto_consent: bool,
    },
    /// Page reached `PageLoadEvent::Finished`. Kicks off ready-polling
    /// for the in-flight render (if any).
    PageFinished,
    /// Poll `window.__brReady` once; on `true` advance to the post-ready
    /// hook (or directly to `Extract` if no hook is configured).
    PollReady { gen: u64, attempt: u32 },
    /// Page is ready; install the Phase 4 hook script and start polling
    /// `__brHookDone`. Posted from the `PollReady` callback because the
    /// callback runs off the main thread and can't touch `webview`.
    ReadyDone { gen: u64 },
    /// Poll `window.__brHookDone` once; on `true` post `Extract`,
    /// otherwise reschedule until the cap.
    PollHook { gen: u64, attempt: u32 },
    Extract,
    Shutdown,
}

/// Shared writer; main thread + completion handlers both write responses.
type SharedOut = Arc<Mutex<Stdout>>;

fn write_resp(out: &SharedOut, resp: &WebKitResp) {
    if let Ok(mut g) = out.lock() {
        let _ = write_frame_sync(&mut *g, resp);
    }
}

/// Hard ceiling per render. Backstop for the JS readiness signal.
const RENDER_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap on the JS-driven readiness wait, measured from `PageFinished`.
/// Above this we extract whatever we have. SPAs that genuinely keep
/// mutating beyond 12 s are usually animation-heavy and not worth
/// waiting on.
const READY_CAP: Duration = Duration::from_secs(12);
/// How often we ask the page if it's quiescent.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Tiny pause after `PageFinished` before the first poll, to let the
/// initialization script hook up its MutationObserver.
const FIRST_POLL_DELAY: Duration = Duration::from_millis(100);
/// Cap on the post-ready hook (wait_for + auto_consent + user eval).
/// This is *additional* to READY_CAP — the hook runs after readiness
/// fires.
const HOOK_CAP: Duration = Duration::from_secs(7);

/// Phase 4 hook params, copied from the `Render` request and held
/// alongside the in-flight render's state.
#[derive(Debug, Clone, Default)]
struct HookParams {
    wait_for: Option<String>,
    eval: Option<String>,
    auto_consent: bool,
}

/// Build the post-ready hook JS for the configured params.
///
/// Composes three optional behaviors into a single self-installing
/// script that sets `window.__brHookDone = true` once finished. The
/// main thread polls that flag.
///
/// 1. **wait_for**  — poll `document.querySelector(<selector>)` every
///    100 ms for up to 5 s.
/// 2. **auto_consent** — try a small list of known consent-library
///    button IDs first (OneTrust, TrustArc, Iubenda, Cookiebot,
///    Didomi). Fall back to text-matching `Accept`/`Agree`/`I am a US
///    Resident`/etc. on visible buttons & links. Click at most one.
/// 3. **eval** — user-supplied JS, wrapped in a try/catch. Errors are
///    swallowed; the hook always completes.
///
/// All three run in sequence on a single async task. Errors from any
/// step short-circuit nothing — we always set `__brHookDone` so the
/// poller doesn't hit its cap on a perfectly successful render.
fn build_hook_script(p: &HookParams) -> String {
    // Quote optional strings as JSON literals (`null` or `"..."`) so
    // they can be inlined safely.
    let wait_for = match &p.wait_for {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let user_eval = match &p.eval {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let auto_consent = if p.auto_consent { "true" } else { "false" };

    format!(
        r#"
(function () {{
  if (window.__brHookInstalled) return;
  window.__brHookInstalled = true;
  window.__brHookDone = false;
  var waitFor = {wait_for};
  var userEval = {user_eval};
  var autoConsent = {auto_consent};
  function sleep(ms) {{ return new Promise(function (r) {{ setTimeout(r, ms); }}); }}
  async function run() {{
    if (waitFor) {{
      var start = Date.now();
      while (Date.now() - start < 5000) {{
        try {{ if (document.querySelector(waitFor)) break; }} catch (e) {{ break; }}
        await sleep(100);
      }}
    }}
    if (autoConsent) {{
      // 1. Known consent-library button IDs.
      var ids = [
        'onetrust-accept-btn-handler', 'truste-consent-button',
        'iubenda-cs-accept-btn', 'cookiebot-accept', '_evidon-accept-button',
        'gdpr-cookie-consent-accept', 'didomi-notice-agree-button',
        'CybotCookiebotDialogBodyButtonAccept',
        'CybotCookiebotDialogBodyLevelButtonAccept',
      ];
      for (var i = 0; i < ids.length; i++) {{
        var el = document.getElementById(ids[i]);
        if (el && el.offsetParent !== null) {{ try {{ el.click(); }} catch (e) {{}}; break; }}
      }}
      // 2. Text-match on visible button-like elements. Patterns are
      //    deliberately loose: false-positives (e.g. clicking the wrong
      //    "Yes") cost a wasted render at worst, while false-negatives
      //    leave us stuck on the interstitial.
      var re = new RegExp(
        '^\\s*('
        + 'yes\\b'                                      // 'Yes, I am a US Resident', etc.
        + '|i(?:\'m| am)\\s+(?:a\\s+)?u\\.?s\\.?'        // 'I\'m a US ...' / 'I am US ...'
        + '|accept(?:\\s+all)?'                         // 'Accept' / 'Accept all'
        + '|agree(?:\\s+(?:and|&)\\s+continue)?'        // 'Agree' / 'Agree and continue'
        + '|got it'
        + '|allow(?:\\s+all)?'
        + '|i accept'
        + '|continue'
        + '|ok'
        + ')\\b',
        'i'
      );
      var els = document.querySelectorAll(
        'button, a, [role="button"], input[type="submit"], input[type="button"]'
      );
      for (var j = 0; j < els.length; j++) {{
        var e2 = els[j];
        var text = (e2.textContent || e2.value || '').trim();
        if (text && re.test(text) && e2.offsetParent !== null) {{
          try {{ e2.click(); }} catch (e) {{}}
          break;
        }}
      }}
      await sleep(500);
    }}
    if (userEval) {{
      try {{ (new Function(userEval))(); }} catch (e) {{}}
      await sleep(200);
    }}
    window.__brHookDone = true;
  }}
  run();
}})();
"#
    )
}

/// Injected at document-start on every navigation. Sets `window.__brReady`
/// to `true` once:
///   1. `document.readyState === 'complete'`, AND
///   2. a `MutationObserver` on `document.documentElement` has seen no
///      mutations for `QUIET_MS`.
/// Self-installing and idempotent (guarded by `__brReadyInstalled`).
const READY_SNIPPET: &str = r#"
(function () {
  if (window.__brReadyInstalled) return;
  window.__brReadyInstalled = true;
  window.__brReady = false;
  var QUIET_MS = 1500;
  var lastChange = Date.now();
  function bump() { lastChange = Date.now(); }
  function attach() {
    var root = document.documentElement;
    if (!root) { setTimeout(attach, 30); return; }
    try {
      new MutationObserver(bump).observe(root, {
        childList: true, subtree: true,
        attributes: true, characterData: true,
      });
    } catch (e) { /* no-op: page CSP or detached doc */ }
  }
  attach();
  function tick() {
    if (document.readyState === 'complete' &&
        (Date.now() - lastChange) >= QUIET_MS) {
      window.__brReady = true;
      return;
    }
    setTimeout(tick, 150);
  }
  tick();
})();
"#;

/// How often the parent-pid watcher checks whether the daemon is still
/// alive. On Unix, when the parent dies the kernel reparents us to
/// init/launchd (PID 1), so a getppid() == 1 reading is a reliable
/// "orphaned" signal. Sub-second polling is overkill; 5 s is fine.
const PARENT_PID_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn a watchdog that exits the process if our parent dies.
///
/// The daemon is responsible for shutting us down via the `Shutdown`
/// frame, but if the daemon itself is `kill -9`'d (or panics in a way
/// that doesn't run drop handlers), we'd otherwise linger as a zombie
/// process holding a WKWebView and ~150 MB of RSS. The watchdog
/// catches that case.
///
/// Captures `getppid()` at start so we can detect any reparent, not
/// just reparenting-to-init. (Some launchd configs reparent to a
/// different ancestor instead of PID 1.)
fn spawn_parent_watch() {
    // SAFETY: getppid is signal-safe and AS-Safe; trivially callable.
    let original_parent = unsafe { libc::getppid() };
    if original_parent <= 1 {
        // Already orphaned (or we *are* init for some reason). Don't
        // bother starting the watcher; the daemon should never spawn
        // us under these conditions and we'd self-exit immediately.
        return;
    }
    thread::spawn(move || loop {
        thread::sleep(PARENT_PID_POLL_INTERVAL);
        let now = unsafe { libc::getppid() };
        if now != original_parent {
            eprintln!(
                "[webkit-worker] parent died (was pid {original_parent}, now {now}); exiting"
            );
            // Hard exit: bypass drop handlers because we're racing the
            // daemon's signal of choice and want to release resources
            // immediately.
            std::process::exit(0);
        }
    });
}

pub fn run() -> Result<()> {
    spawn_parent_watch();

    // Build event loop with our user-event type.
    let event_loop = EventLoopBuilder::<WorkerEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Hidden offscreen window. WKWebView needs to be in a view hierarchy to
    // render but the window never becomes visible.
    let window = WindowBuilder::new()
        .with_title("br-webkit-worker")
        .with_visible(false)
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)?;

    let stdout_w: SharedOut = Arc::new(Mutex::new(stdout()));

    // Build the webview. We keep one WKWebView for the lifetime of the
    // worker (Phase 1); Phase 3 may rebuild per request for clean state.
    let load_proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_visible(false)
        .with_user_agent(concat!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) ",
            "AppleWebKit/605.1.15 (KHTML, like Gecko) ",
            "Version/17.0 Safari/605.1.15 br/",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_initialization_script(READY_SNIPPET)
        .with_on_page_load_handler(move |event, _url| {
            if matches!(event, PageLoadEvent::Finished) {
                // Just announce; the main thread decides whether anything
                // is in flight and starts polling. We do NOT extract here.
                let _ = load_proxy.send_event(WorkerEvent::PageFinished);
            }
        })
        .build(&window)?;

    // Stdin → event loop.
    let stdin_proxy = proxy.clone();
    thread::spawn(move || {
        let mut r = BufReader::new(stdin());
        loop {
            match read_frame_sync::<_, WebKitReq>(&mut r) {
                Ok(Some(WebKitReq::Render {
                    id,
                    url,
                    wait_for,
                    eval,
                    auto_consent,
                })) => {
                    if stdin_proxy
                        .send_event(WorkerEvent::Render {
                            id,
                            url,
                            wait_for,
                            eval,
                            auto_consent,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Some(WebKitReq::Shutdown)) | Ok(None) => {
                    let _ = stdin_proxy.send_event(WorkerEvent::Shutdown);
                    return;
                }
                Err(e) => {
                    eprintln!("[webkit-worker] frame read error: {e}");
                    let _ = stdin_proxy.send_event(WorkerEvent::Shutdown);
                    return;
                }
            }
        }
    });

    // Greet the parent so it knows the worker is up.
    write_resp(
        &stdout_w,
        &WebKitResp::Hello {
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    );

    // Per-request state. Single in-flight render at a time in Phase 1.
    let mut current: Option<u64> = None;
    // Generation counter — bumped on every Render so stragglers from a
    // prior poll loop are dropped on arrival.
    let mut render_gen: u64 = 0;
    let mut hook_params: HookParams = HookParams::default();
    let mut render_timeout_handle: Option<thread::JoinHandle<()>> = None;
    let timeout_proxy = proxy.clone();
    let poll_proxy = proxy.clone();

    fn schedule_poll_ready(
        proxy: &EventLoopProxy<WorkerEvent>,
        gen: u64,
        attempt: u32,
        delay: Duration,
    ) {
        let p = proxy.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = p.send_event(WorkerEvent::PollReady { gen, attempt });
        });
    }
    fn schedule_poll_hook(
        proxy: &EventLoopProxy<WorkerEvent>,
        gen: u64,
        attempt: u32,
        delay: Duration,
    ) {
        let p = proxy.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = p.send_event(WorkerEvent::PollHook { gen, attempt });
        });
    }

    /// Called the first time `window.__brReady === true`. If a Phase 4
    /// hook is configured, install it and start polling `__brHookDone`;
    /// otherwise jump straight to extraction.
    fn advance_after_ready(
        webview: &wry::WebView,
        params: &HookParams,
        proxy: &EventLoopProxy<WorkerEvent>,
        gen: u64,
    ) {
        let needs_hook = params.wait_for.is_some()
            || params.eval.is_some()
            || params.auto_consent;
        if !needs_hook {
            let _ = proxy.send_event(WorkerEvent::Extract);
            return;
        }
        let js = build_hook_script(params);
        // Fire-and-forget; the hook script sets `__brHookDone` itself.
        if let Err(e) = webview.evaluate_script(&js) {
            eprintln!("[webkit-worker] hook install err: {e}; extracting");
            let _ = proxy.send_event(WorkerEvent::Extract);
            return;
        }
        schedule_poll_hook(proxy, gen, 0, POLL_INTERVAL);
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(WorkerEvent::Render {
                id,
                url,
                wait_for,
                eval,
                auto_consent,
            }) => {
                if current.is_some() {
                    write_resp(
                        &stdout_w,
                        &WebKitResp::RenderErr {
                            id,
                            message: "another render in flight".into(),
                        },
                    );
                    return;
                }
                current = Some(id);
                render_gen = render_gen.wrapping_add(1);
                hook_params = HookParams {
                    wait_for,
                    eval,
                    auto_consent,
                };
                eprintln!(
                    "[webkit-worker] render id={id} url={url} wait_for={:?} auto_consent={} eval_len={}",
                    hook_params.wait_for,
                    hook_params.auto_consent,
                    hook_params.eval.as_deref().map(|s| s.len()).unwrap_or(0),
                );
                if let Err(e) = webview.load_url(&url) {
                    current = None;
                    write_resp(
                        &stdout_w,
                        &WebKitResp::RenderErr {
                            id,
                            message: format!("load_url: {e}"),
                        },
                    );
                    return;
                }

                // Hard timeout watchdog. If the page never fires Finished
                // (or polling somehow stalls), this guarantees we always
                // respond.
                let p = timeout_proxy.clone();
                render_timeout_handle = Some(thread::spawn(move || {
                    thread::sleep(RENDER_TIMEOUT);
                    let _ = p.send_event(WorkerEvent::Extract);
                }));
            }
            Event::UserEvent(WorkerEvent::PageFinished) => {
                if current.is_some() {
                    schedule_poll_ready(&poll_proxy, render_gen, 0, FIRST_POLL_DELAY);
                }
            }
            Event::UserEvent(WorkerEvent::PollReady { gen, attempt }) => {
                // Stale poll from a previous render — drop.
                if gen != render_gen || current.is_none() {
                    return;
                }
                // Attempts past the cap fall through to extract.
                let elapsed = Duration::from_millis(
                    FIRST_POLL_DELAY.as_millis() as u64
                        + POLL_INTERVAL.as_millis() as u64 * attempt as u64,
                );
                if elapsed >= READY_CAP {
                    eprintln!("[webkit-worker] ready-cap reached, advancing");
                    advance_after_ready(&webview, &hook_params, &poll_proxy, render_gen);
                    return;
                }
                let p = poll_proxy.clone();
                let g = render_gen;
                let next_attempt = attempt + 1;
                if let Err(e) = webview.evaluate_script_with_callback(
                    "window.__brReady === true",
                    move |raw| {
                        let ready = raw.trim() == "true";
                        // The callback runs off the tao main thread and
                        // mustn't touch `webview` directly. Post an event
                        // so the main loop can do the install.
                        if ready {
                            let _ = p.send_event(WorkerEvent::ReadyDone { gen: g });
                        } else {
                            schedule_poll_ready(&p, g, next_attempt, POLL_INTERVAL);
                        }
                    },
                ) {
                    eprintln!("[webkit-worker] poll eval err: {e}; extracting");
                    let _ = poll_proxy.send_event(WorkerEvent::Extract);
                }
            }
            Event::UserEvent(WorkerEvent::ReadyDone { gen }) => {
                if gen != render_gen || current.is_none() {
                    return;
                }
                advance_after_ready(&webview, &hook_params, &poll_proxy, render_gen);
            }
            Event::UserEvent(WorkerEvent::PollHook { gen, attempt }) => {
                if gen != render_gen || current.is_none() {
                    return;
                }
                let elapsed = Duration::from_millis(
                    POLL_INTERVAL.as_millis() as u64 * attempt as u64,
                );
                if elapsed >= HOOK_CAP {
                    eprintln!("[webkit-worker] hook-cap reached, extracting anyway");
                    let _ = poll_proxy.send_event(WorkerEvent::Extract);
                    return;
                }
                let p = poll_proxy.clone();
                let g = render_gen;
                let next_attempt = attempt + 1;
                if let Err(e) = webview.evaluate_script_with_callback(
                    "window.__brHookDone === true",
                    move |raw| {
                        let done = raw.trim() == "true";
                        if done {
                            let _ = p.send_event(WorkerEvent::Extract);
                        } else {
                            schedule_poll_hook(&p, g, next_attempt, POLL_INTERVAL);
                        }
                    },
                ) {
                    eprintln!("[webkit-worker] hook poll err: {e}; extracting");
                    let _ = poll_proxy.send_event(WorkerEvent::Extract);
                }
            }
            Event::UserEvent(WorkerEvent::Extract) => {
                let Some(id) = current.take() else { return };
                let out_clone = stdout_w.clone();
                let url_at_extract = webview.url().unwrap_or_default();
                if let Err(e) = webview.evaluate_script_with_callback(
                    "document.documentElement.outerHTML",
                    move |raw| {
                        // wry returns the JS expression's value as JSON. For a
                        // string-valued expression that means we get `"<html>…</html>"`
                        // back — quoted, with escapes. Decode it.
                        let html =
                            serde_json::from_str::<String>(&raw).unwrap_or(raw);
                        write_resp(
                            &out_clone,
                            &WebKitResp::RenderOk {
                                id,
                                html,
                                final_url: url_at_extract.clone(),
                            },
                        );
                    },
                ) {
                    write_resp(
                        &stdout_w,
                        &WebKitResp::RenderErr {
                            id,
                            message: format!("evaluate_script: {e}"),
                        },
                    );
                }
                render_timeout_handle = None;
            }
            Event::UserEvent(WorkerEvent::Shutdown) => {
                eprintln!("[webkit-worker] shutdown");
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
