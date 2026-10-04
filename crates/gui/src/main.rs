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

//! Daily driver is the dev binary — the template's `not(debug_assertions)`
//! guard would hand every launch a console window before the GUI shows.
#![windows_subsystem = "windows"]

use clap::Parser;
use tauri::Manager;

mod shell;

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
pub(crate) struct Gui {
    pub(crate) host: sunmao::HostHandle,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) clients: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, (u64, sunmao::Client)>>,
    >,
    pub(crate) client_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) channels: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tauri::ipc::Channel<serde_json::Value>>>,
    >,
    /// latest `sunmao://` URL seen before any page channel attached —
    /// flushed to the first `session_events` attach so a cold-start deep
    /// link isn't dropped while webviews are still booting
    pub(crate) pending_link: std::sync::Mutex<Option<String>>,
    pub(crate) zooms: std::sync::Mutex<std::collections::HashMap<String, f64>>,
}

/// The event channel — ws's replacement (GUI.md §8). The page hands us a
/// JS Channel; we attach a fresh host `Client` (hello + replay are sent
/// by `HostHandle::client` verbatim, same as a new ws connection), then
/// forward every outbound frame as a parsed JSON object — the page's
/// `onmessage` receives objects, not strings.
///
/// `Window`, not `WebviewWindow`: once a dock browser tab adds a guest
/// child webview, `is_webview_window()` is false and a `WebviewWindow`
/// argument stops resolving, which would take the whole transport down
/// (see the trap note in `shell.rs`).
#[tauri::command]
async fn session_events(
    events: tauri::ipc::Channel<serde_json::Value>,
    win: tauri::Window,
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
/// client so sibling windows stay independent tabs. `Window` for the
/// same reason `session_events` takes it.
#[tauri::command]
async fn host_call(
    msg: serde_json::Value,
    win: tauri::Window,
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
    // `get_window`, not `get_webview_window`: the latter is None for a
    // window hosting a dock guest, which would silently drop the focus.
    if let Some(w) = app.get_window("main") {
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
            shell::shell_win,
            shell::shell_drag,
            shell::shell_open,
            shell::shell_notify,
            shell::shell_zoom,
            shell::shell_pick_dir,
            shell::shell_webview,
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
            .initialization_script(shell::SHELL_SHIM)
            .build()?;
            // tray: the window's X hides to the system tray instead of
            // quitting — sessions and the host runtime keep running in the
            // background; left click or 显示 brings the window back
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
                let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
                let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&show, &quit])?;
                let icon = app.default_window_icon().cloned().expect("bundle icon");
                TrayIconBuilder::with_id("main")
                    .icon(icon)
                    .menu(&menu)
                    .tooltip("sunmao")
                    .on_menu_event(|app, e| match e.id().as_ref() {
                        "show" => {
                            // `windows()`, not `webview_windows()`: the dock's
                            // guest webview makes the main window stop being a
                            // "webview window", and hiding to tray would then
                            // leave no way back
                            for w in app.windows().values() {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, e| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = e
                        {
                            for w in tray.app_handle().windows().values() {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    })
                    .build(app)?;
            }
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
        // X is a hide-to-tray, not a quit — the agent runtime and any
        // in-flight sessions stay alive; real exit is the tray's 退出.
        // Auxiliary win-N windows still close for real.
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("sunmao gui");
}
