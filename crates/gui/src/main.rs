//! sunmao desktop shell — GUI phase 2 (GUI.md §8).
//!
//! No HTTP anywhere in this process: the SAME multi-session host
//! (`sunmao::serve_host`) runs in-process and the page is served by the
//! `sunmao` custom URI scheme (`http://sunmao.localhost/` on WebView2 —
//! REST endpoints ride the same scheme). The ws channel degrades to a
//! Tauri `Channel` (`session_events` outbound / `host_call` inbound —
//! same JSON frames), and the MCP Apps sandbox proxy gets its own origin
//! through the `sunmao-sandbox` scheme — the SEP-1865 double-iframe still
//! needs a second origin.
//! The frameless titlebar/drag region the DESIGN.md workbench mandates is
//! rendered by the page itself (enabled under `window.__sunmaoShell`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use tauri::Manager;

/// The page lives on a custom scheme — Tauri never injects its JS API
/// (`window.__TAURI__`) there, but `__TAURI_INTERNALS__` (invoke +
/// transformCallback) IS injected into every frame, so the shell seam is
/// this script: `__sunmaoShell` carries the caption/drag verbs plus a
/// minimal `Channel` class — the wire twin of `@tauri-apps/api`'s
/// Channel (register a persistent callback, buffer out-of-order messages
/// by index, serialize to `__CHANNEL__:{id}` when invoked over JSON).
/// Host frames arrive as already-parsed JSON objects on `onmessage`.
const SHELL_SHIM: &str = r#"
window.__sunmaoShell = {
  win(op) { return window.__TAURI_INTERNALS__.invoke('shell_win', { op }); },
  drag() { return window.__TAURI_INTERNALS__.invoke('shell_drag'); },
  openExternal(path) { return window.__TAURI_INTERNALS__.invoke('shell_open', { path }); },
  notify(title, body) { return window.__TAURI_INTERNALS__.invoke('shell_notify', { title, body }); },
  zoom(op) { return window.__TAURI_INTERNALS__.invoke('shell_zoom', { op }); },
  setZoom(factor) { return window.__TAURI_INTERNALS__.invoke('shell_zoom', { op: factor }); },
  pickDir(dir) { return window.__TAURI_INTERNALS__.invoke('shell_pick_dir', { dir }); },
  Channel: class {
    constructor() {
      this.onmessage = () => {};
      this.idx = 0;
      this.pending = {};
      this.id = window.__TAURI_INTERNALS__.transformCallback(m => {
        if (m && m.end) {
          window.__TAURI_INTERNALS__.unregisterCallback(this.id);
          return;
        }
        const i = m.index;
        if (i === this.idx) {
          this.onmessage(m.message);
          this.idx++;
          while (this.idx in this.pending) {
            const p = this.pending[this.idx];
            delete this.pending[this.idx];
            this.onmessage(p);
            this.idx++;
          }
        } else {
          this.pending[i] = m.message;
        }
      });
    }
    toJSON() { return `__CHANNEL__:${this.id}`; }
  },
};
// In-shell zoom: Ctrl/Cmd+= (in), - (out), 0 (reset). Native webview zoom
// hotkeys stay off (IsZoomControlEnabled=false) so the keydown path is the
// single authority and the Rust-side level can't desync. state.js installs
// `sunmaoZoom` (op → factor → persist into .sunmao/ui.json via PUT /ui);
// before its scripts land the invoke path is the direct fallback.
window.addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && !e.altKey) {
    const op = (e.key === '=' || e.key === '+') ? 'in'
      : (e.key === '-' || e.key === '_') ? 'out'
      : (e.key === '0' || e.key === ')') ? 'reset' : null;
    if (op) {
      e.preventDefault();
      if (window.sunmaoZoom) window.sunmaoZoom(op);
      else window.__sunmaoShell.zoom(op);
    }
  }
});
"#;

/// Everything Tauri-side needs from the host runtime: the transport-free
/// host handle, the tokio runtime it lives on (command handlers run on
/// Tauri's own runtime — host work must spawn back onto `rt`), and each
/// window's viewer client keyed by window label — a second window gets
/// its own Client so tabs don't steal each other's frames. The `u64` is
/// a generation tag: on close/reload a dying channel removes its own
/// client only, never the fresher one that already replaced it.
/// `channels` keeps each window's session_events Channel so host-side
/// events that aren't client frames (deep links) can still reach the page;
/// `zooms` tracks per-window zoom — window-state doesn't persist it, so
/// the level is session-local.
struct Gui {
    host: sunmao::HostHandle,
    rt: tokio::runtime::Handle,
    clients: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, (u64, sunmao::Client)>>,
    >,
    client_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    channels: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tauri::ipc::Channel<serde_json::Value>>>,
    >,
    /// latest `sunmao://` URL seen before any page channel attached —
    /// flushed to the first `session_events` attach so a cold-start deep
    /// link isn't dropped while webviews are still booting
    pending_link: std::sync::Mutex<Option<String>>,
    zooms: std::sync::Mutex<std::collections::HashMap<String, f64>>,
}

