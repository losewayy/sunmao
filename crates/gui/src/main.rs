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
"#;

/// Everything Tauri-side needs from the host runtime: the transport-free
/// host handle, the tokio runtime it lives on (command handlers run on
/// Tauri's own runtime — host work must spawn back onto `rt`), and this
/// window's viewer client (one window → at most one).
struct Gui {
    host: sunmao::HostHandle,
    rt: tokio::runtime::Handle,
    client: std::sync::Arc<tokio::sync::Mutex<Option<sunmao::Client>>>,
}

#[tauri::command]
fn shell_win(win: tauri::WebviewWindow, op: &str) {
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
        _ => {}
    }
}

/// The titlebar is the drag region — a left-button press anywhere on it
/// (except interactive controls) starts a native window drag.
#[tauri::command]
fn shell_drag(win: tauri::WebviewWindow) {
    let _ = win.start_dragging();
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

/// The event channel — ws's replacement (GUI.md §8). The page hands us a
/// JS Channel; we attach a fresh host `Client` (hello + replay are sent
/// by `HostHandle::client` verbatim, same as a new ws connection), then
/// forward every outbound frame as a parsed JSON object — the page's
/// `onmessage` receives objects, not strings.
#[tauri::command]
async fn session_events(
    events: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, Gui>,
) -> Result<(), String> {
    let host = state.host.clone();
    let rt = state.rt.clone();
    let client_slot = state.inner().client.clone();
    // the host's tokio runtime owns every spawn inside `client()`
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let client = rt
        .spawn(async move { host.client(out_tx).await })
        .await
        .map_err(|e| e.to_string())?;
    rt.spawn(async move {
        while let Some(text) = out_rx.recv().await {
            let v = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
            if events.send(v).is_err() {
                break;
            }
        }
    });
    *client_slot.lock().await = Some(client);
    Ok(())
}

/// One inbound frame from the page (`prompt`, `view`, `approval`, …) —
/// same dispatch the ws loop runs, serialized through the client mutex.
#[tauri::command]
async fn host_call(msg: serde_json::Value, state: tauri::State<'_, Gui>) -> Result<(), String> {
    let rt = state.rt.clone();
    let client_slot = state.inner().client.clone();
    rt.spawn(async move {
        if let Some(c) = client_slot.lock().await.as_mut() {
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

fn main() {
    sunmao::init_tracing();
    // Provider/config resolve through the same Cli the CLI uses — argv is
    // real here: `--cwd`/`--model`/`--preset`/`--loop` etc. work on the
    // desktop shell exactly as on `sunmao serve` (task launchers and MSI
    // shortcuts can carry a project dir).
    let cli = sunmao::Cli::parse();
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
        client: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    };
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
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
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("sunmao gui");
}