#[tauri::command]
fn shell_win(app: tauri::AppHandle, win: tauri::WebviewWindow, op: &str) {
    match op {
        "min" => {
            let _ = win.minimize();
        }
        "max" => {
            if win.is_maximized().unwrap_or(false) {
                let _ = win.unmaximize();
            } else {
                let _ = win.maximize();
            }
        }
        "close" => {
            let _ = win.close();
        }
        "new" => {
            // a second window on the same host — its session_events attach
            // claims a client keyed by this window's label (per-window tab)
            let n = app.webview_windows().len() + 1;
            let label = format!("win-{n}");
            let _ = tauri::WebviewWindowBuilder::new(
                &app,
                label,
                tauri::WebviewUrl::External("http://sunmao.localhost/".parse().expect("gui url")),
            )
            .title("sunmao")
            .decorations(false)
            .disable_drag_drop_handler()
            .initialization_script(SHELL_SHIM)
            .build();
        }
        _ => {}
    }
}

/// The titlebar is the drag region — a left-button press anywhere on it
/// (except interactive controls) starts a native window drag.
#[tauri::command]
fn shell_drag(win: tauri::WebviewWindow) {
    let _ = win.start_dragging();
}

/// OS notification — the page fires this when a turn ends or an approval
/// lands while the window is unfocused (focus check stays page-side).
#[tauri::command]
fn shell_notify(app: tauri::AppHandle, title: &str, body: &str) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt as _;
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| e.to_string())
}

/// Open a local file with the OS default handler — artifact islands live on
/// the internal `sunmao` scheme, which no external browser can resolve, so
/// the page hands us the real filesystem path.
#[tauri::command]
fn shell_open(app: tauri::AppHandle, path: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt as _;
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| format!("open {path}: {e}"))
}

/// Native folder picker — the 新对话 project popover's 浏览 row calls
/// this (WebView2 has no directory chooser the page can reach). `dir`
/// seeds the dialog at the viewed session's project; the resolved value
/// is the picked path or null on cancel.
#[tauri::command]
fn shell_pick_dir(win: tauri::WebviewWindow, dir: Option<String>) -> Option<String> {
    use tauri_plugin_dialog::DialogExt as _;
    let mut d = win.dialog().file().set_title("选择项目目录");
    if let Some(p) = dir.filter(|p| !p.is_empty()) {
        d = d.set_directory(p);
    }
    d.blocking_pick_folder().map(|p| p.to_string())
}

/// Browser-style zoom ladder — ±20% steps, 20%..500% clamp.
fn zoom_step(cur: f64, dir: f64) -> f64 {
    (cur * dir).clamp(0.2, 5.0)
}

/// Page zoom driven by the Ctrl/Cmd+=/-/0 keydown listener in SHELL_SHIM.
/// Native webview hotkeys stay disabled so this map stays the single
/// source of truth for the factor. `op` is a ladder step ("in"/"out"/
/// "reset") or an absolute factor (the page's `setZoom` + the ui.json
/// restore send numbers — JS numbers serialize into `op` verbatim).
#[tauri::command]
fn shell_zoom(win: tauri::WebviewWindow, state: tauri::State<'_, Gui>, op: serde_json::Value) {
    let label = win.label().to_string();
    let next = match op.as_str() {
        Some(step) => {
            let zooms = state.zooms.lock().expect("zoom map");
            let cur = *zooms.get(&label).unwrap_or(&1.0);
            match step {
                "in" => zoom_step(cur, 1.2),
                "out" => zoom_step(cur, 1.0 / 1.2),
                "reset" => 1.0,
                _ => return,
            }
        }
        None => match op.as_f64() {
            Some(f) => f.clamp(0.2, 5.0),
            None => return,
        },
    };
    if win.set_zoom(next).is_ok() {
        state.zooms.lock().expect("zoom map").insert(label, next);
    }
}

/// The event channel — ws's replacement (GUI.md §8). The page hands us a
/// JS Channel; we attach a fresh host `Client` (hello + replay are sent
/// by `HostHandle::client` verbatim, same as a new ws connection), then
/// forward every outbound frame as a parsed JSON object — the page's
/// `onmessage` receives objects, not strings.
#[tauri::command]
async fn session_events(
    events: tauri::ipc::Channel<serde_json::Value>,
    win: tauri::WebviewWindow,
    state: tauri::State<'_, Gui>,
) -> Result<(), String> {
    let host = state.host.clone();
    let rt = state.rt.clone();
    let clients = state.inner().clients.clone();
    let channels = state.inner().channels.clone();
    let label = win.label().to_string();
    // the deep-link sink — a live Channel per window so URL events can
    // reach this page even before JS-side handling exists; a link that
    // arrived pre-attach replays now
    {
        let mut chans = channels.lock().expect("channel map");
        chans.insert(label.clone(), events.clone());
        if let Some(url) = state.pending_link.lock().expect("pending link").take() {
            let _ = events.send(serde_json::json!({"type": "deep_link", "url": url}));
        }
    }
    let seq = state
        .inner()
        .client_gen
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    // the host's tokio runtime owns every spawn inside `client()`
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let client = rt
        .spawn(async move { host.client(out_tx).await })
        .await
        .map_err(|e| e.to_string())?;
    clients.lock().await.insert(label.clone(), (seq, client));
    rt.spawn(async move {
        while let Some(text) = out_rx.recv().await {
            let v = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
            if events.send(v).is_err() {
                break;
            }
        }
        // the page went away (window closed / reload) — drop the client so
        // its bus forwarder aborts instead of leaking a live subscription;
        // a reload may have already installed a newer client for the label
        let mut map = clients.lock().await;
        if map.get(&label).map(|(g, _)| *g) == Some(seq) {
            map.remove(&label);
        }
        drop(map);
        channels.lock().expect("channel map").remove(&label);
    });
    Ok(())
}

/// One inbound frame from the page (`prompt`, `view`, `approval`, …) —
/// same dispatch the ws loop runs, routed to the *calling window's*
/// client so sibling windows stay independent tabs.
#[tauri::command]
async fn host_call(
    msg: serde_json::Value,
    win: tauri::WebviewWindow,
    state: tauri::State<'_, Gui>,
) -> Result<(), String> {
    let rt = state.rt.clone();
    let clients = state.inner().clients.clone();
    let label = win.label().to_string();
    rt.spawn(async move {
        if let Some((_, c)) = clients.lock().await.get_mut(&label) {
            c.handle(msg).await;
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// `HostResponse` → the scheme handler's `http::Response` shape.
fn scheme_response(r: sunmao::HostResponse) -> tauri::http::Response<Vec<u8>> {
    let mut b = tauri::http::Response::builder().status(r.status);
    for (k, v) in r.headers {
        b = b.header(k, v);
    }
    b.body(r.body).expect("static response")
}

/// `sunmao://` dispatch: focus the main window and surface the URL.
/// `sunmao://session/<id>` navigates that window's viewer — the same
/// `view` frame the session sidebar sends; unknown ids no-op silently on
/// the host. Every URL also goes out on each window's events channel as
/// `{"type":"deep_link","url":…}` so the page can grow its own routing.
fn open_deep_link(app: &tauri::AppHandle, url: &str) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    let gui = app.state::<Gui>();
    let session = url
        .strip_prefix("sunmao://")
        .and_then(|rest| rest.trim_start_matches('/').strip_prefix("session/"))
        .map(|id| id.trim_end_matches('/').to_string())
        .filter(|id| !id.is_empty());
    if let Some(id) = session {
        let (clients, rt) = (gui.clients.clone(), gui.rt.clone());
        rt.spawn(async move {
            if let Some((_, c)) = clients.lock().await.get_mut("main") {
                c.handle(serde_json::json!({"type": "view", "id": id}))
                    .await;
            }
        });
    }
    let chans = gui.channels.lock().expect("channel map");
    if chans.is_empty() {
        gui.pending_link
            .lock()
            .expect("pending link")
            .replace(url.to_string());
        return;
    }
    for ch in chans.values() {
        let _ = ch.send(serde_json::json!({"type": "deep_link", "url": url}));
    }
}

/// `windows_subsystem="windows"` gives us no console — so every
/// `Command::new("pwsh")` / deno_task_shell child under us gets a FRESH
/// console window in the user's face. Allocating one hidden console at
/// startup gives children something to inherit (`STARTF_USESHOWWINDOW`
/// isn't needed — inheritance alone suppresses the window when the
/// parent's console is hidden). One-time, inert if a console already
/// exists (a debug build launched from a terminal attaches to it).
#[cfg(windows)]
fn hide_child_console() {
    use windows::Win32::System::Console::{AllocConsole, GetConsoleWindow};
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
    unsafe {
        if AllocConsole().is_ok() {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_invalid() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    hide_child_console();
    sunmao::init_tracing();
    // Provider/config resolve through the same Cli the CLI uses — argv is
    // real here: `--cwd`/`--model`/`--preset`/`--loop` etc. work on the
    // desktop shell exactly as on `sunmao serve` (task launchers and MSI
    // shortcuts can carry a project dir). `sunmao://` deep links arrive as
    // the bare argv tail when Windows forwards one to a fresh process —
    // strip them before clap sees an unexpected positional; the deep-link
    // plugin reads env::args() itself for the launch URL.
    let argv: Vec<String> = std::env::args()
        .enumerate()
        .filter(|(i, a)| *i == 0 || !a.starts_with("sunmao://"))
        .map(|(_, a)| a)
        .collect();
    let cli = sunmao::Cli::parse_from(argv);
    // The host owns its own tokio runtime on a dedicated thread — Tauri's
    // main thread belongs to the Win32 event loop. No listeners anywhere:
    // the page reaches the host through the `sunmao` scheme + IPC.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("host tokio runtime");
        let handle = rt.handle().clone();
        rt.block_on(async move {
            match sunmao::serve_host(&cli).await {
                Ok(host) => {
                    let _ = tx.send((host, handle));
                }
                Err(e) => {
                    tracing::error!("gui host failed to start: {e:#}");
                }
            }
            // keep the runtime alive for the process lifetime
            std::future::pending::<()>().await;
        });
    });
    let (host, rt) = match rx.recv() {
        Ok(v) => v,
        Err(_) => panic!("gui host thread exited before wiring"),
    };
    let gui = Gui {
        host,
        rt,
        clients: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        client_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        channels: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        pending_link: std::sync::Mutex::new(None),
        zooms: std::sync::Mutex::new(std::collections::HashMap::new()),
    };
    tauri::Builder::default()
        // deep links spawn a second process on Windows — single-instance
        // forwards its argv here; handle_cli_arguments turns it into the
        // `deep-link://new-url` event our setup listener consumes
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            use tauri_plugin_deep_link::DeepLinkExt as _;
            app.deep_link().handle_cli_arguments(argv.iter());
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin(tauri_plugin_deep_link::init())
        // updater is capability-only this round: `plugins.updater.pubkey`
        // and `endpoints` in tauri.conf.json are placeholders — fill them
        // before `cargo tauri build` for releases (`cargo tauri signer
        // generate` produces the keypair; the private key stays with the
        // maintainer). No code path calls `check` — update cadence is a
        // product decision.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(gui)
        // the whole REST surface, scheme-served — `HostHandle::request`
        // is the same route table `sunmao serve`'s axum fallback answers
        .register_asynchronous_uri_scheme_protocol("sunmao", |ctx, req, responder| {
            let gui = ctx.app_handle().state::<Gui>();
            let (host, rt) = (gui.host.clone(), gui.rt.clone());
            let method = req.method().as_str().to_string();
            let pq = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            let body = req.body().clone();
            rt.spawn(async move {
                responder.respond(scheme_response(host.request(&method, &pq, &body).await));
            });
        })
        // the sandbox proxy page — its own scheme = its own origin
        // (`http://sunmao-sandbox.localhost/`), as SEP-1865 requires
        .register_asynchronous_uri_scheme_protocol("sunmao-sandbox", |_, _, responder| {
            responder.respond(scheme_response(sunmao::HostResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
                body: sunmao::SANDBOX_PAGE.as_bytes().to_vec(),
            }));
        })
        .invoke_handler(tauri::generate_handler![
            shell_win,
            shell_drag,
            shell_open,
            shell_notify,
            shell_zoom,
            shell_pick_dir,
            session_events,
            host_call
        ])
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External("http://sunmao.localhost/".parse().expect("gui url")),
            )
            .title("sunmao")
            .inner_size(1440.0, 900.0)
            .min_inner_size(960.0, 640.0)
            // frameless — the page renders its own titlebar (DESIGN.md)
            .decorations(false)
            .disable_drag_drop_handler()
            .initialization_script(SHELL_SHIM)
            .build()?;
            // deep links: the plugin's launch-arg event already fired in
            // its setup, so drain get_current() after registering the live
            // listener — the MSI registers the `sunmao` scheme; dev builds
            // register it per-user so `start sunmao://session/…` works
            {
                use tauri_plugin_deep_link::DeepLinkExt as _;
                let app_handle = app.handle().clone();
                let dl = app.deep_link();
                dl.on_open_url(move |ev| {
                    for url in ev.urls() {
                        open_deep_link(&app_handle, url.as_str());
                    }
                });
                #[cfg(desktop)]
                if cfg!(debug_assertions) {
                    let _ = dl.register_all();
                }
                let app_handle = app.handle().clone();
                if let Ok(Some(urls)) = dl.get_current() {
                    for url in urls {
                        open_deep_link(&app_handle, url.as_str());
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("sunmao gui");
}
